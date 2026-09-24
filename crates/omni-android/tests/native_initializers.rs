//! **M3's gate on the native backend**: all 3,594 static initializers of the real `libroblox.so`,
//! guest code running at EL0 under Hypervisor.framework, every import an exit-path crossing.
//!
//! ```text
//! CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=$PWD/tools/hvf_run.sh \
//!     cargo test -p omni-android --release --features native-hvf --test native_initializers \
//!     -- --test-threads=1 --nocapture
//! ```
//!
//! The same fixture as `tests/initializers.rs` -- the same loader, boundary, bionic instance and
//! eighteen data objects -- with the backend swapped, and the same evidence demanded of it: the
//! order of the initializers that returned, the pinned words they write, and the exact count of
//! image pointers the writable image gains. The one difference is the budget: the native backend
//! cannot count instructions and refuses a counted run, so each initializer runs unbounded and a
//! wall-clock watchdog halts a runaway through its `HaltHandle`.
//!
//! The measurement below (`#[ignore]`) times the cold run on both backends in one process.

#![cfg(all(feature = "native-hvf", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use omni_android::bionic::{Bionic, GuestProcess, HwcapPolicy, ThreadHost};
use omni_android::{Boundary, BoundaryBuilder, GuestArg};
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicOptions};
use omni_cpu::native::{NativeBackend, NativeOptions};
use omni_cpu::{GuestAddr, GuestCpu, GuestCpuBackend, RunLimit, XReg};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
use omni_mem::{Backing, CommitPolicy, GuestSpace, MapExecutability, Placement, Protection};

const APK_NAME: &str = "Roblox-2.738.1397.apk";
const MAIN_LIB: &str = "libroblox.so";
const INITIALIZERS: usize = 3_594;
const TOTAL_IMPORTS: usize = 565;
const STACK_BYTES: usize = 8 * 1024 * 1024;
const GUEST_MEMORY_BUDGET: u64 = 2 * 1024 * 1024 * 1024;
/// Wall-clock allowance for one whole initializer run, after which the watchdog halts the guest.
const RUN_DEADLINE: Duration = Duration::from_secs(300);

/// `tests/initializers.rs`'s pinned words, copied verbatim (a test file is not a module).
#[derive(Clone, Copy)]
enum Written {
    Pointer(usize),
    Constant(u64),
}
const WROTE: &[(usize, Written)] = &[
    (0x067d_c300, Written::Pointer(0x0636_5a38)),
    (0x067d_c380, Written::Pointer(0x0636_58c8)),
    (0x067d_c438, Written::Pointer(0x0636_aae0)),
    (0x067d_c4b8, Written::Pointer(0x0636_ab58)),
    (0x067d_c330, Written::Pointer(0x0684_03b8)),
    (0x067d_c468, Written::Pointer(0x0684_2988)),
    (0x067d_c338, Written::Constant(0x0010_0008)),
    (0x067d_67d0, Written::Constant(5)),
];
const IMAGE_POINTERS_WRITTEN: usize = 92_431;

static SERIAL: Mutex<()> = Mutex::new(());

fn serialized() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).expect("two ancestors").to_path_buf()
}

fn cached_main_lib() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let apk_path = repo_root().join(APK_NAME);
        assert!(apk_path.is_file(), "the gate needs {APK_NAME} at {}; it is not skippable", apk_path.display());
        let apk = omni_apk::Apk::open(&apk_path).expect("the real APK must open");
        let cache = omni_apk::LibraryCache::new(
            repo_root().join("target").join("omni-elf-fixtures").join("extraction-cache"),
        );
        let library = apk
            .native_libraries_for_abi("arm64-v8a")
            .into_iter()
            .find(|l| l.file_name() == MAIN_LIB)
            .expect("the APK must contain libroblox.so");
        cache.extract(&apk, library.entry()).expect("extract libroblox.so").path().to_path_buf()
    })
}

fn main_lib_bytes() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| std::fs::read(cached_main_lib()).expect("read the cache entry"))
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let at = std::env::temp_dir().join(format!("omni-native-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        Scratch(at)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct ProviderHandle(Arc<BoundaryBuilder>);

impl omni_elf::loader::SymbolProvider for ProviderHandle {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn resolve(&self, request: &omni_elf::loader::SymbolRequest<'_>) -> Option<omni_elf::loader::SymbolValue> {
        self.0.resolve(request)
    }
}

/// Which backend a [`Guest`] runs on.
#[derive(Clone)]
enum Backend {
    Dynarmic(Arc<DynarmicBackend>),
    Native(Arc<NativeBackend>),
}

impl Backend {
    fn stack_guard(&self) -> u64 {
        match self {
            Backend::Dynarmic(b) => b.tls().stack_guard(),
            Backend::Native(b) => b.tls().stack_guard(),
        }
    }

    fn host(&self) -> Arc<dyn GuestCpuBackend> {
        match self {
            Backend::Dynarmic(b) => Arc::clone(b) as _,
            Backend::Native(b) => Arc::clone(b) as _,
        }
    }

    fn thread(&self) -> Box<dyn GuestCpu> {
        match self {
            Backend::Dynarmic(b) => Box::new(b.create_thread_with_tls().expect("a guest thread")),
            Backend::Native(b) => Box::new(b.create_thread_with_tls().expect("a guest thread")),
        }
    }

    /// What the initializers are allowed, per call: counted where the backend can count.
    fn budget(&self) -> RunLimit {
        match self {
            Backend::Dynarmic(_) => RunLimit::Instructions(200_000_000),
            Backend::Native(_) => RunLimit::Unlimited,
        }
    }
}

struct Guest {
    space: Arc<GuestSpace>,
    backend: Backend,
    bionic: Arc<Bionic>,
    boundary: Arc<Boundary>,
    object: LoadedObject,
    stack_top: GuestAddr,
    process_args: [GuestArg; 3],
    _backing: Arc<Backing>,
    _root: Scratch,
}

impl Guest {
    /// `tests/initializers.rs`'s `Guest::load`, with the backend chosen by the caller.
    fn load(native: bool) -> Guest {
        Self::load_in(native, GuestSpace::new().expect("reserve a guest address space"))
    }

    /// As [`load`](Guest::load), in a space the caller reserved.
    fn load_in(native: bool, space: GuestSpace) -> Guest {
        let backing = Backing::open(cached_main_lib(), MapExecutability::Executable).expect("open the cache entry");
        let elf = ElfImage::parse(main_lib_bytes()).expect("parse libroblox.so");
        let space = Arc::new(space);
        let backend = if native {
            Backend::Native(Arc::new(
                NativeBackend::new(Arc::clone(&space), NativeOptions::default()).unwrap_or_else(|e| {
                    panic!("the native backend: {e}. Signed? CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=$PWD/tools/hvf_run.sh")
                }),
            ))
        } else {
            let backend = DynarmicBackend::new(Arc::clone(&space), DynarmicOptions::default()).expect("dynarmic");
            assert!(backend.owns_guest_paging());
            Backend::Dynarmic(Arc::new(backend))
        };
        let bionic = Bionic::new(Arc::clone(&space)).expect("a bionic instance");
        let builder = BoundaryBuilder::new(Arc::clone(&space), TOTAL_IMPORTS, 4096).expect("a thunk region");
        bionic.bind_into(&builder).expect("bind every handler");
        bionic
            .declare_data_into(&builder, &GuestProcess { stack_guard: backend.stack_guard() })
            .expect("declare the data objects");
        bionic.set_log_to_stderr(false);
        bionic.set_hwcap_policy(HwcapPolicy::Decline);
        let root = Scratch::new(if native { "native" } else { "dynarmic" });
        bionic.set_filesystem_root(&root.0).expect("a filesystem root");
        bionic.set_memory_budget(GUEST_MEMORY_BUDGET);
        bionic.set_thread_host(ThreadHost::new(backend.host())).expect("a thread host");
        let shared = Arc::new(builder);
        let object = {
            let mut providers = ProviderRegistry::new();
            providers.register(ProviderHandle(Arc::clone(&shared)));
            loader::load(&space, &backing, &elf, &providers, &LoaderConfig::default()).expect("load")
        };
        let builder = Arc::try_unwrap(shared).unwrap_or_else(|_| panic!("the registry released the builder"));
        let boundary = builder.finish();
        bionic.register_image(&object.dl_phdr_info()).expect("register the image");
        let page = space.page_size();
        let stack_base = space
            .map_anonymous(Placement::Anywhere { align: page }, STACK_BYTES, Protection::ReadWrite, CommitPolicy::Lazy)
            .expect("a guest stack");
        let stack_top = (stack_base + STACK_BYTES) & !0xF;
        let argv_block = space
            .map_anonymous(Placement::Anywhere { align: page }, page, Protection::ReadWrite, CommitPolicy::Eager)
            .expect("a page for argv");
        let mem = boundary.mem();
        let blame = |what: &'static str, at: GuestAddr| omni_android::Blame::new(what, at, 0);
        let name = argv_block + 64;
        mem.write_bytes(name, b"/system/bin/app_process64\0", blame("argv[0]", name)).expect("argv[0]");
        mem.write_u64(argv_block, name as u64, blame("argv", argv_block)).expect("argv");
        mem.write_u64(argv_block + 8, 0, blame("argv", argv_block + 8)).expect("argv[1]");
        let environ = boundary.slot_named("environ").expect("environ").address;
        let envp = mem.read_u64(environ, blame("environ", environ)).expect("environ");
        Guest {
            space,
            backend,
            bionic,
            boundary,
            object,
            stack_top,
            process_args: [GuestArg::Int(1), GuestArg::Pointer(argv_block), GuestArg::Pointer(envp as GuestAddr)],
            _backing: backing,
            _root: root,
        }
    }

    fn thread(&self) -> Box<dyn GuestCpu> {
        let mut cpu = self.backend.thread();
        self.boundary.install(&mut *cpu).expect("install the boundary");
        cpu.set_sp(self.stack_top);
        cpu.set_x(XReg::new(30).expect("X30"), self.boundary.sentinel() as u64);
        cpu
    }

    fn read_u64(&self, address: GuestAddr) -> u64 {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: a mapped, committed range of the loaded image, read between runs.
        unsafe { ptr.cast::<u64>().read_unaligned() }
    }

    fn snapshot_writable(&self) -> Vec<(GuestAddr, Vec<u8>)> {
        self.object
            .ranges
            .iter()
            .filter(|range| range.rest.is_writable())
            .map(|range| {
                let ptr = self.space.ptr(range.start, range.len()).expect("a host pointer");
                // SAFETY: GuestSpace's own pointer for a mapped, committed, writable range, read
                // between runs.
                (range.start, unsafe { std::slice::from_raw_parts(ptr, range.len()) }.to_vec())
            })
            .collect()
    }
}

/// Image pointers written into the writable image by the run: the `tests/initializers.rs` count.
fn image_pointers_written(guest: &Guest, before: &[(GuestAddr, Vec<u8>)], after: &[(GuestAddr, Vec<u8>)]) -> usize {
    let span = guest.object.start..guest.object.end;
    let mut pointers = 0usize;
    for ((_, old), (_, new)) in before.iter().zip(after.iter()) {
        for (o, n) in old.chunks_exact(8).zip(new.chunks_exact(8)) {
            let o = u64::from_le_bytes(o.try_into().expect("eight"));
            let n = u64::from_le_bytes(n.try_into().expect("eight"));
            if o == 0 && n != 0 && span.contains(&(n as GuestAddr)) {
                pointers += 1;
            }
        }
    }
    pointers
}

struct Run {
    completed: Vec<(usize, GuestAddr)>,
    stopped: Option<String>,
    elapsed: Duration,
    guest_instructions: u64,
}

/// Every `init_array` entry in order, from the host, under a wall-clock watchdog.
fn run_initializers(guest: &Guest, cpu: &mut dyn GuestCpu) -> Run {
    let _active = guest.bionic.activate().expect("publish the instance to this thread");
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (done, halt) = (Arc::clone(&done), cpu.halt_handle());
        std::thread::spawn(move || {
            let started = Instant::now();
            while !done.load(Ordering::Relaxed) {
                if started.elapsed() > RUN_DEADLINE {
                    halt.request();
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    let mut completed = Vec::with_capacity(INITIALIZERS);
    let mut stopped = None;
    let mut guest_instructions = 0u64;
    let started = Instant::now();
    for (index, &entry) in guest.object.init_array.iter().enumerate() {
        let caller = format!("init_array[{index}]");
        match guest.boundary.call_guest(cpu, &caller, entry as GuestAddr, &guest.process_args, guest.backend.budget()) {
            Ok(_) => completed.push((index, entry as GuestAddr)),
            Err(error) => {
                stopped = Some(format!("init_array[{index}] ({entry:#x}): {error}"));
                break;
            }
        }
        guest_instructions = guest_instructions.saturating_add(cpu.last_run_instructions());
        if let Some(failure) = guest.bionic.guest_thread_failures().first() {
            stopped = Some(format!(
                "after init_array[{index}]: guest thread {} ({:#x}) died: {}",
                failure.thread, failure.start_routine, failure.why
            ));
            break;
        }
    }
    let elapsed = started.elapsed();
    done.store(true, Ordering::Relaxed);
    watchdog.join().expect("the watchdog");
    Run { completed, stopped, elapsed, guest_instructions }
}

fn census_total(guest: &Guest) -> u64 {
    guest.boundary.census().map_or(0, |census| census.values().sum())
}

/// **The gate, natively.** All 3,594 in order, the pinned words, and the exact image-pointer count
/// the translating backend's run produces.
#[test]
fn the_milestone_gate_natively_all_3594_initializers_run_in_order() {
    let _serial = serialized();
    let guest = Guest::load(true);
    assert_eq!(guest.object.init_array.len(), INITIALIZERS);
    for (vaddr, _) in WROTE {
        assert_eq!(guest.read_u64(guest.object.base + vaddr), 0, "p_vaddr {vaddr:#x} before the run");
    }
    let before = guest.snapshot_writable();
    guest.boundary.start_census();
    let mut cpu = guest.thread();
    assert_eq!(cpu.backend_name(), "native-hvf");
    let run = run_initializers(&guest, &mut *cpu);
    assert!(run.stopped.is_none(), "stopped after {} completed: {}", run.completed.len(), run.stopped.unwrap_or_default());
    let expected: Vec<(usize, GuestAddr)> =
        guest.object.init_array.iter().enumerate().map(|(i, &e)| (i, e as GuestAddr)).collect();
    assert_eq!(run.completed, expected, "the initializers that returned are exactly init_array, in order");
    for (vaddr, expected) in WROTE {
        let value = guest.read_u64(guest.object.base + vaddr);
        match *expected {
            Written::Pointer(offset) => {
                assert_eq!(value as usize, guest.object.base + offset, "p_vaddr {vaddr:#x}")
            }
            Written::Constant(constant) => assert_eq!(value, constant, "p_vaddr {vaddr:#x}"),
        }
    }
    let after = guest.snapshot_writable();
    assert_eq!(
        image_pointers_written(&guest, &before, &after),
        IMAGE_POINTERS_WRITTEN,
        "the writable image gains exactly the image pointers the translating backend's run gives it"
    );
    eprintln!(
        "\nNATIVE M3 GATE: {} initializers in {:?}; {} crossings (census), boundary {:?}; {} guest thread records",
        run.completed.len(),
        run.elapsed,
        census_total(&guest),
        guest.boundary.crossings(),
        guest.bionic.guest_thread_records()
    );
    // The guest thread the initializers started is still running, unbounded (a backend that
    // cannot count has no run windows), and still crossing the boundary. Stopping the instance
    // must stop it anyway -- through its `HaltHandle`, which is what `threads::drive` registers.
    let crossing = guest.boundary.threads().iter().map(|t| t.crossings).sum::<u64>();
    std::thread::sleep(Duration::from_millis(100));
    let still_crossing = guest.boundary.threads().iter().map(|t| t.crossings).sum::<u64>();
    assert!(still_crossing > crossing, "the started thread is live and running guest code, or this stop proves nothing");
    let asked = Instant::now();
    guest.bionic.stop_guest_threads();
    assert!(
        guest.bionic.join_guest_threads(Duration::from_secs(10)),
        "a native guest thread did not stop within 10 s of stop_guest_threads"
    );
    eprintln!("  the started guest thread stopped {:?} after it was asked", asked.elapsed());
}

/// **The cold initializer run, timed**: what a startup pays. One instance per process, because
/// an instance is never released (the ownership cycle `tests/initializers.rs`'s `LEAK_CEILING`
/// documents) and the guest thread it started keeps running -- MEASURED, a second instance in the
/// same process ran 1.4-1.9x slower beside the first one's spinning thread. So the comparison is
/// made by running this binary repeatedly, alternating `OMNI_INIT_BACKEND=dynarmic|native`.
/// Dynarmic counts the guest instructions; the census counts the crossings.
#[test]
#[ignore = "measurement"]
fn measure_the_initializer_run() {
    let _serial = serialized();
    let _ = main_lib_bytes();
    let native = match std::env::var("OMNI_INIT_BACKEND").as_deref() {
        Ok("native") => true,
        Ok("dynarmic") => false,
        other => panic!("set OMNI_INIT_BACKEND=native or dynarmic (got {other:?})"),
    };
    // `phys_footprint` on this host: what the kernel charges the process (dirty private,
    // compressed, swapped). Read around the load and around the run.
    let footprint = || omni_platform::vm::process_commit_charge().expect("phys_footprint") as f64 / 1048576.0;
    let at_start = footprint();
    let guest = Guest::load(native);
    guest.boundary.start_census();
    let mut cpu = guest.thread();
    let loaded = footprint();
    let run = run_initializers(&guest, &mut *cpu);
    assert!(run.stopped.is_none(), "{}", run.stopped.unwrap_or_default());
    assert_eq!(run.completed.len(), INITIALIZERS);
    let ran = footprint();
    eprintln!(
        "\nFOOTPRINT {}: load + first context {:+.1} MiB, the run {:+.1} MiB, total {:.1} MiB",
        if native { "native" } else { "dynarmic" },
        loaded - at_start,
        ran - loaded,
        ran
    );
    let main_thread = guest.boundary.threads().iter().map(|t| t.crossings).max().unwrap_or(0);
    eprintln!(
        "\nINITIALIZERS {}: {:.1} ms, {} guest instructions counted, {} crossings in all ({} on the busiest thread), {} on the exit path",
        if native { "native" } else { "dynarmic" },
        run.elapsed.as_secs_f64() * 1e3,
        run.guest_instructions,
        census_total(&guest),
        main_thread,
        guest.boundary.crossings().exits
    );
}
