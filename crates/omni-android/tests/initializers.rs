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

#![cfg(target_arch = "x86_64")]

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
            // Straight to the process's stderr: `eprintln!` is captured by libtest and thrown away
            // for a passing test, so a skipped gate would look exactly like a passing one.
            let notice = format!(
                "\nSKIP: the M3 gate needs {APK_NAME}, which is not at {}. Every assertion about \
                 the 3,594 initializers was skipped.\n\n",
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
            .find(|l| l.file_name() == MAIN_LIB)
            .expect("the APK must contain libroblox.so");
        let cached = cache.extract(&apk, library.entry()).expect("extract libroblox.so");
        Some(cached.path().to_path_buf())
    })
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
        let bytes = std::fs::read(path).expect("read the cache entry");
        // `Executable` here and nowhere else: a section's protection caps every view's protection
        // for the life of the mapping (D11).
        let backing =
            Backing::open(path, MapExecutability::Executable).expect("open the cache entry");
        let elf = ElfImage::parse(&bytes).expect("parse libroblox.so");
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

    let crossings = guest.boundary.crossings();
    eprintln!(
        "\nM3 GATE: {} initializers, {} guest instructions, {:?}\n  crossings: {:?}\n  \
         __cxa_atexit registrations: {}\n",
        run.completed.len(),
        run.guest_instructions,
        run.elapsed,
        crossings,
        guest.bionic.atexit().pending(),
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
