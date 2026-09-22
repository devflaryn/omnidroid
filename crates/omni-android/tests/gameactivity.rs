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
use omni_android::ndk::{
    DeviceConfiguration, HostWindowSource, Ndk, ScreenSize, WindowGeometry, WindowSource,
    ACONFIGURATION_NAVHIDDEN_NO, SURFACE_CLASS,
};
use omni_android::vulkan::{Vulkan, VulkanHost};
use omni_android::{Boundary, BoundaryBuilder, GuestArg};
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicCpu, DynarmicOptions};
use omni_cpu::{GuestAddr, GuestCpu, RunLimit, XReg};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
use omni_mem::{Backing, CommitPolicy, GuestSpace, MapExecutability, Placement, Protection};
use omni_platform::net::NetPolicy;

const APK_NAME: &str = "Roblox-2.738.1397.apk";

/// The switch every live-GPU test in this crate already uses, and the one that decides whether
/// this gate gives the engine **a real window and a real Vulkan driver**.
///
/// Off, the gate is what it was: a constant geometry and no Vulkan bound, so `dlopen("libvulkan.so")`
/// answers NULL and the run says so. On, the engine is handed an `ANativeWindow` backed by a
/// resizable desktop window through [`HostWindowSource`], and `vkGetInstanceProcAddr` reaches this
/// machine's driver through `omni_gfx::GfxVulkanHost` -- the same two seams the Vulkan stage tests
/// verified against a real GPU, now driven by the engine instead of by assembled test code.
const GRAPHICS_GATE: &str = "OMNI_GFX_WINDOW_TESTS";
const MAIN_LIB: &str = "libroblox.so";

/// `DT_INIT_ARRAY` entries in `libroblox.so`. The same exact figure M3's and M4's gates assert.
const INITIALIZERS: usize = 3_594;

/// The class of the activity `initializeNativeCode` is called on.
///
/// **Not `com/google/androidgamesdk/GameActivity`.** §8 row 13 is called from
/// `GameActivity.onCreate` with `this`, and `apk-analysis.md` §5.3 measured that
/// `MainGameActivity extends com.google.androidgamesdk.GameActivity` — so `this` is a
/// `MainGameActivity`. `the_activity_class_answers_every_member_row_23_looks_up_on_it` is the
/// assertion that says why it matters, and it is a `const` so the fixture and that assertion
/// cannot drift apart.
const ACTIVITY_CLASS: &str = "com/roblox/client/startup/MainGameActivity";

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

/// The class the 23 non-exported `GameActivity` natives are registered against.
///
/// **`GameActivity`, not [`ACTIVITY_CLASS`].** `RegisterNatives` names the class that *declares*
/// the method, and §4.1 read the `JNINativeMethod[24]` array out of the GameActivity glue; the
/// receiver those natives are then called on is a `MainGameActivity`, which is a different
/// statement and is [`ACTIVITY_CLASS`]'s.
const GAME_ACTIVITY_CLASS: &str = "com/google/androidgamesdk/GameActivity";

/// What this host tells the engine its surface is, in pixels.
///
/// **A decision, and the first one this project has made about a window.** `ndk::window` refuses
/// `ANativeWindow_getWidth`/`_getHeight` until an embedding says, so that a device profile nobody
/// chose cannot leak in through a default. This gate is an embedding and it chooses a size a
/// *desktop* window can have: the runtime presents in a resizable host window, not on a phone
/// panel, and picking 1080x2400 here would be inventing a device to be.
///
/// It is deliberately **not** derived from [`device_configuration`]'s 411x731 dp. Those are
/// `AConfiguration`'s density-independent numbers, which the engine reads for layout; these are
/// surface pixels, which it reads for a framebuffer. Deriving one from the other would require a
/// density this host has not measured either.
const SURFACE_WIDTH: i32 = 1280;
/// Pixels down. See [`SURFACE_WIDTH`].
const SURFACE_HEIGHT: i32 = 720;

/// Guest instructions **one §8 row 17-20 lifecycle native** is allowed.
///
/// Generous for the same reason [`STEP_13_BUDGET`] is: `onSurfaceCreatedNative` posts
/// `APP_CMD_INIT_WINDOW` and then *waits* for the game thread to take the window, and everything
/// the engine does with that window in between is charged to this thread's budget only if it runs
/// on this thread — which it does not — but the wait itself is bounded by the watchdog and not by
/// this. Counted rather than `Unlimited` for D16's reason, and below `i64::MAX` for its footgun.
const LIFECYCLE_BUDGET: RunLimit = RunLimit::Instructions(2_000_000_000);

/// How long the game thread is left to run on what §8 rows 17-20 handed it.
///
/// **The measurement is on the other thread.** `report` taking its census the instant step 13
/// returned is already recorded as a measurement of nothing; this is the same hazard one
/// lifecycle along, and the remedy is to let the thread that does the work do some of it. Ended
/// early when no guest thread is left, because a dead thread will not produce more evidence.
const POST_ROWS_SETTLE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long the gate waits for the engine's own flag fetch to answer, before sending the window
/// again.
///
/// # The figure, and the two runs that got it wrong first
///
/// **MEASURED on the first run in which the fetch succeeded**: `settingsUrl` logged at 7.448 s,
/// `getFlags: success = true, payload's size = 1358051.` at 10.927 s — **3.5 s** for a DNS
/// lookup, a TCP connect, a TLS handshake and 1,358,051 bytes of flags off a real CDN. Thirty
/// seconds is roughly nine times that: wide enough that a slow network is not reported as a failed
/// fetch, narrow enough that a fetch which never answers does not eat the suite.
///
/// **Two earlier readings of this said something else and both were wrong, in the same way.** The
/// first took ~2 s from the run where the request was malformed — that was the round trip for a
/// 68-byte `HTTP 400`, not for a settings document. The second concluded the fetch never completes
/// at all, from runs where it stalled at 409,075 of 1,358,051 bytes with the socket counters
/// frozen for 100 s. That stall was not a network figure either: `ldexp` was unbound, the guest
/// thread parsing the document was killed for it, and the frozen `recvfrom` counter was the
/// *symptom* of a death on another thread. `VERIFICATION.md` entry 16 — a thread that dies is not
/// a call that fails — and a bound derived from a truncated transfer is a bound derived from a
/// defect.
///
/// So: this number is only meaningful for a run that completes, and it was set from the first one
/// that did. The wait's *outcome* is printed either way — see `wait_for_log` for why a timeout
/// must not be allowed to look like a failure, and the `Flags-Not-Received` counts around the
/// delivery for the reading that answers whether the engine accepted the window.
const FLAG_FETCH_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The spin lock `nativeInitClientSettings` blocks on, as an offset from the load base.
///
/// **A temporary probe, not a contract.** MEASURED: with the hint handling in place, §8 row 21
/// spends its whole 200,000,000-instruction budget at guest `0x021eba20` -- the `yield` of a
/// three-instruction spin (`ldr w8,[x26,#0xa30]; cbz; yield; b`) whose acquire is
/// `swap(1, 0x06dd0a30)` through the outline atomic helper at `0x032f0950`. The loop exits only
/// when that word reads zero, so the word is the measurement: printing it at each stage says
/// *when* it stopped being zero, which is the difference between a lock another live thread holds
/// and one an earlier refusal abandoned.
const SPIN_LOCK_OFFSET: usize = 0x06dd_0a30;

/// Where `Flag::areFlagsLoaded()`'s byte lives, as an offset from `libroblox.so`'s load base.
///
/// **Decoded, and the decoding is what makes it one address rather than a guess.** A scan of
/// every `ADRP`+`STRB`/`LDRB` pair in `.text` that resolves to `0x072739d4` finds **one** store
/// (`0x022474e8`) and several hundred loads; the store's function is `0x022474cc`, whose six
/// callers include `0x02baf6cc` on the settings loader's success path. The single xref to
/// `Can't initialize the TaskScheduler before flags have been loaded` (`0x0224fc84`) is guarded
/// by `tbz w8, #0` on a load of the same byte at `0x0224fa24`.
///
/// An offset rather than the absolute address because the image is loaded wherever the host maps
/// it; `0x072739d4` is the link-time address, and `object.base` is what turns it into this run's.
const FLAGS_LOADED_OFFSET: usize = 0x0727_39d4;

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
    /// Bound only under [`GRAPHICS_GATE`]; see [`Guest::load`].
    vulkan: Option<Arc<Vulkan>>,
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
    /// `graphics` is the host driver to bind Vulkan to, when [`GRAPHICS_GATE`] asked for one.
    /// `None` binds no Vulkan at all -- not an unhosted one, which would hand the engine a loader
    /// whose every call refuses -- so the default gate's `dlopen("libvulkan.so")` stays NULL.
    fn load(graphics: Option<Arc<dyn VulkanHost>>) -> Self {
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
        // Room for every import, the 241 JNI slots and the NDK surface -- and, with graphics, the
        // Vulkan loader's pool and the data area its handle registries need (8192, not 4096:
        // `vulkan::REQUIRED_DATA_BYTES` records why no smaller arrangement exists).
        let (vulkan_slots, data_bytes) = if graphics.is_some() {
            (omni_android::vulkan::BOUND_SYMBOLS, omni_android::vulkan::REQUIRED_DATA_BYTES)
        } else {
            (0, 4096)
        };
        let builder = BoundaryBuilder::new(
            Arc::clone(&space),
            TOTAL_IMPORTS + JNI_SLOTS + Ndk::bound_symbols().count() + vulkan_slots,
            data_bytes,
        )
        .expect("a thunk region");
        bionic.bind_into(&builder).expect("bind every bionic handler");
        ndk.bind_into(&builder).expect("bind every NDK handler");
        let vulkan = graphics.map(|host| {
            let vulkan = Vulkan::new();
            vulkan.bind_into(&builder).expect("bind the Vulkan loader");
            vulkan.set_host(host);
            vulkan
        });
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
        // **Which network this guest may reach — D30's replacement for Global Constraint 8.**
        //
        // The `EAI_NONAME` diagnostic that used to stand here is gone, with the two `Bionic`
        // methods behind it: `getaddrinfo` resolves for real now, and that switch's own doc
        // comment said to delete it on this day. `VERIFICATION.md` entry 14 is why leaving it
        // would have been the defect rather than the convenience.
        //
        // `NetPolicy::unrestricted()` is the word an embedding has to write on purpose; the gate
        // is a measurement of what the real engine does against the real internet, so narrowing
        // it would make a refused destination look like a network failure.
        bionic
            .set_network_policy(Arc::new(NetPolicy::unrestricted()))
            .expect("a network policy");
        // **The uid the package manager would have assigned**, which only an embedding can
        // supply: it is not in the APK and Windows has none. 10000 is `FIRST_APPLICATION_UID`,
        // what Android's package manager gives the first app it installs, and this gate is
        // standing in for a device with exactly one. SQLite asks, through `geteuid`, whether it
        // is root; an app never is.
        bionic.set_app_uid(10_000).expect("an application uid");
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
            vulkan,
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
/// Every scratch root this process has created, for [`remove_scratch_roots`].
///
/// A `Mutex<Vec<_>>` rather than a single slot: the gate builds one guest per test and the
/// watchdog may end the process during any of them.
static LEAKED_ON_EXIT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Delete every scratch root, for the paths that end the process without unwinding.
///
/// Called from the watchdog immediately before its `exit`. Failures are ignored: a directory that
/// cannot be removed is a leak, and a panic here would replace a useful report with a useless one.
fn remove_scratch_roots() {
    let roots: Vec<PathBuf> = match LEAKED_ON_EXIT.lock() {
        Ok(mut held) => held.drain(..).collect(),
        // A poisoned lock means a thread panicked holding it, which is exactly a run that is
        // ending badly -- the directories still have to go.
        Err(poisoned) => poisoned.into_inner().drain(..).collect(),
    };
    for root in roots {
        let _ = std::fs::remove_dir_all(&root);
    }
}

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
        // The engine's own TLS store lives here; see `CA_BUNDLE_IN_APK`.
        "data/data/com.roblox.client/files/exe",
    ];

    /// Where the APK keeps its certificate authorities, and where the engine looks for them.
    ///
    /// # This is the Java side's job, and D7 says the Java side is defined rather than executed
    ///
    /// **MEASURED, by the miss recorder `Filesystem::open_misses`.** The settings fetch exchanged
    /// bytes over real sockets in both directions and then reported `HttpError: Unknown`, with no
    /// refusal, no dead thread and nothing else wrong in the run. What the engine could not find
    /// was `/data/data/com.roblox.client/files/exe/cacert.pem`, and OpenSSL's compiled-in default
    /// store is `/actions-runner/_work/openssl/openssl/pkg/ssl/cert.pem` -- a build machine's path
    /// that exists on no device, which is why the bundle is shipped and placed rather than found.
    ///
    /// The APK carries it at `assets/ssl/cacert.pem`, 228,725 bytes. On a device the Java side
    /// unpacks it into the app's files directory before the engine starts; nothing here executes
    /// that Java, so the host does it, exactly as it already supplies the app's directories, the
    /// SDK version and the client-settings document.
    ///
    /// **It is copied, not invented.** The bytes are the APK's own. A host that substituted its
    /// own trust store would be deciding, on the guest's behalf, which authorities Roblox trusts.
    const CA_BUNDLE_IN_APK: &'static str = "ssl/cacert.pem";
    /// Where the engine opens it, measured rather than assumed. See [`Self::CA_BUNDLE_IN_APK`].
    const CA_BUNDLE_IN_GUEST: &'static str = "data/data/com.roblox.client/files/exe/cacert.pem";

    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-m5-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        // **Registered so that the watchdog's `exit` can still remove it.**
        //
        // `Drop` is what normally cleans this up, and `std::process::exit` runs no destructors --
        // so every run the watchdog ends leaks the whole extracted library, about 104 MB. MEASURED:
        // 930 GB of disk went to 0.1 GB over one session's worth of stalled runs, and the build
        // then failed with `os error 112` for reasons that had nothing to do with the build.
        //
        // A registry rather than a `Drop` guard on the watchdog thread, because the watchdog does
        // not own this and the thing that has to run is not a drop at all.
        if let Ok(mut held) = LEAKED_ON_EXIT.lock() {
            held.push(at.clone());
        }
        for directory in Self::DIRECTORIES {
            std::fs::create_dir_all(at.join(directory)).expect("an app directory");
        }
        std::fs::write(at.join("data/app/com.roblox.client/base.apk"), b"")
            .expect("a placeholder for the package's own apk");
        // The certificate authorities, out of the APK and into the path the engine opens. See
        // `CA_BUNDLE_IN_APK` for why this is the host's job and why the bytes are the APK's own.
        let apk = omni_apk::Apk::open(apk_path()).expect("the real APK");
        let bundle = apk
            .read_asset(Self::CA_BUNDLE_IN_APK)
            .expect("the APK's certificate authorities");
        assert!(
            bundle.starts_with(b"##
## Bundle of CA Root Certificates")
                || bundle.windows(27).any(|w| w == b"-----BEGIN CERTIFICATE-----"),
            "the bytes at {} are not a PEM bundle, and copying them would be a guess",
            Self::CA_BUNDLE_IN_APK,
        );
        std::fs::write(at.join(Self::CA_BUNDLE_IN_GUEST), &bundle)
            .expect("the certificate authorities, where the engine looks for them");
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
    // **The socket record, off unless this run was asked for it.** See
    // `omni_platform::net::record` for what it can and cannot show -- the short version is that
    // the engine's TLS is its own, so what lands here is a `ClientHello` and then ciphertext, and
    // the one thing in the clear is the server name. It is opt-in because the bytes are the
    // guest's and may carry its cookies; the switch is read here, in the embedding, because
    // `omni-platform` reads no environment of its own.
    let record_bytes = std::env::var("OMNI_NET_RECORD")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if record_bytes > 0 {
        let _ = writeln!(
            std::io::stderr(),
            "OMNI_NET_RECORD={record_bytes}: recording the first {record_bytes} bytes each socket 
                 carries in each direction. THESE ARE THE GUEST'S BYTES and may carry its 
                 credentials; they are printed to this run's stderr and written nowhere else."
        );
        omni_platform::net::record::record_first(record_bytes);
    }
    let graphics = std::env::var(GRAPHICS_GATE).is_ok_and(|v| v == "1");
    let host: Option<Arc<dyn VulkanHost>> = graphics.then(|| {
        omni_gfx::GfxVulkanHost::load().expect(
            "OMNI_GFX_WINDOW_TESTS=1 asks for this machine's Vulkan driver, and there is no \
             loader to reach it through",
        ) as Arc<dyn VulkanHost>
    });
    let guest = Guest::load(host);
    let mut cpu = guest.thread();
    guest.boundary.start_census();

    // ---- steps 1-5, which M3 delivered ---------------------------------------------------
    let _ = writeln!(
        std::io::stderr(),
        "before the initializers: spin lock {}, count {}",
        image_word(&guest, SPIN_LOCK_OFFSET, "the spin lock at 0x06dd0a30"),
        image_word(&guest, SPIN_LOCK_OFFSET + 4, "the count at 0x06dd0a34")
    );
    let completed = guest.run_initializers(&mut cpu);
    assert_eq!(completed, INITIALIZERS, "M5 starts where M4's gate starts");
    let _ = writeln!(
        std::io::stderr(),
        "after the initializers: spin lock {}, count {}",
        image_word(&guest, SPIN_LOCK_OFFSET, "the spin lock at 0x06dd0a30"),
        image_word(&guest, SPIN_LOCK_OFFSET + 4, "the count at 0x06dd0a34")
    );

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
    stall_report(&guest, "after JNI_OnLoad, before step 7");
    let _ = writeln!(
        std::io::stderr(),
        "after JNI_OnLoad: spin lock {}, count {}",
        image_word(&guest, SPIN_LOCK_OFFSET, "the spin lock at 0x06dd0a30"),
        image_word(&guest, SPIN_LOCK_OFFSET + 4, "the count at 0x06dd0a34")
    );

    // ---- steps 7-12: the scripted sequence, which M4 delivered -----------------------------
    // **One step at a time, with the lock read between them.** MEASURED the other way first, and
    // it was a measurement of nothing: running the whole table and then printing the lock beside
    // each outcome reads the *final* value once per row, so every row reported the same number
    // and the value looked as though it had been taken at step 7 whatever had actually happened.
    // `VERIFICATION.md` entry 4's shape -- a census taken after the work is not evidence about
    // when the work happened.
    let mut outcomes = Vec::new();
    for step in script::SEQUENCE {
        let before = image_word(&guest, SPIN_LOCK_OFFSET, "the spin lock at 0x06dd0a30");
        let threads_before = guest.bionic.guest_thread_records();
        let produced = {
            let _bionic = guest.bionic.activate().expect("publish the bionic instance");
            let _jni = guest.jni.activate().expect("publish the JNI instance");
            let _ndk = guest.ndk.activate();
            script::run(
                &guest.jni,
                &guest.boundary,
                &mut cpu,
                &|symbol| guest.exports.get(symbol).copied(),
                std::slice::from_ref(step),
                0,
            )
            .expect("building the scripted arguments must not fail")
        };
        for outcome in &produced {
            let _ = writeln!(
                std::io::stderr(),
                "  step {:>2} {:<72} {}   [spin lock {} -> {}, count {}, threads {} -> {}]",
                outcome.step,
                outcome.symbol,
                match &outcome.result {
                    Ok(()) => "returned".to_string(),
                    Err(error) => format!(
                        "{error} [last crossing from guest {:#x} (link {:#x})]{}",
                        guest.boundary.last_caller(),
                        guest.boundary.last_caller().wrapping_sub(guest.object.base),
                        bytes_before_the_fault(&guest, &error.to_string())
                    ),
                },
                before,
                image_word(&guest, SPIN_LOCK_OFFSET, "the spin lock at 0x06dd0a30"),
                image_word(&guest, SPIN_LOCK_OFFSET + 4, "the count at 0x06dd0a34"),
                threads_before,
                guest.bionic.guest_thread_records()
            );
        }
        outcomes.extend(produced);
    }
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

    stall_report(&guest, "after the scripted sequence");

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
    // `thiz` is the activity instance the Java side would be calling from, and **it is a
    // `MainGameActivity`, not a `GameActivity`**.
    //
    // `initializeNativeCode` is called from `GameActivity.onCreate` with `this`, and
    // `apk-analysis.md` §5.3 records that **`MainGameActivity extends
    // com.google.androidgamesdk.GameActivity`** — so on a device `this` is a `MainGameActivity`.
    // It matters because §8 row 23's helpers do `GetObjectClass` on the global reference to it
    // and then ask that one `jclass` for `MainGameActivity.getNativeHelper`. MEASURED with a
    // `GameActivity` here, n = 1 run: the lookup missed, the engine did not check the id, and the
    // game thread died in `CallObjectMethodV` with `0x0`.
    //
    // The five `gGameActivityClassInfo` members are unaffected: step 13 resolves those through
    // `FindClass("com/google/androidgamesdk/GameActivity")`, which is a class and not this object.
    let thiz = {
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        guest.jni.new_object(ACTIVITY_CLASS).expect("a MainGameActivity")
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
            //
            // **And the scratch roots go first, because `exit` runs no destructors.** See
            // `Scratch::new` for the 930 GB that taught this. The network report is here on the
            // same argument and for the same reason — see `report_network`.
            report_network(&mut out, &boundary, "M5 watchdog");
            remove_scratch_roots();
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
    // **Turned back on, because `report` turns it off to print a stable snapshot and everything
    // interesting happens after that.** MEASURED: every watchdog sample through the §8 row 21
    // stall read the census as FROZEN and it was not frozen, it was *off* -- `Boundary::census`
    // keeps the counts when the flag is cleared, so a stopped census is indistinguishable from a
    // stalled guest at the call site. Three sessions of work were spent on a deadlock that the
    // un-gated `Boundary::crossings` said, in one reading, was a guest running flat out.
    guest.boundary.start_census();

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

    // ---- §8 rows 17-20: the lifecycle and surface callbacks, driven from this thread ---------
    //
    // **This is what ART does, and nothing else can do it.** The game thread is inside
    // `NativeEngine::GameLoop()` blocked in `ALooper_pollOnce(-1)`, waiting for the *main* thread
    // to post `APP_CMD_INIT_WINDOW` down the pipe `initializeNativeCode` created. On a device the
    // poster is `GameActivity.surfaceCreated`/`surfaceChanged`/`onStart`/`onResume`/
    // `onWindowFocusChanged`/`onGlobalLayout`; here it is this block, called exactly the way
    // step 13 is called.
    //
    // The natives are **not exported symbols**. §4.1: of the 24 `GameActivity` natives only
    // `initializeNativeCode` has a `Java_*` export; the other 23 live in the `JNINativeMethod[24]`
    // array at `.data.rel.ro 0x062dc1c8` and arrive through `RegisterNatives`. So they are looked
    // up in what the engine itself registered, which is also the assertion that the registration
    // happened at all.
    //
    // **The `jlong` handle sits in `x2` whether or not the method is static**, because JNI's
    // second argument is a `jobject` for an instance native and a `jclass` for a static one and
    // both occupy one register. That is why this passes `(env, thiz, handle, ..)` without having
    // to establish which these are — a distinction `RegisterNatives` does not carry.
    let game_activity_natives: Vec<omni_android::jni::Registration> = guest
        .jni
        .registrations()
        .into_iter()
        .filter(|r| r.class == GAME_ACTIVITY_CLASS)
        .collect();
    assert!(
        !game_activity_natives.is_empty(),
        "§4.1: the engine's own `RegisterNatives` binds the 23 non-exported GameActivity natives, \
         and none was recorded. Everything below drives them, so an empty list is the measurement \
         and not a missing fixture. All registrations: {:?}",
        guest.jni.registrations()
    );
    {
        let mut out = std::io::stderr();
        let _ = writeln!(
            out,
            "\n§4.1 GameActivity natives registered: {}",
            game_activity_natives.len()
        );
        for r in &game_activity_natives {
            let _ = writeln!(out, "  {}{} -> {:#x}", r.member, r.descriptor, r.function);
        }
        let _ = out.flush();
    }
    let native = |member: &str, descriptor: &str| -> GuestAddr {
        game_activity_natives
            .iter()
            .find(|r| r.member == member && r.descriptor == descriptor)
            .unwrap_or_else(|| {
                panic!(
                    "§4.1 names `{member}{descriptor}` among the 24 GameActivity natives, and the \
                     engine's `RegisterNatives` did not bind it. Bound: {:?}",
                    game_activity_natives
                        .iter()
                        .map(|r| format!("{}{}", r.member, r.descriptor))
                        .collect::<Vec<_>>()
                )
            })
            .function
    };

    // **The geometry is a decision, and this is the embedding that gets to make it.**
    // `ndk::window` refuses `ANativeWindow_getWidth`/`_getHeight` until a host says what the
    // surface is, precisely so that a number nobody chose cannot leak in. This gate is a host, so
    // it chooses — and it chooses a size a desktop window can actually have, not a device
    // profile, because the window this runtime will present in is a resizable host window.
    //
    // **Or, under [`GRAPHICS_GATE`], it hands the engine a real window**, and the surface size is
    // whatever that window's client area actually is -- asked of the OS, not assumed from the
    // size requested, because a desktop window's frame takes its share.
    let mut window: Option<omni_platform::window::Window> = None;
    let mut window_source: Option<Arc<HostWindowSource>> = None;
    let (surface_width, surface_height) = if graphics {
        let opened = omni_platform::window::Window::new(&omni_platform::window::WindowDesc::new(
            "Omnidroid - Roblox",
            SURFACE_WIDTH as u32,
            SURFACE_HEIGHT as u32,
        ))
        .unwrap_or_else(|err| panic!("{GRAPHICS_GATE}=1 and no window could be opened: {err}"));
        opened.show();
        let mut opened = opened;
        let _ = opened.poll_events().count();
        let source = HostWindowSource::watching(&opened).expect("a source watching the window");
        let size = source.geometry().expect("a freshly shown window has pixels");
        guest.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);
        let _ = writeln!(
            std::io::stderr(),
            "GRAPHICS: a real window, client area {}x{}, and Vulkan bound to this machine's driver",
            size.width,
            size.height
        );
        window = Some(opened);
        window_source = Some(source);
        (size.width, size.height)
    } else {
        guest.ndk.set_window_geometry(
            WindowGeometry::new(SURFACE_WIDTH, SURFACE_HEIGHT).expect("a positive geometry"),
        );
        let _ = writeln!(
            std::io::stderr(),
            "GRAPHICS: NOT ATTEMPTED -- a constant {SURFACE_WIDTH}x{SURFACE_HEIGHT} geometry and \
             no Vulkan bound, so dlopen(\"libvulkan.so\") answers NULL. Set {GRAPHICS_GATE}=1 \
             to give the engine a real window and this machine's driver."
        );
        (SURFACE_WIDTH, SURFACE_HEIGHT)
    };
    let surface = {
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        guest.jni.new_object(SURFACE_CLASS).expect("a Java Surface")
    };

    // The watchdog is re-armed, because **row 17 blocks**. The GameActivity glue's
    // `android_app_set_window` writes `APP_CMD_INIT_WINDOW` and then waits on its own condition
    // variable until the game thread has taken the window — so this call cannot return until the
    // poll this session just made possible actually wakes. A hang here is the same failure §8.1's
    // fifth mode names, one lifecycle step along.
    let rows_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let rows_done = Arc::clone(&rows_done);
        let bionic = Arc::clone(&guest.bionic);
        let ndk = Arc::clone(&guest.ndk);
        let boundary = Arc::clone(&guest.boundary);
        let jni = Arc::clone(&guest.jni);
        let space = Arc::clone(&guest.space);
        let image_base = guest.object.base;
        std::thread::spawn(move || {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(WATCHDOG_SECONDS);
            // **Sampled rather than watched.** A watchdog that only fires at the end cannot say
            // whether the run was stuck for the whole period or merely slower than the bound, and
            // those need different work. The crossing total is the discriminator: a figure that
            // moves is a guest executing imports.
            let mut next_sample = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut previous = 0u64;
            while std::time::Instant::now() < deadline {
                if rows_done.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                if std::time::Instant::now() >= next_sample {
                    let total: u64 =
                        boundary.census().map_or(0, |census| census.values().sum());
                    let _ = writeln!(
                        std::io::stderr(),
                        "  M6 watchdog sample: crossings {previous} -> {total} ({}), last {:?},                          live threads {}",
                        if total == previous { "FROZEN" } else { "moving" },
                        boundary.last_call().map(|slot| slot.symbol.clone()),
                        bionic.live_guest_threads()
                    );
                    let _ = writeln!(
                        std::io::stderr(),
                        "      JNI locks held: {:?}, guest space map lock held: {}, raw futex calls {} recorded / {} dropped",
                        jni.locks_held()
                            .iter()
                            .filter(|(_, held)| *held)
                            .map(|(name, _)| *name)
                            .collect::<Vec<_>>(),
                        space.map_lock_is_held(),
                        bionic.futex_calls().len(),
                        bionic.futex_calls_dropped()
                    );
                    for call in bionic.futex_calls().iter().take(6) {
                        let _ = writeln!(
                            std::io::stderr(),
                            "        futex: thread {:#x} {} on {:#x} value {} from link {:#x} -> {}",
                            call.thread,
                            call.op,
                            call.address,
                            call.value,
                            call.caller.wrapping_sub(image_base),
                            if call.outcome == i32::MIN {
                                "ENTERED AND NEVER RETURNED".to_string()
                            } else {
                                call.outcome.to_string()
                            }
                        );
                    }
                    for report in boundary.threads() {
                        let _ = writeln!(
                            std::io::stderr(),
                            "      guest thread {:#x} last crossed {:?} from link {:#x}, {}                              crossing(s), {}",
                            report.guest_thread,
                            report.symbol,
                            report.caller.wrapping_sub(image_base),
                            report.crossings,
                            if report.crossings > report.exits {
                                "INSIDE THE HANDLER"
                            } else {
                                "in guest code"
                            }
                        );
                    }
                    let _ = writeln!(
                        std::io::stderr(),
                        "      futex parked {:?}, indefinite {}, cond-parked {:?}",
                        bionic
                            .futex()
                            .parked_addresses()
                            .iter()
                            .map(|(a, c)| format!("{a:#x} x{c}"))
                            .collect::<Vec<_>>(),
                        bionic.futex().indefinite_parks(),
                        bionic
                            .parked()
                            .iter()
                            .map(|h| format!(
                                "{:?} in {} on cond {:#x} mutex {:#x}",
                                h.thread, h.symbol, h.cond, h.mutex
                            ))
                            .collect::<Vec<_>>()
                    );
                    // **Which guest threads are left, and what killed the rest.** Eight threads
                    // have crossed this boundary and three are live: a thread that died inside a
                    // job holding a future nobody else can complete is exactly the shape of the
                    // stall being looked at, and it is invisible in every other reading here.
                    let _ = writeln!(
                        std::io::stderr(),
                        "      guest threads: {:?}; image base {image_base:#x}; failures {:?}",
                        bionic
                            .guest_thread_list()
                            .iter()
                            .map(|t| format!(
                                "{:?}{}",
                                t.id,
                                if t.running { "" } else { " (exited)" }
                            ))
                            .collect::<Vec<_>>(),
                        bionic.guest_thread_failures()
                    );
                    // **And the frames of whoever is spinning**, which has no park record at
                    // all: a thread calling `sched_yield` in a loop is never blocked, so nothing
                    // in the wait registry knows about it, and it is burning a core.
                    for (thread, stack) in bionic.yield_stacks() {
                        let _ = writeln!(
                            std::io::stderr(),
                            "        {thread:?} yielding at: {:?}",
                            stack
                                .iter()
                                .map(|frame| format!("{:#x}", frame.wrapping_sub(image_base as u64)))
                                .collect::<Vec<_>>()
                        );
                    }
                    // **The frames above the wait, which is what names the caller.** One symbol
                    // is not an answer here: the `pthread_cond_wait` this stalls on is reached
                    // through a helper with ten call sites. Printed as image offsets, because
                    // that is what the disassembler is addressed in.
                    for held in bionic.parked() {
                        let _ = writeln!(
                            std::io::stderr(),
                            "        {:?} stack: {:?}",
                            held.thread,
                            held.backtrace
                                .iter()
                                .map(|frame| format!("{:#x}", frame.wrapping_sub(image_base as u64)))
                                .collect::<Vec<_>>()
                        );
                    }
                    // **Host CPU time, which tells a spin from a block.** Frozen crossings and a
                    // budget that never expires say the guest is executing nothing; they do not
                    // say whether the *host* thread servicing it is burning a core inside a
                    // handler or parked on something. Process CPU time answers that directly and
                    // is the only thing here that can.
                    let cpu = omni_platform::process::cpu_time().ok();
                    let _ = writeln!(
                        std::io::stderr(),
                        "      process CPU time: {:?} (climbing means host code is spinning),                          hints continued through: {}",
                        cpu,
                        omni_cpu::dynarmic::HINTS_OBSERVED
                            .load(std::sync::atomic::Ordering::Relaxed)
                    );
                    // **The fault counters, which are the only thing left that can burn a core
                    // without executing a guest instruction.** A guest access that faults, is
                    // "handled", and then faults again on re-execution never retires the
                    // instruction: the budget does not tick, no import is crossed, and the host
                    // spins. `examined` climbing while `resolved` keeps pace is exactly that
                    // shape, and nothing else in this runtime produces it.
                    let faults = omni_platform::fault::stats();
                    let _ = writeln!(
                        std::io::stderr(),
                        "      faults: examined {} resolved {} declined {} drained {}, run-loop                          iterations {}",
                        faults.examined,
                        faults.resolved,
                        faults.declined,
                        faults.drained,
                        omni_android::RUN_LOOP_ITERATIONS
                            .load(std::sync::atomic::Ordering::Relaxed)
                    );
                    // **What the run loop is going round ON.** The loop's only path back to its
                    // own top is a thunk exit, and a thunk exit is charged to the census -- so a
                    // climbing iteration count beside a frozen census is two readings that cannot
                    // both be true, and this is the one that names which. The site is the thunk
                    // the last turn went through; the instruction total is what those turns
                    // retired, and a budget that never expires while this stays still is a slice
                    // that executes nothing.
                    let site = omni_android::RUN_LOOP_LAST_SITE
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let _ = writeln!(
                        std::io::stderr(),
                        "      run loop last went round on {:?} (thunk {:#x}), guest instructions                          retired {}, entries {}, last exit {:?}",
                        boundary.symbol_at(site),
                        site,
                        omni_android::RUN_LOOP_INSTRUCTIONS
                            .load(std::sync::atomic::Ordering::Relaxed),
                        omni_android::RUN_LOOP_ENTRIES
                            .load(std::sync::atomic::Ordering::Relaxed),
                        *omni_android::RUN_LOOP_LAST_EXIT.lock()
                    );
                    // **The witness the census cannot be checked without.** `exits` is charged in
                    // the same function as the per-symbol count and is *not* census-gated, so the
                    // two are obliged to move together: if this climbs while the census does not,
                    // the reading that is wrong is the census one, and the stall is somewhere else
                    // entirely.
                    let counted = boundary.crossings();
                    let _ = writeln!(
                        std::io::stderr(),
                        "      boundary exits {}, guest calls {}, deepest {}; busiest imports {:?}",
                        counted.exits,
                        counted.guest_calls,
                        counted.deepest,
                        {
                            // The six busiest imports, which is what names a spin. A total says
                            // the guest is running; this says what it is running *at*, and a
                            // guest spinning on one symbol is a different bug from a guest making
                            // progress through many.
                            let mut busiest: Vec<(&str, u64)> = boundary
                                .census()
                                .map(|c| c.into_iter().collect())
                                .unwrap_or_default();
                            busiest.sort_unstable_by_key(|(_, calls)| std::cmp::Reverse(*calls));
                            busiest.truncate(6);
                            busiest
                        }
                    );
                    // **The network census, by name rather than by rank.** The six busiest
                    // imports above cannot show this: a whole HTTPS fetch is a handful of calls
                    // against tens of millions of mutex operations, so every network symbol is
                    // invisible in a ranking and the absence of one is the measurement.
                    //
                    // The order is the order a client walks: a name, a socket, its options, a
                    // connect, the readiness wait, the error the connect reports, and then the
                    // bytes. Reading it left to right says exactly how far the fetch got, and a
                    // zero after a non-zero is where it stopped -- which is a different question
                    // from "did a thread die", and the one `fetch flag exception: HttpError:
                    // Unknown` does not answer.
                    {
                        let census = boundary.census();
                        let count = |symbol: &str| -> u64 {
                            census
                                .as_ref()
                                .and_then(|c| c.get(symbol).copied())
                                .unwrap_or(0)
                        };
                        let _ = writeln!(
                            std::io::stderr(),
                            "      net census: {:?}",
                            [
                                "getaddrinfo",
                                "freeaddrinfo",
                                "socket",
                                "setsockopt",
                                "getsockopt",
                                "getsockname",
                                "ioctl",
                                "fcntl",
                                "connect",
                                "poll",
                                "select",
                                "read",
                                "write",
                                "__write_chk",
                                "sendto",
                                "recvfrom",
                                "shutdown",
                                "close",
                                "getentropy",
                                "mktime",
                            ]
                            .map(|symbol| (symbol, count(symbol)))
                        );
                    }
                    // **What the engine has asked the Java side for, and what it asked for and
                    // did not get.** The whole client-settings phase is a conversation: the
                    // engine loads flags, calls `NativeHelper.gameActivity_onFlagsLoaded`, and
                    // the answer is what marks the DataModel's own "flags received". A stall
                    // here is either an upcall that was never made or one that missed, and those
                    // are opposite bugs. `report` cannot see either: it runs before the game
                    // thread has started.
                    let misses = jni.misses();
                    let calls = jni.calls();
                    let _ = writeln!(
                        std::io::stderr(),
                        "      JNI upcalls {} ({} missed); last: {:?}",
                        calls.len(),
                        misses.len(),
                        calls
                            .iter()
                            .rev()
                            .take(60)
                            .rev()
                            .map(|record| format!("{}.{}", record.class, record.member))
                            .collect::<Vec<_>>()
                    );
                    // **The synchronisation census, which a total cannot answer.** The gate
                    // thread is asleep on a one-shot `pthread_cond_wait` with no predicate, so
                    // "was it ever signalled" is the whole question, and it is one number.
                    let _ = writeln!(
                        std::io::stderr(),
                        "      sync census: {:?}",
                        boundary
                            .census()
                            .map(|c| c
                                .into_iter()
                                .filter(|(symbol, _)| symbol.starts_with("pthread_cond")
                                    || symbol.starts_with("pthread_join")
                                    || *symbol == "syscall"
                                    || *symbol == "sched_yield")
                                .collect::<Vec<_>>())
                            .unwrap_or_default()
                    );
                    for miss in misses.iter().rev().take(6).rev() {
                        let _ = writeln!(
                            std::io::stderr(),
                            "        MISS {} {}.{} {}",
                            miss.function, miss.class, miss.member, miss.descriptor
                        );
                    }
                    previous = total;
                    next_sample += std::time::Duration::from_secs(20);
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            if rows_done.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let mut out = std::io::stderr();
            let _ = writeln!(
                out,
                "\n================ M6 WATCHDOG: §8 rows 17-20 did not finish in \
                 {WATCHDOG_SECONDS} s ================\n\
                 last import: {:?}\n\
                 live guest threads: {}\n\
                 PARKED:",
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
            for event in ndk.events().iter().rev().take(60).rev() {
                let _ = writeln!(
                    out,
                    "  thread {} {:<10} looper {:#x}: {}",
                    event.thread, event.what, event.looper, event.detail
                );
            }
            let _ = writeln!(out, "NDK CENSUS: {:?}", ndk.census());
            let _ = writeln!(out, "================ ending the run ================");
            let _ = out.flush();
            // **Before the `exit`, because the `exit` runs no destructors.** See `Scratch::new`,
            // and `report_network` for why the network numbers are here rather than only in the
            // teardown this `exit` skips.
            report_network(&mut out, &boundary, "M6 watchdog");
            remove_scratch_roots();
            std::process::exit(101);
        });
    }

    // Row order is `GameActivity`'s own lifecycle order, which §4.2 read out of dex bytecode:
    // `surfaceCreated` → `surfaceChanged` → `onStart`/`onResume` → `onWindowFocusChanged` →
    // `onGlobalLayout` → the insets callback. Each is reported before the next is attempted, so a
    // run that stops names the row it stopped on rather than the block.
    let rows: Vec<(&str, &str, Vec<GuestArg>)> = vec![
        (
            "onSurfaceCreatedNative",
            "(JLandroid/view/Surface;)V",
            vec![GuestArg::Int(surface)],
        ),
        (
            "onSurfaceChangedNative",
            "(JLandroid/view/Surface;III)V",
            vec![
                GuestArg::Int(surface),
                // `PixelFormat.RGBA_8888`. The one format this runtime can present and the one
                // `omni-texture` transcodes into; a value nothing here can produce would be a
                // number invented for a field the engine reads.
                GuestArg::Int(1),
                GuestArg::Int(surface_width as u64),
                GuestArg::Int(surface_height as u64),
            ],
        ),
        ("onStartNative", "(J)V", vec![]),
        ("onResumeNative", "(J)V", vec![]),
        ("onWindowFocusChangedNative", "(JZ)V", vec![GuestArg::Int(1)]),
        (
            "onContentRectChangedNative",
            "(JIIII)V",
            vec![
                GuestArg::Int(0),
                GuestArg::Int(0),
                GuestArg::Int(surface_width as u64),
                GuestArg::Int(surface_height as u64),
            ],
        ),
        ("onWindowInsetsChangedNative", "(J)V", vec![]),
    ];
    // **§8 rows 21-22 are opt-in, and the reason is printed every run.** Making the *driver*
    // opt-in is not `VERIFICATION.md` entry 4's shape: nothing that is **asserted** is skipped.
    // Step 13 and rows 17-20 still run and still assert, every time. What is gated is an attempt
    // to cross a frontier, and the run says so out loud with the command that reproduces it.
    let attempt_flags = std::env::var_os("OMNI_M6_ROWS_21_22").is_some();
    if !attempt_flags {
        let _ = writeln!(
            std::io::stderr(),
            "
§8 rows 21-22: NOT ATTEMPTED. Set OMNI_M6_ROWS_21_22=1 to drive the client-settings              phase and the surface rows in the order the engine asks for them."
        );
    }

    // ---- §8 row 21's *first* downcall, driven before the surface rows ----------------------
    //
    // **The engine said the order in §8's table was wrong, in its own words.** With the surface
    // delivered first, the game thread answered
    //
    // ```text
    // [FLog::NativeDM] nativeActivity_onSurfaceChanged: ... Flags-Not-Received. Return.
    // ```
    //
    // -- and *returned*, having done nothing with the window. Nothing re-delivers a surface that
    // was dropped, so the renderer was never asked for, and from outside that was
    // indistinguishable from a graphics problem: the game loop spun in `ALooper_pollOnce`
    // (MEASURED: 138,974,961 calls), `nativePostClientSettingsLoadedInitialization3` blocked in
    // `pthread_cond_wait`, and a worker spun on `sched_yield` waiting for a pointer at
    // `0x02173f8c` that the surface path publishes.
    //
    // On a device the client-settings fetch (`fi.e$f`) runs from `onCreate`, long before the
    // SurfaceView's `surfaceCreated` callback, so the flags are there when the surface arrives.
    // §8's table lists 21 after 20 because that is the order the *dex* names them in; the
    // ordering between those two rows was never independently verified, and the engine has now
    // said what it is. This is the roadmap extended from measured runtime behaviour, which is
    // what the goal asks for when the roadmap and the runtime disagree.
    let settings_outcomes = if attempt_flags {
        drive_flag_rows(&guest, &mut cpu, &script::FLAGS_AND_START[..1])
    } else {
        Vec::new()
    };
    let settings_loaded = attempt_flags
        && settings_outcomes.iter().all(|outcome| outcome.result.is_ok())
        && !settings_outcomes.is_empty();

    let mut row_outcomes: Vec<(String, Result<(), String>)> = Vec::new();
    for (member, descriptor, tail) in rows {
        let target = native(member, descriptor);
        let mut args = vec![
            GuestArg::Pointer(guest.jni.env_for(0)),
            GuestArg::Int(thiz),
            GuestArg::Int(native_code),
        ];
        args.extend(tail);
        let result = {
            let _bionic = guest.bionic.activate().expect("publish the bionic instance");
            let _jni = guest.jni.activate().expect("publish the JNI instance");
            let _ndk = guest.ndk.activate();
            guest.boundary.call_guest(&mut cpu, member, target, &args, LIFECYCLE_BUDGET)
        };
        let _ = writeln!(
            std::io::stderr(),
            "§8 row: {member} -> {}",
            match &result {
                Ok(_) => "returned".to_string(),
                Err(error) => format!("{error}"),
            }
        );
        report_dead_guest_threads(&guest, &format!("after §8 row `{member}`"));
        let failed = result.is_err();
        row_outcomes.push((
            member.to_string(),
            result.map(|_| ()).map_err(|error| error.to_string()),
        ));
        if failed {
            // **Stop at the first failure, because the next row would not be evidence.** These
            // natives go through the glue's `android_app_set_activity_state`/`set_window`, which
            // take `android_app->mutex` and release it on the way out. A refusal unwinds out of
            // the guest *inside* that critical section, so the mutex stays held -- MEASURED: the
            // run that first met `pthread_cond_timedwait` refused inside `onStartNative` and then
            // hung for the whole watchdog in `onResumeNative`, on a lock the refusal had
            // abandoned. Continuing would report a deadlock caused by the harness as though it
            // were the engine's.
            let _ = writeln!(
                std::io::stderr(),
                "§8 rows: stopping at `{member}`; the glue holds its own mutex across these calls, \n                 so a later row would be waiting on a lock this refusal abandoned"
            );
            break;
        }
    }

    // ---- §8 rows 21-22: the client settings the engine is waiting for -----------------------
    //
    // **This is §8.1's sixth failure mode, driven rather than waited out.** With the window
    // taken, the game thread logged `nativeActivity_onSurfaceChanged: ... Flags-Not-Received.
    // Return.` -- the engine will not ask for a renderer until the flags phase has run, and from
    // outside that is indistinguishable from a graphics problem.
    //
    // Run only if the lifecycle rows all returned: these go into the same engine the abandoned
    // glue mutex would be inside, and a flags phase driven over a half-finished lifecycle would
    // measure the harness rather than the engine.
    // **§8 rows 21-22 are opt-in, and the reason is printed every run.**
    //
    // They are the frontier, not a regression: row 21's first downcall returns and loads the
    // engine's flags, and its *second* -- `nativePostClientSettingsLoadedInitialization3` --
    // blocks in `pthread_cond_wait` on a signal that never comes, because the two guest worker
    // threads that would send it are parked in a raw indefinite `futex` nothing ever wakes. The
    // main thread cannot be released from a condition variable by `stop_guest_threads` (that
    // switch reaches the futex, not `omni-bionic`'s waiter registry), so the run cannot get to
    // teardown and the whole workspace suite hangs behind it.
    //
    // Making the *driver* opt-in is not `VERIFICATION.md` entry 4's shape: nothing that is
    // **asserted** is skipped. Step 13 and rows 17-20 still run and still assert, every time.
    // What is gated is an attempt to cross a frontier that is known not to be crossable yet, and
    // the run says so out loud with the command that reproduces it.
    // ---- §8 row 21's second downcall, then the surface again ------------------------------
    //
    // **The engine drops a surface that arrives before the flags, and nothing re-delivers it.**
    // MEASURED, in the run where row 21 finally returned: `nativeActivity_onSurfaceChanged:
    // state:2` and then `... Flags-Not-Received. Return.` at 6.601 s, while
    // `nativePostClientSettingsLoadedInitialization3` -- the call that runs
    // `continueAfterFlagsLoaded_`, which sets the byte at `DataModel + 0x289` that the surface
    // path is gated on -- did not return until 6.808 s. The window was taken and thrown away two
    // hundred milliseconds before the engine was willing to look at it.
    //
    // On a device the surface is not lost, because it is a property of a live `SurfaceView`: the
    // engine picks it up on the next event, and §8 row 24 exists precisely for the case where the
    // app bridge has to hand it back (`nativeAppBridgeV2UpdateSurfaceAppWithPlatformParams`).
    // Here nothing else will send one, so the harness re-sends what a device's view would still
    // be holding.
    let flags_outcomes = if settings_loaded
        && row_outcomes.iter().all(|(_, result)| result.is_ok())
    {
    {
        // Row 21's second downcall on its own first, so the flags are received...
        let mut all = drive_flag_rows(&guest, &mut cpu, &script::FLAGS_AND_START[1..2]);
        if all.iter().all(|outcome| outcome.result.is_ok()) {
            // ...then the surface again, now that the engine will accept it. Only the two rows
            // that carry the window: the lifecycle state is already where it should be, and
            // re-sending `onStart`/`onResume` would be telling the engine about a transition
            // that did not happen.
            for (member, descriptor, tail) in [
                ("onSurfaceCreatedNative", "(JLandroid/view/Surface;)V", vec![GuestArg::Int(surface)]),
                (
                    "onSurfaceChangedNative",
                    "(JLandroid/view/Surface;III)V",
                    vec![
                        GuestArg::Int(surface),
                        GuestArg::Int(1),
                        GuestArg::Int(surface_width as u64),
                        GuestArg::Int(surface_height as u64),
                    ],
                ),
            ] {
                let target = native(member, descriptor);
                let mut args = vec![
                    GuestArg::Pointer(guest.jni.env_for(0)),
                    GuestArg::Int(thiz),
                    GuestArg::Int(native_code),
                ];
                args.extend(tail);
                let result = {
                    let _bionic = guest.bionic.activate().expect("publish the bionic instance");
                    let _jni = guest.jni.activate().expect("publish the JNI instance");
                    let _ndk = guest.ndk.activate();
                    guest.boundary.call_guest(&mut cpu, member, target, &args, LIFECYCLE_BUDGET)
                };
                let _ = writeln!(
                    std::io::stderr(),
                    "§8 row 24: {member} re-sent after the flags -> {}",
                    match &result {
                        Ok(_) => "returned".to_string(),
                        Err(error) => format!("{error}"),
                    }
                );
                report_dead_guest_threads(&guest, &format!("after re-sending `{member}`"));
                if result.is_err() {
                    break;
                }
            }
            // **The reading that says whether any of this worked.** The byte at
            // `DataModel + 0x289` is what `nativeActivity_onSurfaceChanged` tests at guest
            // `0x02bd307c`; until `continueAfterFlagsLoaded_` writes it at `0x02bd3be4` the
            // engine returns without looking at the window.
            let _ = writeln!(
                std::io::stderr(),
                "§8 row 24: engine `Flag::areFlagsLoaded` global now {} (NOT the DataModel+0x289 
                 byte the surface is gated on — see `are_flags_loaded_global`; the reading 
                 that answers that is the `Flags-Not-Received` count below)",
                are_flags_loaded_global(&guest)
            );
            all.extend(drive_flag_rows(&guest, &mut cpu, &script::FLAGS_AND_START[2..]));

            // ---- row 24 again, once the engine's *own* fetch has answered -------------------
            //
            // **The flags the surface path waits for do not arrive on this thread.** Row 21
            // hands the engine a client-settings document; the byte at `DataModel + 0x289` that
            // `nativeActivity_onSurfaceChanged` tests at guest `0x02bd307c` is written by
            // `continueAfterFlagsLoaded_` (`0x02bd3b58`, the store at `0x02bd3be4`), and the only
            // caller on that path is `0x02bd5560` — the success side of `NativeDM`'s own HTTP
            // fetch, which runs on a guest worker and takes about two seconds.
            //
            // MEASURED, before this block existed: the two surface rows above ran at 6.43 s and
            // 6.60 s and the fetch did not answer until 8.50 s, so every surface this gate
            // delivered was delivered while the engine was still waiting and was dropped with
            // `... Flags-Not-Received. Return.` The engine was never wrong about anything; the
            // window simply arrived two seconds early, every run.
            //
            // So: wait for the engine to *say* how the fetch went, in its own log, and only then
            // send the window again. Bounded, and the bound is reported rather than silent — a
            // wait that timed out and carried on would look exactly like a fetch that failed.
            let flags_answer = wait_for_log(&guest, "getFlags: success", FLAG_FETCH_WAIT);
            let _ = writeln!(
                std::io::stderr(),
                "§8 row 24: the engine's flag fetch answered: {}",
                flags_answer.as_deref().unwrap_or("nothing in 30 s")
            );
            if flags_answer.as_deref().is_some_and(|line| line.contains("success = true")) {
                // **`getFlags: success` is logged before the byte is written, so waiting on it
                // alone is a race.** Decoded: the success line comes from `0x02bd5884`, and the
                // store the surface path is gated on is at `0x02bd3be4` — inside
                // `continueAfterFlagsLoaded_`, which the same path does not call until
                // `0x02bd59f8`, after twenty-odd flag initialisers in between. A window delivered
                // in that interval would be dropped exactly as the early ones were, and the run
                // would look like the fix had not worked.
                //
                // `continueAfterFlagsLoaded_` logs its own name at `0x02bd3bac`, six instructions
                // and one call before the store, under the same level guard that let the success
                // line through — so waiting for *that* closes all but those few instructions,
                // against a poll that cannot come back in under 25 ms.
                //
                // **The residual window is not argued away, it is measured**: the count of
                // `Flags-Not-Received` lines is taken before the delivery and again after, and
                // both are printed. If the race were ever lost the second number would be larger,
                // which is the same line this whole block exists to make disappear.
                let entered = wait_for_log(&guest, "continueAfterFlagsLoaded_", FLAG_FETCH_WAIT);
                let _ = writeln!(
                    std::io::stderr(),
                    "§8 row 24: continueAfterFlagsLoaded_ ran: {}",
                    entered.as_deref().unwrap_or("NOT SEEN — the byte at DataModel+0x289 may not \
                     be written yet, and the delivery below is racing it")
                );
                let refusals_before = count_log(&guest, "Flags-Not-Received");
                for (member, descriptor, tail) in [
                    (
                        "onSurfaceCreatedNative",
                        "(JLandroid/view/Surface;)V",
                        vec![GuestArg::Int(surface)],
                    ),
                    (
                        "onSurfaceChangedNative",
                        "(JLandroid/view/Surface;III)V",
                        vec![
                            GuestArg::Int(surface),
                            GuestArg::Int(1),
                            GuestArg::Int(surface_width as u64),
                            GuestArg::Int(surface_height as u64),
                        ],
                    ),
                ] {
                    let target = native(member, descriptor);
                    let mut args = vec![
                        GuestArg::Pointer(guest.jni.env_for(0)),
                        GuestArg::Int(thiz),
                        GuestArg::Int(native_code),
                    ];
                    args.extend(tail);
                    let result = {
                        let _bionic = guest.bionic.activate().expect("publish the bionic instance");
                        let _jni = guest.jni.activate().expect("publish the JNI instance");
                        let _ndk = guest.ndk.activate();
                        guest.boundary.call_guest(&mut cpu, member, target, &args, LIFECYCLE_BUDGET)
                    };
                    let _ = writeln!(
                        std::io::stderr(),
                        "§8 row 24: {member} sent again after the fetch -> {}",
                        match &result {
                            Ok(_) => "returned".to_string(),
                            Err(error) => format!("{error}"),
                        }
                    );
                    report_dead_guest_threads(&guest, &format!("after `{member}` post-fetch"));
                    if result.is_err() {
                        break;
                    }
                }
                // **The reading this whole block is for.** `nativeActivity_onSurfaceChanged`
                // returns without touching the window while the byte at `DataModel + 0x289` is
                // clear, and says so. Equal counts mean the engine accepted the window this time;
                // a larger second number means it refused again, which is a result and not a
                // failure of the harness — it says the byte is still clear and names where to
                // look next.
                let refusals_after = count_log(&guest, "Flags-Not-Received");
                let _ = writeln!(
                    std::io::stderr(),
                    "§8 row 24: `Flags-Not-Received` lines in the ring: {refusals_before} before \n                 the post-fetch delivery, {refusals_after} after"
                );
            }
        }
        all
    }
    } else {
        let _ = writeln!(
            std::io::stderr(),
            "§8 rows 21-22: the rest not attempted, because {}",
            if settings_loaded {
                "a lifecycle row did not return"
            } else if attempt_flags {
                "`nativeInitClientSettings` did not return"
            } else {
                "OMNI_M6_ROWS_21_22 is not set"
            }
        );
        Vec::new()
    };
    let _ = &flags_outcomes;

    rows_done.store(true, std::sync::atomic::Ordering::Relaxed);

    // **Let the game thread run on what it was just handed.** The lifecycle calls above post
    // commands; what the engine does with them happens on the other thread, and a measurement
    // taken the instant the last one returns is a measurement of nothing — the same mistake
    // `report` made before `join_guest_threads` was added below it.
    let settle = std::time::Instant::now();
    while settle.elapsed() < POST_ROWS_SETTLE {
        if guest.bionic.live_guest_threads() == 0 {
            break;
        }
        // The window's own thread is this one, so this is where it is pumped and sampled: a
        // window nobody pumps is one the OS marks as not responding, and a geometry nobody
        // samples goes stale at the first resize.
        if let (Some(open), Some(source)) = (window.as_mut(), window_source.as_ref()) {
            let _ = open.poll_events().count();
            let _ = source.sample(open);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // **What the engine asked the Vulkan loader for, in order** -- the census the stage tests
    // said the first run that reached graphics would produce. Printed whether or not anything
    // was asked, so an empty list is a reading and not a missing line.
    match &guest.vulkan {
        Some(vulkan) => {
            let names = vulkan.names();
            let _ = writeln!(
                std::io::stderr(),
                "VULKAN: the engine resolved {} entry point(s): {names:?}\n{}",
                names.len(),
                vulkan.report()
            );
        }
        None => {
            let _ = writeln!(std::io::stderr(), "VULKAN: not bound ({GRAPHICS_GATE} unset)");
        }
    }

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
    {
        let mut out = std::io::stderr();
        // **`report` runs too early to see any of this, and that is how a defect hid here.**
        // `report` is called the instant step 13 returns, which is *before* the game thread has
        // run `android_main`. MEASURED: the run that ended with the game thread dead on a null
        // `jmethodID` printed `JNI misses: 0` from `report` and had exactly one miss by the time
        // the thread was joined. A measurement taken before the thread that produces it is a
        // measurement of nothing, which is `VERIFICATION.md` entry 4's shape one step along.
        let misses = guest.jni.misses();
        let _ = writeln!(out, "POST-TEARDOWN JNI misses: {}", misses.len());
        for miss in &misses {
            let _ = writeln!(
                out,
                "  MISS {} {}.{} {}",
                miss.function, miss.class, miss.member, miss.descriptor
            );
        }
        let _ = writeln!(out, "POST-TEARDOWN JNI census: {:?}", guest.jni.census());
        let calls = guest.jni.calls();
        let _ = writeln!(out, "POST-TEARDOWN upcalls: {}", calls.len());
        for record in calls.iter().rev().take(40).rev() {
            let _ = writeln!(
                out,
                "  CALL {}.{}{} {:?}",
                record.class, record.member, record.descriptor, record.args
            );
        }
        let _ = out.flush();
    }
    // **The network census, printed on every run rather than only on a stalled one.**
    //
    // It was added to the watchdog's stall report first, and that was `VERIFICATION.md` entry 15's
    // shape arriving as a prediction: a diagnostic that only fires when the run goes wrong says
    // nothing about the run that goes right, and this one is about a fetch that FAILS while
    // everything else looks healthy. MEASURED: the first run that reached teardown did so in 77 s
    // and the watchdog never sampled, so the census was never printed at all.
    //
    // The order is the order a client walks -- a name, a socket, its options, a connect, the
    // readiness wait, the error the connect reports, the bytes -- so reading it left to right says
    // how far the settings fetch got. A zero after a non-zero is where it stopped, which is a
    // different question from "did a thread die" and the one `fetch flag exception: HttpError:
    // Unknown` does not answer.
    {
        let census = guest.boundary.census();
        let count =
            |symbol: &str| -> u64 { census.as_ref().and_then(|c| c.get(symbol).copied()).unwrap_or(0) };
        // **Every path the engine asked for and did not get.** An `ENOENT` is the quietest
        // failure this runtime can produce: `open` answers it correctly, the guest handles it
        // correctly, and whatever goes wrong goes wrong somewhere else entirely. See
        // `Filesystem::open_misses` for the measurement that made this worth printing.
        if let Some(fs) = guest.bionic.filesystem() {
            let misses = fs.open_misses();
            let _ = writeln!(
                std::io::stderr(),
                "POST-TEARDOWN missing paths ({}): {:?}",
                misses.len(),
                misses
                    .iter()
                    .map(|path| String::from_utf8_lossy(path).into_owned())
                    .collect::<Vec<_>>()
            );
        }
        let _ = writeln!(
            std::io::stderr(),
            "POST-TEARDOWN net census: {:?}",
            [
                "getaddrinfo",
                "freeaddrinfo",
                "socket",
                "setsockopt",
                "getsockopt",
                "getsockname",
                "ioctl",
                "fcntl",
                "connect",
                "poll",
                "select",
                "read",
                "write",
                "__write_chk",
                "sendto",
                "recvfrom",
                "shutdown",
                "close",
                "getentropy",
                "mktime",
                "syscall",
            ]
            .map(|symbol| (symbol, count(symbol)))
        );
    }
    // **The socket transcripts, if this run asked for them.** Printed after teardown for the
    // reason the block above gives for the JNI misses: the fetch finishes on a guest thread, and
    // a print taken before that thread is joined is a print of an empty table.
    //
    // **It says so on every run, including the runs where it is off** -- `VERIFICATION.md` entry
    // 15. "No socket record printed" and "no socket carried a byte" are the same shape on the
    // page, and the first is a switch while the second is a finding.
    if record_bytes > 0 {
        omni_platform::net::record::stop();
        let transcripts = omni_platform::net::record::take();
        let _ = writeln!(
            std::io::stderr(),
            "POST-TEARDOWN socket record: {} socket(s) carried bytes",
            transcripts.len()
        );
        for transcript in &transcripts {
            let _ = writeln!(std::io::stderr(), "  {}", transcript.outline());
        }
    } else {
        let _ = writeln!(
            std::io::stderr(),
            "POST-TEARDOWN socket record: NOT RECORDED. Set OMNI_NET_RECORD=<bytes> to keep the 
                 first N bytes each socket carried in each direction. The engine's TLS is its 
                 own, so what that shows is a ClientHello and then ciphertext -- see 
                 omni_platform::net::record."
        );
    }
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

    // ---- the guest's own threads, asserted rather than printed -------------------------------
    //
    // **`VERIFICATION.md` entry 16, and it is here because it was missing.** Every other assertion
    // in this gate is about a call *this* thread made. A guest thread that starts, runs and is
    // killed by this layer is invisible to all of them -- it is not a downcall that returned an
    // error and it is not a refusal on the calling thread -- and `live_guest_threads()` falling is
    // indistinguishable from a worker finishing.
    //
    // MEASURED: the gate reported "21 of 21 scripted downcalls" and seven lifecycle rows returned,
    // for three milestones, while three of the guest's worker threads lay dead -- one on an
    // ordinary log line this layer refused, two on symbols nothing had bound. One of them was
    // holding the future `nativePostClientSettingsLoadedInitialization3` was waiting on, which is
    // the whole of why the runtime hung.
    //
    // No allowlist, for the reason the JNI-miss assertion below gives: a thread this layer killed
    // is a defect in this layer, and one that is genuinely expected belongs here by name beside
    // its evidence, never as a relaxed bound.
    let dead = guest.bionic.guest_thread_failures();
    assert!(
        dead.is_empty(),
        "the run ended with {} guest thread(s) killed by this layer. Each took with it whatever \n         work the guest had given it, and a thread that dies is not a call that fails -- so \n         nothing else in this gate would have said a word:\n{}",
        dead.len(),
        dead.iter()
            .map(|failure| format!(
                "  thread {} started at link {:#x}: {}",
                failure.thread,
                failure.start_routine.wrapping_sub(guest.object.base),
                failure.why
            ))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    // ---- the game thread's own JNI, asserted rather than printed -----------------------------
    //
    // **A printed list nobody asserts on is a watch and not a detector** (`VERIFICATION.md`
    // entry 11). Everything above this line is about the thread that called step 13; these two
    // are about the thread step 13 spawned, and they are the only assertions in this gate that
    // are.
    //
    // A miss on the game thread is a `GetMethodID`/`GetFieldID`/`FindClass` that answered null,
    // and §8.1's third failure mode is that the engine does not check it. There is no allowlist
    // because there is nothing on it: MEASURED 0, n = 2 runs. A Tier X miss that is genuinely
    // expected belongs here **by name**, beside its evidence -- never as a relaxed bound.
    let misses = guest.jni.misses();
    assert!(
        misses.is_empty(),
        "the run ended with {} JNI miss(es), and the engine does not check what a failed lookup \
         answers -- the first of them is {:?}",
        misses.len(),
        misses.first()
    );
    // And the shape a miss turns into one frame later, asserted separately: a miss that the
    // engine happened not to use would still be a miss, and a null id reaching a call is a
    // **crash** rather than a gap. Named by the text the typed error carries, so a refusal that
    // stops naming it stops passing.
    for failure in guest.bionic.guest_thread_failures() {
        assert!(
            !failure.why.contains("as a jmethodID"),
            "a guest thread died on a null jmethodID, which is jni-surface.md §8.1's third \
             failure mode: {failure:?}"
        );
    }
}

/// The bytes **before** an address a refusal named as unreadable.
///
/// A refusal that says "1 byte at `0x…000` is not mapped" says where the read stopped and nothing
/// about why it got there. For a string walk -- `strchr`, `strlen`, `strcmp` -- the question is
/// always whether the terminator was missing or whether the walk had already passed it, and the
/// answer is in the bytes just behind the boundary. Rendered as text with non-printables escaped,
/// because what a string walk ran past is a string.
///
/// The address is parsed out of the message rather than threaded through the error type: this is
/// a probe for one investigation, and a field on `AbiError` would be a permanent surface added
/// for a temporary question.
/// Drive a slice of [`script::FLAGS_AND_START`], **one step at a time**, reporting each before
/// the next is attempted.
///
/// MEASURED with the whole table handed to one `script::run`: a later row hung, the watchdog
/// ended the process, and *none* of the per-row lines had been printed -- so the run said nothing
/// about the rows that had already returned. `VERIFICATION.md` entry 4's shape: the measurement
/// has to survive the failure it is measuring.
fn drive_flag_rows(
    guest: &Guest,
    cpu: &mut DynarmicCpu,
    table: &[script::Downcall],
) -> Vec<script::StepOutcome> {
    let mut all = Vec::new();
    for step in table {
        let one = std::slice::from_ref(step);
        let outcomes = {
            let _bionic = guest.bionic.activate().expect("publish the bionic instance");
            let _jni = guest.jni.activate().expect("publish the JNI instance");
            let _ndk = guest.ndk.activate();
            script::run(
                &guest.jni,
                &guest.boundary,
                cpu,
                &|symbol| guest.exports.get(symbol).copied(),
                one,
                0,
            )
            .expect("building the scripted arguments must not fail")
        };
        for outcome in &outcomes {
            let _ = writeln!(
                std::io::stderr(),
                "§8 row {}: {} -> {}   [Flag::areFlagsLoaded global: {}]",
                outcome.step,
                outcome.symbol,
                match &outcome.result {
                    Ok(()) => format!(
                        "returned {}",
                        match outcome.returned {
                            Some(x0) => format!("{:#x} ({})", x0, x0 as u32 as i32),
                            None => "nothing".to_string(),
                        }
                    ),
                    Err(error) => format!(
                        "{error} [last crossing from guest {:#x} (link {:#x})]",
                        guest.boundary.last_caller(),
                        guest.boundary.last_caller().wrapping_sub(guest.object.base)
                    ),
                },
                are_flags_loaded_global(guest)
            );
        }
        // **A row that returns is not a row that went well.** MEASURED: the gate reported "21 of
        // 21 scripted downcalls" and "rows 17-20 all returned" for three milestones while, behind
        // it, three of the guest's own worker threads had died -- one on a string this layer
        // refused, one on an unbound symbol, one on a memory fault -- and each took with it
        // whatever work it was holding. Nothing printed any of it, because a guest thread that
        // dies is not a downcall that failed.
        //
        // Reported after every row rather than once at the end, so the row that killed a thread is
        // the row it is printed under.
        report_dead_guest_threads(guest, &format!("after §8 row {}", step.step));
        let failed = outcomes.iter().any(|outcome| outcome.result.is_err());
        all.extend(outcomes);
        if failed {
            break;
        }
    }
    all
}

/// Print every guest thread that has died, and what killed it, or say that none has.
///
/// See the call site in [`drive_flag_rows`] for the three that were dying unremarked. This is a
/// **print, not an assertion**, only until those are fixed: the gate cannot assert an empty list
/// while it is not empty, and an assertion added now would be one more thing to remember to turn
/// on. The `report` at the end of the run asserts it.
fn report_dead_guest_threads(guest: &Guest, when: &str) {
    let failures = guest.bionic.guest_thread_failures();
    if failures.is_empty() {
        return;
    }
    let mut out = std::io::stderr();
    let _ = writeln!(out, "  DEAD GUEST THREADS {when} ({}):", failures.len());
    for failure in &failures {
        let _ = writeln!(
            out,
            "    thread {} started at {:#x} (link {:#x}): {}",
            failure.thread,
            failure.start_routine,
            failure.start_routine.wrapping_sub(guest.object.base),
            failure.why
        );
        // PC, X30, then the frame chain, as link addresses so they can be looked up in the
        // binary. A value outside the image prints as itself, prefixed, rather than as a
        // wrapped subtraction that would read like an address in it.
        let _ = writeln!(
            out,
            "      guest stack at death (link): {:?}",
            failure
                .stack
                .iter()
                .map(|&at| {
                    let at = at as usize;
                    match at.checked_sub(guest.object.base) {
                        Some(link) if link < 0x0700_0000 => format!("{link:#x}"),
                        _ => format!("abs {at:#x}"),
                    }
                })
                .collect::<Vec<_>>()
        );
    }
    let _ = out.flush();
}

fn bytes_before_the_fault(guest: &Guest, message: &str) -> String {
    // **The last `at 0x`, not the first.** The first is the thunk's own address, which every
    // refusal carries and which is never the address that faulted.
    let Some(rest) = message.rsplit(" at 0x").next().filter(|_| message.contains(" at 0x")) else {
        return String::new();
    };
    let digits: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
    let Ok(at) = usize::from_str_radix(&digits, 16) else { return String::new() };
    if at < 128 {
        return String::new();
    }
    let mut rendered = String::new();
    // A word at a time, taking the byte out of it: `GuestMem` has no byte reader, and an aligned
    // word read covers the same range.
    for offset in (at - 128)..at {
        match guest.boundary.mem().read_u32(
            offset & !3,
            omni_android::Blame::new("the bytes before a fault", at, 0),
        ) {
            Ok(word) => {
                let byte = (word >> (8 * (offset & 3))) as u8;
                if byte == 0 {
                    rendered.push_str("[NUL]");
                } else if byte.is_ascii_graphic() || byte == b' ' {
                    rendered.push(byte as char);
                } else {
                    rendered.push_str(&format!("<{byte:02x}>"));
                }
            }
            Err(_) => rendered.push('?'),
        }
    }
    format!("\n      the 128 bytes before {at:#x}: [{rendered}]")
}

/// **Everything known about a guest that has stopped making progress, in one place.**
///
/// # What this exists to answer, and why each line is in it
///
/// A stuck guest thread reports nothing about itself: it executes no guest instructions, so no
/// budget expires, and a lock names no owner. Each line here is a fact that was needed to localise
/// M6's stall, and the order is the order they narrow it:
///
/// * **the spin lock and its count** -- the engine state that is blocking, read out of the engine
///   (see [`SPIN_LOCK_OFFSET`]);
/// * **import crossings**, sampled twice -- a total that does not move says no thread is executing
///   anything, which separates "slow" from "stopped";
/// * **the thread list** -- which guest threads exist, what each was asked to run, and whether it
///   has finished. A thread that finished while holding a lock and one that is blocked holding it
///   look identical from the lock;
/// * **`parked`** -- condition-variable waits, which are the ones `Bionic` can name;
/// * **the futex** -- how many parks had no deadline at all
///   ([`AddressFutex::indefinite_parks`]), which addresses still have someone on them, and the
///   **word at each**, because a waiter on a word whose value has moved was woken and missed it
///   while a waiter on an unchanged word is waiting for something that never happened;
/// * **the raw `futex` calls** -- recorded on the way *in*, so a call that never returned is still
///   in the list. That is the line that named this stall: two guest threads in
///   `FUTEX_WAIT_BITSET` on their own words, expecting `0`, with **no `FUTEX_WAKE` anywhere in the
///   run**.
///
/// Printed rather than asserted, deliberately. It is a report, and a report that failed the run
/// would stop it before the rows after it had been tried.
fn stall_report(guest: &Guest, when: &str) {
    let mut out = std::io::stderr();
    let crossings =
        || guest.boundary.census().map_or(0u64, |census| census.values().sum::<u64>());
    let first = crossings();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let second = crossings();

    let _ = writeln!(
        out,
        "\n================ STALL REPORT: {when} ================\n\
         spin lock {} count {} | import crossings {first} -> {second} ({}) | live threads {}",
        image_word(guest, SPIN_LOCK_OFFSET, "the spin lock at 0x06dd0a30"),
        image_word(guest, SPIN_LOCK_OFFSET + 4, "the count at 0x06dd0a34"),
        if second == first { "FROZEN" } else { "moving" },
        guest.bionic.live_guest_threads(),
    );
    for thread in guest.bionic.guest_thread_list() {
        let _ = writeln!(
            out,
            "  thread {:#x}: start routine {:#x} (link {:#x}), detached {}, running {}",
            thread.id.0,
            thread.start_routine,
            thread.start_routine.wrapping_sub(guest.object.base),
            thread.detached,
            thread.running
        );
    }
    let futex = guest.bionic.futex();
    let (waits, wakes) = futex.activity();
    let _ = writeln!(
        out,
        "  futex: {waits} wait(s), {wakes} wake(s), {} with no deadline; cond-parked {:?};          near misses {:?}",
        futex.indefinite_parks(),
        guest.bionic.parked(),
        futex
            .near_misses()
            .iter()
            .map(|(woke, parked)| format!("woke {woke:#x} while {parked:#x} was parked"))
            .collect::<Vec<_>>()
    );
    for (address, waiters) in futex.parked_addresses() {
        let word = guest.boundary.mem().read_u32(
            address as usize,
            omni_android::Blame::new("a parked futex word", address as usize, 0),
        );
        let _ = writeln!(
            out,
            "  parked on {address:#x} x{waiters}: the word reads {word:?}"
        );
    }
    let calls = guest.bionic.futex_calls();
    let _ = writeln!(
        out,
        "  raw futex syscalls: {} recorded, {} dropped past the bound (a large drop count is          itself the finding: the guest is spinning on a futex, not waiting on one)",
        calls.len(),
        guest.bionic.futex_calls_dropped()
    );
    for call in &calls {
        let _ = writeln!(
            out,
            "    thread {:#x} {} on {:#x} value {} from guest {:#x} (link {:#x}) -> {}",
            call.thread,
            call.op,
            call.address,
            call.value,
            call.caller,
            call.caller.wrapping_sub(guest.object.base),
            if call.outcome == i32::MIN {
                "ENTERED AND NEVER RETURNED".to_string()
            } else {
                call.outcome.to_string()
            }
        );
    }
    let _ = writeln!(out, "================ end of stall report ================\n");
    let _ = out.flush();
}

/// A `u32` of the loaded image, by its link-time offset, for a probe that has to name a state
/// inside the engine.
fn image_word(guest: &Guest, offset: usize, what: &str) -> String {
    match guest
        .boundary
        .mem()
        .read_u32(guest.object.base + offset, omni_android::Blame::new(what, guest.object.base, 0))
    {
        Ok(word) => format!("{word:#x}"),
        Err(error) => format!("unreadable: {error}"),
    }
}

/// The engine's `Flag::areFlagsLoaded` **global**, read out of the image.
///
/// # It is not the byte the surface is gated on, and it was named as though it were
///
/// **Renamed, because the old name was `flags_loaded_byte` and the line it printed said "engine
/// flags-received byte now 1".** Two different bytes were being conflated:
///
/// * this one — the `Flag::areFlagsLoaded` global at image offset [`FLAGS_LOADED_OFFSET`], which
///   `nativeInitClientSettings` sets as soon as it has parsed the document it was handed;
/// * `DataModel + 0x289` — a field of a heap object, written by `continueAfterFlagsLoaded_` at
///   guest `0x02bd3be4` and tested at `0x02bd307c`, which is the **only** thing gating
///   `nativeActivity_onSurfaceChanged`.
///
/// This one has read `1` since row 21's first downcall returned, in every run for several
/// milestones — **including every run in which the surface was refused**. A reading that is `1`
/// whether or not the thing it is named after happened is `VERIFICATION.md` entry 15's shape: an
/// instrument whose label promises more than it reads, which will be believed again by whoever
/// sees it next. It is kept because it is a real reading of a real global, and renamed so that it
/// can only be read as that.
///
/// **What to use instead.** The host cannot read `DataModel + 0x289` — it is a heap address this
/// side never learns. What it can read is the engine's own report: the branch at `0x02bd307c` is
/// the sole producer of `[FLog::NativeDM] nativeActivity_onSurfaceChanged: ... Flags-Not-Received.
/// Return.` (format string `0x004ed460`), so **the absence of that line is the byte being set and
/// its presence is the byte being clear**. [`count_log`] taken either side of a surface delivery
/// is the instrument; see row 24's post-fetch block.
///
/// See [`FLAGS_LOADED_OFFSET`] for how this address was decoded and why it is one address and not
/// a guess.
fn are_flags_loaded_global(guest: &Guest) -> String {
    match guest.boundary.mem().read_u32(
        guest.object.base + FLAGS_LOADED_OFFSET,
        omni_android::Blame::new("Flag::areFlagsLoaded", guest.object.base, 0),
    ) {
        Ok(word) => format!("{}", word & 0xff),
        Err(error) => format!("unreadable: {error}"),
    }
}

/// Wait for a line the guest logs, and answer with the line or with nothing.
///
/// **The engine is the only thing that knows when its own fetch finished**, and it says so: a
/// `[FLog::NativeDM] ... getFlags: success = {}` line, on the worker that ran the request. The
/// host cannot see that from a return value — nothing this thread called is still running — and
/// inferring it from a timer would be a guess dressed as a measurement.
///
/// Returns `None` on the timeout rather than blocking for ever, and the caller **prints which it
/// got**. A wait that timed out and then carried on as though the thing had happened is
/// `VERIFICATION.md` entry 14's shape exactly: indistinguishable, a week later, from a wait that
/// succeeded.
///
/// `log_records` is a bounded ring (256 records / 256 KiB) and the engine logs faster than that
/// during startup, so a match can be **evicted** between polls. The poll interval is therefore
/// short relative to how fast the ring turns over, and a `None` from this function means "not
/// seen", not "did not happen".
fn wait_for_log(guest: &Guest, needle: &str, within: std::time::Duration) -> Option<String> {
    let deadline = std::time::Instant::now() + within;
    loop {
        for record in guest.bionic.log_records() {
            if record.message.contains(needle) {
                return Some(record.message);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// How many lines still in the ring contain `needle`.
///
/// **A count taken twice around an event, not a presence test.** `Flags-Not-Received` is logged
/// every time the engine is handed a window it is not ready for, and this gate hands it several;
/// "is that line present" would answer yes for the whole run and say nothing about the delivery
/// that matters. The difference between two counts does.
///
/// It reads the same bounded ring [`wait_for_log`] does, so a count can go *down* if the ring
/// evicted an older match between the two readings. That direction is harmless here — it can only
/// understate a new refusal, and a new refusal is what the caller is looking for.
fn count_log(guest: &Guest, needle: &str) -> usize {
    guest.bionic.log_records().iter().filter(|record| record.message.contains(needle)).count()
}

/// Everything this run learned about the network, printed **before** an `exit` that runs no
/// destructors.
///
/// # A diagnostic that only prints when the run succeeds is not a diagnostic
///
/// Both watchdogs end the process with `std::process::exit(101)`, because the main thread is
/// inside the guest and cannot be made to fail an assertion from outside. `exit` runs no `Drop`
/// and skips the rest of `main`, so **everything the teardown block prints is lost on exactly the
/// runs that needed it** — MEASURED: the run that first fetched a real client-settings document
/// was killed by the M6 watchdog mid-download and reported not one byte, which is the single
/// number that would have said whether the fetch was progressing or stuck.
///
/// `remove_scratch_roots()` already sits beside each `exit` for the same reason — `Scratch::new`
/// records the 930 GB that taught it — and this belongs in the same place on the same argument.
///
/// The two numbers are chosen to be read together. The census counts **calls**; the record counts
/// **bytes**. Bytes ÷ calls is the average transfer, which is what separates "the reads are as
/// large as this layer allows and the thread is simply not being scheduled" from "the guest is
/// asking for a few hundred bytes at a time" — and `SOCKET_IO_BLOCK` caps one call at 64 KiB, so
/// a ratio at that cap makes the cap the next question rather than the scheduling.
fn report_network(out: &mut impl Write, boundary: &Boundary, when: &str) {
    let census = boundary.census();
    let count = |symbol: &str| -> u64 {
        census.as_ref().and_then(|c| c.get(symbol).copied()).unwrap_or(0)
    };
    let _ = writeln!(
        out,
        "NET CENSUS ({when}): {:?}",
        ["getaddrinfo", "socket", "connect", "poll", "select", "sendto", "recvfrom", "read",
         "write", "close"]
            .map(|symbol| (symbol, count(symbol)))
    );
    // Taken, not copied: see `omni_platform::net::record`. On a run that is being killed this is
    // the last chance to say it, and saying it twice would leave the guest's bytes in a global.
    let transcripts = omni_platform::net::record::take();
    if transcripts.is_empty() {
        let _ = writeln!(
            out,
            "SOCKET RECORD ({when}): nothing recorded. Set OMNI_NET_RECORD=<bytes> to capture 
                 the first N bytes each socket carried in each direction."
        );
    } else {
        let _ = writeln!(out, "SOCKET RECORD ({when}): {} socket(s)", transcripts.len());
        for transcript in &transcripts {
            let _ = writeln!(out, "  {}", transcript.outline());
        }
    }
    let _ = out.flush();
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

/// Just enough of a `.dex` to read one method's single `const-string`, and nothing more.
///
/// # Why a parser rather than a substring search
///
/// The first version of the test below searched the APK's dex files for the application name as a
/// string-pool entry — a `uleb128` length, the MUTF-8 bytes, a `NUL`. It would have passed with
/// `"android"` in place, because `android` is also a string in those files, and `"android"` is the
/// exact value that produced `HTTP 400` and kept the engine from ever taking the window. A check
/// that cannot fail on the defect it is named after is `VERIFICATION.md` entry 12's shape, and it
/// was rewritten rather than kept.
///
/// What is actually true of the right value is narrower and is the only thing worth asserting:
/// **`bh.x0.M` returns it**. That method is two instructions — `const-string v0, <s>` then
/// `return-object v0` — and `bh.x0.W0` passes its result to both
/// `nativeOverrideChannelPlatformName` and `nativeOverrideChannelPlatformName2`, which
/// `jni-surface-lists.txt` Section J already records as the caller of both.
///
/// Everything here is bounds-checked and answers `None` rather than panicking: it is a parser run
/// over a file this project does not produce, and a malformed dex should fail the test by saying
/// it could not read the method, not by unwinding out of it.
mod dex {
    /// A `uleb128` at `at`, and where it ends.
    fn uleb(bytes: &[u8], at: usize) -> Option<(u32, usize)> {
        let mut value = 0u32;
        let mut shift = 0;
        let mut at = at;
        loop {
            let byte = *bytes.get(at)?;
            at += 1;
            value |= u32::from(byte & 0x7f).checked_shl(shift)?;
            if byte & 0x80 == 0 {
                return Some((value, at));
            }
            shift += 7;
            if shift > 28 {
                return None;
            }
        }
    }

    fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
    }

    fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    }

    /// One string out of the string pool, by index.
    fn string(dex: &[u8], index: u32) -> Option<String> {
        let (count, off) = (u32_at(dex, 56)?, u32_at(dex, 60)? as usize);
        if index >= count {
            return None;
        }
        let at = u32_at(dex, off + 4 * index as usize)? as usize;
        // The `uleb128` here is a length in UTF-16 units, which is **not** the byte length; the
        // bytes themselves run to the NUL, so that is what is read.
        let (_utf16_units, start) = uleb(dex, at)?;
        let end = start + dex.get(start..)?.iter().position(|byte| *byte == 0)?;
        String::from_utf8(dex.get(start..end)?.to_vec()).ok()
    }

    /// One type descriptor, by `type_idx`.
    fn type_name(dex: &[u8], index: u32) -> Option<String> {
        let (count, off) = (u32_at(dex, 64)?, u32_at(dex, 68)? as usize);
        if index >= count {
            return None;
        }
        string(dex, u32_at(dex, off + 4 * index as usize)?)
    }

    /// The method name of one `method_id`.
    fn method_name(dex: &[u8], index: u32) -> Option<String> {
        let (count, off) = (u32_at(dex, 88)?, u32_at(dex, 92)? as usize);
        if index >= count {
            return None;
        }
        string(dex, u32_at(dex, off + 8 * index as usize + 4)?)
    }

    /// Step over one `encoded_field` or `encoded_method` list, returning where it ends.
    fn skip_pairs(dex: &[u8], mut at: usize, count: u32, fields: bool) -> Option<usize> {
        for _ in 0..count {
            at = uleb(dex, at)?.1;
            at = uleb(dex, at)?.1;
            if !fields {
                at = uleb(dex, at)?.1;
            }
        }
        Some(at)
    }

    /// The operand of the single `const-string` in `class.method`, if there is exactly one.
    ///
    /// `None` covers every way this can fail to be a question with an answer: no such class, no
    /// such method, an abstract method with no code, a truncated file, or a body that is not the
    /// shape the caller expects. A caller that wants those distinguished should not be using a
    /// helper this small.
    pub fn only_const_string(dex: &[u8], class: &str, method: &str) -> Option<String> {
        if dex.get(..4)? != b"dex\n" {
            return None;
        }
        let (class_count, class_off) = (u32_at(dex, 96)?, u32_at(dex, 100)? as usize);
        for i in 0..class_count as usize {
            let base = class_off + 32 * i;
            if type_name(dex, u32_at(dex, base)?)? != class {
                continue;
            }
            let data = u32_at(dex, base + 24)? as usize;
            if data == 0 {
                return None;
            }
            let (static_fields, at) = uleb(dex, data)?;
            let (instance_fields, at) = uleb(dex, at)?;
            let (direct_methods, at) = uleb(dex, at)?;
            let (virtual_methods, at) = uleb(dex, at)?;
            let at = skip_pairs(dex, at, static_fields, true)?;
            let mut at = skip_pairs(dex, at, instance_fields, true)?;
            for count in [direct_methods, virtual_methods] {
                // The index is a **running delta** inside each list and restarts at zero between
                // them, which is the one thing in this format that silently yields a plausible
                // wrong answer if it is got wrong: a method name from the wrong entry.
                let mut index = 0u32;
                for _ in 0..count {
                    let (delta, next) = uleb(dex, at)?;
                    let (_access, next) = uleb(dex, next)?;
                    let (code, next) = uleb(dex, next)?;
                    at = next;
                    index = index.checked_add(delta)?;
                    if code == 0 || method_name(dex, index)? != method {
                        continue;
                    }
                    return single_const_string(dex, code as usize);
                }
            }
            return None;
        }
        None
    }

    /// The one `const-string` in a `code_item`'s instruction stream.
    ///
    /// **Exactly one, or nothing.** A method with two of them is not the one-line accessor this is
    /// written for, and answering with the first would be a guess.
    fn single_const_string(dex: &[u8], code: usize) -> Option<String> {
        let units = u32_at(dex, code + 12)? as usize;
        let insns = dex.get(code + 16..code + 16 + 2 * units)?;
        let mut found: Option<String> = None;
        let mut at = 0usize;
        while at + 2 <= insns.len() {
            // Only the two opcodes this needs are decoded; everything else is **stepped over by
            // its format width**, which is what makes this a walk rather than a scan for a byte
            // that happens to be 0x1a. A scan would find the string index of an unrelated
            // instruction's operand and report a string that is in the file but not in the method.
            let opcode = insns[at];
            let width = match opcode {
                0x00 => match insns.get(at + 1) {
                    // The three payload pseudo-instructions carry their own size, and a walk that
                    // guessed 2 bytes for them would resynchronise onto data.
                    Some(1) => 2 * (u16_at(insns, at + 2)? as usize) + 4,
                    Some(2) => 4 * (u16_at(insns, at + 2)? as usize) + 8,
                    Some(3) => {
                        let element = u16_at(insns, at + 2)? as usize;
                        let count = u32_at(insns, at + 4)? as usize;
                        let bytes = element * count;
                        8 + bytes + (bytes & 1)
                    }
                    _ => 2,
                },
                0x1a => {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(string(dex, u32::from(u16_at(insns, at + 2)?))?);
                    4
                }
                0x1b => {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(string(dex, u32_at(insns, at + 2)?)?);
                    6
                }
                // `const-wide`, format 51l: five 16-bit units.
                0x18 => 10,
                // `invoke-polymorphic` and its range form, 45cc / 4rcc: four units.
                0xfa | 0xfb => 8,
                // Three-unit formats: 32x, 31i, 31c, 31t, 35c, 3rc.
                0x03 | 0x06 | 0x09 | 0x14 | 0x17 | 0x24 | 0x25 | 0x26 | 0x2a | 0x2b | 0x2c
                | 0x6e..=0x72 | 0x74..=0x78 | 0xfc | 0xfd => 6,
                // Two-unit formats: 22x, 21s, 21h, 21c, 22c, 20t, 23x, 22t, 21t, 22s, 22b.
                0x02 | 0x05 | 0x08 | 0x13 | 0x15 | 0x16 | 0x19 | 0x1c | 0x1f | 0x20 | 0x22
                | 0x23 | 0x29 | 0x2d..=0x3d | 0x44..=0x6d | 0x90..=0xaf | 0xd0..=0xe2 | 0xfe
                | 0xff => 4,
                // Everything else is one unit: 10x, 12x, 11x, 11n, 10t, and the unused opcodes,
                // which cannot appear in a verified dex.
                _ => 2,
            };
            at += width;
        }
        found
    }
}

/// The application name the script sends is the one **`bh.x0.M` returns**, read out of the APK.
///
/// # What this catches, and why a comment could not
///
/// The value used to be `"android"`. The engine puts it straight into the path of
/// `/v2/settings/application/{}` and the server answered
/// `{"errors":[{"code":1,"message":"The application name is invalid."}]}` with `HTTP 400` —
/// MEASURED against the real endpoint — and that is the failure that kept
/// `nativeActivity_onSurfaceChanged` returning `Flags-Not-Received`. Nothing offline could tell
/// that string from a right one: it is well-formed, it is plausible, and every test in this suite
/// passed with it in place.
///
/// So the assertion is the narrow, checkable thing: the constant is what the APK's own Java side
/// hands to `nativeOverrideChannelPlatformName`. If a future APK renames its distribution, this
/// fails and names the new value rather than silently asking a CDN for a channel that does not
/// exist.
///
/// It needs neither the engine, a CPU nor a network, so it is cheap enough to run while the thing
/// it guards is being changed — the same argument
/// `the_activity_class_answers_every_member_row_23_looks_up_on_it` makes for itself.
#[test]
fn the_application_name_the_script_sends_is_the_one_the_apk_hands_the_engine() {
    let apk = omni_apk::Apk::open(apk_path()).expect("the real APK");
    let dexes: Vec<String> = apk
        .entries()
        .iter()
        .map(|entry| entry.name().to_string())
        .filter(|name| name.ends_with(".dex"))
        .collect();
    assert!(!dexes.is_empty(), "the APK has no dex at all, so this test is measuring nothing");
    // `bh.x0.M` is the application name and `bh.x0.d1` is the version string, and both are
    // one-instruction accessors in the same class. The version is checked alongside because it
    // costs one more call and because `APP_VERSION`'s own doc says three drifted duplicates of
    // that figure have already appeared in this project — a constant taken from a file *name* is
    // exactly the kind that drifts.
    let read = |method: &str| -> Option<(String, String)> {
        dexes.iter().find_map(|name| {
            let bytes = apk.read_named(name).expect("read a dex out of the APK");
            dex::only_const_string(&bytes, "Lbh/x0;", method).map(|value| (name.clone(), value))
        })
    };

    let (dex_name, value) = read("M").unwrap_or_else(|| {
        panic!(
            "`bh.x0.M` was not readable in any of the APK's {} dex files. That method is what \
             supplies the application name to nativeOverrideChannelPlatformName; if it has moved \
             or changed shape, the constant has to be re-read rather than carried over",
            dexes.len()
        )
    });
    assert_eq!(
        value,
        script::CHANNEL_PLATFORM_NAME,
        "{dex_name} says `bh.x0.M` returns {value:?}, and the script sends {:?}. That string is \
         the path segment of the client-settings request; a value the APK does not name gets \
         `HTTP 400 The application name is invalid.` and the engine never receives its flags",
        script::CHANNEL_PLATFORM_NAME
    );

    let (dex_name, version) = read("d1").unwrap_or_else(|| {
        panic!("`bh.x0.d1` was not readable in any of the APK's {} dex files", dexes.len())
    });
    assert_eq!(
        version,
        script::APP_VERSION,
        "{dex_name} says `bh.x0.d1` returns {version:?} and the script tells the engine it is \
         {:?}. That value reaches nativeSetRobloxVersion and the platform headers, and the APK's \
         own Java side is the thing that knows it",
        script::APP_VERSION
    );
}

/// §8 row 23's three lookups all resolve from the **one** `jclass` the engine derives from the
/// activity it was handed, and each resolves at the class that really declares it.
///
/// # The failure this would have caught
///
/// `NativeEngine::initializing` does `GetObjectClass(activity->javaGameActivity)` **once** and
/// then asks that single `jclass` for members declared at three levels of the Java hierarchy.
/// MEASURED from `libroblox.so`, n = 1 run of the gate plus an instruction-level decode:
///
/// * `0x02bdaca0` → `GetObjectClass` at `0x02bdace8`, `GetMethodID("getResources",
///   "()Landroid/content/res/Resources;")` at `0x02bdad0c`, then `bl 0x2bd8c30` at `0x02bdad1c`
///   — the variadic wrapper whose `CallObjectMethodV` (slot `0x118`) is at `0x02bd8cac`.
/// * `0x02bd8b20` → the same three steps for `getNativeHelper`
///   `()Lcom/roblox/client/startup/NativeHelper;` at `0x02bd8b80`, call at `0x02bd8b90`.
///
/// **Neither site tests the id.** With the wrong activity class, or with no superclass chain,
/// `GetMethodID` answers null and the null reaches `CallObjectMethodV` — `jni-surface.md` §8.1's
/// third failure mode, and it killed the game thread in teardown while the gate itself passed.
///
/// Membership and declaring class, not a count: a count could not see `getResources` being
/// answered by the wrong class.
///
/// Separate from the gate and needing neither the APK nor a run, so that it is cheap enough to be
/// run while the thing it guards is being changed.
#[test]
fn the_activity_class_answers_every_member_row_23_looks_up_on_it() {
    use omni_android::jni::classes::Registry;
    let registry = Registry::with_declared();
    let activity = registry
        .find(ACTIVITY_CLASS)
        .unwrap_or_else(|| panic!("{ACTIVITY_CLASS} must be declared: it is what step 13 is called on"));
    // (member, descriptor, the class that must declare it)
    let required: &[(&str, &str, &str)] = &[
        (
            "getNativeHelper",
            "()Lcom/roblox/client/startup/NativeHelper;",
            "com/roblox/client/startup/MainGameActivity",
        ),
        ("getResources", "()Landroid/content/res/Resources;", "android/content/Context"),
        ("finish", "()V", "com/google/androidgamesdk/GameActivity"),
    ];
    for (member, descriptor, declared_by) in required {
        let id = registry.method(activity, member, descriptor, false).unwrap_or_else(|| {
            panic!(
                "`GetMethodID(GetObjectClass(thiz), \"{member}\", \"{descriptor}\")` answers null, \
                 and the engine does not check it"
            )
        });
        assert_eq!(
            registry.class_name(id.class),
            *declared_by,
            "`{member}{descriptor}` must resolve at the class that declares it"
        );
    }
    // **The chain is directional**, which is the half a "declare it on both" fix would get wrong:
    // a `Context` is not a `MainGameActivity` and must not answer its members.
    let context = registry.find("android/content/Context").expect("declared");
    assert!(
        registry
            .method(context, "getNativeHelper", "()Lcom/roblox/client/startup/NativeHelper;", false)
            .is_none(),
        "the superclass must not resolve the subclass's members"
    );
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
