//! **M4's gate: `JNI_OnLoad` and §8 steps 7-12, against the real `libroblox.so`.**
//!
//! ```text
//! cargo test -p omni-android --release --test jni_startup
//! ```
//!
//! M3's gate proved the 3,594 static initializers run. This file starts where it stops: it runs
//! them, then calls the engine's own `JNI_OnLoad` through the `JavaVM` this layer builds, then
//! drives the scripted Java-side sequence `jni-surface.md` §8 steps 7-12 describes.
//!
//! # What it has to prove, and what it refuses to accept as proof
//!
//! * **`JNI_OnLoad` returns `0x00010006`.** Not "did not crash": the value. §2.2 read that
//!   constant out of the binary at `0x2174dac`, and it is the whole of what step 6 is.
//!   Reaching it means the two `JavaVM` slots answered, `FindClass` found
//!   `LoggingProtocol`, `NewGlobalRef`, `GetStaticMethodID` and `ExceptionCheck` all ran, and
//!   the three registration helpers of step 6b resolved their classes.
//! * **What the engine asked for is read back**, not counted: the census of which JNI functions
//!   were called, the classes and members it looked up, and the misses.
//! * **The scripted sequence is run and reported step by step**, including the steps that fail.
//!   A script that stopped at the first failure would answer a different question.
//!
//! When the APK is absent every test here **fails** rather than skipping. That is this project's
//! own rule, learned twice: a test that early-returns when a fixture is missing passes without
//! asserting anything, and both High findings of the adapter review were exactly that shape.

#![cfg(target_arch = "x86_64")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use omni_android::bionic::{Bionic, GuestProcess, HwcapPolicy, ThreadHost};
use omni_android::jni::{script, slots, Jni};
use omni_android::{Boundary, BoundaryBuilder, GuestArg};
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicCpu, DynarmicOptions};
use omni_cpu::{GuestAddr, GuestCpu, RunLimit, XReg};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
use omni_mem::{Backing, CommitPolicy, GuestSpace, MapExecutability, Placement, Protection};

const APK_NAME: &str = "Roblox-2.738.1397.apk";
const MAIN_LIB: &str = "libroblox.so";

/// `DT_INIT_ARRAY` entries in `libroblox.so`. The same exact figure M3's gate asserts.
const INITIALIZERS: usize = 3_594;

/// Undefined, named symbols in `libroblox.so`'s `.dynsym`.
const TOTAL_IMPORTS: usize = 565;

/// `JNINativeInterface` + `JNIInvokeInterface` slots this layer installs.
const JNI_SLOTS: usize = 233 + 8;

/// Bytes of guest stack for the thread everything runs on.
const STACK_BYTES: usize = 8 * 1024 * 1024;

/// Guest instructions one initializer is allowed. The same budget M3's gate uses.
const PER_INITIALIZER: RunLimit = RunLimit::Instructions(200_000_000);

/// Guest instructions `JNI_OnLoad` is allowed.
///
/// §8 step 6a/6b: it caches the `JavaVM`, resolves one class, and runs three registration
/// helpers that between them take 161 `GetMethodID`, 84 `GetStaticMethodID` and 69 `GetFieldID`.
/// That is a few hundred thousand guest instructions, not hundreds of millions — but the budget
/// is generous for the same reason every other one here is: a counted budget that is too tight
/// reports `StepLimitReached`, which is a fact about the budget.
const ON_LOAD_BUDGET: RunLimit = RunLimit::Instructions(200_000_000);

/// What the gate tells the guest its memory budget is, for `sysinfo`.
const GUEST_MEMORY_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// **Serializes every test in this binary**, as M3's gate does: the 109 MB load is not worth
/// doing several times at once and the process commit charge is a process quantity.
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

fn cached_main_lib() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let apk_path = repo_root().join(APK_NAME);
        assert!(
            apk_path.is_file(),
            "M4's gate needs {APK_NAME}, which is not at {}. It is not skippable: this test is \
             the milestone's evidence, and a skipped test still reports `ok`.",
            apk_path.display()
        );
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

// ============================================================================= the loaded guest

/// A loaded `libroblox.so` with bionic **and** JNI behind it.
struct Guest {
    space: Arc<GuestSpace>,
    backend: Arc<DynarmicBackend>,
    bionic: Arc<Bionic>,
    jni: Arc<Jni>,
    boundary: Arc<Boundary>,
    object: LoadedObject,
    stack_top: GuestAddr,
    process_args: [GuestArg; 3],
    /// Every exported `Java_*` symbol, by name, already biased to a guest address.
    exports: std::collections::BTreeMap<String, GuestAddr>,
    _backing: Arc<Backing>,
    _root: Scratch,
}

impl Guest {
    fn load() -> Self {
        let path = cached_main_lib();
        let bytes = main_lib_bytes();
        let backing =
            Backing::open(path, MapExecutability::Executable).expect("open the cache entry");
        let elf = ElfImage::parse(bytes).expect("parse libroblox.so");
        let space = Arc::new(GuestSpace::new().expect("reserve a guest address space"));

        let backend = Arc::new(
            DynarmicBackend::new(Arc::clone(&space), DynarmicOptions::default())
                .expect("a translating backend"),
        );
        assert!(backend.owns_guest_paging(), "this guest has no demand pager");
        assert!(backend.slice_invariant_armed(), "M2's per-slice callback invariant is not armed");

        let bionic = Bionic::new(Arc::clone(&space)).expect("a bionic instance");
        // Room for every import **and** the 241 JNI slots. A region that ran out would refuse by
        // name (`AbiError::RegionFull`), which is the intended failure and not one this gate
        // wants to spend its load on.
        let builder = BoundaryBuilder::new(Arc::clone(&space), TOTAL_IMPORTS + JNI_SLOTS, 4096)
            .expect("a thunk region for 565 imports and 241 JNI slots");
        bionic.bind_into(&builder).expect("bind every handler");
        bionic
            .declare_data_into(
                &builder,
                &GuestProcess { stack_guard: backend.tls().stack_guard() },
            )
            .expect("declare and fill the eighteen data objects");
        bionic.set_log_to_stderr(false);
        // **D26**, constructed at the call site rather than defaulted into.
        bionic.set_hwcap_policy(HwcapPolicy::Decline);

        // **The JNI tables go in before the loader resolves anything.** They need no ordering
        // against the loader — a JNI slot is spelled `JNIEnv::FindClass` and `.dynstr` has no
        // `::` in it, so the two name spaces cannot collide — but they must be installed before
        // `finish`, because a `Boundary` is immutable.
        let jni = Jni::new(Arc::clone(&space)).expect("a JNI instance");
        let installed = jni.install_into(&builder).expect("install the JNI tables");
        assert_eq!(installed, JNI_SLOTS);
        script::declare_script_classes(&jni);
        define_host_answers(&jni);

        let root = Scratch::new("m4-gate");
        bionic.set_filesystem_root(&root.0).expect("a filesystem root");
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

        bionic.register_image(&object.dl_phdr_info()).expect("register the loaded image");

        // Every `Java_*` export, biased. `st_value` is a link-time `p_vaddr`, so the guest
        // address is `base + st_value` — the same relation `init_array` entries have.
        let exports = elf
            .exported_symbols()
            .expect("read .dynsym")
            .into_iter()
            .filter(|symbol| symbol.name.starts_with("Java_") || symbol.name == "JNI_OnLoad")
            .map(|symbol| {
                (symbol.name.to_string(), object.base + symbol.sym.st_value as GuestAddr)
            })
            .collect();

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

        Self {
            space,
            backend,
            bionic,
            jni,
            boundary,
            object,
            stack_top,
            process_args: [
                GuestArg::Int(1),
                GuestArg::Pointer(argv_block),
                GuestArg::Pointer(envp as GuestAddr),
            ],
            exports,
            _backing: backing,
            _root: root,
        }
    }

    fn thread(&self) -> DynarmicCpu {
        let mut cpu = self.backend.create_thread_with_tls().expect("a guest thread");
        self.boundary.install(&mut cpu).expect("install the boundary");
        cpu.set_sp(self.stack_top);
        cpu.set_x(XReg::new(30).expect("X30"), self.boundary.sentinel() as u64);
        cpu
    }

    /// Run every `init_array` entry, as M3's gate does, and return how many returned.
    ///
    /// Both activations are held across it: a JNI handler reached from an initializer — which
    /// nothing on this path does, but which nothing forbids either — would otherwise be refused
    /// with `JniNotActive`, and that refusal would be attributed to the initializer.
    fn run_initializers(&self, cpu: &mut DynarmicCpu) -> usize {
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _jni = self.jni.activate().expect("publish the JNI instance");
        let mut completed = 0usize;
        for (index, &entry) in self.object.init_array.iter().enumerate() {
            let caller = format!("init_array[{index}]");
            self.boundary
                .call_guest(cpu, &caller, entry as GuestAddr, &self.process_args, PER_INITIALIZER)
                .unwrap_or_else(|error| {
                    panic!("M4 needs every initializer: {caller} at {entry:#x} failed: {error}")
                });
            completed += 1;
        }
        completed
    }
}

/// **The decisions this host makes**, as against the ones the layer declares.
///
/// `Answer::Unanswered` is the registry saying "this member is on the measured surface and this
/// *layer* has not decided what it answers". That is the right default for anything whose value
/// belongs to the embedding, and this function is the embedding deciding — the same shape as
/// `Bionic::set_hwcap_policy`, where the type refuses until a call site chooses (D26).
fn define_host_answers(jni: &Jni) {
    use omni_android::jni::classes::Answer;

    // **`LoggingProtocol.getProcessTimestamp()J` — the one `JNI_OnLoad` itself calls.**
    //
    // §8 step 6a: `JNI_OnLoad` resolves it at `0x21740c0` and the helper at `0x2174c04` calls it.
    // The host owns "when did this process start", so answering is a decision rather than a stub
    // — but the **units are ASSUMED**: Roblox's Java side is not in this analysis, and
    // milliseconds since the Unix epoch is the overwhelmingly common Android spelling
    // (`System.currentTimeMillis`, `SystemClock.elapsedRealtime` and
    // `Process.getStartElapsedRealtime` are all milliseconds, though the last two are measured
    // from boot rather than from the epoch).
    //
    // What that could cost, stated rather than glossed: if the engine subtracts this from its own
    // `CLOCK_MONOTONIC` it gets a duration that is wrong by the machine's uptime. Nothing on
    // steps 6-12 does — the value is taken and stored — so the risk is recorded and not yet
    // discharged. `std::time::SystemTime` is portable standard library, correct on all five
    // targets, and is not a platform claim (see HANDOFF, "the other half of that rule").
    let epoch_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis())
        .min(i64::MAX as u128) as i64;
    jni.define(
        "com/roblox/universalapp/logging/LoggingProtocol",
        "getProcessTimestamp",
        "()J",
        true,
        Answer::Long(epoch_millis),
    )
    .expect("LoggingProtocol.getProcessTimestamp is declared");
}

/// A host directory that removes itself, for the guest's filesystem root.
struct Scratch(PathBuf);

impl Scratch {
    /// The directories the scripted sequence names, created under the confinement root.
    ///
    /// **The engine canonicalises them and throws if they are absent** — MEASURED:
    /// `nativeSetAssetPath` raised `boost::filesystem::canonical: No such file or directory:
    /// "/data/app/com.roblox.client"` and then `raise`d SIGTRAP. On a device the package manager
    /// has made them, so creating them here is the host doing what the platform does, not a
    /// workaround: the paths the script passes and the paths that exist have to be the same set,
    /// and this is the one place both are written down.
    const DIRECTORIES: &'static [&'static str] = &[
        "data/data/com.roblox.client/cache",
        "data/data/com.roblox.client/files",
        "data/data/com.roblox.client/shared_prefs",
        "data/app/com.roblox.client",
        // The engine asks for `dirname(assetPath)/android` as well as the asset path itself.
        // MEASURED: with `assetPath = /data/app/com.roblox.client` it canonicalises
        // `/data/app/android`. What that directory holds is a step-13 question.
        "data/app/android",
        "storage/emulated/0/Android/data/com.roblox.client",
        // Since the asset path became the decoded one (`script::ASSET_PATH`,
        // `app_assets/content`): the Java side's three directories, whose `android` sibling is
        // the `dirname(assetPath)/android` above for the new path.
        "data/data/com.roblox.client/app_assets/ExtraContent",
        "data/data/com.roblox.client/app_assets/android",
        "data/data/com.roblox.client/app_assets/content",
    ];

    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-m4-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        for directory in Self::DIRECTORIES {
            std::fs::create_dir_all(at.join(directory)).expect("an app directory");
        }
        // The APK itself, as a file the engine can canonicalise and open. Its *contents* are not
        // the APK: step 10 only records the path, and nothing before step 13 reads it.
        std::fs::write(at.join("data/app/com.roblox.client/base.apk"), b"")
            .expect("a placeholder for the package's own apk");
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
    fn resolve(
        &self,
        request: &omni_elf::loader::SymbolRequest<'_>,
    ) -> Option<omni_elf::loader::SymbolValue> {
        self.0.resolve(request)
    }
}

// ================================================================================== the tests

/// The whole of M4 in one run, because the 109 MB load and the 3,594 initializers cost too much
/// to repeat per assertion. Every assertion states what it is about.
///
/// **This is the gate.** It is written as one test on purpose: steps 6, 6a, 6b and 7-12 are
/// ordered, and a per-step test would either repeat the run or share mutable state between
/// tests, which is how a test comes to depend on the order `cargo test` happens to choose.
#[test]
fn jni_on_load_returns_jni_version_1_6_and_the_scripted_sequence_runs() {
    let _serial = serialized();
    let guest = Guest::load();
    let mut cpu = guest.thread();

    // **The import census on**, for the whole run. D17's point: 188 was a static lower bound
    // with 17,698 unfollowable indirect call sites behind it, and the only way to know what the
    // engine reaches is to watch it reach. It costs a relaxed load and a predictable branch per
    // crossing, so a timing run and a census run are different runs (`Boundary::start_census`).
    guest.boundary.start_census();

    // ---- steps 1-5, which M3 delivered ---------------------------------------------------
    let completed = guest.run_initializers(&mut cpu);
    assert_eq!(completed, INITIALIZERS, "M4 starts where M3's gate stops");
    assert_eq!(guest.object.init_array.len(), INITIALIZERS);

    // ---- the tables, read back out of guest memory ----------------------------------------
    //
    // The guest reaches a JNI function by `ldr Xb,[ENV]` then `ldr Xt,[Xb,#imm]` then `blr Xt`,
    // so what matters is that every one of the 233 words is a thunk address the boundary knows.
    // A zero anywhere is a branch to zero waiting to happen.
    let mem = guest.boundary.mem();
    let blame = omni_android::Blame::new("jni_startup", 0, 0);
    let mut distinct = std::collections::BTreeSet::new();
    for index in 0..slots::ENV_SLOTS.len() {
        let at = guest.jni.env_functions() + index * slots::SLOT_BYTES;
        let entry = mem.read_u64(at, blame).expect("the table is mapped") as GuestAddr;
        assert_ne!(entry, 0, "slot {index} ({}) is null", slots::ENV_SLOTS[index]);
        assert_eq!(
            guest.jni.env_slot_of(entry),
            Some(index),
            "slot {index} ({}) does not resolve back to itself",
            slots::ENV_SLOTS[index]
        );
        assert!(distinct.insert(entry), "two slots share a thunk address");
    }
    for index in 0..slots::VM_SLOTS.len() {
        let at = guest.jni.vm_functions() + index * slots::SLOT_BYTES;
        let entry = mem.read_u64(at, blame).expect("the table is mapped") as GuestAddr;
        assert_eq!(guest.jni.vm_slot_of(entry), Some(index));
    }

    // ---- step 6: JNI_OnLoad ----------------------------------------------------------------
    let on_load = *guest
        .exports
        .get("JNI_OnLoad")
        .expect("libroblox.so exports JNI_OnLoad; jni-surface.md §2.2 puts it at 0x2173ff4");
    assert_eq!(
        on_load - guest.object.base,
        0x0217_3ff4,
        "the export must be at the address the analysis read out of the binary"
    );

    let returned = {
        let _bionic = guest.bionic.activate().expect("publish the bionic instance");
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        guest.boundary.call_guest(
            &mut cpu,
            "JNI_OnLoad",
            on_load,
            // `(JavaVM* vm, void* reserved)`. `reserved` is null on every real VM.
            &[GuestArg::Pointer(guest.jni.java_vm()), GuestArg::Int(0)],
            ON_LOAD_BUDGET,
        )
    };

    let census = guest.jni.census();
    let misses = guest.jni.misses();
    let calls = guest.jni.calls();
    let (live_refs, live_objects, peak_refs) = guest.jni.reference_stats();
    let (live_pins, pinned_bytes, peak_pinned) = guest.jni.pin_stats();
    let _ = writeln!(
        std::io::stderr(),
        "\nM4 STEP 6: JNI_OnLoad at {on_load:#x} -> {:?}\n  census: {census:?}\n  \
         references: {live_refs} live, {live_objects} objects, peak {peak_refs}\n  \
         pins: {live_pins} live, {pinned_bytes} bytes, peak {peak_pinned}\n  \
         upcalls: {}\n  misses: {}\n",
        returned.as_ref().map(|r| r.as_i32()),
        calls.len(),
        misses.len()
    );
    for miss in misses.iter().take(40) {
        let _ = writeln!(
            std::io::stderr(),
            "  MISS {} {}.{} {}",
            miss.function,
            miss.class,
            miss.member,
            miss.descriptor
        );
    }
    let _ = std::io::stderr().flush();

    let returned = returned.unwrap_or_else(|error| {
        panic!(
            "§8 step 6: JNI_OnLoad at {on_load:#x} did not return. {error}\n  census so far: \
             {census:?}\n  misses so far: {misses:?}"
        )
    });
    assert_eq!(
        returned.as_i32(),
        slots::JNI_VERSION_1_6,
        "§8 step 6: JNI_OnLoad must return 0x00010006. §2.2 read that constant out of the binary \
         at 0x2174dac (`mov w2,#6; movk w2,#1,lsl#16`), so this is the value and not a range"
    );

    // ---- step 6a: what JNI_OnLoad demonstrably did -----------------------------------------
    //
    // Membership, not a count: a census with the right total and the wrong members would pass a
    // count, and this project has paid for that distinction three times.
    for function in ["GetEnv", "FindClass", "GetStaticMethodID", "ExceptionCheck"] {
        assert!(
            census.contains_key(function),
            "§8 step 6a names `{function}` and the census does not have it: {census:?}"
        );
    }
    assert!(
        census.contains_key("NewGlobalRef") || census.contains_key("NewWeakGlobalRef"),
        "step 6a caches the LoggingProtocol class; nothing took a durable reference: {census:?}"
    );
    // The thread began detached, so the engine's own scoped-attach helper had to notice. Either
    // it attached — in which case `AttachCurrentThread` is in the census and the thread carries
    // the name it built from `gettid` — or `GetEnv` answered `JNI_OK` because something else
    // attached first. Both are legitimate; which happened is recorded rather than assumed.
    let attached = guest.jni.is_attached(0);
    let _ = writeln!(
        std::io::stderr(),
        "  step 6a: thread 0 attached={attached}, name={:?}, AttachCurrentThread calls={:?}",
        guest.jni.thread_names().first().cloned().flatten(),
        census.get("AttachCurrentThread")
    );

    // ---- steps 7-11: the scripted sequence (step 12 waits for the engine) -------------------
    let outcomes = {
        let _bionic = guest.bionic.activate().expect("publish the bionic instance");
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        script::run(
            &guest.jni,
            &guest.boundary,
            &mut cpu,
            &|symbol| guest.exports.get(symbol).copied(),
            script::SEQUENCE,
            0,
        )
        .expect("building the scripted arguments must not fail")
    };

    let _ = writeln!(std::io::stderr(), "\nM4 STEPS 7-12:");
    for outcome in &outcomes {
        match &outcome.result {
            Ok(()) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "  step {:>2} OK   {} ({} insn in the last segment)",
                    outcome.step,
                    outcome.symbol,
                    outcome.last_segment_instructions
                );
            }
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "  step {:>2} FAIL {}: {error}",
                    outcome.step,
                    outcome.symbol
                );
                // **A bad guest pointer says nothing on its own.** What a reader needs is what
                // is mapped there, and whether it is inside one of this layer's own arenas --
                // the JNI tables, the pinned pool -- or in the guest's heap.
                if let omni_android::AbiError::BadPointer { pointer, .. } = &error {
                    let _ = writeln!(
                        std::io::stderr(),
                        "         pointer {pointer:#x}: region {:?}; jni arena {:#x}, pinned pool {:#x}..{:#x}",
                        guest.space.region_at(*pointer),
                        guest.jni.java_vm(),
                        guest.jni.pool_base(),
                        guest.jni.pool_base() + guest.jni.pool_bytes(),
                    );
                }
            }
        }
    }
    let _ = std::io::stderr().flush();

    // Every symbol the script names **must exist**: Section G tags all 21 `SHORT:libroblox.so`,
    // so a missing one means the mangling or the class name is wrong here, not that the engine
    // changed.
    for outcome in &outcomes {
        assert!(
            outcome.target.is_some(),
            "§8 step {}: `{}` is tagged SHORT:libroblox.so in Section G and was not found among \
             the {} Java_* exports",
            outcome.step,
            outcome.symbol,
            guest.exports.len()
        );
    }

    let reached = outcomes.iter().filter(|o| o.ok()).count();
    let misses = guest.jni.misses();
    let calls = guest.jni.calls();
    let _ = writeln!(
        std::io::stderr(),
        "\nM4 SUMMARY: {reached} of {} scripted downcalls returned; {} upcalls into Java; {} \
         member lookups nothing declares\n  census: {:?}\n",
        outcomes.len(),
        calls.len(),
        misses.len(),
        guest.jni.census()
    );
    for miss in misses.iter().take(80) {
        let _ = writeln!(
            std::io::stderr(),
            "  MISS {} {}.{} {}",
            miss.function,
            miss.class,
            miss.member,
            miss.descriptor
        );
    }
    for call in calls.iter().take(40) {
        let _ = writeln!(
            std::io::stderr(),
            "  UPCALL {}.{}{} {:?}",
            call.class,
            call.member,
            call.descriptor,
            call.args
        );
    }
    let _ = std::io::stderr().flush();

    // **Steps 7 and 8 are the assertion.** They are the two that take nothing but a `Context` and
    // no arguments at all, so they are the ones whose failure would be this layer's rather than a
    // consequence of an `InitParams` object whose members the analysis could not attribute
    // (Section D's `!unresolved` group). The later steps are **reported**, not asserted: what
    // they need is what M5 has to find out, and asserting a number here would pin a figure this
    // run is measuring rather than checking.
    for outcome in outcomes.iter().filter(|o| o.step <= 8) {
        if let Err(error) = &outcome.result {
            panic!(
                "§8 step {}: `{}` failed and it takes only a Context: {error}",
                outcome.step, outcome.symbol
            );
        }
    }

    guest.boundary.stop_census();
    let imports = guest.boundary.census().expect("the census was started");
    let _ = writeln!(
        std::io::stderr(),
        "
M4 IMPORT CENSUS: {} distinct imported symbols called across the whole run
  {:?}
",
        imports.len(),
        imports
    );
    let _ = std::io::stderr().flush();

    // Nothing may be left pinned: every `Get…Chars` the engine made is paired with a `Release…`,
    // and a non-zero count here is a leak this layer can see.
    let (live_pins, pinned_bytes, _) = guest.jni.pin_stats();
    assert_eq!(live_pins, 0, "{pinned_bytes} bytes are still pinned after the whole sequence");
}

/// The 21 scripted symbols -- [`script::SEQUENCE`]'s 20 and [`script::ENGINE_SETTINGS`]'s one --
/// all exist in the real binary, and each is at a distinct address.
///
/// Separate from the gate because it needs the ELF and **not** the run: a symbol table check
/// that had to pay for 3,594 initializers would not be run when it was the thing being changed.
#[test]
fn every_scripted_symbol_exists_in_the_real_binary() {
    let _serial = serialized();
    let elf = ElfImage::parse(main_lib_bytes()).expect("parse libroblox.so");
    let exports: std::collections::BTreeMap<&str, u64> = elf
        .exported_symbols()
        .expect("read .dynsym")
        .into_iter()
        .map(|symbol| (symbol.name, symbol.sym.st_value))
        .collect();

    assert_eq!(
        exports.get("JNI_OnLoad").copied(),
        Some(0x0217_3ff4),
        "§2.2 puts JNI_OnLoad at 0x2173ff4"
    );

    let mut addresses = std::collections::BTreeSet::new();
    for step in script::SEQUENCE.iter().chain(script::ENGINE_SETTINGS) {
        let symbol = step.symbol();
        let at = exports.get(symbol.as_str()).copied().unwrap_or_else(|| {
            panic!(
                "§8 step {}: `{symbol}` is tagged SHORT:libroblox.so in Section G and is not an \
                 export of this binary",
                step.step
            )
        });
        assert_ne!(at, 0, "{symbol}");
        addresses.insert(at);
    }
    assert_eq!(
        addresses.len(),
        script::SEQUENCE.len() + script::ENGINE_SETTINGS.len(),
        "two scripted downcalls resolve to the same address, so one of them is mangled wrong"
    );

    // The 24 GameActivity natives are bound by `RegisterNatives` rather than exported, with the
    // single documented exception of `initializeNativeCode`, which §4.1 records as appearing in
    // the table **and** as a static export whose trampoline runs the one-time class-info init.
    // Asserted here because it is what step 13 will be called through, and because a change to
    // it would silently move M5's entry point.
    assert!(
        exports.contains_key("Java_com_google_androidgamesdk_GameActivity_initializeNativeCode"),
        "§4.1: initializeNativeCode is exported as well as registered"
    );
    for member in ["onStartNative", "onResumeNative", "onSurfaceCreatedNative"] {
        let symbol = script::mangle("com/google/androidgamesdk/GameActivity", member);
        assert!(
            !exports.contains_key(symbol.as_str()),
            "{symbol} is RegisterNatives-only (§4), so an export of it would mean the binary \
             changed"
        );
    }
}
