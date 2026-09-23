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

use omni_android::aaudio::{AAudio, PlatformOutput};
use omni_android::bionic::{Bionic, GuestProcess, HwcapPolicy, ThreadHost};
use omni_android::jni::classes::Answer;
use omni_android::jni::input::{TouchInput, PASS_INPUT_SYMBOL, STATE_MOVED};
use omni_android::jni::keys::{declare_hardware_keyboard, KeyInput};
use omni_android::jni::mouse::{MouseCall, MouseInput};
use omni_android::jni::text::TextInput;
use omni_android::jni::webview::{
    user_agent, BrowserEvent, BrowserHost, BrowserRequest, BrowserWindow, UserAgentFacts, WebViewProtocol,
};
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
use omni_mem::{
    Backing, CommitPolicy, GuestSpace, GuestSpaceConfig, MapExecutability, Placement, Protection,
};
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

/// The guest address space the gate reserves: 16 GiB. A reservation costs no commit charge
/// whatever its size (D10), so this is not the number to economize on.
///
/// **It was `omni_mem`'s default 4 GiB, and a game outgrew it.** MEASURED (2026-09-23), the first
/// joins to load a world (Pet Simulator 99): the engine reported 3.3-3.75 GB in use, mimalloc's
/// 1 GiB region requests (`0x40010000` bytes) failed with ENOMEM again and again -- no 1 GiB run
/// was left in a 4 GiB space -- and it fell back to 64 KiB pieces; its memory manager kept raising
/// low-memory warnings and unloaded the Lua app. A device process has a 39- or 48-bit address
/// space, so 4 GiB was this runtime's limit and not the guest's.
const GUEST_SPACE_BYTES: usize = 16 << 30;

/// The commit ceiling on that space: 8 GiB. `omni_mem::DEFAULT_MAX_COMMITTED`'s own doc says to
/// scale the ceiling with the space rather than inherit the 3.5 GiB default; this is twice the
/// ~3.75 GB a loaded world was measured using, the RAM of a common phone, and a quarter of this
/// host's 31.8 GB. The per-request ceiling -- the one that refuses a tampered `p_memsz` -- keeps
/// its default.
const GUEST_MAX_COMMITTED: usize = 8 << 30;

/// What the gate tells the guest its memory is -- `MemTotal`, `sysinfo.totalram`,
/// `_SC_PHYS_PAGES`: **the commit ceiling this runtime enforces on the guest's space**
/// ([`GUEST_MAX_COMMITTED`], 8 GiB inside the [`GUEST_SPACE_BYTES`] space). That is the memory the
/// guest can actually have, which is what `MemTotal` means on a device; a larger figure would
/// promise memory the ceiling refuses.
///
/// It was 2 GiB, chosen as "an ordinary application heap limit" -- a per-app limit, which is not
/// what `MemTotal` is. MEASURED why it matters: the engine sizes its device tier from it (its
/// memory profile recorded `TotalOsMem` 2147483648 and a 16-48 MB texture-streaming budget), and
/// raised its own low-memory warning 28 s into a landing-screen session.
const GUEST_MEMORY_BUDGET: u64 = GUEST_MAX_COMMITTED as u64;

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

/// How long the session after the rows runs: [`POST_ROWS_SETTLE`], or `OMNI_SESSION_SECONDS`.
///
/// The settle was sized for "did the engine act on the rows"; whether frames **continue**, and
/// whether the window survives a resize, are questions about minutes, so a run can ask for them.
fn session_length() -> std::time::Duration {
    match std::env::var("OMNI_SESSION_SECONDS") {
        Ok(text) => std::time::Duration::from_secs(text.trim().parse().unwrap_or_else(|_| {
            panic!("OMNI_SESSION_SECONDS={text:?} is not a whole number of seconds")
        })),
        Err(_) => POST_ROWS_SETTLE,
    }
}

/// How often the session prints its frame count.
const FRAMES_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// **The JIT code cache each guest thread gets: 32 MiB**, not `DynarmicOptions`' 8 MiB.
///
/// dynarmic evacuates a thread's **whole** cache whenever less than 1 MiB of it is free
/// (`a64_interface.cpp`, `GetBlock`), and everything that thread runs is then translated again.
/// At 8 MiB the engine's TaskScheduler workers -- which each run every kind of job -- never fit:
/// MEASURED on the landing screen with a drag every second (under a second client's load),
/// 284,000-297,000 guest instructions re-translated per second, almost all of it by guest
/// threads 2, 3, 16, 17 and 18, and 12.9-13.8 frames a second. At 32 MiB: 2,600-3,800 a second
/// and 59.6 frames a second, the same as 64 and 128 MiB; less process CPU, too (1.6-1.7 cores
/// against 2.2-2.3).
///
/// **What it costs**: dynarmic commits a cache as it fills (`EnsureMemoryCommitted`, 1 MiB at a
/// time), so a thread pays for the code it runs, up to this. MEASURED peak private bytes 3.2-3.3
/// GiB against 2.25 GiB at 8 MiB, and 3.76 GiB at 64 or 128 MiB. It is host memory, outside the
/// guest's address space and its commit ceiling. A game runs more code than the landing screen,
/// and no game has been measured.
const CODE_CACHE_BYTES: u64 = 32 << 20;

/// **Where each guest thread's time goes**, from the boundary's own per-thread records: every
/// `PROFILE_EVERY` a thread is either in guest code (`crossings == exits`) or inside the handler
/// its last crossing named. Sampled on its own host thread until `stop`, then summarised.
///
/// "In guest code" is translated code running **and** the JIT translating it, and the demand
/// pager resolving its faults -- the boundary cannot tell those apart -- while every blocking wait
/// is inside a handler (`futex`, `pthread_cond_wait`, ...), so a thread pinned at a core that
/// samples in guest code is CPU-bound in the guest or the translator, not waiting.
fn sample_profile(boundary: &Boundary, stop: &std::sync::atomic::AtomicBool) -> String {
    use std::collections::BTreeMap;
    struct Seen {
        samples: u64,
        in_guest: u64,
        handlers: BTreeMap<String, u64>,
        first_crossings: u64,
        last_crossings: u64,
    }
    let started = std::time::Instant::now();
    let mut seen: BTreeMap<u64, Seen> = BTreeMap::new();
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        for report in boundary.threads() {
            let entry = seen.entry(report.guest_thread).or_insert(Seen {
                samples: 0,
                in_guest: 0,
                handlers: BTreeMap::new(),
                first_crossings: report.crossings,
                last_crossings: report.crossings,
            });
            entry.samples += 1;
            entry.last_crossings = report.crossings;
            if report.crossings == report.exits {
                entry.in_guest += 1;
            } else {
                let symbol = report.symbol.unwrap_or_else(|| "?".to_string());
                *entry.handlers.entry(symbol).or_insert(0) += 1;
            }
        }
        std::thread::sleep(PROFILE_EVERY);
    }
    let seconds = started.elapsed().as_secs_f64().max(0.001);
    let mut threads: Vec<(u64, Seen)> = seen.into_iter().collect();
    // Busiest first: the most samples not parked in a handler that blocks is not knowable here,
    // so order by crossings per second, the one rate every thread has.
    threads.sort_by(|a, b| {
        (b.1.last_crossings - b.1.first_crossings).cmp(&(a.1.last_crossings - a.1.first_crossings))
    });
    let mut out = format!("PROFILE: {seconds:.1}s sampled every {PROFILE_EVERY:?}\n");
    for (thread, seen) in threads {
        let percent = |n: u64| 100.0 * n as f64 / seen.samples.max(1) as f64;
        let mut handlers: Vec<(&String, &u64)> = seen.handlers.iter().collect();
        handlers.sort_by(|a, b| b.1.cmp(a.1));
        let top: Vec<String> = handlers
            .iter()
            .take(6)
            .map(|(symbol, n)| format!("{symbol} {:.0}%", percent(**n)))
            .collect();
        out.push_str(&format!(
            "  guest thread {thread:#x}: {} samples, {:.0}% in guest code, {:.0} crossings/s; in \
             handlers: {}\n",
            seen.samples,
            percent(seen.in_guest),
            (seen.last_crossings - seen.first_crossings) as f64 / seconds,
            top.join(", ")
        ));
    }
    out
}

/// How often [`sample_profile`] looks.
const PROFILE_EVERY: std::time::Duration = std::time::Duration::from_millis(2);

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
const SDK_VERSION: &str = script::ANDROID_SDK_INT;

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

/// Where the package's own APK is, as the guest sees it: a device's `base.apk`.
const GUEST_APK: &str = "/data/app/com.roblox.client/base.apk";

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

    /// The entry's own compression method and place, out of the real APK's central directory.
    /// A stored entry lives in the package at [`GUEST_APK`], where the gate links the real APK.
    fn placement(&self, name: &[u8]) -> Option<omni_android::ndk::AssetPlacement> {
        let name = std::str::from_utf8(name).ok()?;
        let apk = self.apk.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = apk.asset_entry(name)?;
        Some(if entry.is_stored() {
            omni_android::ndk::AssetPlacement::StoredInPackage {
                package: GUEST_APK.to_string(),
                offset: entry.payload_offset(),
                length: entry.uncompressed_size(),
            }
        } else {
            omni_android::ndk::AssetPlacement::NotInAnyFile
        })
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
    /// `libaaudio.so` over the host's default output, bound with the window for the same reason.
    audio: Option<Arc<AAudio>>,
    boundary: Arc<Boundary>,
    object: LoadedObject,
    stack_top: GuestAddr,
    process_args: [GuestArg; 3],
    exports: std::collections::BTreeMap<String, GuestAddr>,
    _backing: Arc<Backing>,
    _root: Scratch,
}

/// What this gate decides the device is. Every field is a decision; see `ndk::config`.
/// The display the engine is told about, from two host facts -- the surface's pixels and the
/// host's DPI -- so that every answer describing it (`Configuration`, `DisplayMetrics`,
/// `AConfiguration`) agrees with every other.
///
/// **MEASURED why it has to be one model**: `DisplayMetrics` answered zeros while `Configuration`
/// described a 411x731 dp phone and the surface was a 1280x720 window, and the renderer divided
/// by the zero density: a light-grid texture sized from the infinity came out 0x0 and the engine's
/// own `HardAssert (Invalid texture dimensions 0x0 on Vulkan)` fired.
#[derive(Debug, Clone, Copy)]
struct Display {
    width_px: i32,
    height_px: i32,
    /// The host's DPI for the window: 96 at 100% scaling.
    dpi: u32,
}

impl Display {
    /// **No window, so no host display**: the surface constants at the host's own baseline scale,
    /// 100% -- a decision this gate makes, stated here.
    const HEADLESS: Display = Display { width_px: SURFACE_WIDTH, height_px: SURFACE_HEIGHT, dpi: 96 };

    /// Android's `density`, 1.0 at 160 dpi, is the host's scale factor: Windows' logical inch is 96
    /// pixels at 100% and Android's is 160 at density 1.0, so the same user scale is `dpi / 96`.
    fn density(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    /// `densityDpi`, the same scale in Android's units.
    fn density_dpi(&self) -> i32 {
        (self.dpi * 160 / 96) as i32
    }

    fn width_dp(&self) -> i32 {
        (self.width_px as f32 / self.density()) as i32
    }

    fn height_dp(&self) -> i32 {
        (self.height_px as f32 / self.density()) as i32
    }

    /// Android's screen-size bucket, from the dp extent (`Configuration.screenLayout`'s rule).
    fn screen_size(&self) -> ScreenSize {
        let (long, short) = (self.width_dp().max(self.height_dp()), self.width_dp().min(self.height_dp()));
        if long >= 960 && short >= 720 {
            ScreenSize::ExtraLarge
        } else if long >= 640 && short >= 480 {
            ScreenSize::Large
        } else if long >= 470 && short >= 320 {
            ScreenSize::Normal
        } else {
            ScreenSize::Small
        }
    }
}

fn device_configuration(display: &Display) -> DeviceConfiguration {
    DeviceConfiguration {
        language: *b"en",
        country: *b"US",
        screen_width_dp: display.width_dp(),
        screen_height_dp: display.height_dp(),
        screen_size: display.screen_size(),
        nav_hidden: ACONFIGURATION_NAVHIDDEN_NO,
    }
}

impl Guest {
    /// `graphics` is the host driver to bind Vulkan to, when [`GRAPHICS_GATE`] asked for one.
    /// `None` binds no Vulkan at all -- not an unhosted one, which would hand the engine a loader
    /// whose every call refuses -- so the default gate's `dlopen("libvulkan.so")` stays NULL.
    fn load(graphics: Option<Arc<dyn VulkanHost>>, display: Display) -> Self {
        let path = cached_main_lib();
        let bytes = main_lib_bytes();
        let backing =
            Backing::open(path, MapExecutability::Executable).expect("open the cache entry");
        let elf = ElfImage::parse(bytes).expect("parse libroblox.so");
        let space = Arc::new(
            GuestSpace::with_config(GuestSpaceConfig {
                size: GUEST_SPACE_BYTES,
                max_committed: GUEST_MAX_COMMITTED,
                ..GuestSpaceConfig::default()
            })
            .expect("reserve a guest address space"),
        );

        // **As many CPUs as bionic has thread blocks.** The backend's default is 32; MEASURED, once
        // the Lua app was starting, the engine's 33rd concurrent thread failed to get a TLS block
        // and a `boost::thread_resource_error` took down the thread that asked (RBXCRASH), while
        // bionic's arena had room for 64. Ids are recycled on exit, so this is concurrency.
        // The code cache: [`CODE_CACHE_BYTES`] per guest thread, or `OMNI_JIT_CACHE_MB` (MiB) to
        // measure another size -- said in the log when it is set.
        let code_cache_size = match std::env::var("OMNI_JIT_CACHE_MB") {
            Ok(mb) => {
                let mb: u64 = mb.trim().parse().expect("OMNI_JIT_CACHE_MB is a number of MiB");
                let _ = writeln!(
                    std::io::stderr(),
                    "JIT: a {mb} MiB code cache per guest thread (OMNI_JIT_CACHE_MB)"
                );
                mb << 20
            }
            Err(_) => CODE_CACHE_BYTES,
        };
        let options = DynarmicOptions {
            max_threads: u32::try_from(omni_android::bionic::MAX_GUEST_THREADS)
                .expect("the thread count fits"),
            code_cache_size,
            ..DynarmicOptions::default()
        };
        let backend = Arc::new(
            DynarmicBackend::new(Arc::clone(&space), options).expect("a translating backend"),
        );
        assert!(backend.owns_guest_paging(), "this guest has no demand pager");
        assert!(backend.slice_invariant_armed(), "M2's per-slice callback invariant is not armed");

        let bionic = Bionic::new(Arc::clone(&space)).expect("a bionic instance");
        let ndk = Ndk::new(Arc::clone(&space)).expect("an NDK instance");
        // Room for every import, the 241 JNI slots and the NDK surface -- and, with graphics, the
        // Vulkan loader's pool and the data area its handle registries need (8192, not 4096:
        // `vulkan::REQUIRED_DATA_BYTES` records why no smaller arrangement exists).
        //
        // **And `libaaudio.so`, with the window**: a session a person sits at has the host's audio
        // output behind FMOD's AAudio output (`omni_android::aaudio`), and a session without a
        // window has none -- FMOD's NOSOUND fallback, as before.
        let with_audio = graphics.is_some();
        let (vulkan_slots, data_bytes) = if graphics.is_some() {
            (
                omni_android::vulkan::BOUND_SYMBOLS + omni_android::aaudio::BOUND_SYMBOLS,
                omni_android::vulkan::REQUIRED_DATA_BYTES + omni_android::aaudio::REQUIRED_DATA_BYTES,
            )
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
        let audio = with_audio.then(|| {
            let audio = AAudio::new(Arc::new(PlatformOutput));
            audio.bind_into(&builder).expect("bind libaaudio.so");
            audio
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
        define_host_answers(&jni, &display);

        let root = Scratch::new("m5-gate");
        bionic.set_filesystem_root(&root.0).expect("a filesystem root");
        // **How earlier runs ended**, as the system hands `getHistoricalProcessExitReasons` on a
        // device: what this host recorded in a kept root (`record_exit`), and nothing for a fresh
        // one -- a fresh install's answer.
        let exits = read_exit_records(&root.0);
        let _ = writeln!(std::io::stderr(), "EXITS: {} earlier run(s) recorded as ended", exits.len());
        jni.set_previous_exits(exits);
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
        // §5.2 step 2. The host has to *set* it or the SDK version field is empty. **Every
        // property this gate decides is set from one list**, which `android.os.Build`'s fields
        // are then derived from by `Build.java`'s own rule (`define_build_fields`).
        for (name, value) in device_properties() {
            bionic
                .set_system_property(name, &value)
                .expect("a system property is a decision this gate makes");
        }
        // **A created guest thread carries all three instances, not just bionic.**
        // MEASURED by an earlier run of this gate: without the NDK instance the game thread
        // `GameActivity_onCreate` spawns died on its first `AConfiguration_new`, never set
        // `app->running`, and the calling thread waited on its condition variable for ever --
        // §8 row 14 and §8.1's fifth failure mode at once. The watchdog and `Bionic::parked()`
        // named the parked thread, its condvar and its mutex, which is the only reason it took
        // three minutes to find.
        let host: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&backend) as _;
        let mut thread_host = ThreadHost::new(host)
            .with_instance(jni.thread_instance())
            .with_instance(ndk.thread_instance());
        // **And Vulkan, when it is bound.** MEASURED, the first run in which the engine chose
        // Vulkan: its render thread's `vkGetInstanceProcAddr` was refused -- "no Vulkan loader
        // instance is published to this thread" -- because the engine creates its device on a
        // guest thread it spawned, not on the one the gate calls from.
        if let Some(vulkan) = &vulkan {
            thread_host = thread_host.with_instance(vulkan.thread_instance());
        }
        // **And AAudio**: FMOD opens its output on the engine's game thread, and the data callback
        // runs on a thread the library itself starts.
        if let Some(audio) = &audio {
            thread_host = thread_host.with_instance(audio.thread_instance());
        }
        bionic.set_thread_host(thread_host).expect("a thread host");

        ndk.set_asset_source(Arc::new(ApkAssets::open())).expect("the real APK's assets");
        ndk.set_configuration(device_configuration(&display));

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
            audio,
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
/// **The system properties this gate decides**, in one list: what `__system_property_get` answers
/// and what `android.os.Build`'s fields are derived from.
///
/// * `ro.build.version.sdk`: [`SDK_VERSION`], §5.2 step 2.
/// * `ro.product.manufacturer`: **this host's maker**, as its firmware reports it -- on a device it
///   is the maker of the hardware the OS runs on, and here that hardware is this machine.
/// * `ro.product.cpu.abilist64` (and `abilist`): `arm64-v8a`, the one ABI this runtime executes --
///   the APK's own `lib/arm64-v8a`.
///
/// Every other `ro.*` property is **unset**, which `__system_property_get` answers as empty and
/// `Build.java` as `"unknown"`: a device whose build left it unset says the same.
fn device_properties() -> Vec<(&'static str, String)> {
    let maker = omni_platform::process::host_manufacturer()
        .unwrap_or_else(|error| panic!("ro.product.manufacturer needs the host's maker: {error}"));
    vec![
        ("ro.build.version.sdk", SDK_VERSION.to_string()),
        ("ro.product.manufacturer", maker),
        ("ro.product.cpu.abilist", "arm64-v8a".to_string()),
        ("ro.product.cpu.abilist64", "arm64-v8a".to_string()),
    ]
}

/// `android.os.Build`'s string fields, derived from [`device_properties`] by Android 13's
/// `Build.java`: each is `SystemProperties.get(<its property>, "unknown")`; `SERIAL` is
/// `"unknown"` for every app since O; `CPU_ABI`/`CPU_ABI2` are the first two of
/// `ro.product.cpu.abilist64` (`""` when there is no second); and `FINGERPRINT` is
/// `ro.build.fingerprint`, or when unset, `deriveFingerprint()`'s composition of the others.
fn define_build_fields(jni: &Jni) {
    let properties: std::collections::BTreeMap<&str, String> = device_properties().into_iter().collect();
    let get = |name: &str| properties.get(name).cloned().unwrap_or_else(|| "unknown".to_string());
    let abis: Vec<String> = properties
        .get("ro.product.cpu.abilist64")
        .map(|list| list.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let fingerprint = properties.get("ro.build.fingerprint").cloned().unwrap_or_else(|| {
        format!(
            "{}/{}/{}:{}/{}/{}:{}/{}",
            get("ro.product.brand"),
            get("ro.product.name"),
            get("ro.product.device"),
            get("ro.build.version.release"),
            get("ro.build.id"),
            get("ro.build.version.incremental"),
            get("ro.build.type"),
            get("ro.build.tags")
        )
    });
    let fields: Vec<(&str, String)> = vec![
        ("ID", get("ro.build.id")),
        ("DISPLAY", get("ro.build.display.id")),
        ("PRODUCT", get("ro.product.name")),
        ("DEVICE", get("ro.product.device")),
        ("BOARD", get("ro.product.board")),
        ("MANUFACTURER", get("ro.product.manufacturer")),
        ("BRAND", get("ro.product.brand")),
        ("MODEL", get("ro.product.model")),
        ("BOOTLOADER", get("ro.bootloader")),
        ("HARDWARE", get("ro.hardware")),
        ("SKU", get("ro.boot.hardware.sku")),
        ("ODM_SKU", get("ro.boot.product.hardware.sku")),
        ("SOC_MANUFACTURER", get("ro.soc.manufacturer")),
        ("SOC_MODEL", get("ro.soc.model")),
        ("TYPE", get("ro.build.type")),
        ("TAGS", get("ro.build.tags")),
        ("USER", get("ro.build.user")),
        ("HOST", get("ro.build.host")),
        ("RADIO", get("gsm.version.baseband")),
        ("SERIAL", "unknown".to_string()),
        ("CPU_ABI", abis.first().cloned().unwrap_or_default()),
        ("CPU_ABI2", abis.get(1).cloned().unwrap_or_default()),
        ("FINGERPRINT", fingerprint),
    ];
    for (field, value) in fields {
        let value: &'static str = Box::leak(value.into_boxed_str());
        jni.define_field("android/os/Build", field, "Ljava/lang/String;", true, Answer::Text(value))
            .unwrap_or_else(|error| panic!("Build.{field} is declared: {error}"));
    }
}

fn define_host_answers(jni: &Jni, display: &Display) {
    define_build_fields(jni);
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

    // **`NetworkUtils.getPublicIPv4Addresseses()`: this host's own addresses**, through the Java
    // body `omni_android::jni::classes::public_ipv4_addresses` transcribes. MEASURED why
    // (2026-09-23): the engine's MicroProfiler asked for them in a game world, the worker that
    // asked died on the refusal, and the game froze. Measured once, here; a device answers at
    // call time, so an address that changes during a session (a VPN reconnecting) is not seen.
    let addresses = omni_platform::net::interface_addresses()
        .unwrap_or_else(|error| panic!("the host's interface addresses: {error}"));
    let ipv4: &'static str = Box::leak(
        omni_android::jni::classes::public_ipv4_addresses(&addresses).into_boxed_str(),
    );
    jni.define_method(
        omni_android::jni::classes::NETWORK_UTILS,
        omni_android::jni::classes::GET_PUBLIC_IPV4_ADDRESSES,
        "()Ljava/lang/String;",
        true,
        Answer::Text(ipv4),
    )
    .expect("NetworkUtils.getPublicIPv4Addresseses is declared");

    // **The Java `Configuration` and the native `AConfiguration` answer the same question**, and a
    // host that decided one and left the other at its declared default would have the engine
    // reading two different screen widths from two places. Both are this gate's decision.
    let decided = device_configuration(display);
    for (field, value) in [
        ("screenWidthDp", decided.screen_width_dp),
        ("screenHeightDp", decided.screen_height_dp),
        ("smallestScreenWidthDp", decided.screen_width_dp.min(decided.screen_height_dp)),
        ("densityDpi", display.density_dpi()),
    ] {
        jni.define_field("android/content/res/Configuration", field, "I", false, Answer::Int(value))
            .unwrap_or_else(|error| panic!("`Configuration.{field}` is declared: {error}"));
    }
    // **`DisplayMetrics`, from the same display.** `xdpi`/`ydpi` are physical on a device; the host
    // reports only its logical DPI, so they carry the same logical figure `densityDpi` does.
    for (field, answer) in [
        ("density", Answer::Float(display.density())),
        ("xdpi", Answer::Float(display.density_dpi() as f32)),
        ("ydpi", Answer::Float(display.density_dpi() as f32)),
    ] {
        jni.define_field("android/util/DisplayMetrics", field, "F", false, answer)
            .unwrap_or_else(|error| panic!("`DisplayMetrics.{field}` is declared: {error}"));
    }
    for (field, value) in [("widthPixels", display.width_px), ("heightPixels", display.height_px)] {
        jni.define_field("android/util/DisplayMetrics", field, "I", false, Answer::Int(value))
            .unwrap_or_else(|error| panic!("`DisplayMetrics.{field}` is declared: {error}"));
    }
}

/// **The facts the app's web view user agent is built from**, from the same decisions the engine
/// is given: the memory [`GUEST_MEMORY_BUDGET`] (`MemTotal`), the display `DisplayMetrics` answers
/// (its pixels stand for `Display.getSize` too: one figure, the app's area), its DPI (`xdpi`/`ydpi`
/// carry `densityDpi`, see [`define_host_answers`]), `Build.MANUFACTURER`/`MODEL`/
/// `VERSION.RELEASE` by `Build.java`'s rule over [`device_properties`] (`"unknown"` when unset),
/// and the phone `InitParams.isTablet` answers.
fn user_agent_facts(display: &Display) -> UserAgentFacts {
    let properties = device_properties();
    let property = |name: &str| {
        properties
            .iter()
            .find(|(key, _)| *key == name)
            .map_or_else(|| "unknown".to_string(), |(_, value)| value.clone())
    };
    let density = display.density();
    UserAgentFacts {
        total_memory_mb: (GUEST_MEMORY_BUDGET / (1024 * 1024)) as i32,
        display_size: (display.width_px, display.height_px),
        dpi: (display.density_dpi(), display.density_dpi()),
        display_dp: ((display.width_px as f32 / density) as i32, (display.height_px as f32 / density) as i32),
        manufacturer: property("ro.product.manufacturer"),
        model: property("ro.product.model"),
        release: property("ro.build.version.release"),
        tablet: false,
        chrome_os: false,
        tv: false,
    }
}

/// **The host's browser for the Java side's web view**: a WebView2 window per page, the size of
/// the app's window -- the fragment `jk.a0.g` puts up fills the activity's container.
struct HostBrowser {
    size: (u32, u32),
}

impl BrowserHost for HostBrowser {
    fn open(&mut self, request: &BrowserRequest) -> Result<Box<dyn BrowserWindow>, String> {
        let view = omni_platform::webview::WebView::open(&omni_platform::webview::WebViewOptions {
            title: if request.title.is_empty() { "Roblox".to_string() } else { request.title.clone() },
            url: request.url.clone(),
            width: self.size.0,
            height: self.size.1,
            init_script: Some(request.init_script.clone()),
            user_agent: Some(request.user_agent.clone()),
        })
        .map_err(|error| error.to_string())?;
        Ok(Box::new(HostPage(view)))
    }
}

/// One WebView2 window, as the Java side's web view sees it.
struct HostPage(omni_platform::webview::WebView);

impl BrowserWindow for HostPage {
    fn poll(&mut self) -> Vec<BrowserEvent> {
        use omni_platform::webview::WebViewEvent;
        let mut events = Vec::new();
        for event in self.0.poll_events() {
            match event {
                // Nothing on the Java side acts on these; `onPageStarted` is only logged there.
                WebViewEvent::Ready | WebViewEvent::NavigationStarting { .. } => {}
                WebViewEvent::NavigationCompleted { url, success } => {
                    events.push(BrowserEvent::PageFinished { url, success });
                }
                WebViewEvent::Message(text) => events.push(BrowserEvent::Bridge(text)),
                // The bridge script posts strings only, so this is some other script's post; the
                // Java side has no listener for it. Said, not dropped.
                WebViewEvent::NonStringMessage { json } => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "WEBVIEW: the page posted a non-string ({} chars of JSON); no Java listener takes it",
                        json.chars().count()
                    );
                }
                WebViewEvent::Closed => events.push(BrowserEvent::Closed),
                WebViewEvent::Failed(why) => events.push(BrowserEvent::Failed(why)),
            }
        }
        events
    }

    fn execute_script(&mut self, script: &str) -> Result<(), String> {
        self.0.execute_script(script).map_err(|error| error.to_string())
    }

    fn close(&mut self) {
        self.0.close();
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

/// Where this host keeps how earlier runs of the app ended: `/data/system/`, where a device's
/// system server keeps its own (`procexitstore`), in the guest's root -- so a kept root
/// (`OMNI_DATA_DIR`) carries it to the next run and a scratch one goes with the run.
const EXIT_RECORDS: &str = "data/system/omnidroid-procexitstore";

/// At most this many records, newest first -- the per-package bound a device keeps.
const MAX_EXIT_RECORDS: usize = 16;

/// The records `record_exit` wrote under `root`, newest first; none when there is no file.
fn read_exit_records(root: &std::path::Path) -> Vec<omni_android::jni::ExitRecord> {
    let Ok(text) = std::fs::read_to_string(root.join(EXIT_RECORDS)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let n: Vec<i64> = line
                .split_whitespace()
                .map(|word| word.parse().unwrap_or_else(|_| panic!("{EXIT_RECORDS}: {line:?} is not a record")))
                .collect();
            let [pid, reason, status, timestamp_ms, importance] = n[..] else {
                panic!("{EXIT_RECORDS}: {line:?} is not five numbers");
            };
            omni_android::jni::ExitRecord {
                pid: pid as i32,
                reason: reason as i32,
                status: status as i32,
                timestamp_ms,
                importance: importance as i32,
            }
        })
        .collect()
}

/// Put `exit` first in the records under `root`, keeping [`MAX_EXIT_RECORDS`].
fn record_exit(root: &std::path::Path, exit: omni_android::jni::ExitRecord) {
    let mut records = vec![exit];
    records.extend(read_exit_records(root));
    records.truncate(MAX_EXIT_RECORDS);
    let text: String = records
        .iter()
        .map(|r| format!("{} {} {} {} {}\n", r.pid, r.reason, r.status, r.timestamp_ms, r.importance))
        .collect();
    let path = root.join(EXIT_RECORDS);
    std::fs::create_dir_all(path.parent().expect("a directory")).expect("the records' directory");
    std::fs::write(&path, text).expect("the exit records");
}

/// **The records round-trip newest first and stay bounded**: a record written is read back
/// exactly, a second goes in front of it, and the seventeenth pushes the oldest out.
#[test]
fn exit_records_round_trip_newest_first_and_stay_bounded() {
    let dir = std::env::temp_dir().join(format!("omni-exit-records-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(read_exit_records(&dir).is_empty(), "no file is no record");
    let exit = |pid: i32| omni_android::jni::ExitRecord {
        pid,
        reason: omni_android::jni::ExitRecord::REASON_USER_REQUESTED,
        status: omni_android::jni::ExitRecord::SIGKILL,
        timestamp_ms: 1_790_000_000_000 + i64::from(pid),
        importance: omni_android::jni::ExitRecord::IMPORTANCE_CACHED,
    };
    record_exit(&dir, exit(1));
    assert_eq!(read_exit_records(&dir), vec![exit(1)]);
    record_exit(&dir, exit(2));
    assert_eq!(read_exit_records(&dir), vec![exit(2), exit(1)]);
    for pid in 3..=17 {
        record_exit(&dir, exit(pid));
    }
    let kept = read_exit_records(&dir);
    assert_eq!(kept.len(), MAX_EXIT_RECORDS);
    assert_eq!((kept[0].pid, kept[15].pid), (17, 2), "the oldest went out");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The signal a device's kernel would have ended the process with**, for a guest thread this
/// layer stopped -- what `ApplicationExitInfo.getStatus()` carries for `REASON_CRASH_NATIVE`.
///
/// A fault this layer caught is the `SIGSEGV` the load or store would have raised; an
/// instruction it would not execute is `SIGILL`; everything else is a refusal by this layer, whose
/// nearest device equivalent is bionic's own `abort()` (`__fortify_fatal`, `async_safe_fatal`):
/// `SIGABRT`. Read from the failure's own text, whose first word is the `ExitReason` variant
/// when the thread stopped on one (`GuestThreadState::Failed` carries only that text).
fn death_signal(why: &str) -> i32 {
    if why.starts_with("MemoryFault") {
        omni_android::jni::ExitRecord::SIGSEGV
    } else if why.starts_with("UnsupportedInstruction") {
        omni_android::jni::ExitRecord::SIGILL
    } else {
        omni_android::jni::ExitRecord::SIGABRT
    }
}

/// The three shapes a death's text takes, each from a real run (2026-09-23 p1/p2, 2026-09-24).
#[test]
fn a_death_is_recorded_with_the_signal_a_device_would_have_raised() {
    use omni_android::jni::ExitRecord;
    assert_eq!(death_signal("MemoryFault { pc: 2169978762496, address: 0, access: Read }"), ExitRecord::SIGSEGV);
    assert_eq!(death_signal("UnsupportedInstruction { pc: 2371533628128, encoding: 3556769793 }"), ExitRecord::SIGILL);
    assert_eq!(
        death_signal("the guest called the imported symbol `__vsprintf_chk` through its thunk at 0x20c9aa39eb0, and nothing in the compatibility layer implements it"),
        ExitRecord::SIGABRT
    );
    assert_eq!((ExitRecord::SIGSEGV, ExitRecord::SIGILL, ExitRecord::SIGABRT), (11, 4, 6), "Linux arm64 numbers");
}

/// The guest's root: a scratch directory removed with the run, or -- with `OMNI_DATA_DIR` --
/// a directory that outlives it (the `bool`, which says not to remove it).
struct Scratch(PathBuf, bool);

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
        // The Java side's unpacked-assets tree -- `script::ASSET_DIRECTORIES` has the decoding.
        // The engine sets its extra-content folder only if `ExtraContent` exists.
        "data/data/com.roblox.client/app_assets/ExtraContent",
        "data/data/com.roblox.client/app_assets/android",
        "data/data/com.roblox.client/app_assets/content",
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
    /// Where the engine looks for `ClientAppSettings.json`, measured (the missing-paths list).
    const CLIENT_APP_SETTINGS_DIRECTORY: &'static str =
        "data/data/com.roblox.client/files/exe/ClientSettings";

    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-m5-gate-{tag}-{}", std::process::id()));
        // **OMNI_DATA_DIR=<dir>: the guest's storage outlives the run**, as a device's does -- off by
        // default, where every run is a fresh install. What the app writes there stays, **which
        // includes a signed-in session**: the reason to set it is that a sign-in made by a person
        // in one interactive run (Quick Sign-in needs their own signed-in device) is still there
        // for the next run. Nothing here reads or writes the session; the engine does.
        if let Some(kept) = std::env::var_os("OMNI_DATA_DIR") {
            let at = PathBuf::from(kept);
            std::fs::create_dir_all(&at).expect("the persistent data directory");
            let _ = writeln!(
                std::io::stderr(),
                "DATA DIR: {} is kept between runs (OMNI_DATA_DIR): it holds whatever the app \
                 stores, a signed-in session included",
                at.display()
            );
            return Self::populate(at, true);
        }
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
        Self::populate(at, false)
    }

    /// Lay out what a device's package manager and the app's Java side put in place before the
    /// engine starts: the directories, the APK at its device path, the certificate
    /// authorities -- idempotently, so a kept root is refreshed rather than refused.
    fn populate(at: PathBuf, keep: bool) -> Scratch {
        for directory in Self::DIRECTORIES {
            std::fs::create_dir_all(at.join(directory)).expect("an app directory");
        }
        // **The package's own APK, where a device has it.** A hard link, so it costs nothing:
        // MEASURED, the engine asks `AAsset_openFileDescriptor` for its shader pack, which is
        // STORED in the APK, and a device answers with a descriptor on `base.apk` and the
        // entry's offset -- so the guest must be able to open the real bytes at that path. This
        // was an empty placeholder while only its existence was read.
        let apk_at = at.join(GUEST_APK.trim_start_matches('/'));
        // A kept root already has one, possibly of an older APK: replace it.
        let _ = std::fs::remove_file(&apk_at);
        std::fs::hard_link(apk_path(), &apk_at)
            .expect("the real APK linked into the guest's root at its device path");
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
        // **OMNI_CLIENT_APP_SETTINGS=<json>: a diagnostic, off by default.** The engine opens
        // `ClientAppSettings.json` (MEASURED: in every run's missing-paths list) -- Roblox's own
        // local flag-override file -- and a device without one is the default this gate keeps.
        // Set, the JSON is written there verbatim, for turning on an engine log channel and
        // reading the engine's own account of itself. **Not yet shown to work for a log**: the
        // engine logs the file's contents (`LoadClientSettingsFromLocal`), but no `FLog` channel
        // it named has printed a line -- MEASURED with 12, 1030, "1030" and 65535 (gate115-116).
        // DECODED, the check at a log site (`FLog::ApplicationFrameRate`, `0x61c96a4`): the
        // flag's low byte at least 6 **and** a bit of `0xfc00` set; how a settings value becomes
        // those bits is not decoded.
        if let Some(json) = std::env::var_os("OMNI_CLIENT_APP_SETTINGS") {
            let json = json.into_string().expect("OMNI_CLIENT_APP_SETTINGS is UTF-8 JSON");
            std::fs::create_dir_all(at.join(Self::CLIENT_APP_SETTINGS_DIRECTORY))
                .expect("the client-settings directory");
            std::fs::write(
                at.join(Self::CLIENT_APP_SETTINGS_DIRECTORY).join("ClientAppSettings.json"),
                json.as_bytes(),
            )
            .expect("the client app settings, where the engine looks for them");
            println!("CLIENT APP SETTINGS (OMNI_CLIENT_APP_SETTINGS): {json}");
        }
        Scratch(at, keep)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.1 {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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
    // **The host's timers at 1 ms for the whole session**, as a game on Windows holds them. The
    // guest was written for Linux's high-resolution timers; at Windows' default ~15.6 ms tick
    // every short sleep and timed wait in its frame pipeline overslept by up to a tick (see
    // `omni_platform::clock::TimerResolution`).
    // `OMNI_TIMER_DEFAULT=1` leaves the host's default tick in place: an A/B for this guard.
    let _timers = if std::env::var_os("OMNI_TIMER_DEFAULT").is_some() {
        let _ = writeln!(
            std::io::stderr(),
            "TIMERS: the host's default resolution, not 1 ms (OMNI_TIMER_DEFAULT)"
        );
        None
    } else {
        Some(
            omni_platform::clock::TimerResolution::raise(std::time::Duration::from_millis(1))
                .expect("a 1 ms timer resolution"),
        )
    };
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
    // **The window first, under the graphics gate**, because the display the engine is told about
    // is the window's -- its pixels and its host's DPI -- and the engine reads that before step 13.
    let early_window = graphics.then(|| {
        let opened = omni_platform::window::Window::new(&omni_platform::window::WindowDesc::new(
            "Omnidroid - Roblox",
            SURFACE_WIDTH as u32,
            SURFACE_HEIGHT as u32,
        ))
        .unwrap_or_else(|err| panic!("{GRAPHICS_GATE}=1 and no window could be opened: {err}"));
        opened.show();
        let mut opened = opened;
        let _ = opened.poll_events().count();
        opened
    });
    let display = match &early_window {
        Some(opened) => {
            let (width, height) = opened.client_size().expect("a shown window has a client area");
            Display {
                width_px: width as i32,
                height_px: height as i32,
                dpi: opened.dpi().expect("the host's DPI for the window"),
            }
        }
        None => Display::HEADLESS,
    };
    let _ = writeln!(
        std::io::stderr(),
        "DISPLAY: {}x{} px at {} DPI -> density {}, densityDpi {}, {}x{} dp",
        display.width_px,
        display.height_px,
        display.dpi,
        display.density(),
        display.density_dpi(),
        display.width_dp(),
        display.height_dp()
    );
    let guest = Guest::load(host, display);
    // **OMNI_HARDWARE_KEYBOARD=1: this host's keyboard, told to the engine**, under the graphics
    // gate (the keys are the window's). Declared here, before step 13 reads the configuration, so
    // `Configuration.keyboard` says QWERTY to the engine and to `vk.g` alike -- see
    // `omni_android::jni::keys`. Opt-in because the engine acts on it and the layer's default
    // device is a phone's, which the rest of this gate has been measured against.
    //
    // **OMNI_KEYBOARD_MOUSE=1: this host's keyboard AND mouse** -- the play configuration
    // (`tools/play.ps1`). The keyboard is declared as above, and the window's pointer is a mouse
    // (`omni_android::jni::mouse`: hover, every button, the wheel, pointer capture for a locked
    // mouse) rather than a finger; a host with no touch screen sends nothing to the touch path.
    // Opt-in for this gate, whose measured default -- every stimulus switch included -- is the
    // phone's: touch, and no keyboard.
    let keyboard_mouse = graphics && std::env::var_os("OMNI_KEYBOARD_MOUSE").is_some();
    let hardware_keyboard =
        graphics && (keyboard_mouse || std::env::var_os("OMNI_HARDWARE_KEYBOARD").is_some());
    if hardware_keyboard {
        declare_hardware_keyboard(&guest.jni).expect("Configuration's keyboard fields are declared");
        let _ = writeln!(
            std::io::stderr(),
            "INPUT: a hardware QWERTY keyboard is declared to the engine ({})",
            if keyboard_mouse { "OMNI_KEYBOARD_MOUSE" } else { "OMNI_HARDWARE_KEYBOARD" }
        );
    }
    if keyboard_mouse {
        let _ = writeln!(
            std::io::stderr(),
            "INPUT: the window's pointer is a MOUSE, not a finger (OMNI_KEYBOARD_MOUSE): hover, \
             all buttons, the wheel and pointer capture reach the engine's mouse natives"
        );
    }
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

    // ---- `RobloxApplication.onCreate`'s two native setups -----------------------------------
    //
    // **Decoded from `classes2.dex`, and missing until the Lua app could not find its content.**
    // Once native libraries are loaded, `RobloxApplication.onCreate` -- the app's first code, on
    // both of its branches -- calls `JNIAAssetManagerSetup.a(context)`, which is
    // `initNative(context.getAssets())` ("Initialize Android AssetReader"), and then
    // `LocalStorageManager.a(context)`, which is `initStorageManagerNativeV3(getAssets(),
    // getFilesDir(), getCacheDir())` on the `LocalStorageManager` singleton. The second is what
    // builds the engine's `RBX::AndroidLocalStorageManager`, whose slot `0x38` (`0x0241b0c8`)
    // opens content out of the APK with `AAssetManager_open` under `android/`, `ExtraContent/` and
    // `content/`. MEASURED without them: `[FLog::LocalStorageHandler] Not available on the current
    // platform.`, `Unable to load rbxasset://configs/UniversalAppPatchConfig/...`, no
    // `AAssetManager_open` at all, and `initializeWithAppStarter` returning before it instantiated
    // the controllers -- a null `UserController` read at `SingleSurfaceAppImpl + 0x440`.
    //
    // The directories are the ones the script's step 9 hands the engine; the `AssetManager` is a
    // Java one `AAssetManager_fromJava` maps onto the real APK's assets, as step 13's is.
    //
    // **`LocalStorageManager.getAllocatableBytes()`** is `new StatFs(Environment
    // .getDataDirectory().getPath()).getAvailableBytes()` in the dex -- `f_bavail * f_bsize` of the
    // volume `/data` is on. That is a fact about this host, so the embedding measures it here,
    // through the same `statvfs` the guest's own calls get; MEASURED reader: the engine, once the
    // storage manager existed. A snapshot taken at setup, which is what a JNI answer can hold.
    {
        let volume = guest
            .bionic
            .filesystem()
            .expect("the gate roots a filesystem")
            .statvfs(b"/data")
            .expect("the volume /data is on");
        let available = volume.blocks_available.saturating_mul(volume.block_size);
        guest
            .jni
            .define_method(
                "com/roblox/client/LocalStorageManager",
                "getAllocatableBytes",
                "()J",
                false,
                Answer::Long(i64::try_from(available).unwrap_or(i64::MAX)),
            )
            .expect("getAllocatableBytes is on the measured surface");
    }
    {
        let _bionic = guest.bionic.activate().expect("publish the bionic instance");
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        let _ndk = guest.ndk.activate();
        let asset_manager = guest.jni.new_object(ASSET_MANAGER_CLASS).expect("a Java AssetManager");
        let setup_class = guest
            .jni
            .class_reference("com/roblox/client/JNIAAssetManagerSetup")
            .expect("declared by the script's classes");
        let storage = guest
            .jni
            .new_object("com/roblox/client/LocalStorageManager")
            .expect("the LocalStorageManager singleton");
        let files = guest.jni.new_string("/data/data/com.roblox.client/files").expect("filesDir");
        let cache = guest.jni.new_string("/data/data/com.roblox.client/cache").expect("cacheDir");
        for (symbol, args) in [
            (
                "Java_com_roblox_client_JNIAAssetManagerSetup_initNative",
                vec![
                    GuestArg::Pointer(guest.jni.env_for(0)),
                    GuestArg::Int(setup_class),
                    GuestArg::Int(asset_manager),
                ],
            ),
            (
                "Java_com_roblox_client_LocalStorageManager_initStorageManagerNativeV3",
                vec![
                    GuestArg::Pointer(guest.jni.env_for(0)),
                    GuestArg::Int(storage),
                    GuestArg::Int(asset_manager),
                    GuestArg::Int(files),
                    GuestArg::Int(cache),
                ],
            ),
        ] {
            let target = *guest.exports.get(symbol).expect("exported by libroblox.so");
            let result = guest.boundary.call_guest(&mut cpu, symbol, target, &args, ON_LOAD_BUDGET);
            let _ = writeln!(
                std::io::stderr(),
                "RobloxApplication.onCreate: {symbol} -> {}",
                match &result {
                    Ok(_) => "returned".to_string(),
                    Err(error) => format!("{error}"),
                }
            );
            result.unwrap_or_else(|error| panic!("RobloxApplication.onCreate: {symbol}: {error}"));
        }
    }
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
    //
    // **`OMNI_IMPORT_CENSUS=off`, a measurement switch**, leaves it off for the session instead:
    // the census costs every crossing on every thread a shared-counter increment and two stores to
    // one process-wide pair of words (238-280 ns per crossing with 8 threads against 35-46 off, a
    // benchmark), and whether that matters in a world is what the switch is for. Frames are
    // counted by the Vulkan layer, not the census, so the FRAMES line is unaffected; every census
    // reading after this point -- the watchdogs' per-thread INSIDE lines included -- is frozen, and
    // says so here (`docs/VERIFICATION.md` entry 15).
    if std::env::var("OMNI_IMPORT_CENSUS").is_ok_and(|value| value.trim() == "off") {
        let _ = writeln!(
            std::io::stderr(),
            "CENSUS: OFF for the session (OMNI_IMPORT_CENSUS=off) -- a measurement; per-symbol counts \
             and every thread's last-crossing record stop here"
        );
    } else {
        guest.boundary.start_census();
    }

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
    let (surface_width, surface_height) = if let Some(mut opened) = early_window {
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

    // ---- §8 row 26: the window's pointer, as the Java side's touch listener delivers it ------
    //
    // Only with a window, because the events are the window's. The density is the display's --
    // the figure `DisplayMetrics.density` answers, which is what `vk.e` divides by. The surface
    // starts dead and comes alive when `onSurfaceCreatedNative` returns, as `jk.o0` does; see
    // `omni_android::jni::input`.
    let mut touch: Option<TouchInput> = window.as_ref().map(|_| {
        TouchInput::new(&guest.jni, &|symbol| guest.exports.get(symbol).copied(), display.density())
            .unwrap_or_else(|error| panic!("§8 row 26: the touch seam could not be built: {error}"))
    });
    // And the keys, when a hardware keyboard was declared above.
    // And the text field, `RbxKeyboard`: shown when the engine focuses a `TextBox`, and while it
    // is open it takes the keys -- a device's focused `EditText` does. See `jni::text`.
    let mut text_field: Option<TextInput> = window.as_ref().map(|_| {
        TextInput::new(&guest.jni, &|symbol| guest.exports.get(symbol).copied())
            .unwrap_or_else(|error| panic!("the text seam could not be built: {error}"))
    });
    let mut keyboard: Option<KeyInput> = hardware_keyboard.then(|| {
        KeyInput::new(&guest.jni, &|symbol| guest.exports.get(symbol).copied())
            .unwrap_or_else(|error| panic!("the key seam could not be built: {error}"))
    });
    // And the mouse, in the keyboard-and-mouse configuration: then the window's pointer events go
    // here and not to `touch`, which stays only as the record of whether the surface is alive.
    let mut mouse: Option<MouseInput> = (keyboard_mouse && window.is_some()).then(|| {
        MouseInput::new(&guest.jni, &|symbol| guest.exports.get(symbol).copied(), display.density())
            .unwrap_or_else(|error| panic!("the mouse seam could not be built: {error}"))
    });
    let mut input_failure: Option<String> = None;
    // **OMNI_INPUT_PROBE=1: one SYNTHETIC press-drag-release**, through the same seam, at the
    // centre of the view -- so a run nobody touches can still show that the engine's own
    // `nativePassInput` is called and returns. Opt-in and said so, because it is a stimulus this
    // gate invents: whatever sits at the centre of the engine's screen receives it.
    let mut input_probe: Vec<omni_platform::window::WindowEvent> =
        match (&touch, std::env::var_os("OMNI_INPUT_PROBE")) {
            (Some(_), Some(_)) => {
                let (x, y) = (surface_width / 2, surface_height / 2);
                let _ = writeln!(
                    std::io::stderr(),
                    "INPUT PROBE: a SYNTHETIC press at ({x}, {y}) px, a drag of 24 px and a \
                     release will be delivered once the surface is alive (OMNI_INPUT_PROBE)"
                );
                vec![
                    omni_platform::window::WindowEvent::PointerDown {
                        button: omni_platform::window::PointerButton::Primary,
                        x,
                        y,
                    },
                    omni_platform::window::WindowEvent::PointerMoved { x: x + 24, y },
                    omni_platform::window::WindowEvent::PointerUp {
                        button: omni_platform::window::PointerButton::Primary,
                        x: x + 24,
                        y,
                    },
                ]
            }
            _ => Vec::new(),
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
            // **The paths the engine asked for and did not get**, here as well as at teardown:
            // a run the watchdog ends never reaches teardown, and an `ENOENT` is the quietest
            // failure this runtime has (see `Filesystem::open_misses`).
            if let Some(fs) = bionic.filesystem() {
                let misses: Vec<String> = fs
                    .open_misses()
                    .iter()
                    .map(|path| String::from_utf8_lossy(path).into_owned())
                    .filter(|path| !path.starts_with("/proc/") && !path.starts_with("/sys/"))
                    .collect();
                let _ = writeln!(out, "MISSING PATHS outside /proc and /sys ({}): {misses:?}", misses.len());
            }
            death_contexts(&mut out, boundary.mem(), image_base, &bionic.guest_thread_failures());
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
        // `MainGameActivity.surfaceCreated` is `super.surfaceCreated` -- this row -- and then
        // `Y.a(!isDestroyed())`, the flag `vk.e.onTouch` reads as `D.b()`.
        if member == "onSurfaceCreatedNative" && result.is_ok() {
            if let Some(seam) = touch.as_mut() {
                seam.set_surface_alive(true);
            }
        }
        let failed = result.is_err();
        row_outcomes.push((
            member.to_string(),
            result.map(|_| ()).map_err(|error| error.to_string()),
        ));
        // The first activity resumed, so the process did: androidx's `ProcessLifecycleOwner`
        // dispatches `ON_RESUME` from `onActivityPostResumed`, once `onResume` has returned, to
        // the observer `RobloxApplication.onCreate` registered.
        if member == "onResumeNative" && !failed {
            let event = process_event(&guest, &mut cpu, script::ProcessEvent::Resume, "§8 rows");
            let refused = event.is_err();
            row_outcomes.push(("ProcessLifecycleOwner ON_RESUME".to_string(), event));
            if refused {
                break;
            }
        }
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

    // ---- the Java side's web view: `new WebViewProtocol(jk.a0)`, then `fh.c.c()` ----------------
    //
    // Where `MainGameActivity.B2` builds it: its UI runnable (`jk.c1`) forces the lazy `fh.c` and
    // `jk.a0` as the assets start to unpack, before `E2` sends the engine settings. See
    // `omni_android::jni::webview`. Only with a window: the pages it opens are host windows, and
    // the headless gate keeps the path it has been measured on. Its failure is reported and the
    // session goes on without a web view, as the app would go on without a page.
    let mut web_view: Option<WebViewProtocol> = None;
    if window.is_some() && row_outcomes.iter().all(|(_, result)| result.is_ok()) {
        let agent = user_agent(&user_agent_facts(&display));
        let _ = writeln!(std::io::stderr(), "WEBVIEW: the app's user agent is {agent:?}");
        let installed = {
            let _bionic = guest.bionic.activate().expect("publish the bionic instance");
            let _jni = guest.jni.activate().expect("publish the JNI instance");
            let _ndk = guest.ndk.activate();
            WebViewProtocol::install(
                &guest.jni,
                &guest.boundary,
                &mut cpu,
                0,
                &|symbol| guest.exports.get(symbol).copied(),
                agent,
            )
        };
        match installed {
            Ok((protocol, lines)) => {
                for line in lines {
                    let _ = writeln!(std::io::stderr(), "WEBVIEW: {line}");
                }
                web_view = Some(protocol);
            }
            Err(error) => {
                let _ = writeln!(std::io::stderr(), "WEBVIEW: NOT BUILT, the session has no web view: {error}");
            }
        }
        report_dead_guest_threads(&guest, "after building the web view protocol");
    }
    let mut browser = HostBrowser { size: (surface_width as u32, surface_height as u32) };
    // **OMNI_WEBVIEW_PROBE=<s>: a SYNTHETIC page, opened through the engine's own message bus** at
    // second `s` of the session: `WebView.openWindow` published as the engine publishes it, with a
    // `data:` page whose script calls the bridge once; then `WebView.closeWindow` ten seconds later.
    // It exercises every hop a captcha takes -- the bus to the Java side's subscription, the host's
    // browser, the bridge object, `signalJavascriptCallback`, the close and `handleWindowClose` --
    // without an account. Opt-in and said so: the engine receives a `handleJavascriptCallback` it
    // never asked for.
    let mut webview_probe: Option<(f32, u8)> = std::env::var("OMNI_WEBVIEW_PROBE").ok().map(|at| {
        let at = at.trim().parse::<f32>().unwrap_or_else(|_| panic!("OMNI_WEBVIEW_PROBE={at:?} is not a second"));
        let _ = writeln!(
            std::io::stderr(),
            "WEBVIEW PROBE: a SYNTHETIC page will be opened through the engine's bus at +{at}s (OMNI_WEBVIEW_PROBE)"
        );
        (at, 0)
    });
    let mut webview_signals = 0usize;

    // ---- §8 step 12: the engine settings, once there is an engine to take them -------------
    //
    // `MainGameActivity.E2` sends these after `super.onCreate` has created the engine; sent
    // before, the engine logs `nativeEngine is not created!` and drops them, and `initEngine_`
    // never runs (see `script::ENGINE_SETTINGS`). The lifecycle rows above are the proof the
    // engine exists: each waits for the game thread to take its command, and that thread creates
    // the `NativeEngine` before its loop. Asserted by the engine's own words, not by a return.
    if row_outcomes.iter().all(|(_, result)| result.is_ok()) {
        let dropped_before = count_log(&guest, "nativeEngine is not created");
        let outcomes = {
            let _bionic = guest.bionic.activate().expect("publish the bionic instance");
            let _jni = guest.jni.activate().expect("publish the JNI instance");
            let _ndk = guest.ndk.activate();
            script::run(
                &guest.jni,
                &guest.boundary,
                &mut cpu,
                &|symbol| guest.exports.get(symbol).copied(),
                script::ENGINE_SETTINGS,
                0,
            )
            .expect("building the step-12 arguments must not fail")
        };
        for outcome in &outcomes {
            let _ = writeln!(
                std::io::stderr(),
                "§8 step 12: {} -> {}",
                outcome.symbol,
                match &outcome.result {
                    Ok(()) => "returned".to_string(),
                    Err(error) => format!("{error}"),
                }
            );
            if let Err(error) = &outcome.result {
                panic!("§8 step 12: `{}` failed: {error}", outcome.symbol);
            }
        }
        assert_eq!(
            count_log(&guest, "nativeEngine is not created"),
            dropped_before,
            "§8 step 12: the engine dropped its settings -- it did not exist yet"
        );
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
            // ...then the surface again, now that the engine will accept it. Only the row that
            // carries the window's size: the lifecycle state is already where it should be, and
            // re-sending `onStart`/`onResume` would be telling the engine about a transition
            // that did not happen.
            //
            // **Not `onSurfaceCreatedNative`.** A live `SurfaceView` does not create its surface
            // twice, and the glue answers a second one for a window it already holds with
            // `APP_CMD_TERM_WINDOW` then `APP_CMD_INIT_WINDOW`. MEASURED, once the engine had its
            // settings and ran `initEngine_` and `initializeLuaApp_`: the TERM reached it as
            // `nativeActivity_onKillSurface: state:5` and `pauseExperienceOrLuaApp_` -- the gate
            // pausing the Lua app it had just initialised.
            for (member, descriptor, tail) in [
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
                // Not `onSurfaceCreatedNative`, for the reason the pre-fetch delivery above gives.
                for (member, descriptor, tail) in [
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
                    // **The third call a device's `SurfaceView` makes, and the one that starts the
                    // engine's surface path.** `SurfaceHolder.Callback2.surfaceRedrawNeeded`
                    // follows `surfaceChanged`, and `GameActivity` forwards it as this native.
                    // DECODED: `nativeActivity_onSurfaceChanged` (`0x2bd3000`) is called from the
                    // command handler only after `APP_CMD_WINDOW_INSETS_CHANGED` or
                    // `APP_CMD_WINDOW_REDRAW_NEEDED` (`0x2bcdb08`), or when a game loads
                    // (`0x2bd3938`). MEASURED without it: the window was accepted -- zero
                    // `Flags-Not-Received` either side -- and nothing asked for a renderer in 20 s.
                    (
                        "onSurfaceRedrawNeededNative",
                        "(JLandroid/view/Surface;)V",
                        vec![GuestArg::Int(surface)],
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
    let session = session_length();
    // **Frames, counted from the census**: every `vkQueuePresentKHR` the engine made, sampled on
    // this thread every `FRAMES_EVERY`, so "frames continue" is a rate the run prints rather than
    // a claim about one frame.
    let presents = || {
        guest
            .vulkan
            .as_ref()
            .and_then(|vulkan| vulkan.call_counts().get("vkQueuePresentKHR").copied())
            .unwrap_or(0)
    };
    let mut next_frames = std::time::Instant::now() + FRAMES_EVERY;
    let mut last_presents = presents();
    // **The size the engine was last told the surface has**, and the resizes to make if
    // `OMNI_RESIZE_PROBE` asks: to 960x540 at 40% of the session and back at 70%. A size change
    // from anywhere -- the probe, or a user dragging the frame -- is delivered as a device's
    // `SurfaceView` delivers one: `onSurfaceChangedNative` with the new size, then
    // `onContentRectChangedNative`, on this, the UI thread.
    let mut told_size = (surface_width as u32, surface_height as u32);
    let mut resize_probe: Vec<(f32, (u32, u32))> =
        if window.is_some() && std::env::var_os("OMNI_RESIZE_PROBE").is_some() {
            let _ = writeln!(
                std::io::stderr(),
                "RESIZE PROBE: the window will be resized to 960x540 at 40% of the {}s session and \
                 back to {surface_width}x{surface_height} at 70% (OMNI_RESIZE_PROBE)",
                session.as_secs()
            );
            vec![(0.4, (960, 540)), (0.7, (surface_width as u32, surface_height as u32))]
        } else {
            Vec::new()
        };
    // (presents when the size was delivered, the size) for each resize, to check frames follow.
    let mut resizes: Vec<(u64, (u32, u32))> = Vec::new();
    // **OMNI_LATE_INPUT=<s>[,<s>...]: a SYNTHETIC drag at each listed second of the session** --
    // press at the centre, 30 moves 8 px apart every 50 ms (240 px over 1.5 s), release. Opt-in
    // and said so, for the question `OMNI_INPUT_PROBE` cannot answer: that one lands before
    // the Lua app has drawn anything, and this one lands on whatever screen the engine is
    // showing by then, so the FRAMES lines around it say whether the engine redraws for it.
    let mut late_input: Vec<(f32, omni_platform::window::WindowEvent)> =
        match (&touch, std::env::var("OMNI_LATE_INPUT")) {
            (Some(_), Ok(list)) => {
                use omni_platform::window::{PointerButton, WindowEvent};
                let (x, y) = (surface_width / 2, surface_height / 2);
                let mut planned = Vec::new();
                for second in list.split(',') {
                    let at: f32 = second.trim().parse().unwrap_or_else(|_| {
                        panic!("OMNI_LATE_INPUT={list:?}: {second:?} is not a second of the session")
                    });
                    planned.push((at, WindowEvent::PointerDown { button: PointerButton::Primary, x, y }));
                    for step in 1..=30 {
                        planned.push((
                            at + 0.05 * step as f32,
                            WindowEvent::PointerMoved { x: x - 8 * step, y },
                        ));
                    }
                    planned.push((
                        at + 1.55,
                        WindowEvent::PointerUp { button: PointerButton::Primary, x: x - 240, y },
                    ));
                }
                planned.sort_by(|a, b| a.0.total_cmp(&b.0));
                let _ = writeln!(
                    std::io::stderr(),
                    "LATE INPUT: a SYNTHETIC 240 px drag from ({x}, {y}) at +{list}s (OMNI_LATE_INPUT)"
                );
                planned
            }
            _ => Vec::new(),
        };
    // **OMNI_LATE_TAP=<s>@<x>,<y>[;...]: a SYNTHETIC tap** -- press, and release 100 ms later, at
    // window pixel (x, y) -- for pressing one of the engine's own buttons where the screen shows
    // it. Opt-in and said so, like the drag: a stimulus this gate invents, aimed by a person.
    if let (Some(_), Ok(list)) = (&touch, std::env::var("OMNI_LATE_TAP")) {
        use omni_platform::window::{PointerButton, WindowEvent};
        for tap in list.split(';') {
            let parsed = tap.split_once('@').and_then(|(at, place)| {
                let (x, y) = place.split_once(',')?;
                Some((at.trim().parse::<f32>().ok()?, x.trim().parse::<i32>().ok()?, y.trim().parse::<i32>().ok()?))
            });
            let (at, x, y) = parsed.unwrap_or_else(|| {
                panic!("OMNI_LATE_TAP={list:?}: {tap:?} is not <second>@<x>,<y>")
            });
            late_input.push((at, WindowEvent::PointerDown { button: PointerButton::Primary, x, y }));
            late_input.push((at + 0.1, WindowEvent::PointerUp { button: PointerButton::Primary, x, y }));
        }
        late_input.sort_by(|a, b| a.0.total_cmp(&b.0));
        let _ = writeln!(std::io::stderr(), "LATE TAP: SYNTHETIC taps {list} (OMNI_LATE_TAP)");
    }
    // **OMNI_LATE_TEXT=<s>@<text>[;...]: SYNTHETIC typing**, one `Text` event per character 50 ms
    // apart from second `s`; `<enter>` as the text is one Enter key. For showing that typed
    // text reaches a focused `TextBox` -- never a credential: the run's log keeps lengths only.
    if let (Some(_), Ok(list)) = (&touch, std::env::var("OMNI_LATE_TEXT")) {
        use omni_platform::window::WindowEvent;
        for typed in list.split(';') {
            let (at, text) = typed
                .split_once('@')
                .and_then(|(at, text)| Some((at.trim().parse::<f32>().ok()?, text)))
                .unwrap_or_else(|| panic!("OMNI_LATE_TEXT: {typed:?} is not <second>@<text>"));
            if text == "<enter>" {
                late_input.push((at, WindowEvent::KeyDown { keycode: 0x0D, scancode: 0x1C, repeat: false }));
                late_input.push((at + 0.05, WindowEvent::KeyUp { keycode: 0x0D, scancode: 0x1C }));
            } else {
                for (index, character) in text.chars().enumerate() {
                    late_input.push((
                        at + 0.05 * index as f32,
                        WindowEvent::Text { text: character.to_string() },
                    ));
                }
            }
            let _ = writeln!(
                std::io::stderr(),
                "LATE TEXT: SYNTHETIC typing of {} at +{at}s (OMNI_LATE_TEXT)",
                if text == "<enter>" { "Enter".to_string() } else { format!("{} chars", text.chars().count()) }
            );
        }
        late_input.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    // **OMNI_LATE_KEYS=<s>@<make>[:<ms>][;...]: SYNTHETIC key presses** -- the physical key with
    // set-1 make code `make` (hex; `e0` in front for an extended key: `11` is W, `0f` Tab, `e048`
    // Up) down at second `s` and up `ms` later (default 100). The keys go where the host's keys go
    // (a declared keyboard's `nativePassKeyEvent`, or an open text field), and nothing types: no
    // `Text` event is made. Opt-in and said so, like every stimulus here.
    if let (Some(_), Ok(list)) = (&touch, std::env::var("OMNI_LATE_KEYS")) {
        use omni_platform::window::WindowEvent;
        for press in list.split(';') {
            let parsed = press.split_once('@').and_then(|(at, key)| {
                let (make, hold) = key.split_once(':').unwrap_or((key, "100"));
                Some((
                    at.trim().parse::<f32>().ok()?,
                    u32::from_str_radix(make.trim(), 16).ok()?,
                    hold.trim().parse::<u32>().ok()?,
                ))
            });
            let (at, scancode, hold) = parsed.unwrap_or_else(|| {
                panic!("OMNI_LATE_KEYS={list:?}: {press:?} is not <second>@<make-hex>[:<ms>]")
            });
            late_input.push((at, WindowEvent::KeyDown { keycode: 0, scancode, repeat: false }));
            late_input.push((at + hold as f32 / 1000.0, WindowEvent::KeyUp { keycode: 0, scancode }));
        }
        late_input.sort_by(|a, b| a.0.total_cmp(&b.0));
        let _ = writeln!(std::io::stderr(), "LATE KEYS: SYNTHETIC key presses {list} (OMNI_LATE_KEYS)");
    }
    // **OMNI_LATE_WHEEL=<s>@<x>,<y>,<notches>[;...]: SYNTHETIC wheel notches** at window pixel
    // (x, y) -- a move there, then one notch every 50 ms, positive away from the user.
    if let (Some(_), Ok(list)) = (&touch, std::env::var("OMNI_LATE_WHEEL")) {
        use omni_platform::window::WindowEvent;
        for turn in list.split(';') {
            let parsed = turn.split_once('@').and_then(|(at, rest)| {
                let mut parts = rest.split(',').map(|part| part.trim().parse::<i32>().ok());
                Some((at.trim().parse::<f32>().ok()?, parts.next()??, parts.next()??, parts.next()??))
            });
            let (at, x, y, notches) = parsed.unwrap_or_else(|| {
                panic!("OMNI_LATE_WHEEL={list:?}: {turn:?} is not <second>@<x>,<y>,<notches>")
            });
            late_input.push((at, WindowEvent::PointerMoved { x, y }));
            for notch in 0..notches.unsigned_abs() {
                late_input.push((
                    at + 0.05 * (notch + 1) as f32,
                    WindowEvent::Wheel { x, y, dx: 0, dy: 120 * notches.signum() },
                ));
            }
        }
        late_input.sort_by(|a, b| a.0.total_cmp(&b.0));
        let _ = writeln!(std::io::stderr(), "LATE WHEEL: SYNTHETIC wheel notches {list} (OMNI_LATE_WHEEL)");
    }
    // **OMNI_LATE_DRAG=<s>@<button>,<x>,<y>,<dx>,<dy>[;...]: a SYNTHETIC drag with any button**
    // (`left`, `right`, `middle`) -- a move to (x, y), the press, 20 moves over one second to
    // (x+dx, y+dy), the release. For the right-drag camera; `OMNI_LATE_INPUT` is the primary's.
    if let (Some(_), Ok(list)) = (&touch, std::env::var("OMNI_LATE_DRAG")) {
        use omni_platform::window::{PointerButton, WindowEvent};
        for drag in list.split(';') {
            let parsed = drag.split_once('@').and_then(|(at, rest)| {
                let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
                let button = match *parts.first()? {
                    "left" => PointerButton::Primary,
                    "right" => PointerButton::Secondary,
                    "middle" => PointerButton::Middle,
                    _ => return None,
                };
                let number = |index: usize| parts.get(index)?.parse::<i32>().ok();
                Some((at.trim().parse::<f32>().ok()?, button, number(1)?, number(2)?, number(3)?, number(4)?))
            });
            let (at, button, x, y, dx, dy) = parsed.unwrap_or_else(|| {
                panic!("OMNI_LATE_DRAG={list:?}: {drag:?} is not <second>@<left|right|middle>,<x>,<y>,<dx>,<dy>")
            });
            late_input.push((at, WindowEvent::PointerMoved { x, y }));
            late_input.push((at + 0.02, WindowEvent::PointerDown { button, x, y }));
            for step in 1..=20 {
                late_input.push((
                    at + 0.02 + 0.05 * step as f32,
                    WindowEvent::PointerMoved { x: x + dx * step / 20, y: y + dy * step / 20 },
                ));
            }
            late_input.push((at + 1.1, WindowEvent::PointerUp { button, x: x + dx, y: y + dy }));
        }
        late_input.sort_by(|a, b| a.0.total_cmp(&b.0));
        let _ = writeln!(std::io::stderr(), "LATE DRAG: SYNTHETIC drags {list} (OMNI_LATE_DRAG)");
    }
    let mut resize_failure: Option<String> = None;
    // **OMNI_PROFILE=1**: `sample_profile` on its own thread for the whole session.
    let profiler = std::env::var_os("OMNI_PROFILE").is_some().then(|| {
        let boundary = Arc::clone(&guest.boundary);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || sample_profile(&boundary, &stop))
        };
        (stop, handle)
    });
    // **OMNI_WAIT_TRACE=<s>: a diagnostic, off by default** -- from that second of the session,
    // every handler is timed by guest thread, call site and object (`omni_android::waits`), and
    // the totals are printed when the session ends.
    let mut wait_trace: Option<(f32, Option<std::time::Instant>)> =
        std::env::var("OMNI_WAIT_TRACE").ok().map(|at| {
            let at = at.trim().parse::<f32>().unwrap_or_else(|_| panic!("OMNI_WAIT_TRACE={at:?} is not a second"));
            let _ = writeln!(
                std::io::stderr(),
                "WAIT TRACE: ON from +{at}s of the session (OMNI_WAIT_TRACE): every handler is timed"
            );
            (at, None)
        });
    // Guest instructions fetched for translation so far, counted while the wait trace is asked for.
    let mut last_fetches = if wait_trace.is_some() { omni_cpu::dynarmic::count_code_fetches() } else { 0 };
    // Each host thread's translation count when the wait trace began.
    let mut fetches_at_trace: Vec<(String, u64)> = Vec::new();
    // (events, time, longest) the touch seam spent delivering while the wait trace was on.
    let mut input_timing = (0u64, std::time::Duration::ZERO, std::time::Duration::ZERO);
    let settle = std::time::Instant::now();
    // **A person closing the window ends the session**, and the app is then closed as a device
    // closes it (below), rather than the run carrying on into a window that is gone.
    let mut close_requested = false;
    // **A guest thread's death, reported when it happens.** They were reported only after the
    // stop request, so a session whose game thread died read as "frozen" for as long as a person
    // sat in front of it -- MEASURED, the first signed-in sessions: the render thread died on a
    // refused JNI lookup and the window simply stopped changing.
    let mut deaths_reported = guest.bionic.guest_thread_failures().len();
    // **How often this loop turns.** The phone configuration keeps the measured 100 ms sleep. With
    // a keyboard and mouse the loop instead waits on the window for up to that long and turns as
    // soon as input arrives -- a key's press and release are otherwise handed over up to 100 ms
    // late and in one batch -- but no more than once per `INPUT_TURN`, the ~60 Hz at which a
    // device's `Choreographer` hands a view its batched motion.
    const IDLE_TURN: std::time::Duration = std::time::Duration::from_millis(100);
    const INPUT_TURN: std::time::Duration = std::time::Duration::from_millis(16);
    // How many times the loop turned, for the report: the cost of turning on input.
    let mut turns = 0u64;
    // The last pointer-capture outcome logged, `(asked for, held)`.
    let mut capture_said: Option<(bool, bool)> = None;
    while settle.elapsed() < session {
        if guest.bionic.live_guest_threads() == 0 || close_requested {
            break;
        }
        let turn_started = std::time::Instant::now();
        turns += 1;
        if let Some((at, started @ None)) = wait_trace.as_mut() {
            if settle.elapsed().as_secs_f32() >= *at {
                omni_android::waits::enable();
                *started = Some(std::time::Instant::now());
                fetches_at_trace = omni_cpu::dynarmic::code_fetches_by_thread();
                let _ = writeln!(std::io::stderr(), "WAIT TRACE: tracing from +{:.1}s, {} presents so far", settle.elapsed().as_secs_f32(), presents());
            }
        }
        if std::time::Instant::now() >= next_frames {
            next_frames += FRAMES_EVERY;
            let failures = guest.bionic.guest_thread_failures();
            for failure in failures.iter().skip(deaths_reported) {
                let _ = writeln!(
                    std::io::stderr(),
                    "GUEST THREAD DIED at +{:.0}s: thread {} (started at link {:#x}): {}",
                    settle.elapsed().as_secs_f32(),
                    failure.thread,
                    failure.start_routine.wrapping_sub(guest.object.base),
                    failure.why
                );
            }
            deaths_reported = failures.len();
            let now = presents();
            // Under the wait trace, how much guest code was translated in the window as well.
            let translated = wait_trace.as_ref().map(|_| {
                let fetched = omni_cpu::dynarmic::count_code_fetches();
                let delta = fetched - last_fetches;
                last_fetches = fetched;
                format!("; {delta} guest instructions translated")
            });
            // The descriptor table's fill beside it: the join met `EMFILE` at the table's bound
            // (2026-09-23), and whether the count climbs and stays (a leak) or peaks with loading
            // (demand) is read off this line.
            let descriptors = guest
                .bionic
                .filesystem()
                .map(|fs| format!("; {} descriptors open", fs.open_count()))
                .unwrap_or_default();
            let _ = writeln!(
                std::io::stderr(),
                "FRAMES: +{:.0}s into the session, {now} presents (+{} in the last {}s){}{}",
                settle.elapsed().as_secs_f32(),
                now - last_presents,
                FRAMES_EVERY.as_secs(),
                translated.unwrap_or_default(),
                descriptors
            );
            last_presents = now;
        }
        if let Some(open) = window.as_ref() {
            let due = resize_probe
                .first()
                .is_some_and(|(at, _)| settle.elapsed().as_secs_f32() >= at * session.as_secs_f32());
            if due {
                let (_, (width, height)) = resize_probe.remove(0);
                let _ = writeln!(std::io::stderr(), "RESIZE PROBE: the window to {width}x{height}");
                if let Err(error) = open.set_client_size(width, height) {
                    resize_failure = Some(format!("set_client_size({width}, {height}): {error}"));
                }
            }
            if let Ok((width, height)) = open.client_size() {
                if (width, height) != told_size && width > 0 && height > 0 && resize_failure.is_none() {
                    for (member, descriptor, tail) in [
                        (
                            "onSurfaceChangedNative",
                            "(JLandroid/view/Surface;III)V",
                            vec![
                                GuestArg::Int(surface),
                                GuestArg::Int(1),
                                GuestArg::Int(u64::from(width)),
                                GuestArg::Int(u64::from(height)),
                            ],
                        ),
                        (
                            "onContentRectChangedNative",
                            "(JIIII)V",
                            vec![
                                GuestArg::Int(0),
                                GuestArg::Int(0),
                                GuestArg::Int(u64::from(width)),
                                GuestArg::Int(u64::from(height)),
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
                            "RESIZE: {member} {width}x{height} -> {}",
                            match &result {
                                Ok(_) => "returned".to_string(),
                                Err(error) => format!("{error}"),
                            }
                        );
                        if let Err(error) = result {
                            resize_failure = Some(format!("{member} {width}x{height}: {error}"));
                            break;
                        }
                    }
                    told_size = (width, height);
                    resizes.push((presents(), (width, height)));
                }
            }
        }
        // The window's own thread is this one, so this is where it is pumped and sampled: a
        // window nobody pumps is one the OS marks as not responding, and a geometry nobody
        // samples goes stale at the first resize.
        if let (Some(open), Some(source)) = (window.as_mut(), window_source.as_ref()) {
            let due = late_input
                .iter()
                .take_while(|(at, _)| settle.elapsed().as_secs_f32() >= *at)
                .count();
            let late: Vec<omni_platform::window::WindowEvent> =
                late_input.drain(..due).map(|(_, event)| event).collect();
            if late.iter().any(|event| matches!(event, omni_platform::window::WindowEvent::PointerDown { .. })) {
                let _ = writeln!(
                    std::io::stderr(),
                    "LATE INPUT: a SYNTHETIC press at +{:.1}s, {} presents so far",
                    settle.elapsed().as_secs_f32(),
                    presents()
                );
            }
            let events: Vec<omni_platform::window::WindowEvent> = match &touch {
                Some(seam) if seam.surface_alive() => {
                    input_probe.drain(..).chain(late).chain(open.poll_events()).collect()
                }
                _ => open.poll_events().collect(),
            };
            let _ = source.sample(open);
            // **§8 row 26, on this thread** -- the UI thread, which is where `vk.e.onTouch` and
            // `MainGameActivity.onKeyDown` run on a device, and where every lifecycle row above
            // was called from.
            let view = open.client_size().map_err(|error| {
                format!("the window's client size, which is the view `vk.e` divides: {error}")
            });
            // **What the engine asked of the keyboard**, carried out here, on the UI thread, where
            // the Java side's `runOnUiThread` puts it. Logged by length only: see `jni::text`.
            if let Some(field) = text_field.as_mut() {
                for request in guest.jni.take_keyboard_requests() {
                    let _bionic = guest.bionic.activate().expect("publish the bionic instance");
                    let _jni = guest.jni.activate().expect("publish the JNI instance");
                    let _ndk = guest.ndk.activate();
                    let shown = match &request {
                        omni_android::jni::KeyboardRequest::Show { text_box, text, manual_focus_release } => format!(
                            "show for text box {text_box:#x}, <{} chars>, manual focus release {manual_focus_release:?}",
                            text.chars().count()
                        ),
                        omni_android::jni::KeyboardRequest::Hide => "hide".to_string(),
                    };
                    match field.apply(&guest.jni, &guest.boundary, &mut cpu, 0, &request) {
                        Ok(calls) => {
                            let made: Vec<String> = calls.iter().map(|call| call.redacted()).collect();
                            let _ = writeln!(std::io::stderr(), "TEXT: keyboard {shown} -> {made:?}");
                        }
                        Err(error) => input_failure = Some(format!("keyboard {shown}: {error}")),
                    }
                }
            }
            if events.iter().any(|event| matches!(event, omni_platform::window::WindowEvent::CloseRequested)) {
                let _ = writeln!(
                    std::io::stderr(),
                    "WINDOW: closed by the person at it at +{:.1}s -- the session ends here",
                    settle.elapsed().as_secs_f32()
                );
                close_requested = true;
            }
            for event in &events {
                let _bionic = guest.bionic.activate().expect("publish the bionic instance");
                let _jni = guest.jni.activate().expect("publish the JNI instance");
                let _ndk = guest.ndk.activate();
                // An open text field takes the keys and the typed text, before the activity
                // would pass keys to `nativePassKeyEvent`.
                let for_field = matches!(
                    event,
                    omni_platform::window::WindowEvent::Text { .. }
                        | omni_platform::window::WindowEvent::KeyDown { .. }
                        | omni_platform::window::WindowEvent::KeyUp { .. }
                ) && text_field.as_ref().is_some_and(TextInput::is_open);
                if for_field && input_failure.is_none() {
                    if let Some(field) = text_field.as_mut() {
                        match field.deliver(&guest.jni, &guest.boundary, &mut cpu, 0, event) {
                            Ok(calls) => {
                                for call in calls {
                                    let _ = writeln!(std::io::stderr(), "TEXT: {}", call.redacted());
                                }
                            }
                            // Not `{event:?}`: a `Text` event is a character of what was typed.
                            Err(error) => input_failure = Some(format!("the text field: {error}")),
                        }
                    }
                }
                if let Some(seam) = mouse.as_mut() {
                    // **The keyboard-and-mouse configuration: the pointer is a mouse.** Every event
                    // goes through `jni::mouse`; a capture the listener asks for or gives back is
                    // applied to the window, and what the window did is told back.
                    if input_failure.is_none() {
                        match seam.deliver(&guest.jni, &guest.boundary, &mut cpu, 0, event) {
                            Ok(delivery) => {
                                for call in delivery.calls.iter().filter(|call| !matches!(call, MouseCall::Move { .. })) {
                                    let _ = writeln!(std::io::stderr(), "INPUT: mouse {call:?}");
                                }
                                if let Some(wanted) = delivery.capture {
                                    match open.set_pointer_capture(wanted) {
                                        Ok(held) => {
                                            seam.set_pointer_capture(held);
                                            // Said when the outcome changes: a request the window
                                            // declines (no focus) is repeated on every hover.
                                            if capture_said != Some((wanted, held)) {
                                                capture_said = Some((wanted, held));
                                                let _ = writeln!(
                                                    std::io::stderr(),
                                                    "INPUT: pointer capture {} by vk.e at +{:.1}s -> the window {}",
                                                    if wanted { "requested" } else { "released" },
                                                    settle.elapsed().as_secs_f32(),
                                                    if held { "holds it" } else { "does not hold it (no focus)" }
                                                );
                                            }
                                        }
                                        Err(error) => input_failure = Some(format!("pointer capture: {error}")),
                                    }
                                }
                                if matches!(event, omni_platform::window::WindowEvent::PointerCaptureLost) {
                                    capture_said = None;
                                    let _ = writeln!(
                                        std::io::stderr(),
                                        "INPUT: pointer capture lost with the focus at +{:.1}s",
                                        settle.elapsed().as_secs_f32()
                                    );
                                }
                            }
                            Err(error) => input_failure = Some(format!("{event:?}: {error}")),
                        }
                    }
                } else if let Some(seam) = touch.as_mut() {
                    let delivering = std::time::Instant::now();
                    let delivered = view.clone().and_then(|view| {
                        seam.deliver(&guest.jni, &guest.boundary, &mut cpu, 0, event, view)
                            .map_err(|error| format!("{event:?}: {error}"))
                    });
                    if omni_android::waits::enabled() {
                        let took = delivering.elapsed();
                        input_timing.0 += 1;
                        input_timing.1 += took;
                        input_timing.2 = input_timing.2.max(took);
                    }
                    match delivered {
                        Ok(calls) => {
                            for call in calls.iter().filter(|call| call.state != STATE_MOVED) {
                                let _ = writeln!(std::io::stderr(), "INPUT: nativePassInput {call:?}");
                            }
                        }
                        Err(error) => input_failure = Some(error),
                    }
                }
                let keyed = match (keyboard.as_mut(), input_failure.is_none() && !for_field) {
                    (Some(seam), true) => {
                        seam.deliver(&guest.jni, &guest.boundary, &mut cpu, 0, event)
                    }
                    _ => Ok(Vec::new()),
                };
                match keyed {
                    Ok(calls) => {
                        for call in calls {
                            let _ = writeln!(std::io::stderr(), "INPUT: nativePassKeyEvent {call:?}");
                        }
                    }
                    Err(error) => input_failure = Some(format!("{event:?}: {error}")),
                }
                if let Some(error) = &input_failure {
                    let _ = writeln!(std::io::stderr(), "INPUT: delivery failed: {error}");
                    report_dead_guest_threads(&guest, "after a failed input delivery");
                    touch = None;
                    keyboard = None;
                    mouse = None;
                    // A capture held for a mouse that is gone would pin the cursor for the rest
                    // of the session.
                    let _ = open.set_pointer_capture(false);
                    break;
                }
            }
        }
        // **The Java side's web view, on this thread** -- the UI thread, where its callbacks'
        // `Handler.post`s land on a device: the engine's calls since the last turn, then the page's.
        if let Some(protocol) = web_view.as_mut() {
            let pumped = {
                let _bionic = guest.bionic.activate().expect("publish the bionic instance");
                let _jni = guest.jni.activate().expect("publish the JNI instance");
                let _ndk = guest.ndk.activate();
                let probed = match webview_probe.as_mut() {
                    Some((at, stage)) if *stage < 2 && settle.elapsed().as_secs_f32() >= *at + 10.0 * f32::from(*stage) => {
                        let (id, json) = if *stage == 0 {
                            (
                                protocol.protocol().names().open_window.clone(),
                                webview_probe_open_message(protocol.protocol().names()),
                            )
                        } else {
                            (protocol.protocol().names().close_window.clone(), "{}".to_string())
                        };
                        *stage += 1;
                        let _ = writeln!(std::io::stderr(), "WEBVIEW PROBE: publishing {id:?}");
                        protocol.publish_raw(&guest.jni, &guest.boundary, &mut cpu, 0, &id, &json)
                    }
                    _ => Ok(()),
                };
                probed.and_then(|()| protocol.pump(&guest.jni, &guest.boundary, &mut cpu, 0, &mut browser))
            };
            match pumped {
                Ok(lines) => {
                    for line in lines {
                        if line.starts_with("signalJavascriptCallback(") {
                            webview_signals += 1;
                        }
                        let _ = writeln!(std::io::stderr(), "WEBVIEW: {line}");
                    }
                }
                Err(error) => {
                    let _ = writeln!(std::io::stderr(), "WEBVIEW: a call into the engine failed, no more web view: {error}");
                    report_dead_guest_threads(&guest, "after a failed web view call");
                    web_view = None;
                }
            }
        }
        match window.as_ref() {
            Some(open) if keyboard_mouse => {
                let spent = turn_started.elapsed();
                if spent < INPUT_TURN {
                    std::thread::sleep(INPUT_TURN - spent);
                }
                open.wait(IDLE_TURN.saturating_sub(turn_started.elapsed()));
            }
            _ => std::thread::sleep(IDLE_TURN),
        }
    }
    // A capture still held when the session ends is given back before the app is closed, so the
    // cursor is not pinned through the close.
    if let Some(open) = window.as_mut() {
        if open.has_pointer_capture() {
            let _ = open.set_pointer_capture(false);
        }
    }
    // The page's window, if one is up, closes with the session.
    drop(web_view);
    // **The probe, asserted at the end with the rest**: the page's one bridge call has to have
    // reached `signalJavascriptCallback` and returned.
    let probe_failure = match webview_probe {
        Some((at, 0)) => Some(format!("the session ended before the probe's second (+{at}s)")),
        Some(_) if webview_signals == 0 => Some(
            "the probe's page was published and its bridge call never reached signalJavascriptCallback".to_string(),
        ),
        _ => None,
    };
    if let Some(failure) = &probe_failure {
        let _ = writeln!(std::io::stderr(), "WEBVIEW PROBE: FAILED: {failure}");
    } else if webview_probe.is_some() {
        let _ = writeln!(std::io::stderr(), "WEBVIEW PROBE: {webview_signals} bridge call(s) reached the engine");
    }
    if let Some((_, Some(started))) = wait_trace {
        let seconds = started.elapsed().as_secs_f64();
        let boundary = &guest.boundary;
        let name = |slot: u64| {
            boundary
                .symbol_at(slot as GuestAddr)
                .map_or_else(|| format!("{slot:#x}"), str::to_string)
        };
        let report = omni_android::waits::report(seconds, guest.object.base as u64, 14, &name);
        // **Which threads translate**, over the traced window: guest instructions fetched for
        // translation by each host thread (a guest thread's host thread is named for it).
        let mut translated: Vec<(String, u64)> = omni_cpu::dynarmic::code_fetches_by_thread()
            .into_iter()
            .map(|(thread, now)| {
                let before = fetches_at_trace
                    .iter()
                    .find(|(name, _)| *name == thread)
                    .map_or(0, |(_, count)| *count);
                (thread, now - before)
            })
            .filter(|(_, delta)| *delta > 0)
            .collect();
        translated.sort_by(|a, b| b.1.cmp(&a.1));
        let total: u64 = translated.iter().map(|(_, delta)| delta).sum();
        let _ = writeln!(
            std::io::stderr(),
            "TRANSLATED over the trace: {total} guest instructions ({:.0}/s), by thread: {}",
            total as f64 / seconds.max(0.001),
            translated
                .iter()
                .take(16)
                .map(|(thread, delta)| format!("{thread}={delta}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let _ = writeln!(
            std::io::stderr(),
            "{report}{} presents in all; the touch seam took {:?} over {} window events (longest {:?})",
            presents(),
            input_timing.1,
            input_timing.0,
            input_timing.2
        );
    }
    if let Some((stop, handle)) = profiler {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        match handle.join() {
            Ok(report) => {
                let _ = write!(std::io::stderr(), "{report}");
            }
            Err(_) => {
                let _ = writeln!(std::io::stderr(), "PROFILE: the sampler panicked");
            }
        }
    }
    // **After each resize, frames**: the engine has to have presented again since the size it
    // was told changed, or the window survived the resize and the game did not.
    let final_presents = presents();
    let _ = writeln!(
        std::io::stderr(),
        "FRAMES: {final_presents} presents in all; resizes delivered {:?}",
        resizes
    );
    let frames_after_resize = resizes
        .iter()
        .find(|(at, _)| final_presents <= *at)
        .map(|(at, size)| format!("no present after the resize to {size:?} (at present {at})"));
    match &touch {
        Some(seam) => {
            let _ = writeln!(
                std::io::stderr(),
                "INPUT: {} nativePassInput call(s) returned; {} held back while the surface was \
                 dead; finger {}{}",
                seam.delivered(),
                seam.held_back(),
                if seam.finger_down() { "down" } else { "up" },
                if input_probe.is_empty() { "" } else { "; the SYNTHETIC probe was never sent" }
            );
        }
        None => {
            let _ = writeln!(
                std::io::stderr(),
                "INPUT: {}",
                match &input_failure {
                    Some(error) => format!("delivery stopped at the first failure: {error}"),
                    None => format!(
                        "not wired -- no window to take pointer events from ({GRAPHICS_GATE} unset)"
                    ),
                }
            );
        }
    }
    if let Some(seam) = &keyboard {
        let _ = writeln!(
            std::io::stderr(),
            "INPUT: {} nativePassKeyEvent call(s) returned; {} host key(s) with no Linux input \
             code; {} withheld by vk.g (BACK, VOLUME)",
            seam.delivered(),
            seam.unmapped(),
            seam.withheld()
        );
        let _ = writeln!(
            std::io::stderr(),
            "INPUT: {} key release(s) sent for keys held when the window lost the focus",
            seam.cancelled()
        );
    }
    let _ = writeln!(
        std::io::stderr(),
        "INPUT: the UI loop turned {turns} time(s) in {:.0}s ({})",
        settle.elapsed().as_secs_f32(),
        if keyboard_mouse { "waiting on the window, at most every 16 ms" } else { "every 100 ms" }
    );
    if let Some(seam) = &mouse {
        let counts = seam.counts();
        let _ = writeln!(
            std::io::stderr(),
            "INPUT: mouse -- {} nativePassMouseMove, {} nativePassMouseButton, {} nativePassMouseWheel \
             call(s) returned; the engine answered 'locked at the centre' {} time(s); vk.e asked for \
             the pointer capture {} time(s) and gave it back {} time(s)",
            counts.moves,
            counts.buttons,
            counts.wheels,
            counts.locked,
            counts.capture_requests,
            counts.capture_releases
        );
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
    // **And what FMOD asked `libaaudio.so` for**, the same way.
    match &guest.audio {
        Some(audio) => {
            let _ = writeln!(std::io::stderr(), "AAUDIO: {}", audio.report());
        }
        None => {
            let _ = writeln!(std::io::stderr(), "AAUDIO: not bound ({GRAPHICS_GATE} unset)");
        }
    }

    // ---- the app is closed, as a device closes it --------------------------------------------
    //
    // **A person leaving the app** is, from the UI thread and in this order:
    // `onWindowFocusChanged(false)`, `onPause`, the surface destroyed (`MainGameActivity`'s
    // `surfaceDestroyed` clears the touch listener's surface flag first, then `super`), `onStop`.
    // The glue hands each to the game thread and waits for it to be taken. `terminateNativeCode`
    // (`onDestroy`) is not sent: it waits for `android_main` to return.
    //
    // MEASURED why: runs ended by stopping threads left the engine's session unclosed, and the next
    // launch of a kept data directory (OMNI_DATA_DIR) took its inferred-crash path
    // (`InferredCrash`, link 0x23834e4) and died on a reporter this runtime does not set up.
    //
    // **Asserted, at the end with the others**: a close call that fails, and a close the engine
    // never records as the app going to the background (below).
    let mut close_failure: Option<String> = None;
    if window.is_some() && guest.bionic.live_guest_threads() > 0 {
        if let Some(seam) = touch.as_mut() {
            seam.set_surface_alive(false);
        }
        // **A close that does not finish is reported, then released.** The glue waits for the
        // game thread to take each command; if it never does, this says where every guest thread
        // is and stops them, which ends the glue's wait (its predicate loop exhausts the call's
        // budget) so the run ends with a report rather than a hang. MEASURED first: gate95 sat
        // nine minutes in `onSurfaceDestroyedNative` after the engine logged `APP_CMD_TERM_WINDOW`.
        //
        // **And the UI thread's own call is halted with them.** Stopping the guest threads makes
        // every `pthread_cond_wait` return at once, so the glue's predicate loop then *spins* --
        // MEASURED 2026-09-23 p1, p2 and 2026-09-24 relaunch-a: `onSurfaceDestroyedNative` ran its
        // whole 2e9-instruction budget after every close that hung behind a dead worker. Halting
        // the context ends the call where it is, which is all the spin was waiting for.
        let close_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ui_halt = cpu.halt_handle();
        {
            let close_done = Arc::clone(&close_done);
            let boundary = Arc::clone(&guest.boundary);
            let bionic = Arc::clone(&guest.bionic);
            let image_base = guest.object.base;
            let ui_halt = ui_halt.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                while std::time::Instant::now() < deadline {
                    if close_done.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let mut out = std::io::stderr();
                let _ = writeln!(out, "CLOSE WATCHDOG: the close did not finish in 20 s; every guest thread:");
                for report in boundary.threads() {
                    let _ = writeln!(
                        out,
                        "  thread {:#x}: {} {:?}, call site link {:#x}, {} crossings / {} exits",
                        report.guest_thread,
                        if report.crossings > report.exits { "INSIDE" } else { "in guest code after" },
                        report.symbol,
                        report.caller.wrapping_sub(image_base),
                        report.crossings,
                        report.exits
                    );
                }
                let _ = writeln!(out, "  parked: {:?}", bionic.parked());
                bionic.stop_guest_threads();
                ui_halt.request();
            });
        }
        let mut closed = true;
        // `ProcessLifecycleOwner` sends `ON_PAUSE` `TIMEOUT_MS` (700 ms) after the last activity
        // paused, from a message on the UI thread, so it runs between two of the calls below once
        // that time has passed. `ON_STOP` follows `onStop` if the pause has been sent by then, and
        // otherwise comes with the delayed pause, after it. (androidx.lifecycle
        // `ProcessLifecycleOwner.activityPaused`/`activityStopped`/`dispatchStopIfNeeded`.)
        const PROCESS_PAUSE_DELAY: std::time::Duration = std::time::Duration::from_millis(700);
        let mut paused_at: Option<std::time::Instant> = None;
        let mut pause_sent = false;
        let mut stopped = false;
        for (member, descriptor, tail) in [
            ("onWindowFocusChangedNative", "(JZ)V", vec![GuestArg::Int(0)]),
            ("onPauseNative", "(J)V", vec![]),
            ("onSurfaceDestroyedNative", "(J)V", vec![]),
            ("onStopNative", "(J)V", vec![]),
            // The system tells a process its UI is hidden once no activity of it is visible:
            // `onTrimMemory(TRIM_MEMORY_UI_HIDDEN)`, 20, which GameActivity passes on.
            ("onTrimMemoryNative", "(JI)V", vec![GuestArg::Int(20)]),
        ] {
            if paused_at.is_some_and(|at| at.elapsed() >= PROCESS_PAUSE_DELAY) && !pause_sent {
                pause_sent = true;
                let mut sent = process_event(&guest, &mut cpu, script::ProcessEvent::Pause, "CLOSE");
                if stopped && sent.is_ok() {
                    sent = process_event(&guest, &mut cpu, script::ProcessEvent::Stop, "CLOSE");
                }
                if let Err(error) = sent {
                    close_failure = Some(error);
                    closed = false;
                    break;
                }
            }
            let target = native(member, descriptor);
            let mut args = vec![GuestArg::Pointer(guest.jni.env_for(0)), GuestArg::Int(thiz), GuestArg::Int(native_code)];
            args.extend(tail);
            let result = {
                let _bionic = guest.bionic.activate().expect("publish the bionic instance");
                let _jni = guest.jni.activate().expect("publish the JNI instance");
                let _ndk = guest.ndk.activate();
                guest.boundary.call_guest(&mut cpu, member, target, &args, LIFECYCLE_BUDGET)
            };
            let _ = writeln!(
                std::io::stderr(),
                "CLOSE: {member} -> {}",
                match &result {
                    Ok(_) => "returned".to_string(),
                    Err(error) => format!("{error}"),
                }
            );
            if let Err(error) = &result {
                close_failure = Some(format!("{member}: {error}"));
                closed = false;
                break;
            }
            match member {
                "onPauseNative" => paused_at = Some(std::time::Instant::now()),
                "onStopNative" => {
                    stopped = true;
                    if pause_sent {
                        if let Err(error) = process_event(&guest, &mut cpu, script::ProcessEvent::Stop, "CLOSE") {
                            close_failure = Some(error);
                            closed = false;
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        if closed && !pause_sent {
            if let Some(at) = paused_at {
                std::thread::sleep(PROCESS_PAUSE_DELAY.saturating_sub(at.elapsed()));
            }
            if let Err(error) = process_event(&guest, &mut cpu, script::ProcessEvent::Pause, "CLOSE")
                .and_then(|()| process_event(&guest, &mut cpu, script::ProcessEvent::Stop, "CLOSE"))
            {
                close_failure = Some(error);
                closed = false;
            }
        }
        close_done.store(true, std::sync::atomic::Ordering::Relaxed);
        // The halt was for the close's call, and nothing after it must inherit it.
        ui_halt.clear();
        // **A close that could not finish behind a dead guest thread is a crashed process**, and
        // is recorded as the one a device's system server would have recorded: on a device the
        // thread's fatal signal ended the whole process when it happened, and the next launch is
        // told `REASON_CRASH_NATIVE`. Told nothing, the engine infers a crash and dies in its own
        // report -- MEASURED 2026-09-24: the same frozen data directory relaunched with no record
        // (relaunch-a) presented **0** frames and hung its close; with this record (relaunch-b)
        // it reached Landing at 300 presents per 5 s and closed cleanly.
        //
        // **Only when the close could not finish**: a death the app ran on past (relaunch-b's
        // own, a worker at +5 s) still gets the device's close, `onStop` included -- recording
        // those as crashes would make every later launch another inferred crash, and the engine
        // persists the sign-in on the way to the background.
        if !closed {
            let deaths = guest.bionic.guest_thread_failures();
            if let Some(first) = deaths.first() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX));
                let signal = death_signal(&first.why);
                record_exit(&guest._root.0, omni_android::jni::ExitRecord {
                    pid: i32::try_from(omni_platform::process::pid()).unwrap_or(0),
                    reason: omni_android::jni::ExitRecord::REASON_CRASH_NATIVE,
                    status: signal,
                    timestamp_ms: now,
                    importance: omni_android::jni::ExitRecord::IMPORTANCE_FOREGROUND,
                });
                let _ = writeln!(
                    std::io::stderr(),
                    "CLOSE: could not finish behind {} dead guest thread(s); recorded as REASON_CRASH_NATIVE \
                     (signal {signal}) for the next launch -- the first: thread {} (started at link {:#x}): {}",
                    deaths.len(),
                    first.thread,
                    first.start_routine.wrapping_sub(guest.object.base),
                    first.why
                );
            }
        }
        // **In the background until the engine has recorded it.** A device's backgrounded app
        // keeps running until it is removed, and the engine writes its session record
        // (`memProfStorage<pid>.json`) periodically, not on `onStop`. MEASURED why this waits:
        // a run torn down 3 s after `onStop` left that record saying the session never left the
        // foreground (`SessionHistory` `I`), and the next launch judged it a crash. Waiting for
        // the engine's own write, capped, is the device's order of events.
        //
        // **And asserted**: the record must end with the app in the background. MEASURED, the
        // letter that says so: `I` after each close without the process lifecycle events
        // (gate108, gate109, and gate113 with them switched off) -- and gate108's next launch
        // died in the engine's inferred-crash report (gate109) -- and `IB` after each close with
        // them (gate110-112, gate114), whose next launches ran.
        if closed {
            let record = guest._root.0.join(format!(
                "data/data/com.roblox.client/files/appData/LocalStorage/memProfStorage{}.json",
                omni_platform::process::pid()
            ));
            let history = || {
                std::fs::read_to_string(&record).ok().and_then(|text| {
                    text.split("\"SessionHistory\":\"").nth(1).and_then(|rest| rest.split('"').next()).map(str::to_string)
                })
            };
            let backgrounded = std::time::Instant::now();
            while backgrounded.elapsed() < std::time::Duration::from_secs(60)
                && !history().is_some_and(|letters| letters.ends_with('B'))
            {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            let history = history();
            let _ = writeln!(
                std::io::stderr(),
                "CLOSE: in the background {:.1}s; the engine's session record says SessionHistory {history:?}",
                backgrounded.elapsed().as_secs_f32()
            );
            if !history.as_deref().is_some_and(|letters| letters.ends_with('B')) {
                close_failure = Some(format!(
                    "60 s after the close, the engine's session record ({}) says SessionHistory \
                     {history:?}, not an app in the background -- the next launch of a kept root \
                     would judge this session a crash",
                    record.display()
                ));
            }
        }
        // **The run ends here at a person's request**, closed the way a device closes an app --
        // which is what Android records as `REASON_USER_REQUESTED` (the task removed, or force
        // stop), ended with `SIGKILL` while `IMPORTANCE_CACHED`. Recorded only when every close
        // call returned: a run that failed did not end that way, and the next launch is told
        // nothing about it, so the engine infers a crash -- which is then the truth.
        if closed {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX));
            record_exit(&guest._root.0, omni_android::jni::ExitRecord {
                pid: i32::try_from(omni_platform::process::pid()).unwrap_or(0),
                reason: omni_android::jni::ExitRecord::REASON_USER_REQUESTED,
                status: omni_android::jni::ExitRecord::SIGKILL,
                timestamp_ms: now,
                importance: omni_android::jni::ExitRecord::IMPORTANCE_CACHED,
            });
        }
        // The game thread acts on each command after the glue has handed it over.
        std::thread::sleep(std::time::Duration::from_secs(3));
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
    // **Where each thread that would not stop is, and what killed the ones that died** -- printed
    // before the assertion below, which ends the run and would otherwise take both with it.
    if !stopped {
        let mut out = std::io::stderr();
        for report in guest.boundary.threads() {
            let _ = writeln!(
                out,
                "  NOT STOPPED? guest thread {:#x} last crossed {:?} from link {:#x}, {}",
                report.guest_thread,
                report.symbol,
                report.caller.wrapping_sub(guest.object.base),
                if report.crossings > report.exits { "INSIDE THE HANDLER" } else { "in guest code" }
            );
        }
    }
    report_dead_guest_threads(&guest, "after the stop request");
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
        let lookups = guest.jni.lookups();
        let _ = writeln!(out, "POST-TEARDOWN JNI lookups (the last {}, oldest first):", lookups.len());
        for lookup in &lookups {
            let _ = writeln!(
                out,
                "  LOOKUP slot {} {} {} class {:#x}{}",
                lookup.thread,
                lookup.function,
                lookup.what,
                lookup.class,
                if lookup.pending_before { " (an exception was already pending)" } else { "" }
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
        // **And every asset the engine asked the APK for**, whether it was there or not: the
        // early report's census predates the renderer, which is where textures are read.
        let _ = writeln!(std::io::stderr(), "POST-TEARDOWN NDK census: {:?}", guest.ndk.census());
        for event in guest.ndk.events().iter().filter(|e| {
            matches!(e.what, "openAsset" | "getBuffer" | "openFileDescriptor" | "read" | "close")
        }) {
            let _ = writeln!(std::io::stderr(), "  ASSET {} {}", event.what, event.detail);
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
    // **Which thread, and where**: a count says a thread did not stop, and a thread that did not
    // stop produces no more evidence about itself. Its start routine names the code it runs, and
    // the boundary's record says whether it is inside a handler (and which) or in guest code.
    if !stopped {
        let reports = guest.boundary.threads();
        for summary in guest.bionic.guest_thread_list().iter().filter(|summary| summary.running) {
            let where_ = reports.iter().find(|report| report.guest_thread == summary.id.0).map_or_else(
                || "no crossing recorded".to_string(),
                |report| {
                    format!(
                        "{} {:?}, call site link {:#x}, {} crossings / {} exits",
                        if report.crossings > report.exits { "INSIDE" } else { "in guest code after" },
                        report.symbol,
                        report.caller.wrapping_sub(guest.object.base),
                        report.crossings,
                        report.exits
                    )
                },
            );
            let _ = writeln!(
                std::io::stderr(),
                "  STILL RUNNING: thread {:#x} started at link {:#x}: {where_}",
                summary.id.0,
                summary.start_routine.wrapping_sub(guest.object.base)
            );
        }
    }
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
    // (Their stacks and death contexts were printed straight after the stop request, above.)
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
    // **§3.1 Tier X, by name, beside its evidence** -- the one shape this comment allows.
    // `com/roblox/platform/util/DeviceUtils` has no declaring class in any of the APK's 26,620
    // dex classes (`jni-surface.md` §3.1, VERIFIED), so a device fails this same lookup; and the
    // engine says it tolerates that in its own words, MEASURED once the surface path first ran:
    // `[FLog::JNINativeHelper] getViewportDisplaySize: Failed to find class 'DeviceUtils'`, then
    // carried on. Anything else missed still fails, by membership rather than by count.
    const TIER_X: &[(&str, &str)] =
        &[("java/lang/ClassLoader.findClass", "com/roblox/platform/util/DeviceUtils")];
    let misses: Vec<_> = guest
        .jni
        .misses()
        .into_iter()
        .filter(|miss| {
            !TIER_X.iter().any(|(function, class)| miss.function == *function && miss.class == *class)
        })
        .collect();
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

    // **§8 row 26, asserted rather than printed**: a touch the engine's own `nativePassInput` did
    // not return from is a call that failed on this thread, and the line printed at the time is
    // not a detector (`VERIFICATION.md` entry 11).
    assert!(
        input_failure.is_none(),
        "§8 row 26: a window event did not reach the engine: {}",
        input_failure.as_deref().unwrap_or_default()
    );
    // **And a resize, asserted the same way**: the surface change reaching the engine, and the
    // engine presenting again after it.
    assert!(
        resize_failure.is_none(),
        "a window resize did not reach the engine: {}",
        resize_failure.as_deref().unwrap_or_default()
    );
    assert!(
        frames_after_resize.is_none(),
        "the window was resized and the engine stopped presenting: {}",
        frames_after_resize.as_deref().unwrap_or_default()
    );
    // **And the close**, asserted the same way: each call a device makes returned, and the
    // engine recorded the app going to the background.
    assert!(
        close_failure.is_none(),
        "the app was not closed as a device closes it: {}",
        close_failure.as_deref().unwrap_or_default()
    );
    assert!(
        probe_failure.is_none(),
        "OMNI_WEBVIEW_PROBE: {}",
        probe_failure.as_deref().unwrap_or_default()
    );
}

/// The `WebView.openWindow` message `OMNI_WEBVIEW_PROBE` publishes, in the keys the engine named: a `data:` page whose script calls the bridge once, with the Roblox hybrid bridge's
/// command shape (`cl.d.e` reads `moduleID`, `functionName`, `params`, `callbackID`).
fn webview_probe_open_message(names: &omni_android::jni::webview::ProtocolNames) -> String {
    let page = "<!doctype html><meta charset=\"utf-8\"><title>Omnidroid web view probe</title>\
                <p>Omnidroid web view probe: this page calls the app's bridge once.</p>\
                <script>__globalRobloxAndroidBridge__.executeRoblox(JSON.stringify(\
                {moduleID:\"OmnidroidProbe\",functionName:\"ping\",params:{},callbackID:\"probe-1\"}));</script>";
    let encoded: String = page
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(byte).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect();
    use omni_android::jni::webview::json::quote;
    format!(
        "{{{}:{},{}:{}}}",
        quote(&names.url_key),
        quote(&format!("data:text/html,{encoded}")),
        quote(&names.title_key),
        quote("Omnidroid web view probe")
    )
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
/// **Send a process lifecycle event as the app's own observer does**, through
/// [`script::process_lifecycle`], and say what happened -- the call, its result and any guest
/// thread that died meanwhile.
fn process_event(
    guest: &Guest,
    cpu: &mut DynarmicCpu,
    event: script::ProcessEvent,
    when: &str,
) -> Result<(), String> {
    let result = {
        let _bionic = guest.bionic.activate().expect("publish the bionic instance");
        let _jni = guest.jni.activate().expect("publish the JNI instance");
        let _ndk = guest.ndk.activate();
        script::process_lifecycle(
            &guest.jni,
            &guest.boundary,
            cpu,
            &|symbol| guest.exports.get(symbol).copied(),
            event,
        )
    };
    let _ = writeln!(
        std::io::stderr(),
        "{when}: ProcessLifecycleOwner {event:?} -> JNIAppLifecycleNativeAdapter.{} -> {}",
        event.native(),
        match &result {
            Ok(()) => "returned".to_string(),
            Err(error) => format!("{error}"),
        }
    );
    report_dead_guest_threads(guest, &format!("after ProcessLifecycleOwner {event:?}"));
    result.map_err(|error| error.to_string())
}

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
    death_contexts(&mut out, guest.boundary.mem(), guest.object.base, &failures);
    let _ = out.flush();
}

/// **What each dead guest thread was holding**: its registers and the top of its stack, from the
/// failure record's `DeathContext`, with every word that points into the image printed
/// link-relative and every word that points at readable guest memory dereferenced eight words
/// deep -- which is how a null read out of a field is traced to the object it was read from.
///
/// Printed by the watchdog too, because a run that dies this way usually blocks and never
/// reaches teardown: the thread that died was often doing work another thread waits for.
fn death_contexts(
    out: &mut impl Write,
    mem: &omni_android::GuestMem,
    base: GuestAddr,
    failures: &[omni_android::bionic::GuestThreadFailure],
) {
    let label = |value: u64| -> String {
        match (value as usize).checked_sub(base) {
            Some(link) if link < 0x0700_0000 => format!("{value:#x} (link {link:#x})"),
            _ => format!("{value:#x}"),
        }
    };
    let deref = |value: u64| -> Option<String> {
        let at = usize::try_from(value).ok().filter(|at| *at >= 0x10000 && at % 8 == 0)?;
        let words: Vec<String> = (0..8)
            .map_while(|k| {
                mem.read_u64(at + 8 * k, omni_android::Blame::new("a death context", at, 0))
                    .ok()
            })
            .map(label)
            .collect();
        (!words.is_empty()).then(|| words.join(", "))
    };
    for failure in failures {
        let context = &failure.context;
        if context.registers.is_empty() {
            continue;
        }
        let _ = writeln!(out, "DEATH CONTEXT: thread {} -- {}", failure.thread, failure.why);
        for (n, value) in context.registers.iter().enumerate() {
            let name = if n == 31 { "sp".to_string() } else { format!("x{n}") };
            let pointee = deref(*value).map(|p| format!("  -> [{p}]")).unwrap_or_default();
            let _ = writeln!(out, "    {name:>3} = {}{pointee}", label(*value));
        }
        for (k, chunk) in context.stack_bytes.chunks_exact(8).enumerate() {
            let word = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
            let pointee = deref(word).map(|p| format!("  -> [{p}]")).unwrap_or_default();
            let _ = writeln!(out, "    [sp+{:#05x}] {}{pointee}", k * 8, label(word));
        }
    }
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

/// **The registers the real `nativePassInput` reads are the ones the touch seam writes.**
///
/// `tests/input.rs` proves `jni::input` puts the pointer id in `w2`, x and y in `s0`/`s1` and the
/// state in `w3`, through real translated code. This proves that is where **`libroblox.so`'s own
/// native** takes them from: the moves at the top of `0x02bbba88` that park each argument in a
/// callee-saved register before the first call, and the moves that hand them on to the engine's
/// handler before the second -- `(input, sxtw(pointerId), state, x, y)` into `0x2e4e68c`.
///
/// Each is matched as an exact instruction word, encoded from its fields, so a build that moved an
/// argument to another register fails here naming it rather than delivering touches whose x is a
/// state. Needs the ELF and not a run.
#[test]
fn the_touch_native_reads_the_registers_the_seam_writes() {
    let _serial = serialized();
    let bytes = main_lib_bytes();
    let elf = ElfImage::parse(bytes).expect("parse libroblox.so");
    let symbol = elf
        .exported_symbols()
        .expect("read .dynsym")
        .into_iter()
        .find(|symbol| symbol.name == PASS_INPUT_SYMBOL)
        .unwrap_or_else(|| panic!("libroblox.so does not export `{PASS_INPUT_SYMBOL}`"));
    let offset = elf.vaddr_to_offset(symbol.sym.st_value).expect("the native is in a load segment");
    let words: Vec<u32> = bytes[offset..offset + symbol.sym.st_size as usize]
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().expect("four bytes")))
        .collect();
    // `MOV Wd, Wm` is `ORR Wd, WZR, Wm`; `FMOV Sd, Sn` is the single-precision register move;
    // `SXTW Xd, Wn` is `SBFM Xd, Xn, #0, #31`. Fields as the ARM ARM lays them out.
    let mov_w = |rd: u32, rm: u32| 0x2A00_03E0 | (rm << 16) | rd;
    let fmov_s = |rd: u32, rn: u32| 0x1E20_4000 | (rn << 5) | rd;
    let sxtw = |rd: u32, rn: u32| 0x9340_7C00 | (rn << 5) | rd;
    let is_bl = |word: &u32| word & 0xFC00_0000 == 0x9400_0000;
    let first = words.iter().position(is_bl).expect("the native calls the input singleton");
    let second = first
        + 1
        + words[first + 1..].iter().position(is_bl).expect("and then the engine's handler");
    let (parked, handed_on) = (&words[..first], &words[first + 1..second]);
    for (what, word) in [
        ("the pointer id is read from w2", mov_w(20, 2)),
        ("x is read from s0", fmov_s(9, 0)),
        ("y is read from s1", fmov_s(8, 1)),
        ("the state is read from w3", mov_w(19, 3)),
    ] {
        assert!(
            parked.contains(&word),
            "{what} ({word:#010x}) is not in {PASS_INPUT_SYMBOL}'s prologue: {parked:08x?}"
        );
    }
    for (what, word) in [
        ("x is handed on in s0", fmov_s(0, 9)),
        ("y is handed on in s1", fmov_s(1, 8)),
        ("the pointer id is handed on, sign-extended, in x1", sxtw(1, 20)),
        ("the state is handed on in w2", mov_w(2, 19)),
    ] {
        assert!(
            handed_on.contains(&word),
            "{what} ({word:#010x}) is not before the handler call: {handed_on:08x?}"
        );
    }
}

/// **The mouse natives read the registers `jni::mouse` writes, and mean what it says they mean**,
/// out of the real binary (`jni::mouse`'s module documentation has the decoding):
///
/// * `nativePassMouseMove` (`0x02bbbcf4`) parks `s0`-`s3` and hands them on in the same order --
///   x, y, dx, dy;
/// * `nativePassMouseButton` (`0x02bbbd78`) hands on x and y truncated to int (`fcvtzs` from `s0`,
///   `s1`), the button from `w3` and the down flag from `w2`; its handler maps the button through
///   a table bounded at 2 that holds the masks **1, 2, 4** -- so 0, 1, 2 are MouseButton1, 2, 3,
///   and the 3 the Java side sends for the middle button is past it;
/// * `nativePassMouseWheel` (`0x02bbbe00`) stores x and y from `s0`/`s1` and the delta from `s2`,
///   scaled;
/// * `nativeGetMainWindowIsMouseLockedCenter` (`0x02bbbca0`) answers the low bit of a handler
///   that compares the lock state with **1** (`LockCenter`) and nothing else.
///
/// Matched as exact instruction words, encoded from their fields. Needs the ELF and not a run.
#[test]
fn the_mouse_natives_read_the_registers_the_seam_writes() {
    use omni_android::jni::mouse::{
        MOUSE_LOCKED_CENTER_SYMBOL, PASS_MOUSE_BUTTON_SYMBOL, PASS_MOUSE_MOVE_SYMBOL,
        PASS_MOUSE_WHEEL_SYMBOL,
    };
    let _serial = serialized();
    let bytes = main_lib_bytes();
    let elf = ElfImage::parse(bytes).expect("parse libroblox.so");
    let symbols = elf.exported_symbols().expect("read .dynsym");
    let words_at = |vaddr: u64, count: usize| -> Vec<u32> {
        let offset = elf.vaddr_to_offset(vaddr).expect("in a load segment");
        bytes[offset..offset + 4 * count]
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().expect("four bytes")))
            .collect()
    };
    let native = |name: &str| -> (u64, Vec<u32>) {
        let symbol = symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("libroblox.so does not export `{name}`"));
        (symbol.sym.st_value, words_at(symbol.sym.st_value, symbol.sym.st_size as usize / 4))
    };
    let is_bl = |word: &u32| word & 0xFC00_0000 == 0x9400_0000;
    // The instructions before the native's first call, and those between it and the second.
    let split = |words: &[u32]| -> (Vec<u32>, Vec<u32>, usize) {
        let first = words.iter().position(is_bl).expect("the native calls the input singleton");
        let second = first + 1 + words[first + 1..].iter().position(is_bl).expect("and the handler");
        (words[..first].to_vec(), words[first + 1..second].to_vec(), second)
    };
    let target = |at: u64, word: u32| -> u64 {
        let displacement = ((word & 0x03FF_FFFF) << 6) as i32 >> 6;
        (at as i64 + 4 * i64::from(displacement)) as u64
    };
    let fmov_s = |rd: u32, rn: u32| 0x1E20_4000 | (rn << 5) | rd;
    let mov_w = |rd: u32, rm: u32| 0x2A00_03E0 | (rm << 16) | rd;
    let fcvtzs_w_s = |rd: u32, rn: u32| 0x1E38_0000 | (rn << 5) | rd;
    let require = |name: &str, part: &str, words: &[u32], what: &str, word: u32| {
        assert!(words.contains(&word), "{name}: {what} ({word:#010x}) is not in its {part}: {words:08x?}");
    };

    let (_, words) = native(PASS_MOUSE_MOVE_SYMBOL);
    let (parked, handed_on, _) = split(&words);
    for (what, word) in [
        ("x is read from s0", fmov_s(11, 0)),
        ("y is read from s1", fmov_s(10, 1)),
        ("dx is read from s2", fmov_s(9, 2)),
        ("dy is read from s3", fmov_s(8, 3)),
    ] {
        require(PASS_MOUSE_MOVE_SYMBOL, "prologue", &parked, what, word);
    }
    for (what, word) in [
        ("x is handed on in s0", fmov_s(0, 11)),
        ("y is handed on in s1", fmov_s(1, 10)),
        ("dx is handed on in s2", fmov_s(2, 9)),
        ("dy is handed on in s3", fmov_s(3, 8)),
    ] {
        require(PASS_MOUSE_MOVE_SYMBOL, "hand-off", &handed_on, what, word);
    }

    let (start, words) = native(PASS_MOUSE_BUTTON_SYMBOL);
    let (parked, handed_on, second) = split(&words);
    for (what, word) in [
        ("x is read from s0", fmov_s(9, 0)),
        ("y is read from s1", fmov_s(8, 1)),
        ("down is read from w2", mov_w(20, 2)),
        ("the button is read from w3", mov_w(19, 3)),
    ] {
        require(PASS_MOUSE_BUTTON_SYMBOL, "prologue", &parked, what, word);
    }
    for (what, word) in [
        ("x is handed on, truncated, in w1", fcvtzs_w_s(1, 9)),
        ("y is handed on, truncated, in w2", fcvtzs_w_s(2, 8)),
        ("the button is handed on in w3", mov_w(3, 19)),
        // `cset w4, ne` after `tst w20, #0xff`: down as a bool.
        ("down is handed on in w4", 0x1A9F_07E4),
    ] {
        require(PASS_MOUSE_BUTTON_SYMBOL, "hand-off", &handed_on, what, word);
    }
    // The handler is a shim that branches to the body; the body maps the button through its table.
    let shim_at = target(start + 4 * second as u64, words[second]);
    let shim = words_at(shim_at, 8);
    let branch = shim
        .iter()
        .position(|word| word & 0xFC00_0000 == 0x1400_0000)
        .expect("the handler branches to its body");
    let body_at = target(shim_at + 4 * branch as u64, shim[branch]);
    let body = words_at(body_at, 0x140);
    // `ldr w8, [x8, w28, uxtw #2]`: a load from a table of words indexed by the button.
    let lookup = body
        .iter()
        .position(|&word| word == 0xB87C_5908)
        .expect("the body indexes a table of words by the button (w28)");
    assert!(
        body[lookup.saturating_sub(6)..lookup].contains(&0x7100_0B9F),
        "the table is bounded at 2 (`cmp w28, #2`) before it is read: {:08x?}",
        &body[lookup.saturating_sub(6)..lookup]
    );
    let (adrp, add) = (body[lookup - 2], body[lookup - 1]);
    assert_eq!(adrp & 0x9F00_0000, 0x9000_0000, "the table is addressed with adrp: {adrp:#010x}");
    assert_eq!(add & 0xFFC0_0000, 0x9100_0000, "and its add: {add:#010x}");
    let pages = ((((adrp >> 5) & 0x7FFFF) << 2 | ((adrp >> 29) & 3)) << 11) as i32 >> 11;
    let page = ((body_at + 4 * (lookup as u64 - 2)) & !0xFFF) as i64 + (i64::from(pages) << 12);
    let table_at = page as u64 + u64::from((add >> 10) & 0xFFF);
    assert_eq!(
        words_at(table_at, 3),
        [1, 2, 4],
        "the button masks at {table_at:#x}: 0 is MouseButton1 (1), 1 is MouseButton2 (2), 2 is \
         MouseButton3 (4) -- and the middle button's 3 is past the table"
    );

    let (_, words) = native(PASS_MOUSE_WHEEL_SYMBOL);
    for (what, word) in [
        // `stp s0, s1, [sp]`: x and y, side by side, at the bottom of the event it builds.
        ("x and y are stored from s0 and s1", 0x2D00_07E0),
        // `fmul s2, s3, s2`: the delta, from s2, scaled.
        ("the delta is read from s2 and scaled", 0x1E22_0862),
        // `str s2, [sp, #8]`: after x and y.
        ("the scaled delta is stored after them", 0xBD00_0BE2),
    ] {
        require(PASS_MOUSE_WHEEL_SYMBOL, "body", &words, what, word);
    }

    let (start, words) = native(MOUSE_LOCKED_CENTER_SYMBOL);
    let (_, _, second) = split(&words);
    require(MOUSE_LOCKED_CENTER_SYMBOL, "body", &words[second..], "the answer is the low bit (`and w0, w0, #1`)", 0x1200_0000);
    let handler = words_at(target(start + 4 * second as u64, words[second]), 24);
    let compare = handler
        .iter()
        .position(|&word| word == 0x7100_051F)
        .expect("the handler compares the lock state with 1 (`cmp w8, #1`)");
    assert_eq!(handler[compare + 1], 0x1A9F_17F3, "and answers equality (`cset w19, eq`): locked at the centre only");
}

/// **A host key reaches the USB HID usage the engine expects for it**, through the engine's own
/// table.
///
/// `nativePassKeyEvent` (`0x02baebdc`) hands its scan code, `w3`, to `0x2e4eca8` as its first
/// argument (`mov w0, w3` before the first call), and that function is a bounds check (`cmp w0,
/// #0x7f`) and a load from a 128-entry table it addresses with `adrp`/`add`. Both are decoded
/// here, the table is read out of the real binary, and each host scan code is sent through
/// `jni::keys::evdev_code` and then through the table. The expected values are the **USB HID
/// Usage Tables**' own (Keyboard/Keypad page): `a` 4, `w` 26, Space 44, Left Shift 225, Up Arrow
/// 82. A host-side code that named the wrong physical key lands on the wrong usage.
#[test]
fn the_scan_codes_reach_the_usages_the_engine_expects() {
    use omni_android::jni::keys::{evdev_code, PASS_KEY_EVENT_SYMBOL};
    let _serial = serialized();
    let bytes = main_lib_bytes();
    let elf = ElfImage::parse(bytes).expect("parse libroblox.so");
    let words_at = |vaddr: u64, count: usize| -> Vec<u32> {
        let offset = elf.vaddr_to_offset(vaddr).expect("in a load segment");
        bytes[offset..offset + 4 * count]
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().expect("four bytes")))
            .collect()
    };
    let native = elf
        .exported_symbols()
        .expect("read .dynsym")
        .into_iter()
        .find(|symbol| symbol.name == PASS_KEY_EVENT_SYMBOL)
        .unwrap_or_else(|| panic!("libroblox.so does not export `{PASS_KEY_EVENT_SYMBOL}`"));
    let start = native.sym.st_value;
    let words = words_at(start, native.sym.st_size as usize / 4);
    let first = words
        .iter()
        .position(|word| word & 0xFC00_0000 == 0x9400_0000)
        .expect("the native calls the scan-code lookup first");
    // `mov w0, w3`: the scan code is the lookup's argument.
    assert!(
        words[..first].contains(&0x2A03_03E0),
        "the scan code (w3) is not what {PASS_KEY_EVENT_SYMBOL} hands its first call"
    );
    let displacement = ((words[first] & 0x03FF_FFFF) << 6) as i32 >> 6;
    let lookup = (start as i64 + 4 * first as i64 + 4 * i64::from(displacement)) as u64;
    let body = words_at(lookup, 8);
    assert!(body.contains(&0x7101_FC1F), "the lookup is bounded at 0x7f: {body:08x?}");
    let at_adrp = body
        .iter()
        .position(|word| word & 0x9F00_0000 == 0x9000_0000)
        .expect("the lookup addresses its table with adrp");
    let adrp = body[at_adrp];
    let add = body[at_adrp + 1];
    assert_eq!(add & 0xFFC0_0000, 0x9100_0000, "adrp is followed by its add: {add:#010x}");
    let pages = ((((adrp >> 5) & 0x7FFFF) << 2 | ((adrp >> 29) & 3)) << 11) as i32 >> 11;
    let page = ((lookup + 4 * at_adrp as u64) & !0xFFF) as i64 + (i64::from(pages) << 12);
    let table_at = page as u64 + u64::from((add >> 10) & 0xFFF);
    let table = words_at(table_at, 128);

    for (scancode, usage, key) in [
        (0x1E, 4, "a"),
        (0x11, 26, "w"),
        (0x1F, 22, "s"),
        (0x20, 7, "d"),
        (0x02, 30, "1"),
        (0x0B, 39, "0"),
        (0x1C, 40, "Enter"),
        (0x01, 41, "Escape"),
        (0x0E, 42, "Backspace"),
        (0x0F, 43, "Tab"),
        (0x39, 44, "Space"),
        (0x1D, 224, "Left Control"),
        (0x2A, 225, "Left Shift"),
        (0x38, 226, "Left Alt"),
        (0xE01D, 228, "Right Control"),
        (0x36, 229, "Right Shift"),
        (0xE04D, 79, "Right Arrow"),
        (0xE04B, 80, "Left Arrow"),
        (0xE050, 81, "Down Arrow"),
        (0xE048, 82, "Up Arrow"),
        (0x3B, 58, "F1"),
        (0x58, 69, "F12"),
    ] {
        let evdev = evdev_code(scancode).unwrap_or_else(|| panic!("{key} ({scancode:#x}) has no code"));
        assert_eq!(
            table[usize::from(evdev)],
            usage,
            "{key}: host scan code {scancode:#x} -> input code {evdev} -> the engine's table at \
             {table_at:#x} answers usage {}, and the key's usage is {usage}",
            table[usize::from(evdev)]
        );
    }
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
