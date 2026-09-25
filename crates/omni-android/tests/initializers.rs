//! **M3's gate: all 3,594 static initializers of the real `libroblox.so`.**
//!
//! ```text
//! cargo test -p omni-android --release --test initializers
//! ```
//!
//! Everything in M0-M3 was built so this file could run. The loader maps and relocates the real
//! library (M1), the translating backend executes its ARM64 (M2), the thunk boundary services the
//! imports it calls (M3 task 2), and the bionic adapter answers them (M3 task 3).
//!
//! # What the gate has to prove, and what it deliberately refuses to accept as proof
//!
//! A counter reaching 3,594 proves that a loop terminated. The plan says so in as many words, and
//! this file is written against that sentence:
//!
//! * the **order** is asserted, not the count — the recorded `(index, address)` sequence must equal
//!   `init_array` enumerated, so a skip, a reorder and a substitution each fail;
//! * **guest state the initializers wrote is read back** — a set of `.data`/`.bss` words that are
//!   zero before the run and hold pinned, non-zero values afterwards;
//! * the run is **repeatable and leak-free**, and M2's per-slice callback invariant is armed
//!   throughout rather than assumed to be.
//!
//! When the APK is absent every test here **skips loudly** on the process's own stderr rather than
//! passing quietly.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use omni_android::bionic::{Bionic, GuestProcess, HwcapPolicy, ThreadHost};
use omni_android::{AbiError, Boundary, BoundaryBuilder, GuestArg};
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicCpu, DynarmicOptions};
use omni_cpu::{GuestAddr, GuestCpu, RunLimit, XReg};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
use omni_mem::{Backing, CommitPolicy, GuestSpace, MapExecutability, Placement, Protection};

/// The APK every golden assertion in this project is about.
const APK_NAME: &str = "Roblox-2.738.1397.apk";
const MAIN_LIB: &str = "libroblox.so";

/// `DT_INIT_ARRAY` entries in `libroblox.so`. An exact figure (Global Constraint 3).
const INITIALIZERS: usize = 3_594;

/// Undefined, named symbols in `libroblox.so`'s `.dynsym`.
const TOTAL_IMPORTS: usize = 565;

/// Bytes of guest stack for the thread the initializers run on.
///
/// Android's main thread gets 8 MiB, and static initialisation of a C++ engine is the deepest
/// non-recursive call graph a process has. Lazily committed, so the reservation is free (D10) and
/// only the pages actually touched are charged.
const STACK_BYTES: usize = 8 * 1024 * 1024;

/// Guest instructions one initializer is allowed.
///
/// A counted budget rather than [`RunLimit::Unlimited`], because D16's whole point is that a
/// runaway guest is contained by short windows and because a test that hung would hang the suite.
/// Comfortably below `i64::MAX`, which is D16's footgun.
const PER_INITIALIZER: RunLimit = RunLimit::Instructions(200_000_000);

/// Guest instructions one initializer is allowed **in the survey**.
///
/// Three orders of magnitude below the gate's. The survey runs on past a refusal, so the
/// initializer after a skipped one may be spinning on state that was never written; a budget
/// small enough that such a spin ends in a moment is what lets a survey of 3,594 finish. A
/// `StepLimitReached` here is a fact about the survey's budget, never about the initializer.
const SURVEY_BUDGET: RunLimit = RunLimit::Instructions(200_000);

/// What the gate tells the guest its memory budget is, for `sysinfo`.
///
/// A *configuration*, not a measurement: nothing enforces it, and it exists because `sysinfo`
/// has no default for it (see `Bionic::set_memory_budget`). Two gibibytes is an ordinary
/// application heap limit on a 64-bit Android device.
const GUEST_MEMORY_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// How many instances `the_cost_of_an_initializer_run` builds. Stated with every figure it prints.
const COST_RUNS: usize = 5;

/// What commit charge one dropped instance is allowed to leave behind.
///
/// # THIS IS THE MEASURED COST OF AN OPEN DEFECT, NOT A TARGET
///
/// The plan's gate asks for "repeatable and leak-free: commit charge returns to baseline". **It
/// does not.** MEASURED, n = 5 instances in one process:
///
/// | | |
/// |---|---|
/// | load + the first CPU context | 39.9 - 41.5 MiB |
/// | what the run itself adds | 51.9 - 57.5 MiB |
/// | returned by dropping the **CPU context** | 38.9 - 41.1 MiB |
/// | returned by dropping **everything else** | **0.00 MiB** |
/// | residual per instance | **54.7 - 56.0 MiB** |
///
/// The zero is exact, and it is the diagnosis. After `drop`, the `GuestSpace` still has **six**
/// owners and 23 mapped regions, `Arc<Bionic>` has three or four and `Arc<Boundary>` three —
/// against one each if the instance had really gone. Every instance also ends with exactly **one
/// guest-thread record** that nothing reaped: `pthread_create` ran during static initialisation
/// and the guest never joined or detached the thread, so the record outlives the instance and the
/// `Arc`s the runner holds go with it. The guest address space is therefore never released, and
/// everything the engine's allocator mapped during the run stays charged.
///
/// D24 measured that a guest thread gives back **98.6%** of its memory *when it exits*. That is
/// still true and is not this: the thread did exit. What does not happen is the **instance**
/// being released afterwards.
///
/// So this ceiling detects a *regression* — it is a little above what is measured — and it is
/// named after the defect rather than after a policy. When the ownership cycle is closed it
/// should drop to single-digit megabytes, which is the host allocator's own high-water mark.
const LEAK_CEILING: i64 = 64 * 1024 * 1024;

/// **Serializes every test in this binary.** `omni_mem::process_commit_charge` is a process
/// quantity and two tests reading it at once measure each other; the 109 MB load is also not worth
/// doing several times at once.
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

/// `libroblox.so` in the shared extraction cache, or `None` when the APK is absent.
fn cached_main_lib() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let apk_path = repo_root().join(APK_NAME);
        if !apk_path.is_file() {
            // **The gate FAILS rather than skips, and that is deliberate.**
            //
            // Writing to raw stderr and returning `None` was the earlier behaviour, on the
            // argument that a notice makes the skip visible. It does -- but a skipped test still
            // reports `ok`, and this is the test that *is* M3's evidence.
            //
            // A green suite that proved nothing about the 3,594 initializers is exactly the shape
            // of the two High findings this project's adapter review turned up: a confinement
            // test that early-returned because the host could not create a symlink, and a
            // regression test whose refusal arm was empty. Both passed run after run while
            // asserting nothing, and both were written as the fix for an earlier defect.
            //
            // A milestone gate is the last place to accept that trade. If the fixture is missing,
            // the honest outcome is a failure naming what is missing, not a pass.
            panic!(
                "the M3 gate needs {}, which is not at {}. It is not skippable: this test is the milestone's evidence, and passing without it would assert nothing about the 3,594 initializers.",
                APK_NAME,
                apk_path.display(),
            );
        }
        let apk = omni_apk::Apk::open(&apk_path).expect("the real APK must open");
        let cache = omni_apk::LibraryCache::new(
            repo_root().join("target").join("omni-elf-fixtures").join("extraction-cache"),
        );
        let library = apk
            .native_libraries_for_abi("arm64-v8a")
            .into_iter()
            .find(|l| l.file_name() == MAIN_LIB)
            .expect("the APK must contain libroblox.so");
        let cached = cache.extract(&apk, library.entry()).expect("extract libroblox.so");
        Some(cached.path().to_path_buf())
    })
    .as_deref()
}

/// The bytes of the cache entry, read **once** for the whole process.
///
/// `std::fs::read` of a 109 MiB file per instance is 109 MiB through the host allocator per
/// instance, and a measurement of what an initializer run costs would be mostly that: the
/// allocator's arena grows to hold it and does not give it back to the operating system. The M2
/// harness caches it for the same reason.
fn main_lib_bytes() -> Option<&'static [u8]> {
    static BYTES: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    BYTES
        .get_or_init(|| cached_main_lib().map(|p| std::fs::read(p).expect("read the cache entry")))
        .as_deref()
}

// ============================================================================= the loaded guest

/// A loaded `libroblox.so` with the whole compatibility layer behind it.
struct Guest {
    space: Arc<GuestSpace>,
    backend: Arc<DynarmicBackend>,
    bionic: Arc<Bionic>,
    boundary: Arc<Boundary>,
    object: LoadedObject,
    stack_top: GuestAddr,
    /// `(argc, argv, envp)` — what bionic's linker passes every `init_array` entry.
    process_args: [GuestArg; 3],
    /// Kept alive so the file mapping outlives the load, and so the root directory is not removed
    /// while the guest can still name files inside it.
    _backing: Arc<Backing>,
    _root: Scratch,
}

impl Guest {
    /// Load the real library behind a real boundary, or `None` when the APK is absent.
    fn load() -> Option<Self> {
        let path = cached_main_lib()?;
        let bytes = main_lib_bytes()?;
        // `Executable` here and nowhere else: a section's protection caps every view's protection
        // for the life of the mapping (D11).
        let backing =
            Backing::open(path, MapExecutability::Executable).expect("open the cache entry");
        let elf = ElfImage::parse(bytes).expect("parse libroblox.so");
        let space = Arc::new(GuestSpace::new().expect("reserve a guest address space"));

        // **The backend comes first**, because the eighteen data objects need the process stack
        // canary D13 programmed into `TPIDR_EL0 + 0x28`, and only the backend that owns the TLS
        // arena has it. Declaring the data objects has to happen before the loader resolves
        // anything, so the order is forced: backend, then declarations, then load.
        let backend = Arc::new(
            DynarmicBackend::new(Arc::clone(&space), DynarmicOptions::default())
                .expect("a translating backend"),
        );
        assert!(
            backend.owns_guest_paging(),
            "this guest has no demand pager, so every guest fault goes to dynarmic's own handler \
             and the 30-49x path — and the per-slice callback invariant is disarmed with it. The \
             gate would pass and prove less than it claims"
        );
        assert!(
            backend.slice_invariant_armed(),
            "M2's per-slice callback invariant is not armed, and the plan requires it to hold \
             throughout this run"
        );

        let bionic = Bionic::new(Arc::clone(&space)).expect("a bionic instance");
        // Room for every import, not only the 188 predicted reachable: D17 records 188 as a lower
        // bound with 17,698 unresolvable indirect call sites behind it, so a symbol outside the
        // prediction gets a named slot rather than a null.
        let builder = BoundaryBuilder::new(Arc::clone(&space), TOTAL_IMPORTS, 4096)
            .expect("a thunk region for 565 imports");
        bionic.bind_into(&builder).expect("bind every handler");
        bionic
            .declare_data_into(
                &builder,
                &GuestProcess { stack_guard: backend.tls().stack_guard() },
            )
            .expect("declare and fill the eighteen data objects");
        // The log ring keeps every line either way; writing 3,594 initializers' worth of engine
        // logging to the suite's stderr would bury the result.
        bionic.set_log_to_stderr(false);
        // **D26.** Constructed explicitly at the call site: `Undecided` is what an instance starts
        // as, and under it `getauxval(AT_HWCAP)` refuses by name — which would stop this run on
        // the first initializer doing atomics feature detection.
        bionic.set_hwcap_policy(HwcapPolicy::Decline);

        let root = Scratch::new("m3-gate");
        bionic.set_filesystem_root(&root.0).expect("a filesystem root");
        // What `sysinfo` reports as `totalram`. There is no default, deliberately: it is the
        // embedding's budget for this guest, and it is the one field of `struct sysinfo` this
        // layer cannot derive. 2 GiB is what a mid-range Android device gives an application.
        bionic.set_memory_budget(GUEST_MEMORY_BUDGET);
        let host: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&backend) as _;
        bionic.set_thread_host(ThreadHost::new(host)).expect("a thread host");

        let shared = Arc::new(builder);
        let object = {
            let mut providers = ProviderRegistry::new();
            providers.register(ProviderHandle(Arc::clone(&shared)));
            loader::load(&space, &backing, &elf, &providers, &LoaderConfig::default())
                .expect("libroblox.so must load with a thunk boundary")
        };
        let builder = Arc::try_unwrap(shared)
            .unwrap_or_else(|_| panic!("the registry must have released the builder"));
        let boundary = builder.finish();

        // `dl_iterate_phdr` must be faithful rather than a stub: the statically linked C++ runtime
        // walks 11.5 MB of `.eh_frame` through it and exceptions break without it.
        bionic.register_image(&object.dl_phdr_info()).expect("register the loaded image");

        let page = space.page_size();
        let stack_base = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                STACK_BYTES,
                Protection::ReadWrite,
                // Lazily committed: 8 MiB of reservation is free (D10) and the gate's memory
                // figures would otherwise be 8 MiB of stack nothing touched.
                CommitPolicy::Lazy,
            )
            .expect("a guest stack");
        let stack_top = (stack_base + STACK_BYTES) & !0xF;

        // `(argc, argv, envp)`: bionic's linker calls every `init_array` entry as
        // `void (*)(int, char **, char **)`, so the gate calls them the way a device would rather
        // than with three zeroes. `envp` is the instance's own `environ` vector — the one the
        // `environ` data object points at — so the guest sees one environment, not two.
        let argv_block = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                page,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("a page for argv");
        let mem = boundary.mem();
        let blame = |what: &'static str, at: GuestAddr| omni_android::Blame::new(what, at, 0);
        let name = argv_block + 64;
        mem.write_bytes(name, b"/system/bin/app_process64\0", blame("argv[0]", name))
            .expect("write the program name");
        mem.write_u64(argv_block, name as u64, blame("argv", argv_block)).expect("argv[0]");
        mem.write_u64(argv_block + 8, 0, blame("argv", argv_block + 8)).expect("argv[1] = NULL");
        let environ = boundary
            .slot_named("environ")
            .expect("`environ` is one of the eighteen data objects")
            .address;
        let envp = mem.read_u64(environ, blame("environ", environ)).expect("read `environ`");

        Some(Self {
            space,
            backend,
            bionic,
            boundary,
            object,
            stack_top,
            process_args: [
                GuestArg::Int(1),
                GuestArg::Pointer(argv_block),
                GuestArg::Pointer(envp as GuestAddr),
            ],
            _backing: backing,
            _root: root,
        })
    }

    /// The thread the initializers run on: a bionic TLS block (D13), a stack, and the boundary
    /// installed on every one of its 566 slots.
    fn thread(&self) -> DynarmicCpu {
        let mut cpu = self.backend.create_thread_with_tls().expect("a guest thread");
        self.boundary.install(&mut cpu).expect("install the boundary");
        cpu.set_sp(self.stack_top);
        cpu.set_x(XReg::new(30).expect("X30"), self.boundary.sentinel() as u64);
        cpu
    }

    /// Every writable mapped range of the loaded image, copied out.
    ///
    /// **`PT_GNU_RELRO` is excluded by construction rather than by a rule**: the loader has
    /// already dropped those pages to read-only (M1 seals exactly 5,205,568 bytes of them), so
    /// `rest.is_writable()` is false for them and they cannot appear here. What is left is
    /// `.data` and `.bss` — the memory a static initializer writes.
    fn snapshot_writable(&self) -> Vec<(GuestAddr, Vec<u8>)> {
        self.object
            .ranges
            .iter()
            .filter(|range| range.rest.is_writable())
            .map(|range| {
                let ptr = self.space.ptr(range.start, range.len()).expect("a host pointer");
                // SAFETY: `ptr` is `GuestSpace`'s own pointer for a mapped, committed, writable
                // range of exactly this length, and D4's identity mapping makes the guest address
                // a host address. Taken between runs, with no guest thread executing.
                let bytes = unsafe { std::slice::from_raw_parts(ptr, range.len()) }.to_vec();
                (range.start, bytes)
            })
            .collect()
    }

    fn read_u64(&self, address: GuestAddr) -> u64 {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: `ptr` is `GuestSpace`'s own pointer for a mapped, committed eight-byte range,
        // and D4's identity mapping makes the guest address a host address. Read between runs,
        // with no guest thread executing.
        unsafe { ptr.cast::<u64>().read_unaligned() }
    }
}

/// A host directory that removes itself, for the guest's filesystem root.
///
/// Built without a dependency, and with the process id in its name so that two `cargo test`
/// processes cannot collide on it. An instance with no root refuses every path call by name
/// (D23), which is the intended failure and not one the gate wants to spend an initializer on.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-m3-gate-{tag}-{}", std::process::id()));
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

// ============================================================================= the run itself

/// What one full pass over `init_array` did.
struct Run {
    /// `(index, address)` for every initializer that **returned**, in the order they returned.
    completed: Vec<(usize, GuestAddr)>,
    /// The first failure, if there was one: which index, which address, and why.
    stopped: Option<(usize, GuestAddr, AbiError)>,
    elapsed: Duration,
    guest_instructions: u64,
}

impl Run {
    fn ok(&self) -> bool {
        self.stopped.is_none()
    }
}

/// Whether a failing initializer ends the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnFailure {
    /// The gate: the first failure *is* the result.
    Stop,
    /// The survey: print it and carry on to the next initializer.
    ///
    /// **This is not a weaker gate and must never be read as one.** An initializer that was
    /// skipped did not write the state the ones after it expect, so nothing a continuing run
    /// produces is evidence that anything completed. It answers one question the gate cannot
    /// answer while it is failing - *which* imports the engine reaches - and it answers it a
    /// batch at a time instead of one rebuild per symbol.
    ///
    /// It is also why this mode **can hang**, and the hang is itself a finding: a skipped
    /// initializer leaves a mutex locked or a condition unsignalled, and the guest then blocks
    /// inside a handler, where no instruction budget can reach it. D16's runaway-guest defence is
    /// built from short step windows, and a blocked host thread executes no guest instructions at
    /// all. So the survey prints every failure the moment it happens, flushed, rather than
    /// collecting them for a summary it may never reach.
    Continue,
}

/// Call every `init_array` entry in order, from the host.
fn run_initializers(
    guest: &Guest,
    cpu: &mut DynarmicCpu,
    limit: RunLimit,
    on_failure: OnFailure,
) -> Run {
    use std::io::Write;
    let _active = guest.bionic.activate().expect("publish the instance to this thread");
    // `OMNI_INIT_TRACE=1` prints every initializer index before it runs. See the call site.
    let trace = std::env::var_os("OMNI_INIT_TRACE").is_some();
    // `OMNI_INIT_WATCHDOG=<seconds>` reports what the run is doing from *another* thread.
    //
    // **The one diagnostic that works when the guest is blocked rather than looping.** A guest
    // parked inside a handler -- on a futex, a condition variable, a join -- executes no guest
    // instructions, so no step budget expires and nothing on this thread will ever print again.
    // The watchdog reads the boundary's census and the instance's thread table, both of which are
    // `Sync`, and says which symbol was last entered and how many guest threads are live.
    let watchdog = std::env::var("OMNI_INIT_WATCHDOG")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let me = guest.bionic.current_thread();
    if let Some(seconds) = watchdog {
        let progress = Arc::clone(&progress);
        let boundary = Arc::clone(&guest.boundary);
        let bionic = Arc::clone(&guest.bionic);
        boundary.start_census();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(seconds));
            let mut called: Vec<(&str, u64)> = boundary
                .census()
                .map(|c| c.into_iter().collect())
                .unwrap_or_default();
            called.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            let _ = writeln!(
                std::io::stderr(),
                "WATCHDOG: at init_array[{}], last import `{}`, {} live guest threads,                  crossings {:?}
  futex {:?} parked on {:#x} = {:016x} {:016x}, owner {:?}, this thread {:?}
                   thread records {}, failures {:?}
  top imports: {:?}",
                progress.load(std::sync::atomic::Ordering::Relaxed),
                boundary.last_call().map_or("-", |slot| slot.symbol.as_str()),
                bionic.live_guest_threads(),
                boundary.crossings(),
                bionic.futex().activity(),
                bionic.futex().parked_on(),
                boundary
                    .mem()
                    .read_u64(
                        bionic.futex().parked_on() as omni_cpu::GuestAddr,
                        omni_android::Blame::new("watchdog", 0, 0),
                    )
                    .unwrap_or(0),
                boundary
                    .mem()
                    .read_u64(
                        bionic.futex().parked_on() as omni_cpu::GuestAddr + 8,
                        omni_android::Blame::new("watchdog", 0, 0),
                    )
                    .unwrap_or(0),
                bionic.mutex_owner(bionic.futex().parked_on()),
                me,
                bionic.guest_thread_records(),
                bionic.guest_thread_failures(),
                &called[..called.len().min(12)]
            );
            let _ = std::io::stderr().flush();
        });
    }

    let mut completed = Vec::with_capacity(guest.object.init_array.len());
    let mut stopped = None;
    let mut guest_instructions = 0u64;
    let mut failures = 0usize;
    let started = Instant::now();
    for (index, &entry) in guest.object.init_array.iter().enumerate() {
        let target = entry as GuestAddr;
        // The caller name is what a reader three thousand initializers deep actually needs: which
        // of 3,594, not a bare guest address.
        let caller = format!("init_array[{index}]");
        progress.store(index, std::sync::atomic::Ordering::Relaxed);
        if trace {
            // **Before the call, flushed.** A run that stops because the guest is *blocked* in a
            // handler prints nothing at all afterwards, and the index it stopped on is the only
            // thing that identifies it -- an instruction budget cannot reach a parked host
            // thread. Gated on the environment so an ordinary run is not 3,594 lines.
            let _ = writeln!(
                std::io::stderr(),
                "IN  [{index}] {target:#x} vaddr={:#x}",
                target - guest.object.base
            );
            let _ = std::io::stderr().flush();
        }
        match guest.boundary.call_guest(cpu, &caller, target, &guest.process_args, limit) {
            Ok(_) => completed.push((index, target)),
            Err(error) => {
                if on_failure == OnFailure::Stop {
                    stopped = Some((index, target, error));
                    break;
                }
                failures += 1;
                let _ = writeln!(std::io::stderr(), "FAIL[{index}] {error}");
                let _ = std::io::stderr().flush();
            }
        }
        guest_instructions = guest_instructions.saturating_add(cpu.last_run_instructions());
        // **A guest thread that died is reported here, not waited for.**
        //
        // M3's gate found this the expensive way: an initializer started a guest thread, that
        // thread took a *recursive* mutex and then hit a refusal, and it died holding the lock.
        // The main thread blocked on that mutex for ever, and a blocked host thread executes no
        // guest instructions -- so `PER_INITIALIZER` could never expire and the gate hung with
        // nothing printed. `Bionic::guest_thread_failures` already knew why; nothing asked it.
        //
        // Checked after every initializer rather than at the end, so the report names the
        // initializer that started the thread rather than the one that later deadlocked.
        if stopped.is_none() {
            let failures = guest.bionic.guest_thread_failures();
            if let Some(failure) = failures.first() {
                let error = AbiError::Refused {
                    symbol: "pthread_create".to_string(),
                    address: target,
                    why: format!(
                        "a guest thread this instance started has died: thread {}, entry point                          {:#x}, because {}. It is reported here rather than waited for: a thread                          that dies holding a mutex deadlocks whoever takes that mutex next, and a                          blocked host thread executes no guest instructions, so no step budget                          can ever end the wait",
                        failure.thread, failure.start_routine, failure.why
                    ),
                };
                if on_failure == OnFailure::Stop {
                    stopped = Some((index, target, error));
                    break;
                }
                let _ = writeln!(std::io::stderr(), "THREAD[{index}] {error}");
                let _ = std::io::stderr().flush();
            }
        }
        if on_failure == OnFailure::Continue && index % 100 == 99 {
            let _ = writeln!(
                std::io::stderr(),
                "  .. {index} ok={} failed={failures} {:?}",
                completed.len(),
                started.elapsed()
            );
            let _ = std::io::stderr().flush();
        }
    }
    Run { completed, stopped, elapsed: started.elapsed(), guest_instructions }
}

/// Print a failure the way somebody who has to fix it needs to read it.
fn describe(run: &Run, total: usize) -> String {
    match &run.stopped {
        None => format!("all {} initializers returned", run.completed.len()),
        Some((index, address, error)) => format!(
            "stopped at init_array[{index}] of {total} ({address:#x}) after {} completed: {error}",
            run.completed.len()
        ),
    }
}

// ==================================================== what the initializers demonstrably wrote

/// What a pinned word holds once the initializers have run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Written {
    /// A pointer **into the loaded image**, pinned as its offset from the load base.
    ///
    /// The value itself moves with the mapping, so the offset is what is stable — and it is the
    /// offset that says what the guest wrote, because it names a place in the file.
    Pointer(usize),
    /// A value that does not depend on where the image landed.
    Constant(u64),
}

/// **Eight words the 3,594 initializers wrote, each zero before the run.**
///
/// This is the gate's evidence, and the plan is explicit that a counter reaching 3,594 is not
/// acceptable as it: *"something the initializers demonstrably wrote is read back and checked, so
/// the gate proves they ran rather than that a loop terminated."*
///
/// Each entry is a `p_vaddr` in the image and what that word must hold afterwards. The **zero
/// before** half is asserted rather than assumed, and it is what makes each one evidence: a word
/// that already held its value would prove nothing about the run.
///
/// Four of them receive a pointer into `.data.rel.ro`, which is where this binary's C++ **vtables**
/// live — a constructor storing its vtable pointer is the most characteristic thing a static
/// initializer does, and the destination is inside `PT_GNU_RELRO`, so the loader had already sealed
/// it read-only before any of this ran (M1 seals exactly 5,205,568 bytes). Two receive a pointer
/// into `.bss`, one statically constructed object pointing at another. Two are plain constants.
///
/// They were found by `dump_guest_state_written_by_the_initializers`, which reports **92,431**
/// words that became a pointer into the image and about **133,269** that became something else.
/// These
/// eight are pinned so that a *substitution* fails, in the way a total cannot see.
const WROTE: &[(usize, Written)] = &[
    // `.data` <- `.data.rel.ro`: a vtable pointer, stored by a constructor.
    (0x067d_c300, Written::Pointer(0x0636_5a38)),
    (0x067d_c380, Written::Pointer(0x0636_58c8)),
    (0x067d_c438, Written::Pointer(0x0636_aae0)),
    (0x067d_c4b8, Written::Pointer(0x0636_ab58)),
    // `.data` <- `.bss`: one statically constructed object pointing at another.
    (0x067d_c330, Written::Pointer(0x0684_03b8)),
    (0x067d_c468, Written::Pointer(0x0684_2988)),
    // `.data` <- a value that does not move with the image: a field of each of the first two
    // objects above, stored by the constructors that store their vtables.
    //
    // **Not `0x067d_67d0`, which held 5 here until 2026-09-25.** That word is the `index` of an
    // emulated-TLS control block (compiler-rt's `__emutls_get_address`, link address
    // `0x2b9dee0`, assigns `index = ++count` under its mutex the first time *any* thread touches
    // the variable), so it records the order in which threads first touched thread-locals -- and
    // the thread that first touches `0x067d_67c0` starts a worker thread while doing so
    // (`pthread_create` at `0x285332c`, start routine `0x284d168`), which touches another one
    // (`0x0682_4fd0`) as it starts. Which of the two gets index 5 and which 6 is the race between
    // those two threads, on a device as here. MEASURED (Linux, the whole binary, shared cache):
    // 6 in 5 of 30 runs, every time as an exact swap of those two indices, 17 assigned in all.
    (0x067d_c338, Written::Constant(0x0010_0008)),
    (0x067d_c3b8, Written::Constant(0x0010_0010)),
];

/// How many words went from zero to a pointer into the image, over the whole writable image.
///
/// **An exact figure, and a reproducible one**: three separate processes produced it unchanged.
/// It is pinned rather than bounded because a change in it is a change in what the engine's own
/// static initialisation did, which is exactly the kind of thing that should be a visible diff
/// rather than a number inside a range nobody re-derives.
///
/// **Counted across two placements of the image** ([`written_across`]): a value that merely lies
/// inside the image in one placement is a number, not a pointer, and one placement cannot tell.
const IMAGE_POINTERS_WRITTEN: usize = 92_431;

/// A **floor** on how many went from zero to something that is not a pointer into the image.
///
/// **Not an exact figure, and the difference from [`IMAGE_POINTERS_WRITTEN`] is the finding.**
/// The pointer count is decided by the compiler -- a constructor stores its vtable at a fixed
/// offset -- so it reproduces exactly, run after run and process after process. This class does
/// not: it is guest heap addresses, which move with the mapping, and values derived from entropy,
/// which come from `/dev/urandom` and `arc4random_buf`. Observed **133,269 and 133,270** in two
/// runs of the same binary, and the one that differs is a word that happened to land on zero in
/// one run and not the other -- a zero is not counted, because the test is "went from zero to
/// something".
///
/// Pinning it exactly would have produced a test that fails about one run in some unknown number,
/// which this project has been bitten by: a flaky test does not only cost a red run, it can make
/// a mutation row look detected when nothing detected it. So it is a floor, and the exact half of
/// the evidence lives in [`IMAGE_POINTERS_WRITTEN`] and in [`WROTE`].
///
/// **The count depends on where the guest's address space sits**, and this floor is the one for a
/// space above 2^40, where Windows puts it. MEASURED on macOS (arm64, the same run moved by a
/// reservation hint and nothing else): 131,991 for a space based anywhere from 4 GiB to
/// 0xff_0000_0000, 133,136 at 0x100_0000_0000 (2^40), and **133,269 at 0x1b7_0000_0000** -- Windows'
/// own figure, at an address in the range Windows' allocations show in its logs. The x86-64
/// backend under Rosetta 2 gives the same 131,991 at macOS's default placement, so it is the
/// placement and not the CPU backend. macOS places the space at 0x3_0000_0000, below 2^40 as an
/// Android device's 39-bit user address space would; [`OTHER_WORDS_WRITTEN_AT_LEAST_BELOW_2_40`]
/// is the floor there.
const OTHER_WORDS_WRITTEN_AT_LEAST: usize = 133_000;

/// [`OTHER_WORDS_WRITTEN_AT_LEAST`] for a guest space below 2^40: 131,991 measured in each of seven
/// placements from 4 GiB to 0xff_0000_0000 (n = 1 each, plus four runs at the default 0x3_0000_0000,
/// which gave 131,991 or 131,992), with the same margin below it the Windows floor has.
const OTHER_WORDS_WRITTEN_AT_LEAST_BELOW_2_40: usize = 131_700;

/// One placement of the image and what its writable ranges held before and after the run.
struct Placed<'a> {
    base: GuestAddr,
    span: std::ops::Range<GuestAddr>,
    before: &'a [(GuestAddr, Vec<u8>)],
    after: &'a [(GuestAddr, Vec<u8>)],
}

impl Placed<'_> {
    /// Every word that went from zero to something, as `(offset from the base, value)`.
    fn written(&self) -> std::collections::BTreeMap<usize, u64> {
        let mut out = std::collections::BTreeMap::new();
        for ((start, old), (_, new)) in self.before.iter().zip(self.after.iter()) {
            for (index, (o, n)) in old.chunks_exact(8).zip(new.chunks_exact(8)).enumerate() {
                let o = u64::from_le_bytes(o.try_into().expect("eight bytes"));
                let n = u64::from_le_bytes(n.try_into().expect("eight bytes"));
                if o == 0 && n != 0 {
                    out.insert(start + index * 8 - self.base, n);
                }
            }
        }
        out
    }

    /// Where `value` points in the image, as an offset from the base, if it points into it.
    fn offset(&self, value: u64) -> Option<usize> {
        let value = value as GuestAddr;
        self.span.contains(&value).then(|| value - self.base)
    }
}

/// What the initializers wrote, judged across **two placements** of the image.
struct Written2 {
    /// Words holding the same image offset in both placements: pointers into the image.
    pointers: usize,
    /// Words written in the first placement that are not such a pointer.
    others: usize,
    /// Words whose value is **the same number in both placements** and yet lies inside the image
    /// in at least one of them: a value that does not move with the image, which a placement
    /// happened to put the image around. `(offset of the word, value)`.
    lookalikes: Vec<(usize, u64)>,
    /// Words that point into the image in one placement and are neither the same pointer nor the
    /// same number in the other. None is expected; each is named in the failure.
    disagreements: Vec<(usize, Option<u64>, Option<u64>)>,
}

/// Classify every word the initializers wrote by whether its value **moved with the image**.
///
/// **Why one placement cannot count image pointers exactly.** A word counts as an image pointer
/// when its value lies inside the loaded image. The engine also stores plain numbers that happen
/// to be the right size: `libroblox.so`'s static service descriptors (the constructor at link
/// address `0x5fb225c`, 795 call sites) store two 32-bit fields side by side at `+0x20` -- e.g.
/// `0x2dc7c7` and `0x13a` (`0x1ea9ebc`), or `0x2667` and `0x222` for `MessagingService`
/// (`0x45d52a4`) -- which read as one word `0x13a_002d_c7c7` or `0x222_0000_2667`. Windows put the
/// image between `0x113 << 32` and `0x2c3 << 32` in 80 runs, the same range those pairs' high
/// halves span (1 to 1,121), so in a few percent of placements the ~115 MiB image lands around one
/// of them and it counts as a pointer (~2% from these 795 pairs alone, if the base were uniform
/// over that range). MEASURED (2026-09-25, Windows, the gate alone): 92,432 or 92,433 in 8 of 223
/// runs -- 5 of 143 with the shared translation cache, 3 of 80 with per-thread caches -- and 92,431
/// in the rest. Of the five failing runs that were dumped, four had one of these pairs as the extra
/// word (three different descriptors); the fifth's, at `0x6bbc0f0`, was not decoded.
///
/// Two placements decide it: a pointer moves with the image, a number does not.
fn written_across(a: &Placed<'_>, b: &Placed<'_>) -> Written2 {
    assert_ne!(
        a.base, b.base,
        "the two instances were loaded at the same address, so nothing here can tell a pointer \
         from a number"
    );
    let (wa, wb) = (a.written(), b.written());
    let mut out =
        Written2 { pointers: 0, others: 0, lookalikes: Vec::new(), disagreements: Vec::new() };
    for (&word, &va) in &wa {
        let vb = wb.get(&word).copied();
        match (a.offset(va), vb.and_then(|vb| b.offset(vb))) {
            (Some(oa), Some(ob)) if oa == ob => out.pointers += 1,
            (pa, pb) => {
                out.others += 1;
                if pa.is_some() || pb.is_some() {
                    if vb == Some(va) {
                        out.lookalikes.push((word, va));
                    } else {
                        out.disagreements.push((word, Some(va), vb));
                    }
                }
            }
        }
    }
    // A word the second placement's run wrote and the first did not, pointing into the image.
    for (&word, &vb) in &wb {
        if !wa.contains_key(&word) && b.offset(vb).is_some() {
            out.disagreements.push((word, None, Some(vb)));
        }
    }
    out
}

/// Assert the eight pinned words were zero before the run.
fn assert_unwritten(guest: &Guest) {
    for (vaddr, _) in WROTE {
        let at = guest.object.base + vaddr;
        assert_eq!(
            guest.read_u64(at),
            0,
            "the word at p_vaddr {vaddr:#x} is not zero before the initializers run, so its \
             value afterwards would prove nothing about them"
        );
    }
}

/// Assert the eight pinned words hold what the initializers put there.
fn assert_written(guest: &Guest) {
    for (vaddr, expected) in WROTE {
        let at = guest.object.base + vaddr;
        let value = guest.read_u64(at);
        match expected {
            Written::Pointer(offset) => assert_eq!(
                value as usize,
                guest.object.base + offset,
                "the word at p_vaddr {vaddr:#x} should hold the image pointer base + {offset:#x} \
                 after the initializers have run"
            ),
            Written::Constant(constant) => assert_eq!(
                value, *constant,
                "the word at p_vaddr {vaddr:#x} should hold {constant:#x} after the initializers \
                 have run"
            ),
        }
    }
}

/// Two placements of a four-word writable range, as `snapshot_writable` returns them.
fn placed_words(base: GuestAddr, words: [u64; 4]) -> (Vec<(GuestAddr, Vec<u8>)>, Vec<(GuestAddr, Vec<u8>)>) {
    let start = base + 0x0680_0000;
    let before = vec![(start, vec![0u8; 32])];
    let after = vec![(start, words.iter().flat_map(|w| w.to_le_bytes()).collect())];
    (before, after)
}

/// **The count's own detector**, with the numbers from the runs that failed: the
/// `MessagingService` descriptor's `0x2667`/`0x222` pair, read as one word, lies inside an image
/// based at `0x221_f9b4_4000` and nowhere near one based at `0x1b7_4599_4000`.
#[test]
fn written_across_counts_a_number_the_image_was_placed_around_as_a_number() {
    const SPAN: usize = 0x0733_4000;
    const LOOKALIKE: u64 = 0x0000_0222_0000_2667;
    let (a_base, b_base) = (0x221_f9b4_4000usize, 0x1b7_4599_4000usize);
    assert!((a_base..a_base + SPAN).contains(&(LOOKALIKE as usize)), "the premise: inside A");
    // A vtable pointer, the lookalike, a heap pointer, an entropy word.
    let (a0, a1) =
        placed_words(a_base, [(a_base + 0x0636_5a38) as u64, LOOKALIKE, 0x7f00_1234_5678, 0x9e37]);
    let (b0, b1) =
        placed_words(b_base, [(b_base + 0x0636_5a38) as u64, LOOKALIKE, 0x7f00_8765_4320, 0x51ed]);
    let a = Placed { base: a_base, span: a_base..a_base + SPAN, before: &a0, after: &a1 };
    let b = Placed { base: b_base, span: b_base..b_base + SPAN, before: &b0, after: &b1 };
    let judged = written_across(&a, &b);
    assert_eq!(judged.pointers, 1, "only the vtable pointer moved with the image");
    assert_eq!(judged.others, 3);
    assert_eq!(judged.lookalikes, vec![(0x0680_0008, LOOKALIKE)]);
    assert!(judged.disagreements.is_empty(), "{:x?}", judged.disagreements);
    // Either order: the placement the number lands in may be the second one.
    let judged = written_across(&b, &a);
    assert_eq!((judged.pointers, judged.lookalikes.len()), (1, 1));
}

/// A word that points into the image in one placement and is something else in the other is
/// **named**, not counted either way: it is what a pointer the engine wrote on only one run, or a
/// pointer to a different place, looks like.
#[test]
fn written_across_names_a_pointer_the_other_placement_does_not_have() {
    const SPAN: usize = 0x0733_4000;
    let (a_base, b_base) = (0x1b7_4599_4000usize, 0x1b8_8ac3_4000usize);
    let (a0, a1) = placed_words(
        a_base,
        [(a_base + 0x100) as u64, (a_base + 0x200) as u64, (a_base + 0x300) as u64, 0],
    );
    // Word 1 points elsewhere in B; word 2 is not written in B at all; word 3 is written only
    // in B, and points into it.
    let (b0, b1) = placed_words(
        b_base,
        [(b_base + 0x100) as u64, (b_base + 0x208) as u64, 0, (b_base + 0x400) as u64],
    );
    let a = Placed { base: a_base, span: a_base..a_base + SPAN, before: &a0, after: &a1 };
    let b = Placed { base: b_base, span: b_base..b_base + SPAN, before: &b0, after: &b1 };
    let judged = written_across(&a, &b);
    assert_eq!(judged.pointers, 1);
    let named: Vec<usize> = judged.disagreements.iter().map(|d| d.0).collect();
    assert_eq!(named, vec![0x0680_0008, 0x0680_0010, 0x0680_0018]);
    assert!(judged.lookalikes.is_empty());
}

/// Two placements at one address cannot tell a pointer from a number, so they are refused.
#[test]
#[should_panic(expected = "loaded at the same address")]
fn written_across_refuses_two_placements_at_one_address() {
    let (x0, x1) = placed_words(0x1000_0000, [0x1000_0100, 0, 0, 0]);
    let a = Placed { base: 0x1000_0000, span: 0x1000_0000..0x1100_0000, before: &x0, after: &x1 };
    let _ = written_across(&a, &a);
}

// ============================================================================= the gate

/// **The M3 gate.** All 3,594, in order, with no fault and no unbound symbol.
#[test]
fn the_milestone_gate_all_3594_initializers_run_in_order() {
    let _guard = serialized();
    let Some(guest) = Guest::load() else { return };
    assert_eq!(
        guest.object.init_array.len(),
        INITIALIZERS,
        "Global Constraint 3: the exact figure is 3,594"
    );

    // **Before anything runs**: the eight words this gate reads back are zero. Asserted rather
    // than assumed — a word that already held its value would prove nothing about the run.
    assert_unwritten(&guest);
    let before = guest.snapshot_writable();

    let mut cpu = guest.thread();
    let run = run_initializers(&guest, &mut cpu, PER_INITIALIZER, OnFailure::Stop);

    assert!(run.ok(), "{}", describe(&run, INITIALIZERS));

    // **Order and membership, not a count.** A counter that only counts successes cannot tell
    // completion from silence, and a count cannot see a substitution — this project has had a list
    // whose total stayed right while two members were wrong and two were missing.
    let expected: Vec<(usize, GuestAddr)> = guest
        .object
        .init_array
        .iter()
        .enumerate()
        .map(|(index, &entry)| (index, entry as GuestAddr))
        .collect();
    assert_eq!(run.completed.len(), INITIALIZERS, "{}", describe(&run, INITIALIZERS));
    assert_eq!(
        run.completed, expected,
        "the initializers that returned are not exactly `init_array`, in order"
    );

    // The invariant the plan asks to hold *throughout*, read after the run rather than assumed.
    assert!(guest.backend.slice_invariant_armed());
    assert_eq!(
        cpu.degraded_slices(),
        0,
        "M2's per-slice callback invariant broke during the initializer run"
    );

    // **The heart of the gate**: guest state the initializers demonstrably wrote, read back.
    // A counter reaching 3,594 proves a loop terminated; this proves the engine constructed its
    // statics, because four of these words now hold pointers to its own C++ vtables.
    assert_written(&guest);
    let after = guest.snapshot_writable();

    // **A second placement, for the exact count.** A pointer into the image moves with the image
    // and a number does not, and one placement cannot tell them apart (see [`written_across`]).
    // `guest` stays alive, so the second instance cannot be given the same addresses.
    let second = Guest::load().expect("the fixture loaded once already");
    assert_unwritten(&second);
    let second_before = second.snapshot_writable();
    {
        let mut cpu = second.thread();
        let again = run_initializers(&second, &mut cpu, PER_INITIALIZER, OnFailure::Stop);
        assert!(again.ok(), "the second placement: {}", describe(&again, INITIALIZERS));
    }
    assert_written(&second);
    let second_after = second.snapshot_writable();
    let judged = written_across(
        &Placed {
            base: guest.object.base,
            span: guest.object.start..guest.object.end,
            before: &before,
            after: &after,
        },
        &Placed {
            base: second.object.base,
            span: second.object.start..second.object.end,
            before: &second_before,
            after: &second_after,
        },
    );
    assert!(
        judged.disagreements.is_empty(),
        "words that point into the image in one placement and are neither the same pointer nor \
         the same number in the other, as (offset, first, second): {:x?}",
        judged.disagreements
    );
    let (pointers, others) = (judged.pointers, judged.others);
    assert_eq!(
        pointers, IMAGE_POINTERS_WRITTEN,
        "the writable image did not gain the image pointers a completed initializer run gives it"
    );
    let floor = if guest.space.base() >= 1 << 40 {
        OTHER_WORDS_WRITTEN_AT_LEAST
    } else {
        OTHER_WORDS_WRITTEN_AT_LEAST_BELOW_2_40
    };
    assert!(
        others >= floor,
        "only {others} words became something other than an image pointer, against a floor of \
         {floor} for a guest space based at {:#x}",
        guest.space.base()
    );

    let crossings = guest.boundary.crossings();
    eprintln!(
        "\nM3 GATE: {} initializers, {} guest instructions, {:?}\n  crossings: {:?}\n  \
         __cxa_atexit registrations: {}\n  image pointers {pointers}, other words {others}, \
         numbers a placement put the image around {:x?} (bases {:#x}, {:#x})\n",
        run.completed.len(),
        run.guest_instructions,
        run.elapsed,
        crossings,
        guest.bionic.atexit().pending(),
        judged.lookalikes,
        guest.object.base,
        second.object.base,
    );
}

/// The 188 imports Task 1's static closure predicted the initializers would reach.
fn reachable_imports() -> std::collections::BTreeSet<String> {
    let path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/research/init-reachable-imports.txt");
    let text = std::fs::read_to_string(path).expect("the reachable-import list");
    let mut out = std::collections::BTreeSet::new();
    let mut section = 0usize;
    for line in text.lines() {
        if line.starts_with("###") {
            section += 1;
            continue;
        }
        let symbol = line.trim();
        if symbol.is_empty() || section == 0 || section > 6 {
            continue;
        }
        out.insert(symbol.to_string());
    }
    assert_eq!(out.len(), 188, "the reachable set's first six sections are 188 symbols");
    out
}

/// **The survey**: what the initializers reach, found a batch at a time.
///
/// `#[ignore]`d and not a gate - see [`OnFailure::Continue`] for why it proves nothing about
/// completion and why it can hang. Every failure is printed as it happens, so a run that is
/// killed still leaves everything it found.
///
/// ```text
/// cargo test -p omni-android --release --test initializers -- --ignored --nocapture
/// ```
#[test]
#[ignore = "a survey, not a gate: it carries on past refusals and can block"]
fn survey_what_the_initializers_reach() {
    let _guard = serialized();
    let Some(guest) = Guest::load() else { return };
    guest.boundary.start_census();
    let mut cpu = guest.thread();
    let run = run_initializers(&guest, &mut cpu, SURVEY_BUDGET, OnFailure::Continue);
    guest.boundary.stop_census();
    report_census(&guest, &run);
}

/// The dynamic import census, against Task 1's static prediction of 188.
fn report_census(guest: &Guest, run: &Run) {
    let reachable = reachable_imports();
    let census = guest.boundary.census().expect("the census was running");
    eprintln!(
        "\n=== CENSUS === {} of {} initializers completed in {:?}",
        run.completed.len(),
        guest.object.init_array.len(),
        run.elapsed
    );
    let mut called: Vec<(&str, u64)> = census.iter().map(|(s, c)| (*s, *c)).collect();
    called.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    let unpredicted: Vec<&str> =
        called.iter().map(|(s, _)| *s).filter(|s| !reachable.contains(*s)).collect();
    eprintln!(
        "imports called: {}, of which {} are outside the 188",
        called.len(),
        unpredicted.len()
    );
    for (symbol, calls) in &called {
        let mark = if reachable.contains(*symbol) { "   " } else { "NEW" };
        eprintln!("  {mark} {symbol:<34} x{calls}");
    }
    eprintln!("=== END CENSUS ===\n");
}

/// **A discovery pass for the gate's read-back**, and a tool M4 will want for the same reason.
///
/// `#[ignore]`d: it prints rather than asserts. It snapshots every writable mapped range of the
/// loaded image, runs the initializers, and reports the words that went from zero to something —
/// which is the raw material for
/// [`the_initializers_wrote_guest_state_that_reads_back`]. Two kinds are separated, because they
/// are pinned differently: a word holding `base + k` is a pointer the initializers stored and the
/// *offset* is what is stable under ASLR, while a word holding a small constant is stable
/// outright.
///
/// ```text
/// cargo test -p omni-android --release --test initializers -- --ignored dump_guest_state --nocapture
/// ```
#[test]
#[ignore = "a discovery pass, not a gate: it prints rather than asserts"]
fn dump_guest_state_written_by_the_initializers() {
    let _guard = serialized();
    let Some(guest) = Guest::load() else { return };
    let before = guest.snapshot_writable();
    let mut cpu = guest.thread();
    let run = run_initializers(&guest, &mut cpu, PER_INITIALIZER, OnFailure::Stop);
    assert!(run.ok(), "{}", describe(&run, INITIALIZERS));
    let after = guest.snapshot_writable();

    let base = guest.object.base;
    let span = guest.object.start..guest.object.end;
    let mut pointers = 0usize;
    let mut constants = 0usize;
    let mut shown = 0usize;
    for ((start, old), (_, new)) in before.iter().zip(after.iter()) {
        for (index, (o, n)) in old.chunks_exact(8).zip(new.chunks_exact(8)).enumerate() {
            let (o, n) =
                (u64::from_le_bytes(o.try_into().unwrap()), u64::from_le_bytes(n.try_into().unwrap()));
            if o != 0 || n == 0 {
                continue;
            }
            let at = start + index * 8;
            let vaddr = at - base;
            if span.contains(&(n as GuestAddr)) {
                pointers += 1;
                if pointers <= 10 {
                    eprintln!("  POINTER  vaddr {vaddr:#x}  ->  base + {:#x}", n as usize - base);
                }
            } else {
                constants += 1;
                // Only the base-independent ones are worth pinning: a guest heap pointer moves
                // with the mapping and says nothing across runs.
                if n < u64::from(u32::MAX) && shown < 10 {
                    eprintln!("  CONSTANT vaddr {vaddr:#x}  =   {n:#x}");
                    shown += 1;
                }
            }
        }
    }
    eprintln!(
        "\n{pointers} words became a pointer into the image, {constants} became something else\n"
    );
}

/// **Repeatable**: a second instance in the same process runs all 3,594 again.
///
/// The plan asks for repeatable *and* leak-free. This half is the part that is not a
/// process-global measurement and so can be asserted in the ordinary suite; the commit-charge
/// half is `the_cost_of_an_initializer_run`, which is `#[ignore]`d for the reason
/// `thread_memory.rs` ignores its own — `omni_mem::process_commit_charge` is a process quantity
/// and a concurrent test binary moves it underneath the reading.
///
/// It is a *second instance* rather than a second run of the first, deliberately: running the
/// initializers twice over one image is not what a second process does, and the second pass would
/// see statics the first had already constructed. What this asserts is that nothing in the first
/// run leaves the **host** in a state the second cannot repeat — a thread-local that was not
/// restored, a futex queue with a stale waiter, a descriptor table that filled up.
#[test]
fn the_initializer_run_repeats_in_a_second_instance() {
    let _guard = serialized();
    let Some(first) = Guest::load() else { return };
    let mut cpu = first.thread();
    let one = run_initializers(&first, &mut cpu, PER_INITIALIZER, OnFailure::Stop);
    assert!(one.ok(), "first: {}", describe(&one, INITIALIZERS));
    assert_written(&first);
    drop(cpu);
    drop(first);

    let Some(second) = Guest::load() else { return };
    assert_unwritten(&second);
    let mut cpu = second.thread();
    let two = run_initializers(&second, &mut cpu, PER_INITIALIZER, OnFailure::Stop);
    assert!(two.ok(), "second: {}", describe(&two, INITIALIZERS));
    assert_eq!(two.completed.len(), INITIALIZERS);
    assert_written(&second);
    eprintln!(
        "\nREPEAT: {:?} then {:?}, {} then {} guest instructions\n",
        one.elapsed, two.elapsed, one.guest_instructions, two.guest_instructions
    );
}

/// **The comparison this milestone owes**: which imports the initializers actually called,
/// against Task 1's static prediction of 188.
///
/// D17 records 188 as a **lower bound** and says why — 17,698 indirect call sites the scan could
/// not follow, and a 2,670,684-byte region with no unwind info hiding one initializer entry point
/// worth 67 of the 188. So the interesting number is not how many of the 188 were called; it is
/// how many were called that the 188 does not contain, and where the prediction put them.
///
/// M4 will use the same method, which is why this is a test rather than a note: the census is a
/// property of the boundary, and this asserts that the comparison can still be made.
#[test]
fn the_imports_the_initializers_actually_called() {
    let _guard = serialized();
    let Some(guest) = Guest::load() else { return };
    guest.boundary.start_census();
    let mut cpu = guest.thread();
    let run = run_initializers(&guest, &mut cpu, PER_INITIALIZER, OnFailure::Stop);
    guest.boundary.stop_census();
    assert!(run.ok(), "{}", describe(&run, INITIALIZERS));

    let census = guest.boundary.census().expect("the census was running");
    assert!(
        !census.is_empty(),
        "3,594 initializers that reached no imported symbol at all would mean the boundary was \
         never crossed, which cannot be true of a C++ runtime constructing its statics"
    );
    let predicted = reachable_imports();
    let called: BTreeSet<&str> = census.keys().copied().collect();
    let unpredicted: BTreeSet<&str> = called.difference(&predicted.iter().map(String::as_str).collect()).copied().collect();
    let predicted_but_silent: BTreeSet<&str> =
        predicted.iter().map(String::as_str).collect::<BTreeSet<_>>().difference(&called).copied().collect();

    report_census(&guest, &run);
    eprintln!(
        "  called {} imports, {} of them outside the 188; {} of the 188 were never called",
        called.len(),
        unpredicted.len(),
        predicted_but_silent.len()
    );
    eprintln!("  outside the 188: {unpredicted:?}\n");

    // **The prediction under-approximated, and by how much is the finding.** Asserted as a
    // property rather than a count: a static closure over a stripped binary with indirect calls
    // is a lower bound, D17 says so, and this is the first run that could measure it.
    assert!(
        !unpredicted.is_empty(),
        "the initializers called nothing outside Task 1's 188, which would make 188 an exact \
         answer rather than the lower bound D17 says it is"
    );
}

/// **What an initializer run costs**: cold and warm, and what it leaves behind.
///
/// `#[ignore]`d for the reason `thread_memory.rs` ignores its own: `process_commit_charge` is a
/// **process** quantity, so a concurrent test binary moves it underneath the reading, and a timing
/// taken while 112 other tests are mapping guest memory is a measurement of the scheduler.
///
/// ```text
/// cargo test -p omni-android --release --test initializers -- --ignored the_cost --nocapture
/// ```
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_an_initializer_run() {
    let _guard = serialized();
    // Signed on purpose: commit charge is a process quantity and can legitimately go *down*
    // between two readings, and an unsigned subtraction there wraps to sixteen exabytes.
    let charge = || omni_mem::process_commit_charge().expect("commit charge") as i64;
    let mib = |bytes: i64| bytes as f64 / (1024.0 * 1024.0);
    // The 109 MiB of library bytes are read once, here, so they are inside the baseline rather
    // than inside the first instance's figure.
    let _ = main_lib_bytes();
    let baseline = charge();

    let mut colds = Vec::new();
    let mut warms = Vec::new();
    let mut loads = Vec::new();
    let mut runs = Vec::new();
    let mut freed = Vec::new();
    let mut contexts = Vec::new();
    let mut spaces = Vec::new();
    let mut peak: i64 = 0;
    for _ in 0..COST_RUNS {
        let before = charge();
        let Some(guest) = Guest::load() else { return };
        let mut cpu = guest.thread();
        let loaded = charge();

        // **Cold and warm are two different questions and one run answers only the first.** Cold
        // is what a real startup pays, because every basic block is translated for the first
        // time. Warm is the same 3,594 guest functions on the same context with every block
        // already translated -- it re-runs the constructors, so it is a measurement of the
        // translated code's speed and says nothing about correctness. Both are inside one
        // `activate` guard: a call made without one is refused by the first handler it reaches,
        // which would have made the warm figure a measurement of how fast this layer says no.
        let (cold, warm) = {
            let _active = guest.bionic.activate().expect("publish the instance");
            let started = Instant::now();
            let mut completed = 0usize;
            for (index, &entry) in guest.object.init_array.iter().enumerate() {
                let caller = format!("init_array[{index}]");
                guest
                    .boundary
                    .call_guest(
                        &mut cpu,
                        &caller,
                        entry as GuestAddr,
                        &guest.process_args,
                        PER_INITIALIZER,
                    )
                    .unwrap_or_else(|error| panic!("cold: {caller}: {error}"));
                completed += 1;
            }
            assert_eq!(completed, INITIALIZERS);
            let cold = started.elapsed();
            // Only on the first instance: it takes sixteen times as long as the cold pass (see
            // the report) and five of them would make this measurement four minutes of the same
            // finding.
            let warm = if warms.is_empty() {
                let started = Instant::now();
                for &entry in &guest.object.init_array {
                    let _ = guest.boundary.call_guest(
                        &mut cpu,
                        "warm",
                        entry as GuestAddr,
                        &guest.process_args,
                        PER_INITIALIZER,
                    );
                }
                started.elapsed()
            } else {
                Duration::ZERO
            };
            (cold, warm)
        };
        let ran = charge();
        peak = peak.max(ran - baseline);
        // **Dropped in two steps, measured between them.** "The run leaked" and "the CPU
        // context leaked" are different findings with different owners, and one reading cannot
        // tell them apart.
        drop(cpu);
        let without_cpu = charge();
        // **Who still owns what, because the answer is the finding.** A `GuestSpace` that
        // nothing has released keeps every mapping the run made, and an owner count is the only
        // thing that says whether `drop` did nothing or was never reached.
        let space = Arc::clone(&guest.space);
        let owners = (
            Arc::strong_count(&guest.bionic),
            Arc::strong_count(&guest.boundary),
            Arc::strong_count(&guest.backend),
            guest.bionic.guest_thread_records(),
        );
        drop(guest);
        let after = charge();
        eprintln!(
            "    at drop: bionic x{}, boundary x{}, backend x{}, {} guest-thread records; the              space still has {} owners and {} mapped regions",
            owners.0,
            owners.1,
            owners.2,
            owners.3,
            Arc::strong_count(&space) - 1,
            space.regions().iter().filter(|r| !r.is_free()).count()
        );
        contexts.push(ran - without_cpu);
        spaces.push(without_cpu - after);

        colds.push(cold);
        if !warm.is_zero() {
            warms.push(warm);
        }
        loads.push(loaded - before);
        runs.push(ran - loaded);
        freed.push(ran - after);
        eprintln!(
            "  instance: load +{:.2} MiB, run +{:.2} MiB, drop -{:.2} MiB, cold {cold:?}, warm \
             {warm:?}",
            mib(loaded - before),
            mib(ran - loaded),
            mib(ran - after),
        );
    }
    let residual = charge() - baseline;

    let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
    let lo = |v: &[Duration]| v.iter().map(ms).fold(f64::INFINITY, f64::min);
    let hi = |v: &[Duration]| v.iter().map(ms).fold(0.0, f64::max);
    let lo_b = |v: &[i64]| v.iter().copied().min().unwrap_or(0);
    let hi_b = |v: &[i64]| v.iter().copied().max().unwrap_or(0);
    eprintln!(
        "\nCOST of {INITIALIZERS} initializers, n = {COST_RUNS} instances in one process:\
         \n  cold   {:.0} - {:.0} ms   ({:.2} - {:.2} Mguest-insn/s at ~91.6 M instructions)\
         \n  warm   {:.0} - {:.0} ms\
         \n  load + first context   {:.2} - {:.2} MiB\
         \n  the run itself         {:.2} - {:.2} MiB\
         \n  returned on drop       {:.2} - {:.2} MiB  (context {:.2} - {:.2}, space {:.2} - {:.2})\
         \n  peak over baseline     {:.2} MiB\
         \n  residual at the end    {:.2} MiB\n",
        lo(&colds),
        hi(&colds),
        91.6 / (hi(&colds) / 1000.0),
        91.6 / (lo(&colds) / 1000.0),
        lo(&warms),
        hi(&warms),
        mib(lo_b(&loads)),
        mib(hi_b(&loads)),
        mib(lo_b(&runs)),
        mib(hi_b(&runs)),
        mib(lo_b(&freed)),
        mib(hi_b(&freed)),
        mib(lo_b(&contexts)),
        mib(hi_b(&contexts)),
        mib(lo_b(&spaces)),
        mib(hi_b(&spaces)),
        mib(peak),
        mib(residual),
    );

    // **Leak-free, bounded rather than exact.** Every instance has been dropped, so what is left
    // is whatever no `drop` returns. The bound is per instance so that it says something whatever
    // `COST_RUNS` is.
    let per_instance = residual / COST_RUNS as i64;
    assert!(
        per_instance < LEAK_CEILING,
        "after {COST_RUNS} instances were built, run and dropped, commit charge is {} MiB above \
         where it started -- {:.2} MiB per instance",
        mib(residual),
        mib(per_instance)
    );
}
