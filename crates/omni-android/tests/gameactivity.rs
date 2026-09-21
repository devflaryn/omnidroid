//! **M5's gate: `jni-surface.md` §8 step 13, against the real `libroblox.so`.**
//!
//! ```text
//! cargo test -p omni-android --release --test gameactivity -- --nocapture
//! ```
//!
//! M4's gate proved `JNI_OnLoad` returns `0x00010006` and drove §8 steps 7-12. This one starts
//! where that stops: it does all of it again, then calls the **exported**
//! `Java_com_google_androidgamesdk_GameActivity_initializeNativeCode` — §5.2's fourteen-step
//! constructor, the `GameActivity_onCreate` it calls, the game thread that spawns, and the
//! `pthread_cond_wait` it blocks on until that thread signals.
//!
//! # What it has to prove, and what it refuses to accept as proof
//!
//! * **The returned `jlong` is non-zero.** §8.1's fourth failure mode is that
//!   `ALooper_forThread()` returning null makes this call return `0` — *silently*, because a zero
//!   is indistinguishable from a handle the Java side would then pass to all 23 other natives. So
//!   a looper is prepared and **asserted present before the call**, which turns that failure mode
//!   from a diagnosis into a precondition.
//! * **The `NativeCode` is read back out of guest memory at §5.2's own offsets** — `sdkVersion` at
//!   `+0x30`, `callbacks == this + 0x50` at `+0x00`, the looper at `+0x158`, `msgread`/`msgwrite`
//!   at `+0x150`/`+0x154`, `assetManager` at `+0x40`, `instance` at `+0x38`. Membership, not a
//!   count: each is a separate assertion naming its offset, because a handle that is merely
//!   non-zero proves only that something was allocated.
//! * **A hang is a failure, not a slow pass.** §8.1's fifth failure mode is that a deadlock in the
//!   cond-wait is indistinguishable from a hang. A watchdog on another thread prints
//!   `Bionic::parked()`, the looper event log and the JNI misses, and then ends the process — a
//!   watchdog that printed and continued would turn a hang into a test that never finishes.
//!
//! When the APK is absent every test here **fails** rather than skipping: `VERIFICATION.md` entry
//! 4, learned twice.

#![cfg(target_arch = "x86_64")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use omni_android::bionic::{Bionic, GuestProcess, HwcapPolicy, ThreadHost};
use omni_android::jni::classes::Answer;
use omni_android::jni::{script, slots, Jni};
use omni_android::ndk::assets::{AssetSource, ASSET_MANAGER_CLASS};
use omni_android::ndk::{DeviceConfiguration, Ndk, ScreenSize, ACONFIGURATION_NAVHIDDEN_NO};
use omni_android::{Boundary, BoundaryBuilder, GuestArg};
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicCpu, DynarmicOptions};
use omni_cpu::{GuestAddr, GuestCpu, RunLimit, XReg};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
use omni_mem::{Backing, CommitPolicy, GuestSpace, MapExecutability, Placement, Protection};

const APK_NAME: &str = "Roblox-2.738.1397.apk";
const MAIN_LIB: &str = "libroblox.so";

/// `DT_INIT_ARRAY` entries in `libroblox.so`. The same exact figure M3's and M4's gates assert.
const INITIALIZERS: usize = 3_594;

/// Undefined, named symbols in `libroblox.so`'s `.dynsym`.
const TOTAL_IMPORTS: usize = 565;

/// `JNINativeInterface` + `JNIInvokeInterface` slots this layer installs.
const JNI_SLOTS: usize = 233 + 8;

/// Bytes of guest stack for the thread everything runs on.
const STACK_BYTES: usize = 8 * 1024 * 1024;

/// Guest instructions one initializer is allowed. The same budget M3's and M4's gates use.
const PER_INITIALIZER: RunLimit = RunLimit::Instructions(200_000_000);

/// Guest instructions `JNI_OnLoad` is allowed.
const ON_LOAD_BUDGET: RunLimit = RunLimit::Instructions(200_000_000);

/// Guest instructions **step 13** is allowed.
///
/// Larger than the others because this one call is the whole GameActivity constructor, the app
/// glue's `onCreate`, and a wait for a thread that itself runs `android_app_entry` and enters
/// `android_main`. A counted budget rather than `RunLimit::Unlimited` for D16's reason, and
/// comfortably below `i64::MAX`, which is D16's footgun: the emitted comparison is signed.
const STEP_13_BUDGET: RunLimit = RunLimit::Instructions(2_000_000_000);

/// What the gate tells the guest its memory budget is, for `sysinfo`.
const GUEST_MEMORY_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// How long step 13 may take in **wall-clock** time before the watchdog ends the run.
///
/// Wall clock rather than instructions because the failure this bounds is a *wait*: a guest parked
/// on a condition variable executes no guest instructions, so [`STEP_13_BUDGET`] can never expire
/// for it. That is §8.1's fifth failure mode, and it is why this number exists at all.
const WATCHDOG_SECONDS: u64 = 180;

/// What the host says `ro.build.version.sdk` is.
///
/// **A decision.** §5.2 step 2 reads it into `activity->sdkVersion` at `+0x30`, and the glue
/// branches on it — the window-insets path, the text-input path and the
/// `setImeEditorInfoFields` call all test it. 33 is Android 13, which is comfortably inside the
/// range GameActivity 2.x supports and old enough that nothing requires an API this layer has not
/// been asked for yet. Reported rather than assumed correct: what the engine *does* with it is
/// one of the measurements this gate takes.
const SDK_VERSION: &str = "33";

/// **Serializes every test in this binary**, as M3's and M4's gates do.
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

fn apk_path() -> PathBuf {
    repo_root().join(APK_NAME)
}

fn cached_main_lib() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let at = apk_path();
        assert!(
            at.is_file(),
            "M5's gate needs {APK_NAME}, which is not at {}. It is not skippable: this test is \
             the milestone's evidence, and a skipped test still reports `ok`.",
            at.display()
        );
        let apk = omni_apk::Apk::open(&at).expect("the real APK must open");
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

// ====================================================================== the real APK's assets

/// The engine's own assets, out of the real APK.
///
/// **The real thing rather than a table**, because what `AAssetManager_open` is asked for is one
/// of this gate's measurements and a fixture would answer for names nobody chose. `omni-apk` is a
/// dev-dependency of this crate exactly so a test can do this and the crate itself cannot.
#[derive(Debug)]
struct ApkAssets {
    apk: Mutex<omni_apk::Apk>,
}

impl ApkAssets {
    fn open() -> ApkAssets {
        ApkAssets { apk: Mutex::new(omni_apk::Apk::open(apk_path()).expect("the real APK")) }
    }
}

impl AssetSource for ApkAssets {
    fn read(&self, name: &[u8]) -> Option<Vec<u8>> {
        let name = std::str::from_utf8(name).ok()?;
        let apk = self.apk.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        apk.read_asset(name).ok()
    }
}

// ============================================================================= the loaded guest

/// A loaded `libroblox.so` with bionic, JNI **and** the NDK surface behind it.
struct Guest {
    #[allow(dead_code)]
    space: Arc<GuestSpace>,
    backend: Arc<DynarmicBackend>,
    bionic: Arc<Bionic>,
    jni: Arc<Jni>,
    ndk: Arc<Ndk>,
    boundary: Arc<Boundary>,
    object: LoadedObject,
    stack_top: GuestAddr,
    process_args: [GuestArg; 3],
    exports: std::collections::BTreeMap<String, GuestAddr>,
    _backing: Arc<Backing>,
    _root: Scratch,
}

/// What this gate decides the device is. Every field is a decision; see `ndk::config`.
fn device_configuration() -> DeviceConfiguration {
    DeviceConfiguration {
        language: *b"en",
        country: *b"US",
        screen_width_dp: 411,
        screen_height_dp: 731,
        screen_size: ScreenSize::Normal,
        nav_hidden: ACONFIGURATION_NAVHIDDEN_NO,
    }
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
        let ndk = Ndk::new(Arc::clone(&space)).expect("an NDK instance");
        // Room for every import, the 241 JNI slots and the NDK surface.
        let builder = BoundaryBuilder::new(
            Arc::clone(&space),
            TOTAL_IMPORTS + JNI_SLOTS + Ndk::bound_symbols().count(),
            4096,
        )
        .expect("a thunk region");
        bionic.bind_into(&builder).expect("bind every bionic handler");
        ndk.bind_into(&builder).expect("bind every NDK handler");
        bionic
            .declare_data_into(
                &builder,
                &GuestProcess { stack_guard: backend.tls().stack_guard() },
            )
            .expect("declare and fill the eighteen data objects");
        bionic.set_log_to_stderr(true);
        bionic.set_hwcap_policy(HwcapPolicy::Decline);

        let jni = Jni::new(Arc::clone(&space)).expect("a JNI instance");
        let installed = jni.install_into(&builder).expect("install the JNI tables");
        assert_eq!(installed, JNI_SLOTS);
        script::declare_script_classes(&jni);
        define_host_answers(&jni);

        let root = Scratch::new("m5-gate");
        bionic.set_filesystem_root(&root.0).expect("a filesystem root");
        bionic.set_memory_budget(GUEST_MEMORY_BUDGET);
        // §5.2 step 2. The host has to *set* it or the SDK version field is empty.
        bionic
            .set_system_property("ro.build.version.sdk", SDK_VERSION)
            .expect("the SDK version is a decision this gate makes");
        // **A created guest thread carries all three instances, not just bionic.**
        // MEASURED by an earlier run of this gate: without the NDK instance the game thread
        // `GameActivity_onCreate` spawns died on its first `AConfiguration_new`, never set
        // `app->running`, and the calling thread waited on its condition variable for ever --
        // §8 row 14 and §8.1's fifth failure mode at once. The watchdog and `Bionic::parked()`
        // named the parked thread, its condvar and its mutex, which is the only reason it took
        // three minutes to find.
        let host: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&backend) as _;
        bionic
            .set_thread_host(
                ThreadHost::new(host)
                    .with_instance(jni.thread_instance())
                    .with_instance(ndk.thread_instance()),
            )
            .expect("a thread host");

        ndk.set_asset_source(Arc::new(ApkAssets::open())).expect("the real APK's assets");
        ndk.set_configuration(device_configuration());

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
            ndk,
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

    /// Run every `init_array` entry, and return how many returned.
    fn run_initializers(&self, cpu: &mut DynarmicCpu) -> usize {
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _jni = self.jni.activate().expect("publish the JNI instance");
        let _ndk = self.ndk.activate();
        let mut completed = 0usize;
        for (index, &entry) in self.object.init_array.iter().enumerate() {
            let caller = format!("init_array[{index}]");
            self.boundary
                .call_guest(cpu, &caller, entry as GuestAddr, &self.process_args, PER_INITIALIZER)
                .unwrap_or_else(|error| {
                    panic!("M5 needs every initializer: {caller} at {entry:#x} failed: {error}")
                });
            completed += 1;
        }
        completed
    }

    /// Read a `u32` out of the `NativeCode` the engine allocated.
    fn field_u32(&self, base: GuestAddr, offset: usize) -> u32 {
        self.boundary
            .mem()
            .read_u32(base + offset, omni_android::Blame::new("NativeCode", base, 0))
            .unwrap_or_else(|error| panic!("NativeCode+{offset:#x} is not readable: {error}"))
    }

    /// Read a `u64` out of the `NativeCode`.
    fn field_u64(&self, base: GuestAddr, offset: usize) -> u64 {
        self.boundary
            .mem()
            .read_u64(base + offset, omni_android::Blame::new("NativeCode", base, 0))
            .unwrap_or_else(|error| panic!("NativeCode+{offset:#x} is not readable: {error}"))
    }
}

/// **The decisions this host makes**, as against the ones the layer declares.
fn define_host_answers(jni: &Jni) {
    // `LoggingProtocol.getProcessTimestamp()J`, as M4's gate decides it. D28 records that the
    // units are ASSUMED to be milliseconds since the Unix epoch.
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

    // **The Java `Configuration` and the native `AConfiguration` answer the same question**, and a
    // host that decided one and left the other at its declared default would have the engine
    // reading two different screen widths from two places. Both are this gate's decision.
    let decided = device_configuration();
    for (field, value) in [
        ("screenWidthDp", decided.screen_width_dp),
        ("screenHeightDp", decided.screen_height_dp),
        ("smallestScreenWidthDp", decided.screen_width_dp),
        // 411 x 731 dp at 1080 x 1920 px is 2.625x, which is `DENSITY_DPI` 420 -- the density
        // bucket a 1080p phone of that size reports. Derived from the two numbers above rather
        // than chosen separately, so the three cannot disagree.
        ("densityDpi", 420),
    ] {
        jni.define_field("android/content/res/Configuration", field, "I", false, Answer::Int(value))
            .unwrap_or_else(|error| panic!("`Configuration.{field}` is declared: {error}"));
    }
}

/// A host directory that removes itself, for the guest's filesystem root.
struct Scratch(PathBuf);

impl Scratch {
    /// The directories the engine canonicalises. M4's gate measured this list; see its copy.
    const DIRECTORIES: &'static [&'static str] = &[
        "data/data/com.roblox.client/cache",
        "data/data/com.roblox.client/files",
        "data/data/com.roblox.client/shared_prefs",
        "data/app/com.roblox.client",
        "data/app/android",
        "storage/emulated/0/Android/data/com.roblox.client",
        // §5.2 step 9: `initializeNativeCode` takes three directory paths and stores them. The
        // glue does not create them, so the host does -- which is what the package manager does
        // on a device.
        "data/data/com.roblox.client/obb",
        "storage/emulated/0/Android/obb/com.roblox.client",
    ];

    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-m5-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        for directory in Self::DIRECTORIES {
            std::fs::create_dir_all(at.join(directory)).expect("an app directory");
        }
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

// ================================================================================== the gate

/// **§5.2's own offsets into the 632-byte `NativeCode`.** Each is asserted separately.
mod native_code {
    /// `activity->callbacks`, which must be `this + 0x50`.
    pub const CALLBACKS: usize = 0x00;
    /// `activity->vm`, from `env->GetJavaVM`.
    pub const VM: usize = 0x08;
    /// `activity->env`.
    pub const ENV: usize = 0x10;
    /// `activity->javaGameActivity`, a global reference to `thiz`.
    pub const JAVA_GAME_ACTIVITY: usize = 0x18;
    /// `activity->internalDataPath`.
    pub const INTERNAL_DATA_PATH: usize = 0x20;
    /// `activity->externalDataPath`.
    pub const EXTERNAL_DATA_PATH: usize = 0x28;
    /// `activity->sdkVersion`.
    pub const SDK_VERSION: usize = 0x30;
    /// `activity->instance`, the `android_app *` `GameActivity_onCreate` allocated.
    pub const INSTANCE: usize = 0x38;
    /// `activity->assetManager`, from `AAssetManager_fromJava`.
    pub const ASSET_MANAGER: usize = 0x40;
    /// `activity->obbPath`.
    pub const OBB_PATH: usize = 0x48;
    /// `msgread`.
    pub const MSGREAD: usize = 0x150;
    /// `msgwrite`.
    pub const MSGWRITE: usize = 0x154;
    /// The `ALooper *`.
    pub const LOOPER: usize = 0x158;
    /// The `AAssetManager` global reference.
    pub const ASSET_MANAGER_REF: usize = 0x160;
    /// Bytes `operator new` was asked for: `0x278`.
    pub const BYTES: usize = 0x278;
}

/// The whole of M5 in one run.
///
/// **This is the gate**, and it is one test for M4's reason: steps 1-13 are ordered, and a
/// per-step test would either repeat the 109 MB load and the 3,594 initializers or share mutable
/// state between tests.
#[test]
fn initialize_native_code_returns_a_native_code_and_the_game_thread_starts() {
    let _serial = serialized();
    let guest = Guest::load();
    let mut cpu = guest.thread();
    guest.boundary.start_census();

    // ---- steps 1-5, which M3 delivered ---------------------------------------------------
    let completed = guest.run_initializers(&mut cpu);
    assert_eq!(completed, INITIALIZERS, "M5 starts where M4's gate starts");

    // ---- step 6: JNI_OnLoad, which M4 delivered --------------------------------------------
    let on_load = *guest.exports.get("JNI_OnLoad").expect("libroblox.so exports JNI_OnLoad");
    let returned = {
        let _bionic = guest.bionic.activate().expect("publish the bionic instance");
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        let _ndk = guest.ndk.activate();
        guest.boundary.call_guest(
            &mut cpu,
            "JNI_OnLoad",
            on_load,
            &[GuestArg::Pointer(guest.jni.java_vm()), GuestArg::Int(0)],
            ON_LOAD_BUDGET,
        )
    }
    .expect("§8 step 6: JNI_OnLoad must return");
    assert_eq!(returned.as_i32(), slots::JNI_VERSION_1_6, "§8 step 6");

    // ---- steps 7-12: the scripted sequence, which M4 delivered -----------------------------
    let outcomes = {
        let _bionic = guest.bionic.activate().expect("publish the bionic instance");
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        let _ndk = guest.ndk.activate();
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
    let reached = outcomes.iter().filter(|o| o.ok()).count();
    let _ = writeln!(
        std::io::stderr(),
        "\nM5: steps 7-12 reached {reached} of {} scripted downcalls",
        outcomes.len()
    );
    for outcome in outcomes.iter().filter(|o| o.step <= 8) {
        if let Err(error) = &outcome.result {
            panic!("§8 step {}: `{}` failed: {error}", outcome.step, outcome.symbol);
        }
    }

    // ---- §8.1's fourth failure mode, made into a precondition -------------------------------
    //
    // The looper has to exist on **this** thread before step 13, because `ALooper_forThread()`
    // returning null makes the call return 0 and a zero is indistinguishable from a handle. A
    // gate that asserts the looper is there is diagnosing nothing; one that reads a zero back
    // afterwards is diagnosing everything at once.
    let looper = guest.ndk.prepare_looper().expect("a looper on the thread that calls step 13");
    assert_ne!(looper, 0, "§8.1 failure mode 4");
    assert_eq!(
        guest.ndk.looper_for_current_thread(),
        Some(looper),
        "and `ALooper_forThread` will find it"
    );

    // ---- step 13's arguments ----------------------------------------------------------------
    let (internal, external, obb, asset_manager, configuration) = {
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        (
            guest.jni.new_string("/data/data/com.roblox.client/files").expect("internalDataDir"),
            guest
                .jni
                .new_string("/storage/emulated/0/Android/data/com.roblox.client")
                .expect("externalDataDir"),
            guest.jni.new_string("/data/data/com.roblox.client/obb").expect("obbDir"),
            guest.jni.new_object(ASSET_MANAGER_CLASS).expect("a Java AssetManager"),
            guest.jni.new_object("android/content/res/Configuration").expect("a Configuration"),
        )
    };
    // `thiz` is the `GameActivity` instance the Java side would be calling from.
    let thiz = {
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        guest.jni.new_object("com/google/androidgamesdk/GameActivity").expect("a GameActivity")
    };

    let step_13 = *guest
        .exports
        .get("Java_com_google_androidgamesdk_GameActivity_initializeNativeCode")
        .expect("§4.1: initializeNativeCode is exported as well as registered");

    // ---- the watchdog -----------------------------------------------------------------------
    //
    // **A hang is a failure.** A guest parked in `pthread_cond_wait` executes no guest
    // instructions, so `STEP_13_BUDGET` can never expire for it; this is the only thing that can
    // end such a run, and it ends it with the diagnosis rather than silently.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let done = Arc::clone(&done);
        let bionic = Arc::clone(&guest.bionic);
        let ndk = Arc::clone(&guest.ndk);
        let jni = Arc::clone(&guest.jni);
        let boundary = Arc::clone(&guest.boundary);
        std::thread::spawn(move || {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(WATCHDOG_SECONDS);
            while std::time::Instant::now() < deadline {
                if done.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            if done.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let mut out = std::io::stderr();
            let _ = writeln!(
                out,
                "\n================ M5 WATCHDOG: step 13 did not return in {WATCHDOG_SECONDS} s \
                 ================\n\
                 last import: {:?}\n\
                 live guest threads: {}\n\
                 PARKED (jni-surface.md §8.1 failure mode 5):",
                boundary.last_call().map(|slot| slot.symbol.clone()),
                bionic.live_guest_threads(),
            );
            for held in bionic.parked() {
                let _ = writeln!(
                    out,
                    "  thread {:?} in `{}` on cond {:#x} holding mutex {:#x} for {:?}",
                    held.thread, held.symbol, held.cond, held.mutex, held.waiting
                );
            }
            let _ = writeln!(out, "LOOPER EVENTS ({} dropped):", ndk.events_dropped());
            for event in ndk.events().iter().rev().take(40).rev() {
                let _ = writeln!(
                    out,
                    "  thread {} {:<10} looper {:#x}: {}",
                    event.thread, event.what, event.looper, event.detail
                );
            }
            let _ = writeln!(out, "NDK CENSUS: {:?}", ndk.census());
            let _ = writeln!(out, "JNI CENSUS: {:?}", jni.census());
            for miss in jni.misses().iter().take(40) {
                let _ = writeln!(
                    out,
                    "  MISS {} {}.{} {}",
                    miss.function, miss.class, miss.member, miss.descriptor
                );
            }
            let _ = writeln!(out, "================ ending the run ================");
            let _ = out.flush();
            // The main thread is blocked inside the guest and will never return, so failing the
            // assertion from here is not possible. Ending the process with a failing status is,
            // and it is what turns a hang into a red run instead of one that never finishes.
            std::process::exit(101);
        });
    }

    // ---- step 13 ----------------------------------------------------------------------------
    let handle = {
        let _bionic = guest.bionic.activate().expect("publish the bionic instance");
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        let _ndk = guest.ndk.activate();
        guest.boundary.call_guest(
            &mut cpu,
            "GameActivity_initializeNativeCode",
            step_13,
            &[
                GuestArg::Pointer(guest.jni.env_for(0)),
                GuestArg::Int(thiz),
                GuestArg::Int(internal),
                GuestArg::Int(obb),
                GuestArg::Int(external),
                GuestArg::Int(asset_manager),
                GuestArg::Int(0), // savedState: a null byte array, which is the first launch
                GuestArg::Int(configuration),
            ],
            STEP_13_BUDGET,
        )
    };
    done.store(true, std::sync::atomic::Ordering::Relaxed);

    // **Everything is reported before anything is asserted**, because what step 13 reached is the
    // measurement this gate takes and a panic would take it with it.
    report(&guest);

    let handle = handle.unwrap_or_else(|error| {
        panic!("§8 step 13: initializeNativeCode did not return. {error}")
    });
    let native_code = handle.x0;
    assert_ne!(
        native_code, 0,
        "§8 step 13 returned 0. §8.1's fourth failure mode is that this happens when \
         `ALooper_forThread()` finds nothing -- but a looper was asserted present before the \
         call, so the cause is something else, and the report above is where it is"
    );
    let base = GuestAddr::try_from(native_code).expect("a guest pointer");

    // ---- §5.2's offsets, one assertion each --------------------------------------------------
    assert_eq!(
        guest.field_u64(base, native_code::CALLBACKS),
        (base + 0x50) as u64,
        "§5.2 step 6: activity->callbacks must be this + 0x50"
    );
    assert_eq!(
        guest.field_u64(base, native_code::VM),
        guest.jni.java_vm() as u64,
        "§5.2 step 7: activity->vm is what env->GetJavaVM wrote"
    );
    assert_eq!(
        guest.field_u64(base, native_code::ENV),
        guest.jni.env_for(0) as u64,
        "§5.2 step 7: activity->env is the JNIEnv it was called with"
    );
    assert_ne!(
        guest.field_u64(base, native_code::JAVA_GAME_ACTIVITY),
        0,
        "§5.2 step 8: activity->javaGameActivity is a global reference to thiz"
    );
    assert_eq!(
        guest.field_u32(base, native_code::SDK_VERSION),
        SDK_VERSION.parse::<u32>().expect("the gate's own constant"),
        "§5.2 step 2: activity->sdkVersion is what __system_property_get answered"
    );
    for (what, offset) in [
        ("internalDataPath", native_code::INTERNAL_DATA_PATH),
        ("externalDataPath", native_code::EXTERNAL_DATA_PATH),
        ("obbPath", native_code::OBB_PATH),
    ] {
        assert_ne!(guest.field_u64(base, offset), 0, "§5.2 step 9: activity->{what}");
    }
    assert_ne!(
        guest.field_u64(base, native_code::ASSET_MANAGER),
        0,
        "§5.2 step 10: activity->assetManager is what AAssetManager_fromJava returned"
    );
    assert_ne!(
        guest.field_u64(base, native_code::ASSET_MANAGER_REF),
        0,
        "§5.2 step 10: the global reference to the Java AssetManager"
    );
    assert_eq!(
        guest.field_u64(base, native_code::LOOPER),
        looper as u64,
        "§5.2 step 3: the looper is the one this thread prepared"
    );
    let msgread = guest.field_u32(base, native_code::MSGREAD) as i32;
    let msgwrite = guest.field_u32(base, native_code::MSGWRITE) as i32;
    assert!(msgread >= 3 && msgwrite >= 3, "§5.2 step 4: a real pipe, not two zeroes");
    assert_eq!(msgwrite, msgread + 1, "§5.2 step 4: the two ends of one pipe()");
    assert!(
        guest
            .ndk
            .registrations(looper)
            .iter()
            .any(|held| held.fd == msgread),
        "§5.2 step 5: the looper watches msgread"
    );
    assert_ne!(
        guest.field_u64(base, native_code::INSTANCE),
        0,
        "§5.2 GameActivity_onCreate step 5: activity->instance is the android_app it allocated, \
         which it only reaches after the game thread has signalled app->running -- so this is the \
         assertion that §8 row 14's cond-wait completed"
    );

    // The game thread exists and did its own `ALooper_prepare`: two loopers, not one.
    assert!(guest.ndk.live_loopers() >= 2, "§5.2 android_app_entry prepares the game thread's own");

    // ---- teardown, which is not a formality --------------------------------------------------
    //
    // **The game thread is still running**, inside `android_main`, and it will still be running
    // when this function returns. Dropping the guest under it unmaps an address space a thread is
    // executing translated code in.
    //
    // MEASURED before this was here: the whole-workspace run died with `STATUS_ACCESS_VIOLATION`
    // **after both tests reported `ok`**, with the guest's own
    // `[FLog::NativeMain] [android_main] Create a new NativeEngine:` as the last line. HANDOFF has
    // carried that exact shape since M4 as a one-off that never reproduced, with "teardown of a
    // guest with live guest threads is the obvious suspect" beside it. It reproduces, and it was.
    //
    // Asserted rather than best-effort: a join that timed out and carried on would put the crash
    // back and leave this comment claiming it had been fixed.
    guest.bionic.stop_guest_threads();
    let stopped = guest.bionic.join_guest_threads(std::time::Duration::from_secs(60));
    let _ = writeln!(
        std::io::stderr(),
        "M5 teardown: {} guest thread(s) still running, failures {:?}",
        guest.bionic.live_guest_threads(),
        guest.bionic.guest_thread_failures()
    );
    assert!(
        stopped,
        "the game thread did not stop within 60 s of being asked, so this address space cannot          be torn down: {} still running, parked {:?}",
        guest.bionic.live_guest_threads(),
        guest.bionic.parked()
    );
    assert_eq!(
        guest.field_u32(base, native_code::BYTES - 4) as u64 as u32 as u64,
        guest.field_u32(base, native_code::BYTES - 4) as u64,
        "the NativeCode really is at least 0x278 bytes, because its last word is readable"
    );
}

/// Everything this run measured, printed before anything is asserted.
fn report(guest: &Guest) {
    let mut out = std::io::stderr();
    let _ = writeln!(out, "\n================ M5 REPORT ================");
    let _ = writeln!(out, "NDK census: {:?}", guest.ndk.census());
    let _ = writeln!(
        out,
        "loopers {}, asset managers {}, open assets {}, configurations {}",
        guest.ndk.live_loopers(),
        guest.ndk.live_asset_managers(),
        guest.ndk.live_assets(),
        guest.ndk.live_configurations()
    );
    let _ = writeln!(out, "LOOPER EVENTS ({} dropped):", guest.ndk.events_dropped());
    for event in guest.ndk.events() {
        let _ = writeln!(
            out,
            "  thread {} {:<12} looper {:#x}: {}",
            event.thread, event.what, event.looper, event.detail
        );
    }
    let _ = writeln!(
        out,
        "guest threads: {} live, park peak {}, still parked {:?}",
        guest.bionic.live_guest_threads(),
        guest.bionic.parked_peak(),
        guest.bionic.parked()
    );
    let misses = guest.jni.misses();
    let _ = writeln!(out, "JNI misses: {}", misses.len());
    for miss in misses.iter().take(60) {
        let _ = writeln!(
            out,
            "  MISS {} {}.{} {}",
            miss.function, miss.class, miss.member, miss.descriptor
        );
    }
    guest.boundary.stop_census();
    if let Some(imports) = guest.boundary.census() {
        let _ = writeln!(out, "IMPORT CENSUS: {} distinct symbols\n  {:?}", imports.len(), imports);
    }
    let _ = writeln!(out, "================ end of report ================\n");
    let _ = out.flush();
}

/// NDK symbols this layer binds that **`libroblox.so` does not import**, and why each is bound.
///
/// The same discipline `tests/bionic.rs`'s `BEYOND_THE_PREDICTION` applies to the bionic surface,
/// and the same shape as its `freelocale` entry: a named exception with its evidence, rather than
/// a relaxed rule. A symbol that is neither an import nor listed here is a typo or scope creep,
/// and both are silent — a typo leaves the real symbol `Unbound` and the typo unreachable.
const NDK_BEYOND_THE_IMPORTS: &[(&str, &str)] = &[(
    "AAsset_read",
    "MEASURED in docs/research/apk-undefined-symbols.txt: `AAsset_read` is imported by \
     `libzstd-jni-1.5.7-6.so` and NOT by `libroblox.so`, which reaches its assets through \
     AAsset_getBuffer instead. It is bound for `freelocale`'s reason -- `AAssetManager_open` \
     hands the guest an AAsset, and a layer that hands one out and cannot read it is worse than \
     one that does neither -- and because `AAsset_openFileDescriptor`'s refusal names it as the \
     fallback the NDK's own documentation tells a caller to use.",
)];

/// The NDK symbols this layer binds are imports of the real binary, and none of them is among the
/// 188 the initializers reach.
///
/// Separate from the gate because it needs the ELF and **not** the run: a symbol-table check that
/// had to pay for 3,594 initializers would not be run when it was the thing being changed.
#[test]
fn every_ndk_symbol_is_an_import_of_the_real_binary_and_outside_the_188() {
    let _serial = serialized();
    let elf = ElfImage::parse(main_lib_bytes()).expect("parse libroblox.so");
    let imports: std::collections::BTreeSet<String> = elf
        .undefined_symbols()
        .expect("read .dynsym")
        .into_iter()
        .map(|symbol| symbol.name.to_string())
        .collect();
    let beyond: std::collections::BTreeSet<&str> =
        NDK_BEYOND_THE_IMPORTS.iter().map(|(symbol, _)| *symbol).collect();
    let bound: Vec<&str> = Ndk::bound_symbols().collect();
    assert!(!bound.is_empty());
    for symbol in &bound {
        assert!(
            imports.contains(*symbol) || beyond.contains(*symbol),
            "`{symbol}` is bound, `libroblox.so` does not import it, and it is not named in \
             NDK_BEYOND_THE_IMPORTS with the evidence for binding it anyway"
        );
    }
    // And the exception list must stay honest in the other direction: a symbol listed as beyond
    // the imports that IS imported is a stale entry, which is how an exception list becomes a
    // place to hide things.
    for (symbol, _) in NDK_BEYOND_THE_IMPORTS {
        assert!(
            !imports.contains(*symbol),
            "`{symbol}` IS imported by libroblox.so, so listing it as beyond the imports is wrong"
        );
        assert!(bound.contains(symbol), "`{symbol}` is listed and is not bound");
    }
    // **And none is in the 188**, which is what makes them M5's rather than M3's: every one is
    // reached from `initializeNativeCode` or from the game thread it spawns.
    let reachable = reachable_imports();
    for symbol in &bound {
        assert!(
            !reachable.contains(*symbol),
            "`{symbol}` IS one of the 188 the initializers reach, so it belonged to M3"
        );
    }
    // Membership, not a total: the four families, named.
    let bound: std::collections::BTreeSet<&str> = bound.into_iter().collect();
    for symbol in [
        "ALooper_prepare",
        "ALooper_forThread",
        "ALooper_acquire",
        "ALooper_release",
        "ALooper_addFd",
        "ALooper_removeFd",
        "ALooper_pollOnce",
        "AAssetManager_fromJava",
        "AAssetManager_open",
        "AAsset_read",
        "AAsset_getLength",
        "AAsset_getBuffer",
        "AAsset_close",
        "AAsset_openFileDescriptor",
        "AConfiguration_new",
        "AConfiguration_delete",
        "AConfiguration_fromAssetManager",
        "AConfiguration_getLanguage",
        "AConfiguration_getCountry",
        "AConfiguration_getNavHidden",
        "AConfiguration_getScreenWidthDp",
        "AConfiguration_getScreenHeightDp",
        "AConfiguration_getScreenSize",
        // **Five `ANativeWindow`, not nine.** MEASURED in
        // `docs/research/apk-undefined-symbols.txt`, per-library section `libroblox.so (565
        // undefined; ...)`: that binary imports exactly these five. `_getFormat` is imported only
        // by `libsurface_util_jni.so`; `_lock`, `_setBuffersGeometry` and `_unlockAndPost` only by
        // `libimage_processing_util_jni.so`. `apk-analysis.md` §4.4's "ANativeWindow (9)" is the
        // count across the whole APK, which D29 already records. Binding any of the other four
        // would fail the import check above rather than this list.
        "ANativeWindow_fromSurface",
        "ANativeWindow_acquire",
        "ANativeWindow_release",
        "ANativeWindow_getWidth",
        "ANativeWindow_getHeight",
    ] {
        assert!(bound.contains(symbol), "`{symbol}` is not bound");
    }
    assert_eq!(
        bound.len(),
        28,
        "seven ALooper, seven AAsset*, nine AConfiguration, five ANativeWindow"
    );
}

/// The 188 imports the initializers statically reach, from the research file the adapter's own
/// tests read.
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
