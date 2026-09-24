//! A real, loaded `libroblox.so` behind a real translating backend: everything M2 needs and
//! nothing it does not.
//!
//! The library goes through the **production path** — `omni-apk`'s extraction cache, then
//! `omni-elf`'s loader — so the 568,806 relocations really are applied and `PT_GNU_RELRO` really is
//! sealed before a single guest instruction runs. What is deliberately *not* done is
//! `init_array`: those 3,594 initializers call imported symbols, which needs the thunk boundary,
//! and that is M3.
//!
//! When the APK is absent every test that uses this **skips loudly** rather than passing: a golden
//! run that looked exactly like a real one would be the worst outcome available.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use omni_cpu::dynarmic::{DynarmicBackend, DynarmicCpu, DynarmicOptions};
use omni_cpu::{GuestAddr, GuestCpu, XReg};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
use omni_mem::{Backing, CommitPolicy, GuestSpace, MapExecutability, Placement, Protection};

/// The APK every golden assertion in this project is about.
pub const APK_NAME: &str = "Roblox-2.738.1397.apk";
/// The library M2 executes out of.
pub const MAIN_LIB: &str = "libroblox.so";

/// Bytes of guest stack per thread. Generous: `libroblox.so`'s stack-protected leaves use 32 bytes
/// of frame, and the cost of a reservation is address space, which D10 measured as free.
pub const STACK_BYTES: usize = 256 * 1024;

/// **Serializes every test in a binary that uses this fixture.**
///
/// `omni_mem::process_commit_charge` is a *process* quantity, and two tests measuring it at once
/// measure each other. Task 1 spent three rounds learning that about this exact counter, and
/// `omni-cpu`'s own `tests/bench.rs` carries the same guard for the same reason. It also keeps the
/// 109 MB load from happening several times at once, which would exhaust nothing but would make
/// every timing in the report a measurement of the scheduler.
static SERIAL: Mutex<()> = Mutex::new(());

pub fn serialized() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate manifest directory has two ancestors")
        .to_path_buf()
}

fn apk_path() -> PathBuf {
    repo_root().join(APK_NAME)
}

/// `libroblox.so` in the shared extraction cache, or `None` when the APK is absent.
///
/// The cache directory is the one `omni-elf`'s fixtures use, so a machine that has run either
/// suite does not extract 104 MiB again.
pub fn cached_main_lib() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        if !apk_path().is_file() {
            // Straight to the process's stderr: `eprintln!` is captured by libtest and then thrown
            // away for a passing test, so a skipped M2 gate would be indistinguishable from a
            // passing one.
            let notice = format!(
                "\nSKIP: the M2 gate needs {}, which is not at {}. Every assertion against real \
                 libroblox.so code was skipped.\n\n",
                APK_NAME,
                apk_path().display()
            );
            let _ = std::io::Write::write_all(&mut std::io::stderr(), notice.as_bytes());
            return None;
        }
        let apk = omni_apk::Apk::open(apk_path()).expect("the real APK must open");
        let cache = omni_apk::LibraryCache::new(
            repo_root().join("target").join("omni-elf-fixtures").join("extraction-cache"),
        );
        let library = apk
            .native_libraries_for_abi("arm64-v8a")
            .into_iter()
            .find(|l| l.file_name() == MAIN_LIB)
            .expect("the APK must contain libroblox.so");
        let cached = cache.extract(&apk, library.entry()).expect("extract libroblox.so");
        assert!(cached.is_directly_mappable(), "the cache entry must be page-aligned (D11)");
        Some(cached.path().to_path_buf())
    })
    .as_deref()
}

/// The bytes of the cache entry, so that parsing and mapping provably work from one file.
pub fn main_lib_bytes() -> Option<&'static [u8]> {
    static BYTES: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    BYTES
        .get_or_init(|| cached_main_lib().map(|p| std::fs::read(p).expect("read the cache entry")))
        .as_deref()
}

/// A loaded `libroblox.so`, a backend over its address space, and a guest stack.
pub struct Roblox {
    pub space: Arc<GuestSpace>,
    pub backend: DynarmicBackend,
    pub object: LoadedObject,
    /// The load bias: add it to a `p_vaddr` to get a guest address.
    pub base: GuestAddr,
    /// Lowest address of the guest stack mapping.
    pub stack_base: GuestAddr,
    /// What `SP` starts at: the top of the stack mapping, 16-byte aligned.
    pub stack_top: GuestAddr,
    /// The address planted in `X30`, which the guest's own `RET` lands on.
    pub sentinel: GuestAddr,
    /// Held so the file mapping outlives the load.
    _backing: Arc<Backing>,
}

impl Roblox {
    /// Load with the default backend options, or `None` when the APK is absent.
    pub fn load() -> Option<Self> {
        Self::with_options(DynarmicOptions::default())
    }

    pub fn with_options(options: DynarmicOptions) -> Option<Self> {
        let path = cached_main_lib()?;
        let bytes = main_lib_bytes()?;
        // `Executable` here and nowhere else: a section's protection caps every view's protection
        // for the life of the mapping, so `.text` can never be raised to executable later (D11).
        let backing =
            Backing::open(path, MapExecutability::Executable).expect("open the cache entry");
        let elf = ElfImage::parse(bytes).expect("parse libroblox.so");

        let space = Arc::new(super::high_guest_space());
        let object = loader::load(
            &space,
            &backing,
            &elf,
            // Nothing is provided: with no symbol provider all 565 imports stay unresolved and
            // bound to null, which is exactly right for M2 — a leaf function touches none of them,
            // and a function that did would fault rather than call into a fabricated stub.
            &ProviderRegistry::empty_provider(),
            &LoaderConfig::default(),
        )
        .expect("libroblox.so must load");

        let page = space.page_size();
        let stack_base = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                STACK_BYTES,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("a guest stack");
        // AArch64 requires `SP` to be 16-byte aligned for every `SP`-relative access; the mapping
        // is page-aligned, so the top already is, but the arithmetic is written out rather than
        // assumed.
        let stack_top = (stack_base + STACK_BYTES) & !0xF;

        // A page of its own for the return sentinel, so the address the guest's `RET` lands on is
        // one nothing else owns. `read_code` answers for it before touching guest memory, so it
        // needs no contents — but it must be an address that cannot collide with real code, and a
        // dedicated mapping is the only way to be sure of that.
        let sentinel = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                page,
                Protection::ReadExecute,
                CommitPolicy::Eager,
            )
            .expect("a return trampoline");

        let backend =
            DynarmicBackend::new(Arc::clone(&space), options).expect("a translating backend");
        // **The same assertion `harness/mod.rs` makes, and it matters more here.** A backend with no
        // demand pager silently *disarms the per-slice callback invariant* — `armed` is
        // `assert_callback_free_slices && owns_guest_paging` — and that invariant is the M2 gate's
        // only defence against a degradation no functional test can see. Every test in this binary
        // is `serialized()`, so the window is narrow; leaving it disarmable on the gate that proves
        // the milestone is the wrong place to economise.
        assert!(
            backend.owns_guest_paging(),
            "the M2 guest has no demand pager, so the per-slice callback invariant is disarmed and \
             every guest fault goes to dynarmic's own handler and the 30-49x path. The gate would \
             pass and prove less than it claims"
        );
        let base = object.base;
        Some(Self {
            space,
            backend,
            object,
            base,
            stack_base,
            stack_top,
            sentinel,
            _backing: backing,
        })
    }

    /// The guest address of a `p_vaddr` from the file.
    #[must_use]
    pub fn at(&self, vaddr: u64) -> GuestAddr {
        self.base + vaddr as usize
    }

    /// A guest thread: a bionic TLS block (D13), a stack, and the return sentinel armed.
    pub fn thread(&self) -> DynarmicCpu {
        let mut cpu = self.backend.create_thread_with_tls().expect("a guest thread");
        cpu.set_return_sentinel(self.sentinel).expect("arm the sentinel");
        cpu.set_sp(self.stack_top);
        cpu.set_x(XReg::new(30).expect("X30"), self.sentinel as u64);
        cpu
    }

    /// Re-arm a context for another call: `X30` and `SP` are clobbered by a call, so a caller that
    /// reuses a context has to put them back or the second run returns somewhere else.
    pub fn rearm(&self, cpu: &mut DynarmicCpu) {
        cpu.set_sp(self.stack_top);
        cpu.set_x(XReg::new(30).expect("X30"), self.sentinel as u64);
    }

    /// The instruction word at a guest address, read out of the loaded, relocated image.
    #[must_use]
    pub fn word_at(&self, address: GuestAddr) -> u32 {
        let ptr = self.space.ptr(address, 4).expect("a host pointer for the instruction");
        // SAFETY: `ptr` is `GuestSpace`'s own pointer for a mapped, committed four-byte range, and
        // D4's identity mapping makes the guest address a host address. No guest thread is running.
        unsafe { ptr.cast::<u32>().read_unaligned() }
    }

    /// Read a `u64` out of guest memory.
    #[must_use]
    pub fn read_u64(&self, address: GuestAddr) -> u64 {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: as `word_at`.
        unsafe { ptr.cast::<u64>().read_unaligned() }
    }

    /// Write a `u64` into guest memory.
    pub fn write_u64(&self, address: GuestAddr, value: u64) {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: as `word_at`; the range is checked by `ptr`.
        unsafe { ptr.cast::<u64>().write_unaligned(value) }
    }

    /// An address inside the guest space that no mapping covers, taken from the region list rather
    /// than guessed at.
    #[must_use]
    pub fn unmapped(&self) -> GuestAddr {
        self.space
            .regions()
            .into_iter()
            .find(|r| r.is_free() && r.len >= self.space.page_size())
            .map(|r| (r.start + r.len / 2) & !0xF)
            .expect("some free address space")
    }
}
