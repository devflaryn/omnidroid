//! **A second real Android library from the stock APK, loaded and run through the translator.**
//!
//! The genuine open-source `libzstd-jni-1.5.7-6.so` (the Zstandard compression library, 603,960
//! bytes) shipped inside `lib/arm64-v8a/` of the same APK the rest of this suite is about. It is
//! loaded as a second guest library behind the same machinery `initializers.rs` uses for
//! `libroblox.so` — the loader maps and relocates it (M1), the translating backend executes its
//! ARM64 (M2), and the bionic adapter answers the imports it calls (M3) — and then its plain C
//! `ZSTD_*` API is called from the host and its answers are read back.
//!
//! This is a capability demonstration: proof that the same loader/boundary/backend that runs
//! `libroblox.so` runs an unrelated real ARM64 library and produces correct results from it. It is
//! **not** loaded by `omnidroid play`; nothing in the product path touches this library.
//!
//! Unlike `libroblox.so`, `libzstd` has **no** `DT_INIT_ARRAY` and no `JNI_OnLoad`: its exports are
//! callable directly with no JVM. Its 32 undefined imports are all standard bionic (which the
//! adapter binds) except four **weak** `ZSTD_trace_*` symbols, which are left null exactly as a
//! device's linker leaves them.
//!
//! When the APK is absent every test here **skips loudly** on the process's own stderr rather than
//! passing quietly.
//!
//! ```text
//! cargo test -p omni-android --release --test libzstd -- --test-threads=1 --nocapture
//! ```

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use omni_android::bionic::{Bionic, GuestProcess, HwcapPolicy, ThreadHost};
use omni_android::{
    AbiError, AbiResult, Blame, Boundary, BoundaryBuilder, GuestArg, GuestReturn, ImportCall,
};
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicCpu, DynarmicOptions};
use omni_cpu::{GuestAddr, GuestCpu, RunLimit, XReg};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
use omni_mem::{Backing, CommitPolicy, GuestSpace, MapExecutability, Placement, Protection};

/// The APK every real-library assertion in this project is about.
const APK_NAME: &str = "Roblox-2.738.1397.apk";
/// The zstd library shipped inside it, by exact file name and size.
const ZSTD_LIB: &str = "libzstd-jni-1.5.7-6.so";
const ZSTD_BYTES: u64 = 603_960;

/// Undefined, named symbols in `libzstd-jni-1.5.7-6.so`'s `.dynsym`.
///
/// 28 standard bionic imports the adapter binds, plus the four weak `ZSTD_trace_*` that a device's
/// linker leaves null.
const TOTAL_IMPORTS: usize = 32;
const WEAK_TRACE_IMPORTS: usize = 4;

/// The four **weak** undefined `ZSTD_trace_*` symbols.
///
/// zstd's tracing hooks: the codec calls each only after a null test (`if (ZSTD_trace_* != NULL)`),
/// so a device's linker leaves the weak reference null and the call is skipped. This layer does not
/// null a weak import automatically — it gives every undefined symbol a thunk slot — so, exactly as
/// `libroblox.so`'s weak `__gcov_*` imports are handled, each is **declared absent** so its GOT slot
/// resolves to null and the guest's own null test takes the un-traced path.
const WEAK_TRACE_SYMBOLS: [&str; WEAK_TRACE_IMPORTS] = [
    "ZSTD_trace_compress_begin",
    "ZSTD_trace_compress_end",
    "ZSTD_trace_decompress_begin",
    "ZSTD_trace_decompress_end",
];

/// `ZSTD_versionNumber()`: 1.5.7 encoded as `1*10000 + 5*100 + 7`.
const ZSTD_VERSION: u64 = 10_507;

/// Guest instructions one `ZSTD_*` call is allowed. Comfortably below `i64::MAX` (D16's footgun).
const CALL_LIMIT: RunLimit = RunLimit::Instructions(2_000_000_000);

/// Bytes of guest stack. 8 MiB, Android's main-thread size, lazily committed.
const STACK_BYTES: usize = 8 * 1024 * 1024;

/// Bytes of guest heap the test's `malloc` hands out of.
///
/// zstd's compress/decompress contexts allocate a workspace at level 3; 64 MiB is far more than one
/// round trip of a 4 KiB payload needs, since this allocator never reclaims (see [`GuestBump`]).
const HEAP_BYTES: usize = 64 * 1024 * 1024;

// ============================================================================= a guest malloc
//
// **`libroblox.so` bundles its own allocator (mimalloc, statically linked) and imports no libc
// `malloc`, so the bionic adapter deliberately implements none.** `libzstd` is an ordinary C
// library: it imports `malloc`/`calloc`/`free` from libc and `ZSTD_compress` allocates its context
// through them. So this test supplies them itself, bound into the same boundary as inline handlers,
// backed by a bump allocator over one mapped guest region. `free` is a no-op — a round trip does not
// outlive the instance, so leaking is correct and simplest — which is why the region is sized for the
// whole run rather than the live set.

/// A bump allocator over one mapped guest region. Reset for each loaded instance.
struct GuestBump {
    base: GuestAddr,
    size: usize,
    next: usize,
}

/// The current instance's heap. The suite is serialized, so exactly one instance is live at a time.
static HEAP: Mutex<GuestBump> = Mutex::new(GuestBump { base: 0, size: 0, next: 0 });

/// Hand out `len` bytes, 16-byte aligned, from the bump region.
fn heap_alloc(len: usize) -> AbiResult<GuestAddr> {
    let mut heap = HEAP.lock().unwrap_or_else(|p| p.into_inner());
    let start = heap.next.next_multiple_of(16);
    let len = len.max(1);
    if start.saturating_add(len) > heap.size {
        return Err(AbiError::Refused {
            symbol: "malloc".to_string(),
            address: heap.base,
            why: format!(
                "the test's {}-byte guest heap is exhausted: {} used, {len} more requested",
                heap.size, start
            ),
        });
    }
    heap.next = start + len;
    Ok(heap.base + start)
}

/// `void *malloc(size_t)`.
fn guest_malloc(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let size = call.args().next_u64()? as usize;
    let addr = heap_alloc(size)?;
    call.ret().u64(addr as u64);
    Ok(())
}

/// `void *calloc(size_t nmemb, size_t size)`. The bump region is freshly mapped and never reused, so
/// it is already zero; the explicit zeroing keeps `calloc`'s contract regardless of that.
fn guest_calloc(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (nmemb, size) = {
        let mut args = call.args();
        (args.next_u64()? as usize, args.next_u64()? as usize)
    };
    let total = nmemb.saturating_mul(size);
    let addr = heap_alloc(total)?;
    call.mem().write_bytes(addr, &vec![0u8; total.max(1)], call.blame(0))?;
    call.ret().u64(addr as u64);
    Ok(())
}

/// `void free(void *)`. A no-op: this allocator never reclaims.
fn guest_free(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    call.ret().void();
    Ok(())
}

/// **Serializes every test in this binary.** Bionic activation and the boundary use thread-locals,
/// and the load is not worth doing several times at once.
static SERIAL: Mutex<()> = Mutex::new(());

fn serialized() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate manifest directory has two ancestors")
        .to_path_buf()
}

/// `libzstd-jni-1.5.7-6.so` in the shared extraction cache, or `None` when the APK is absent.
fn cached_zstd_lib() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let apk_path = repo_root().join(APK_NAME);
        if !apk_path.is_file() {
            // Straight to the process's stderr: `eprintln!` is captured by libtest and thrown away
            // for a passing test, so a skipped assertion would be indistinguishable from a real
            // one. A skip must never read as a pass.
            let notice = format!(
                "\nSKIP: the libzstd capability test needs {APK_NAME}, which is not at {}. \
                 Every assertion against the real {ZSTD_LIB} was skipped.\n\n",
                apk_path.display()
            );
            let _ = std::io::Write::write_all(&mut std::io::stderr(), notice.as_bytes());
            return None;
        }
        let apk = omni_apk::Apk::open(&apk_path).expect("the real APK must open");
        let cache = omni_apk::LibraryCache::new(
            repo_root().join("target").join("omni-elf-fixtures").join("extraction-cache"),
        );
        let library = apk
            .native_libraries_for_abi("arm64-v8a")
            .into_iter()
            .find(|l| l.file_name() == ZSTD_LIB)
            .expect("the APK must contain libzstd-jni-1.5.7-6.so");
        let cached = cache.extract(&apk, library.entry()).expect("extract libzstd");
        Some(cached.path().to_path_buf())
    })
    .as_deref()
}

/// The bytes of the cache entry, read once for the whole process.
fn zstd_lib_bytes() -> Option<&'static [u8]> {
    static BYTES: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    BYTES
        .get_or_init(|| cached_zstd_lib().map(|p| std::fs::read(p).expect("read the cache entry")))
        .as_deref()
}

// ============================================================================= the loaded guest

/// A loaded `libzstd`, behind the whole compatibility layer, ready to call.
struct Loaded {
    space: Arc<GuestSpace>,
    backend: Arc<DynarmicBackend>,
    bionic: Arc<Bionic>,
    boundary: Arc<Boundary>,
    object: LoadedObject,
    stack_top: GuestAddr,
    _backing: Arc<Backing>,
}

impl Loaded {
    /// Load the real library behind a real boundary, or `None` when the APK is absent.
    fn load() -> Option<Self> {
        let path = cached_zstd_lib()?;
        let bytes = zstd_lib_bytes()?;
        assert_eq!(bytes.len() as u64, ZSTD_BYTES, "the genuine {ZSTD_LIB} is {ZSTD_BYTES} bytes");
        // `Executable` here: a section's protection caps every view's protection for the life of
        // the mapping (D11).
        let backing =
            Backing::open(path, MapExecutability::Executable).expect("open the cache entry");
        let elf = ElfImage::parse(bytes).expect("parse libzstd");
        let space = Arc::new(GuestSpace::new().expect("reserve a guest address space"));

        // The backend comes first: the bionic data objects need the process stack canary the TLS
        // arena holds, and declaring them must precede the loader resolving anything.
        let backend = Arc::new(
            DynarmicBackend::new(Arc::clone(&space), DynarmicOptions::default())
                .expect("a translating backend"),
        );

        let bionic = Bionic::new(Arc::clone(&space)).expect("a bionic instance");
        // `bind_into` binds the adapter's *entire* handler set — the same one `libroblox.so` uses,
        // not merely libzstd's 32 imports — so the function area must hold all of them. 1024 slots
        // is comfortably above the adapter's count (libroblox sizes the same region at 565).
        let builder = BoundaryBuilder::new(Arc::clone(&space), 1024, 4096)
            .expect("a thunk region for the adapter's handlers");
        bionic.bind_into(&builder).expect("bind every standard bionic handler");
        // libzstd imports libc `malloc`/`calloc`/`free`, which the adapter does not provide (see
        // the guest-malloc section). Supply them as inline handlers over a bump heap.
        builder.bind_inline("malloc", guest_malloc).expect("bind malloc");
        builder.bind_inline("calloc", guest_calloc).expect("bind calloc");
        builder.bind_inline("free", guest_free).expect("bind free");
        bionic
            .declare_data_into(
                &builder,
                &GuestProcess { stack_guard: backend.tls().stack_guard() },
            )
            .expect("declare and fill the data objects (e.g. __sF)");
        bionic.set_hwcap_policy(HwcapPolicy::Decline);
        bionic.set_memory_budget(2 << 30);
        let host: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&backend) as _;
        bionic.set_thread_host(ThreadHost::new(host)).expect("a thread host");
        bionic.set_log_to_stderr(false);

        // Leave the four weak `ZSTD_trace_*` imports null, as a device does. See
        // `WEAK_TRACE_SYMBOLS`.
        for symbol in WEAK_TRACE_SYMBOLS {
            builder.declare_absent(symbol).expect("declare a weak trace symbol absent");
        }

        let shared = Arc::new(builder);
        let object = {
            let mut providers = ProviderRegistry::new();
            providers.register(ProviderHandle(Arc::clone(&shared)));
            loader::load(&space, &backing, &elf, &providers, &LoaderConfig::default())
                .expect("libzstd must load with a thunk boundary")
        };
        let builder = Arc::try_unwrap(shared)
            .unwrap_or_else(|_| panic!("the registry must have released the builder"));
        let boundary = builder.finish();

        // `dl_iterate_phdr` must be faithful for the loaded image.
        bionic.register_image(&object.dl_phdr_info()).expect("register the loaded image");

        let page = space.page_size();
        let stack_base = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                STACK_BYTES,
                Protection::ReadWrite,
                CommitPolicy::Lazy,
            )
            .expect("a guest stack");
        let stack_top = (stack_base + STACK_BYTES) & !0xF;

        // The guest heap `malloc` hands out of, reset for this instance.
        let heap_base = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                HEAP_BYTES,
                Protection::ReadWrite,
                CommitPolicy::Lazy,
            )
            .expect("a guest heap");
        {
            let mut heap = HEAP.lock().unwrap_or_else(|p| p.into_inner());
            *heap = GuestBump { base: heap_base, size: HEAP_BYTES, next: 0 };
        }

        Some(Self { space, backend, bionic, boundary, object, stack_top, _backing: backing })
    }

    /// A guest thread: a bionic TLS block, a stack, and the boundary installed on its slots.
    fn thread(&self) -> DynarmicCpu {
        let mut cpu = self.backend.create_thread_with_tls().expect("a guest thread");
        self.boundary.install(&mut cpu).expect("install the boundary");
        cpu.set_sp(self.stack_top);
        cpu.set_x(XReg::new(30).expect("X30"), self.boundary.sentinel() as u64);
        cpu
    }

    /// The guest address of an exported symbol. libzstd is a shared object, so `st_value` is the
    /// vaddr and the guest address is `base + st_value`.
    fn export(&self, want: &str) -> GuestAddr {
        let bytes = zstd_lib_bytes().expect("the library bytes");
        let elf = ElfImage::parse(bytes).expect("parse libzstd");
        for s in elf.exported_symbols().expect("the exported symbols") {
            if s.name == want {
                return self.object.base + s.sym.st_value as usize;
            }
        }
        panic!("`{want}` is not an exported symbol of {ZSTD_LIB}");
    }

    /// Call an exported function by name and return its raw register result.
    fn call(&self, cpu: &mut DynarmicCpu, name: &str, args: &[GuestArg]) -> GuestReturn {
        let target = self.export(name);
        self.boundary
            .call_guest(cpu, name, target, args, CALL_LIMIT)
            .unwrap_or_else(|error| panic!("calling `{name}` failed: {error}"))
    }

    /// Map a fresh, eagerly committed read/write guest buffer at least `len` bytes.
    fn buffer(&self, len: usize) -> GuestAddr {
        let page = self.space.page_size();
        self.space
            .map_anonymous(
                Placement::Anywhere { align: page },
                len.max(1).next_multiple_of(page),
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("a guest buffer")
    }
}

/// A `SymbolProvider` that forwards to a shared builder.
struct ProviderHandle(Arc<BoundaryBuilder>);

impl omni_elf::loader::SymbolProvider for ProviderHandle {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn resolve(
        &self,
        request: &omni_elf::loader::SymbolRequest<'_>,
    ) -> Option<omni_elf::loader::SymbolValue> {
        self.0.resolve(request)
    }
}

// ============================================================================= the tests

/// **The library loads, has no initializers, and every import is accounted for.**
///
/// Each of libzstd's 32 undefined imports must either have a boundary slot (the 28 standard bionic
/// ones the adapter binds) or be one of the four weak `ZSTD_trace_*` that a device leaves null.
#[test]
fn it_loads() {
    let _guard = serialized();
    let Some(loaded) = Loaded::load() else { return };

    assert!(
        loaded.object.init_array.is_empty(),
        "libzstd declares no DT_INIT_ARRAY, so nothing runs before its exports are callable; got {}",
        loaded.object.init_array.len()
    );

    let bytes = zstd_lib_bytes().expect("the library bytes");
    let elf = ElfImage::parse(bytes).expect("parse libzstd");
    let imports = elf.undefined_symbols().expect("the undefined symbols");
    assert_eq!(imports.len(), TOTAL_IMPORTS, "libzstd's exact undefined-import count");

    let mut with_slot = 0usize;
    let mut weak_trace = 0usize;
    for import in &imports {
        match loaded.boundary.slot_named(import.name) {
            Some(_) => with_slot += 1,
            None => {
                // Only a weak `ZSTD_trace_*` may lack a slot: a device's linker leaves it null and
                // the loader's weak-undef null handling is what the guest already expects.
                assert!(
                    import.name.starts_with("ZSTD_trace_") && import.sym.is_weak(),
                    "`{}` has no boundary slot and is not a weak ZSTD_trace_* import — the guest's \
                     GOT would hold a null where a call target belongs",
                    import.name
                );
                weak_trace += 1;
            }
        }
    }
    assert_eq!(weak_trace, WEAK_TRACE_IMPORTS, "the four weak ZSTD_trace_* imports, left null");
    assert_eq!(with_slot, TOTAL_IMPORTS - WEAK_TRACE_IMPORTS, "every other import got a slot");

    eprintln!(
        "\nLIBZSTD it_loads: {} bytes at base {:#x}, 0 initializers, {} imports \
         ({} bound + {} weak ZSTD_trace_* left null)\n",
        bytes.len(),
        loaded.object.base,
        imports.len(),
        with_slot,
        weak_trace,
    );
}

/// **`ZSTD_versionNumber()` returns 10507**, run through the translator with no args.
#[test]
fn version() {
    let _guard = serialized();
    let Some(loaded) = Loaded::load() else { return };
    let _active = loaded.bionic.activate().expect("publish the instance to this thread");
    let mut cpu = loaded.thread();

    let version = loaded.call(&mut cpu, "ZSTD_versionNumber", &[]).x0;
    assert_eq!(version, ZSTD_VERSION, "ZSTD_versionNumber() for 1.5.7 is 10507");
    eprintln!("\nLIBZSTD version: ZSTD_versionNumber() = {version} (1.5.7)\n");
}

/// **A real compress/decompress round trip** through the translated zstd code: the engine mallocs
/// internally (malloc is bound and the bionic instance is activated), so this exercises the whole
/// stack — imports, allocation, and thousands of instructions of ARM64 codec — and the recovered
/// bytes are compared to the original.
#[test]
fn compress_then_decompress_round_trip() {
    let _guard = serialized();
    let Some(loaded) = Loaded::load() else { return };
    let _active = loaded.bionic.activate().expect("publish the instance to this thread");
    let mut cpu = loaded.thread();

    // A compressible 4 KiB payload: a short repeating pattern, so a correct codec shrinks it well.
    const SRC_LEN: usize = 4096;
    let input: Vec<u8> = (0..SRC_LEN).map(|i| (i % 64) as u8).collect();

    let src = loaded.buffer(SRC_LEN);
    loaded
        .boundary
        .mem()
        .write_bytes(src, &input, Blame::new("src", src, 0))
        .expect("fill the source buffer");

    // ZSTD_compressBound(4096) -> the worst-case output size for a 4 KiB input.
    let bound = loaded.call(&mut cpu, "ZSTD_compressBound", &[GuestArg::Int(SRC_LEN as u64)]).x0;
    assert!(bound >= SRC_LEN as u64, "the compress bound must cover the input; got {bound}");
    let dst = loaded.buffer(bound as usize);

    // ZSTD_compress(dst, bound, src, SRC_LEN, level=3) -> compressed size.
    let csize = loaded
        .call(
            &mut cpu,
            "ZSTD_compress",
            &[
                GuestArg::Pointer(dst),
                GuestArg::Int(bound),
                GuestArg::Pointer(src),
                GuestArg::Int(SRC_LEN as u64),
                GuestArg::Int(3),
            ],
        )
        .x0;
    assert_zstd_ok(&loaded, &mut cpu, csize, "ZSTD_compress");
    assert!(
        0 < csize && csize < SRC_LEN as u64,
        "the compressible payload must shrink: csize {csize}, input {SRC_LEN}"
    );

    // ZSTD_decompress(out, SRC_LEN, dst, csize) -> decompressed size.
    let out = loaded.buffer(SRC_LEN);
    let dsize = loaded
        .call(
            &mut cpu,
            "ZSTD_decompress",
            &[
                GuestArg::Pointer(out),
                GuestArg::Int(SRC_LEN as u64),
                GuestArg::Pointer(dst),
                GuestArg::Int(csize),
            ],
        )
        .x0;
    assert_zstd_ok(&loaded, &mut cpu, dsize, "ZSTD_decompress");
    assert_eq!(dsize, SRC_LEN as u64, "the whole input must come back out");

    let recovered = loaded
        .boundary
        .mem()
        .read_bytes(out, SRC_LEN, Blame::new("out", out, 0))
        .expect("read the decompressed buffer");
    assert_eq!(recovered, input, "the round trip must reproduce the original bytes exactly");

    eprintln!(
        "\nLIBZSTD round trip: {SRC_LEN} bytes -> {csize} compressed (bound {bound}) -> {dsize} \
         restored, bytes identical\n"
    );
}

/// A zstd size-or-error code: on error, print `ZSTD_getErrorName` and fail; otherwise assert
/// `ZSTD_isError` agrees it is not an error.
fn assert_zstd_ok(loaded: &Loaded, cpu: &mut DynarmicCpu, code: u64, what: &str) {
    let is_error = loaded.call(cpu, "ZSTD_isError", &[GuestArg::Int(code)]).x0;
    if is_error != 0 {
        let name_ptr = loaded.call(cpu, "ZSTD_getErrorName", &[GuestArg::Int(code)]).as_pointer();
        let name = loaded
            .boundary
            .mem()
            .cstr(name_ptr, Blame::new("ZSTD_getErrorName", name_ptr, 0))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|_| "<unreadable>".to_string());
        panic!("`{what}` returned zstd error code {code:#x}: {name}");
    }
}
