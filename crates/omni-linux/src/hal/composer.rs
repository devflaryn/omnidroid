//! The display's composer, served from the host: `android.hardware.graphics.composer3.IComposer`
//! (AIDL V3), what SurfaceFlinger composes through (D3b design,
//! `docs/superpowers/specs/2026-09-27-d3b-composer-design.md`).
//!
//! One display. When every layer of a frame is one it can compose itself (`super::compose`: RGBA
//! buffers at 1:1 with no transform, solid colours), the composer keeps them `DEVICE` and blends
//! them from their gralloc regions (D2) into the host [`Framebuffer`] at present: SurfaceFlinger's
//! RenderEngine draws nothing. Otherwise every layer is changed to `CLIENT`, RenderEngine composes
//! them (on the host GPU, D3a) into the client target, and presenting copies that target.
//! `OMNI_COMPOSER_DEVICE=0` makes every frame `CLIENT`. Composition is synchronous, so there are no
//! fences to report (the `composer_fences` lever answers signalled ones: [`FENCES`] says what an
//! absent fence costs SurfaceFlinger). A frame that goes `CLIENT` says why, once per layer and
//! reason (`[composer] CLIENT composition: ...`).
//!
//! **The display can be resized** ([`Composer::set_display_size`]), as an external display whose
//! mode changes is: the composer offers one configuration of the new size under a new id and
//! reports the display connected again (`onHotplug`). SurfaceFlinger takes that as a reconnect
//! ("Reconnecting ..."): it reloads the display's modes, recreates the display at the new size and
//! tells DisplayManager, whose LocalDisplayAdapter updates the display device -- and from there
//! WindowManager and every app get the configuration change a resized display causes on a device.
//!
//! **Only the app is presented** (the default; `OMNI_APP_ONLY=0` or
//! [`Composer::set_show_chrome`] shows the whole display): the system's chrome -- SystemUI's status
//! and navigation bars and screen decorations, the launcher's taskbar -- are layers like any other
//! here, and the composer leaves them out of what it presents. It knows them by the window their
//! buffers are for: a window's BufferQueue names its buffers after it when it asks the allocator
//! for them (`VRI[StatusBar]#0(BLAST Consumer)0`; `hal::gralloc` keeps the name, [`is_chrome`]).
//! A chrome layer stays `DEVICE`, which is the composer's to draw -- and it draws nothing. When the
//! frame's other layers are the composer's too, they are composed as they are; when one is not,
//! they go to SurfaceFlinger (`CLIENT`), whose client target then holds everything but the chrome,
//! with the app's own pixels where the chrome would have been on top of them. The app's layer is
//! not moved or scaled: an app that draws edge to edge (a game; any app on a device without
//! SystemUI) fills the display, and one that keeps clear of the bars shows its own background there.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;

use super::aidl::android_hardware_common::NativeHandle;
use super::aidl::android_hardware_graphics_common as common;
use super::aidl::android_hardware_graphics_composer3::{
    i_composer, i_composer_client, Capability, ChangedCompositionLayer, ChangedCompositionTypes, ColorMode, CommandError, CommandResultPayload, Composition,
    DisplayAttribute, DisplayCapability, DisplayCommand, DisplayConfiguration, DisplayConfiguration_Dpi, DisplayConnectionType, HdrCapabilities,
    IComposerCallbackProxy, IComposerClientServer, IComposerServer, PerFrameMetadataKey, PowerMode, PresentOrValidate, PresentOrValidate_Result,
    PresentFence, ReleaseFences, ReleaseFences_Layer, RenderIntent, ContentType, ClockMonotonicTimestamp, VsyncPeriodChangeConstraints, VsyncPeriodChangeTimeline,
};
use super::aidl::{Binder, Ctx, Fd, Status};
use super::framebuffer::Framebuffer;
use super::gralloc::{NAME_AT, PIXELS_AT};
use crate::binder::{Broker, STABILITY_VINTF};
use crate::fd::FileKind;
use crate::shm::Shm;

/// The instance SurfaceFlinger waits for (declared by the image's `hwc3.xml`).
pub const INSTANCE: &str = "android.hardware.graphics.composer3.IComposer/default";

/// The one display's id, initial size, refresh and density.
const DISPLAY: i64 = 0;
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
/// The smallest display [`Composer::set_display_size`] makes, in either axis: Android's smallest
/// screen width is 320 dp (the CDD's minimum), 320 pixels at this display's 160 dpi.
pub const MIN_SIDE: u32 = 320;
const DPI: f32 = 160.0;

/// **The display's refresh period**, in nanoseconds: what the vsync thread paces to and what every
/// answer about the display's timing says (the attribute, the configuration, each `onVsync`), so
/// SurfaceFlinger is told the rate it is given. 60 Hz unless `OMNI_VSYNC_HZ=<n>` (read when the
/// composer starts, so SurfaceFlinger's model of the display has it from boot) or the live
/// `vsync_hz=<n>` lever (`crate::lever`) says otherwise. A live change reaches SurfaceFlinger only
/// through the vsync timestamps and periods it hears from then on: it read the display's modes at
/// boot, so it is for finding out whether pacing is the frame-rate ceiling, not for a real mode.
pub static VSYNC_PERIOD_NS: AtomicI32 = AtomicI32::new(16_666_666);

/// `vsync_pace=1` (the default): vsync is paced to absolute deadlines ([`Pacer`]). `0`: the old
/// loop, a `sleep(period)` after each callback, to compare. MEASURED (Windows, i7-13700F, normal
/// priority, 5 s each): the old loop ran at **58.8 Hz** (the sleep's ~0.4 ms overshoot plus the
/// callback's work were added to every period), the deadlines at **60.00 Hz** (overshoot p50
/// 0.36 ms, p99 0.9-1.1 ms, never accumulated). SurfaceFlinger fits its software vsync to these
/// timestamps (and turns hardware vsync off once its model is confident), so the old loop's rate
/// became the whole system's: a ceiling under 60 fps.
pub static VSYNC_PACE: AtomicBool = AtomicBool::new(true);

/// The last vsync's time on the guest's clock (ns), for a present fence's signal time.
static LAST_VSYNC_NS: AtomicI64 = AtomicI64::new(0);

/// **`composer_skip_validate=1`** (lever; `OMNI_COMPOSER_SKIP_VALIDATE=1` from the start): a frame
/// the composer composes itself is presented at `presentOrValidateDisplay` and answered
/// `Presented`, as a hardware composer with nothing to change does -- rather than `Validated`,
/// after which SurfaceFlinger makes a **second `executeCommands` call** (`acceptDisplayChanges` +
/// `presentDisplay`) for the same frame. SurfaceFlinger takes this path for every frame without
/// client composition (AOSP 15 `HWComposer::getDeviceCompositionChanges`: `canSkipValidate` is
/// true, because the AIDL composer always "supports" the expected present time). Saves one
/// binder round trip per frame: the guest's marshalling of the call and of its reply, the
/// broker's hand-off to a host thread and back, SurfaceFlinger's thread put to sleep and woken.
/// Off by default, for an in-session A/B.
pub static SKIP_VALIDATE: AtomicBool = AtomicBool::new(false);

/// **`composer_fences=0|1|2`** (lever): 0 (the default) answers no fences, as before; 1 a present
/// fence with each presented frame; 2 also a release fence for each layer the composer read.
///
/// Every fence is a sync file signalled when it is made (`crate::sync_file`: composition here is
/// synchronous, so nothing is ever pending). A present fence's signal time is the first vsync at
/// or after the present (the time the display would show it), so the vsync model SurfaceFlinger
/// fits to present fences (`VSyncReactor::addPresentFence`) sees vsync-phase timestamps and not
/// composition times, which it would reject as outliers and turn hardware vsync back on for.
///
/// **What an absent fence (-1) costs SurfaceFlinger, read in AOSP 15**: nothing in its pacing.
/// `FrameTargeter::beginFrame` treats `NO_FENCE` as "not pending" and "not missed" (no
/// backpressure, no skipped commit); `computeEarliestPresentTime` is not used (expected present
/// time is supported), so it never sleeps before presenting; `FrameTimeline` flushes an invalid
/// fence at once. It costs one `E/SurfaceFlinger: trackPendingFrame: Invalid present fence` log
/// line a frame (`PresentLatencyTracker`, only while the composer does not report
/// `PRESENT_FENCE_IS_NOT_RELIABLE`: [`unreliable_present_fence`]), and no `DISPLAY_PRESENT` frame
/// timestamps. A real fence costs more: a descriptor made and translated into SurfaceFlinger per
/// frame (two with release fences per layer), and SurfaceFlinger passes the present fence on in
/// every transaction-completed callback -- to the game, in another host process, so a descriptor
/// crossing processes every frame. Release fences change nothing for the app: without one, a
/// buffer is released with no fence, which the app may reuse at once -- and the composer has
/// finished reading it when `executeCommands` returns.
pub static FENCES: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// `OMNI_COMPOSER_UNRELIABLE_PRESENT_FENCE=1`: the composer reports the
/// `PRESENT_FENCE_IS_NOT_RELIABLE` capability, which SurfaceFlinger reads once, at boot. It then
/// stops tracking present latency (no `trackPendingFrame` log line a frame), ignores present fences
/// in its vsync model, offers no `DISPLAY_PRESENT` frame event and sets
/// `service.sf.present_timestamp=0` -- which Android's Vulkan loader reads to offer
/// `VK_GOOGLE_display_timing` (a pacing library in the game may use it): an A/B, off by default.
fn unreliable_present_fence() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_COMPOSER_UNRELIABLE_PRESENT_FENCE").as_deref() == Ok("1"))
}

fn skip_validate() -> bool {
    static FROM_ENV: std::sync::Once = std::sync::Once::new();
    FROM_ENV.call_once(|| {
        if std::env::var("OMNI_COMPOSER_SKIP_VALIDATE").as_deref() == Ok("1") {
            SKIP_VALIDATE.store(true, Ordering::Relaxed);
        }
    });
    SKIP_VALIDATE.load(Ordering::Relaxed)
}

/// The first vsync at or after `now` (guest ns), on the vsync thread's phase.
fn vsync_at_or_after(now: i64) -> i64 {
    next_vsync(LAST_VSYNC_NS.load(Ordering::Relaxed), i64::from(vsync_period_ns()), now)
}

/// The first of `last + k * period` (k >= 0) at or after `now`; `now` itself before any vsync.
fn next_vsync(last: i64, period: i64, now: i64) -> i64 {
    let period = period.max(1);
    if last <= 0 || now <= last {
        return now.max(last);
    }
    last + ((now - last) + period - 1) / period * period
}

/// The refresh period now, in nanoseconds.
fn vsync_period_ns() -> i32 {
    VSYNC_PERIOD_NS.load(Ordering::Relaxed)
}

/// Set the refresh rate to `hz` (1..=1000); the period set, or `None` for a rate out of range.
pub fn set_vsync_hz(hz: u32) -> Option<i32> {
    if !(1..=1000).contains(&hz) {
        return None;
    }
    let period = i32::try_from((1_000_000_000 + u64::from(hz) / 2) / u64::from(hz)).ok()?;
    VSYNC_PERIOD_NS.store(period, Ordering::Relaxed);
    Some(period)
}

/// **Vsync's deadlines**: tick `k` is due at `origin + k * period`, so the time a tick's wake-up
/// and callback take is never added to the next period (a relative `sleep(period)` loop drifts
/// below the rate by exactly that). A tick woken later than its deadline fires at once; one woken
/// later than the *next* deadline fires once for the latest deadline passed, the ones between
/// counted as missed -- never fired back to back to catch up, which would hand SurfaceFlinger a
/// burst of vsyncs no display makes. A new period starts a new origin at the last deadline.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pacer {
    origin: std::time::Instant,
    period: Duration,
    n: u64,
}

impl Pacer {
    pub(crate) fn new(now: std::time::Instant, period: Duration) -> Self {
        Self { origin: now, period, n: 0 }
    }

    fn deadline(&self, n: u64) -> std::time::Instant {
        self.origin + Duration::from_nanos(u64::try_from(u128::from(n) * self.period.as_nanos()).unwrap_or(u64::MAX))
    }

    /// The next tick's deadline, at `now` (the last tick done), with `period` the period now; and
    /// how many deadlines were missed outright and dropped.
    pub(crate) fn next(&mut self, now: std::time::Instant, period: Duration) -> (std::time::Instant, u64) {
        if period != self.period && !period.is_zero() {
            self.origin = self.deadline(self.n);
            self.n = 0;
            self.period = period;
        }
        self.n += 1;
        let mut missed = 0;
        if now >= self.deadline(self.n + 1) {
            // More than a whole period late: the latest deadline passed is the one fired.
            let passed = u64::try_from(now.saturating_duration_since(self.origin).as_nanos() / self.period.as_nanos().max(1)).unwrap_or(u64::MAX);
            missed = passed - self.n;
            self.n = passed;
        }
        (self.deadline(self.n), missed)
    }
}

/// The vsync thread's own account, said every [`VsyncRate::EVERY`] (`[vsync] ...`): the rate it
/// really ticked at, how late it woke, how many deadlines it missed, how many it delivered.
struct VsyncRate {
    since: std::time::Instant,
    ticks: u64,
    delivered: u64,
    missed: u64,
    late_sum: Duration,
    late_max: Duration,
}

impl VsyncRate {
    const EVERY: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self { since: std::time::Instant::now(), ticks: 0, delivered: 0, missed: 0, late_sum: Duration::ZERO, late_max: Duration::ZERO }
    }

    fn tick(&mut self, late: Duration, missed: u64, paced: bool) {
        self.ticks += 1;
        self.missed += missed;
        self.late_sum += late;
        self.late_max = self.late_max.max(late);
        let took = self.since.elapsed();
        if took < Self::EVERY {
            return;
        }
        let period = f64::from(vsync_period_ns()) / 1e6;
        eprintln!(
            "[vsync] {:.2} Hz {} (period {period:.3} ms = {:.2} Hz): {} ticks in {:.1} s, {} delivered, {} missed, late avg {:.2} max {:.2} ms",
            self.ticks as f64 / took.as_secs_f64(),
            if paced { "paced" } else { "sleep-loop" },
            1e3 / period,
            self.ticks,
            took.as_secs_f64(),
            self.delivered,
            self.missed,
            self.late_sum.as_secs_f64() * 1e3 / self.ticks as f64,
            self.late_max.as_secs_f64() * 1e3,
        );
        *self = Self::new();
    }
}

/// `IComposerClient`'s service-specific errors.
const EX_BAD_DISPLAY: i32 = 2;
const EX_BAD_LAYER: i32 = 3;
const EX_NO_RESOURCES: i32 = 6;
const EX_UNSUPPORTED: i32 = 8;

/// The gralloc handle's ints (`hal::gralloc`): the format at 5, the stride at 8, the pixels at 13.
const HANDLE_INTS: usize = 14;
const RGBA_8888: i32 = 1;
const RGBX_8888: i32 = 2;
const BGRA_8888: i32 = 5;
const IMPLEMENTATION_DEFINED: i32 = 0x22;

/// The display's one configuration: its id and size. A resize is a new id, so that nothing
/// SurfaceFlinger or DisplayManager kept of the old mode can be mistaken for the new one.
#[derive(Debug, Clone, Copy)]
struct Mode {
    config: i32,
    width: u32,
    height: u32,
}

/// The windows that are the system's chrome, by the start of their title: SystemUI's bars and
/// screen decorations (rounded corners, cutout), and the launcher's taskbar.
const CHROME: &[&str] = &["StatusBar", "NavigationBar", "Taskbar", "ScreenDecorOverlay", "ScreenDecorHwcLayer"];

/// Whether a buffer of this name (`hal::gralloc`'s metadata page) is one of the system's chrome:
/// a view root's BufferQueue is named `VRI[<window title>]#<n>(BLAST Consumer)<n>`, and the title
/// is one of [`CHROME`]'s.
#[must_use]
pub fn is_chrome(buffer_name: &str) -> bool {
    let Some(title) = buffer_name.strip_prefix("VRI[").and_then(|rest| rest.split(']').next()) else { return false };
    CHROME.iter().any(|c| title.starts_with(c))
}

/// **One display**: the framebuffer it is presented into, its configuration, and the client state
/// that belongs to it -- its layers, its client targets, and what the last validation decided.
///
/// A display's layers are its own: `createLayer` names the display it is for, and every command
/// carries one. So the state that used to be the client's is a screen's, and the client holds only
/// what is the client's -- its callback and the counter that makes layer ids unique across displays.
struct Screen {
    framebuffer: Arc<Framebuffer>,
    mode: Mutex<Mode>,
    state: Mutex<State>,
}

impl Screen {
    fn mode(&self) -> Mode {
        *self.mode.lock()
    }
}

/// The displays this composer serves, by id, shared by the composer and its client.
type Screens = Arc<Mutex<std::collections::BTreeMap<i64, Arc<Screen>>>>;

pub struct Composer {
    broker: Arc<Broker>,
    screens: Screens,
    client: Mutex<Option<Arc<Client>>>,
    /// Whether the system's chrome is presented (see this module's "Only the app").
    show_chrome: Arc<AtomicBool>,
}

impl Composer {
    #[must_use]
    pub fn new(broker: Arc<Broker>, framebuffer: Arc<Framebuffer>) -> Arc<Self> {
        let (width, height) = framebuffer.size();
        let show_chrome = std::env::var("OMNI_APP_ONLY").as_deref() == Ok("0");
        eprintln!("[composer] {}", if show_chrome { "the whole display is presented (OMNI_APP_ONLY=0)" } else { "only the app is presented: the system's bars and taskbar are left out" });
        let screen = Arc::new(Screen { framebuffer, mode: Mutex::new(Mode { config: 0, width, height }), state: Mutex::default() });
        let screens: Screens = Arc::new(Mutex::new([(DISPLAY, screen)].into_iter().collect()));
        Arc::new(Self { broker, screens, client: Mutex::new(None), show_chrome: Arc::new(AtomicBool::new(show_chrome)) })
    }

    /// The display `id`, or the first one if it is gone (the one display, for the callers that
    /// speak of "the display": the window, the screenshot, the resize).
    fn screen(&self, id: i64) -> Option<Arc<Screen>> {
        let screens = self.screens.lock();
        screens.get(&id).cloned()
    }

    /// Whether the system's chrome is presented.
    #[must_use]
    pub fn shows_chrome(&self) -> bool {
        self.show_chrome.load(Ordering::Relaxed)
    }

    /// Present the system's chrome with the app, or the app alone (see this module's "Only the
    /// app"), from the next frame on -- which is asked for at once (`onRefresh`), so that a still
    /// screen changes too.
    pub fn set_show_chrome(&self, show: bool) {
        if self.show_chrome.swap(show, Ordering::Relaxed) == show {
            return;
        }
        eprintln!("[composer] {}", if show { "the whole display is presented" } else { "only the app is presented" });
        let callback = self.client.lock().as_ref().and_then(|c| *c.callback.lock());
        if let Some(callback) = callback {
            let _ = IComposerCallbackProxy::new(Arc::clone(&self.broker), callback).on_refresh(DISPLAY);
        }
    }

    /// The display's size now.
    #[must_use]
    pub fn display_size(&self) -> (u32, u32) {
        let m = self.screen(DISPLAY).map_or(Mode { config: 0, width: WIDTH, height: HEIGHT }, |s| s.mode());
        (m.width, m.height)
    }

    /// **Resize the display** to `width` x `height` (each at least [`MIN_SIDE`]): a configuration
    /// of that size under a new id, and the display reported connected again -- see this module's
    /// doc. Answers the size the display now has. Before SurfaceFlinger has registered its
    /// callback the size is only recorded, for its first look at the display.
    ///
    /// # Errors
    /// The hotplug's delivery failed (SurfaceFlinger gone).
    pub fn set_display_size(&self, width: u32, height: u32) -> Result<(u32, u32), String> {
        let (width, height) = (width.max(MIN_SIDE), height.max(MIN_SIDE));
        let Some(screen) = self.screen(DISPLAY) else { return Ok((width, height)) };
        let config = {
            let mut m = screen.mode.lock();
            if (m.width, m.height) == (width, height) {
                return Ok((width, height));
            }
            *m = Mode { config: m.config + 1, width, height };
            m.config
        };
        let callback = self.client.lock().as_ref().and_then(|c| *c.callback.lock());
        eprintln!("[composer] display {width}x{height} (config {config}): {}", if callback.is_some() { "hotplug" } else { "before SurfaceFlinger" });
        if let Some(callback) = callback {
            IComposerCallbackProxy::new(Arc::clone(&self.broker), callback).on_hotplug(DISPLAY, true).map_err(|e| format!("onHotplug: {e:?}"))?;
        }
        Ok((width, height))
    }

    /// **Add a display** of this size, and connect it: its id, and the framebuffer its frames are
    /// presented into (a window of its own shows that, as display 0's does).
    ///
    /// SurfaceFlinger learns of it by hotplug, DisplayManager makes a `Display` for it, and an
    /// activity started with `am start --display <id>` is resumed on it **beside** the one on
    /// display 0 -- which is what lets two apps run side by side, each drawing, neither backgrounded
    /// (the spike in the multi-instance design). Added before SurfaceFlinger has registered its
    /// callback, the display is only recorded, and `registerCallback` hotplugs every display there
    /// is -- which is the simple way to start with more than one.
    ///
    /// # Errors
    /// The hotplug's delivery failed (SurfaceFlinger gone).
    pub fn add_display(&self, width: u32, height: u32) -> Result<(i64, Arc<Framebuffer>), String> {
        let (width, height) = (width.max(MIN_SIDE), height.max(MIN_SIDE));
        let framebuffer = Arc::new(Framebuffer::new(width, height));
        let id = {
            let mut screens = self.screens.lock();
            let id = screens.keys().copied().max().unwrap_or(DISPLAY) + 1;
            screens.insert(id, Arc::new(Screen { framebuffer: Arc::clone(&framebuffer), mode: Mutex::new(Mode { config: 0, width, height }), state: Mutex::default() }));
            id
        };
        let callback = self.client.lock().as_ref().and_then(|c| *c.callback.lock());
        eprintln!("[composer] display {id} added, {width}x{height}: {}", if callback.is_some() { "hotplug" } else { "before SurfaceFlinger" });
        if let Some(callback) = callback {
            IComposerCallbackProxy::new(Arc::clone(&self.broker), callback).on_hotplug(id, true).map_err(|e| format!("onHotplug: {e:?}"))?;
        }
        Ok((id, framebuffer))
    }

    /// Serve the composer and publish it with `servicemanager` as [`INSTANCE`].
    ///
    /// # Errors
    /// `servicemanager`'s refusal.
    pub fn register(self: &Arc<Self>) -> Result<(), String> {
        let me = Arc::clone(self);
        let ptr = self.broker.create_host_service_objects(move |call| i_composer::dispatch(&*me, call));
        self.broker.add_service_with_stability(INSTANCE, ptr, STABILITY_VINTF)
    }
}

impl IComposerServer for Composer {
    fn create_client(&self, _ctx: &Ctx<'_>) -> Result<Binder, Status> {
        let mut client = self.client.lock();
        if client.is_some() {
            // One client at a time, as the interface says.
            return Err(Status::ServiceSpecific(EX_NO_RESOURCES));
        }
        let c = Client::new(Arc::clone(&self.broker), Arc::clone(&self.screens), Arc::clone(&self.show_chrome));
        let serve = Arc::clone(&c);
        let trace = std::env::var("OMNI_COMPOSER_TRACE").as_deref() == Ok("1");
        let ptr = self.broker.create_host_service_objects(move |call| {
            if trace {
                eprintln!("[composer] IComposerClient transaction {}", call.code);
            }
            i_composer_client::dispatch(&*serve, call)
        });
        *client = Some(c);
        Ok(Binder::Host(ptr))
    }

    fn get_capabilities(&self, _ctx: &Ctx<'_>) -> Result<Vec<Capability>, Status> {
        Ok(if unreliable_present_fence() { vec![Capability::PRESENT_FENCE_IS_NOT_RELIABLE] } else { Vec::new() })
    }
}

/// A client target the client has sent in some slot.
struct Target {
    shm: Arc<Shm>,
    format: i32,
    width: u32,
    height: u32,
    stride: u32,
    pixels_at: u64,
}

/// A buffer a layer has sent in some slot.
struct LayerBuffer {
    shm: Arc<Shm>,
    format: i32,
    stride: u32,
    width: u32,
    height: u32,
    pixels_at: u64,
    /// The name its requestor gave the allocator (`hal::gralloc`'s metadata page): a BufferQueue's
    /// consumer name, which names the window it is for.
    name: String,
}

/// What the client last set on a layer (a command carries only what changed).
#[derive(Default)]
struct LayerState {
    buffers: HashMap<i32, LayerBuffer>,
    slot: Option<i32>,
    frame: Option<common::Rect>,
    crop: Option<common::FRect>,
    blend: Option<common::BlendMode>,
    alpha: Option<f32>,
    z: i32,
    transform: Option<common::Transform>,
    color: Option<[f32; 4]>,
}

impl LayerState {
    /// Whether the composer can compose this layer itself as `composition`.
    fn composable(&self, composition: Composition) -> bool {
        let Some(frame) = &self.frame else { return false };
        if frame.right <= frame.left || frame.bottom <= frame.top {
            return true; // nothing to draw
        }
        if composition == Composition::SOLID_COLOR {
            return self.color.is_some();
        }
        if composition != Composition::DEVICE || self.transform.is_some_and(|t| !(0..=7).contains(&t.0)) {
            return false;
        }
        let (Some(buffer), Some(crop)) = (self.slot.and_then(|s| self.buffers.get(&s)), &self.crop) else { return false };
        matches!(buffer.format, RGBA_8888 | RGBX_8888 | BGRA_8888 | IMPLEMENTATION_DEFINED)
            && crop.left >= 0.0
            && crop.top >= 0.0
            && crop.right > crop.left
            && crop.bottom > crop.top
            && crop.right <= buffer.width as f32
            && crop.bottom <= buffer.height as f32
    }

    /// Whether its buffer is shown at 1:1 from a whole-pixel corner, untransformed, as stored: the
    /// fast path (rows copied or blended as they are).
    fn one_to_one(&self) -> bool {
        let (Some(f), Some(c), Some(b)) = (&self.frame, &self.crop, self.slot.and_then(|s| self.buffers.get(&s))) else { return false };
        self.transform.is_none_or(|t| t == common::Transform::NONE)
            && b.format != BGRA_8888
            && c.left.fract() == 0.0
            && c.top.fract() == 0.0
            && ((c.right - c.left) - (f.right - f.left) as f32).abs() < 0.01
            && ((c.bottom - c.top) - (f.bottom - f.top) as f32).abs() < 0.01
    }
}

#[derive(Default)]
struct State {
    /// Each layer and the composition its client last asked for.
    layers: HashMap<i64, Composition>,
    targets: HashMap<i32, Target>,
    current_target: Option<i32>,
    refused_format: Option<i32>,
    /// Each layer's properties, for the composer's own composition.
    device: HashMap<i64, LayerState>,
    /// Whether the frame being presented is the composer's own (`DEVICE`), decided at validation.
    device_frame: bool,
    /// The chrome layers left out of the frame being presented, decided at validation.
    hidden: std::collections::HashSet<i64>,
    /// Frames presented by each path, for the `[composer]` line.
    frames_device: u64,
    frames_client: u64,
    /// The layer list `OMNI_COMPOSER_TRACE=layers` last printed.
    traced: String,
    /// Under `compose::FAST`: the last frame's layer pixels and composed frame, their memory used
    /// again rather than ~3.7 MB allocated (and faulted in, zeroed) per buffer per frame.
    scratch_layers: HashMap<i64, Vec<u8>>,
    scratch_out: Vec<u8>,
}

pub struct Client {
    broker: Arc<Broker>,
    screens: Screens,
    /// SurfaceFlinger's callback, and the counter that keeps layer ids unique across displays:
    /// the client's, not any one display's.
    callback: Mutex<Option<u32>>,
    next_layer: Mutex<i64>,
    vsync: Arc<AtomicBool>,
    show_chrome: Arc<AtomicBool>,
}

impl Client {
    fn new(broker: Arc<Broker>, screens: Screens, show_chrome: Arc<AtomicBool>) -> Arc<Self> {
        let c = Arc::new(Self { broker, screens, callback: Mutex::default(), next_layer: Mutex::default(), vsync: Arc::default(), show_chrome });
        // Vsync, every period while enabled, for as long as the client lives: paced to deadlines
        // (`Pacer`, `VSYNC_PACE`).
        static FROM_ENV: std::sync::Once = std::sync::Once::new();
        FROM_ENV.call_once(|| {
            if let Some(hz) = std::env::var("OMNI_VSYNC_HZ").ok().and_then(|v| v.trim().parse().ok()) {
                match set_vsync_hz(hz) {
                    Some(period) => eprintln!("[vsync] OMNI_VSYNC_HZ={hz}: period {period} ns"),
                    None => eprintln!("[vsync] OMNI_VSYNC_HZ={hz} is out of range (1..=1000): 60 Hz"),
                }
            }
        });
        let weak: Weak<Self> = Arc::downgrade(&c);
        let _ = std::thread::Builder::new().name("omni-composer-vsync".into()).spawn(move || {
            let period = || Duration::from_nanos(u64::try_from(vsync_period_ns()).unwrap_or(16_666_666).max(1));
            let mut pacer = Pacer::new(std::time::Instant::now(), period());
            let mut rate = VsyncRate::new();
            loop {
                let paced = VSYNC_PACE.load(Ordering::Relaxed);
                // `std::thread::sleep` is a high-resolution waitable timer on Windows 10 1803+
                // (Rust's own, since 1.75): a deadline is overshot by ~0.4 ms, not a 15.6 ms tick.
                let (due, missed) = if paced {
                    let (due, missed) = pacer.next(std::time::Instant::now(), period());
                    let now = std::time::Instant::now();
                    if due > now {
                        std::thread::sleep(due - now);
                    }
                    (due, missed)
                } else {
                    std::thread::sleep(period());
                    let now = std::time::Instant::now();
                    pacer = Pacer::new(now, period());
                    (now, 0)
                };
                let late = std::time::Instant::now().saturating_duration_since(due);
                rate.tick(late, missed, paced);
                // The vsync's time is its deadline, as a display's is when it happened rather than
                // when it was heard: SurfaceFlinger fits its model to these, and the wake-up's
                // jitter is not the display's.
                let now = crate::sys::monotonic().saturating_sub(late).as_nanos() as i64;
                LAST_VSYNC_NS.store(now, Ordering::Relaxed);
                let Some(c) = weak.upgrade() else { return };
                if !c.vsync.load(Ordering::Relaxed) {
                    continue;
                }
                let Some(callback) = *c.callback.lock() else { continue };
                rate.delivered += 1;
                // Every display vsyncs: SurfaceFlinger drives each one's frames from its own.
                let displays: Vec<i64> = c.screens.lock().keys().copied().collect();
                let proxy = IComposerCallbackProxy::new(Arc::clone(&c.broker), callback);
                for display in displays {
                    let _ = proxy.on_vsync(display, now, vsync_period_ns());
                }
            }
        });
        c
    }

    /// The display this call names, or `BAD_DISPLAY`.
    fn screen(&self, display: i64) -> Result<Arc<Screen>, Status> {
        self.screens.lock().get(&display).cloned().ok_or(Status::ServiceSpecific(EX_BAD_DISPLAY))
    }

    /// Present the frame, and answer the fences [`FENCES`] asks for into `results`.
    fn present_answering(&self, screen: &Screen, display: i64, results: &mut Vec<CommandResultPayload>) {
        self.present(screen);
        let fences = FENCES.load(Ordering::Relaxed);
        if fences == 0 {
            return;
        }
        let now = crate::sys::monotonic().as_nanos() as i64;
        let shown = vsync_at_or_after(now);
        results.push(CommandResultPayload::PresentFence(PresentFence { display, fence: Fd(crate::sync_file::signalled_at(shown as u64)) }));
        if fences < 2 {
            return;
        }
        // The layers whose buffers the composer read for this frame: done with now.
        let read: Vec<i64> = {
            let st = screen.state.lock();
            if !st.device_frame {
                return;
            }
            st.layers
                .iter()
                .filter(|(l, c)| **c == Composition::DEVICE && !st.hidden.contains(l) && st.device.get(l).is_some_and(|d| d.slot.is_some_and(|s| d.buffers.contains_key(&s))))
                .map(|(l, _)| *l)
                .collect()
        };
        if !read.is_empty() {
            let layers = read.into_iter().map(|layer| ReleaseFences_Layer { layer, fence: Fd(crate::sync_file::signalled_at(now as u64)) }).collect();
            results.push(CommandResultPayload::ReleaseFences(ReleaseFences { display, layers }));
        }
    }

    /// Present the frame: the composer's own composition of its layers, or the client target.
    fn present(&self, screen: &Screen) {
        let mut st = screen.state.lock();
        if st.device_frame {
            st.frames_device += 1;
            log_paths(&st);
            let mut order: Vec<(i32, i64)> = st.layers.keys().map(|l| (st.device.get(l).map_or(0, |d| d.z), *l)).collect();
            order.sort_unstable();
            super::compose::levers_from_env();
            // `present_zero`: shown by the window's GPU from the app's share images, when every
            // layer can be; else this frame is composed on the CPU as below.
            if crate::gpu::share::on() {
                if let Some(sink) = screen.framebuffer.sink() {
                    if present_shared(&st, screen, &order, &*sink) {
                        return;
                    }
                }
            }
            let bgra = super::compose::BGRA_OUT.load(Ordering::Relaxed);
            if super::compose::ZERO.load(Ordering::Relaxed) {
                // The copying path's buffers are not needed (~17 MB at 1575x890).
                if !st.scratch_out.is_empty() || !st.scratch_layers.is_empty() {
                    st.scratch_out = Vec::new();
                    st.scratch_layers = HashMap::new();
                }
                present_in_place(&st, screen, &order, bgra);
                return;
            }
            // Each buffer layer's pixels, read whole from its region (into last frame's memory for
            // that layer, under `compose::FAST`: `read_at` writes every byte the frame uses).
            let fast = super::compose::FAST.load(Ordering::Relaxed);
            let mut spare = if fast { std::mem::take(&mut st.scratch_layers) } else { HashMap::new() };
            let mut pixels: HashMap<i64, Vec<u8>> = HashMap::new();
            for &(_, l) in &order {
                let Some(d) = st.device.get(&l) else { continue };
                if st.layers.get(&l) != Some(&Composition::DEVICE) || st.hidden.contains(&l) {
                    continue;
                }
                if let Some(b) = d.slot.and_then(|s| d.buffers.get(&s)) {
                    // The app's copy into the buffer lands first, as a release fence is waited on
                    // (and a region the release left stale is filled: `region_lazy`).
                    crate::gpu::native::wait_written(&b.shm, std::time::Duration::from_millis(50));
                    crate::gpu::share::ensure_region(&b.shm, u64::from(b.stride) * 4);
                    let need = b.stride as usize * b.height as usize * 4;
                    let mut bytes = spare.remove(&l).unwrap_or_default();
                    if fast {
                        bytes.resize(need, 0);
                    } else {
                        bytes = vec![0u8; need];
                    }
                    if b.shm.read_at(&mut bytes, b.pixels_at).is_ok() {
                        pixels.insert(l, bytes);
                    }
                }
            }
            drop(spare);
            let layers: Vec<_> = order.iter().filter_map(|&(_, l)| layer_of(&st, l, pixels.get(&l).map(Vec::as_slice))).collect();
            let Mode { width, height, .. } = screen.mode();
            let need = width as usize * height as usize * 4;
            let mut out = if fast { std::mem::take(&mut st.scratch_out) } else { Vec::new() };
            out.resize(need, 0);
            super::compose::compose_with(&mut out, width as usize, height as usize, &layers, fast);
            drop(layers);
            if fast {
                st.scratch_layers = pixels;
            }
            drop(st);
            if bgra {
                // The frame BGRA for the window: swapped as it is copied in.
                screen.framebuffer.present_with(width, height, true, |dst| {
                    for (d, s) in dst.chunks_exact_mut(4).zip(out.chunks_exact(4)) {
                        d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
                    }
                });
            } else {
                screen.framebuffer.present_frame(&out, width, height, width);
            }
            if fast {
                screen.state.lock().scratch_out = out;
            }
            return;
        }
        st.frames_client += 1;
        log_paths(&st);
        let Some(target) = st.current_target.and_then(|slot| st.targets.get(&slot)) else { return };
        if !matches!(target.format, RGBA_8888 | RGBX_8888 | IMPLEMENTATION_DEFINED) {
            if st.refused_format != Some(target.format) {
                eprintln!("[composer] a client target of pixel format {:#x} is not presented (RGBA_8888 only)", target.format);
                st.refused_format = Some(target.format);
            }
            return;
        }
        // The target's own size: the display's, except for a frame SurfaceFlinger drew for the
        // size before a resize.
        let (width, height) = (target.width, target.height);
        // (A client target is never left stale -- the composer never takes it from a share image --
        // but whatever reads a region makes sure: `region_lazy`.)
        crate::gpu::share::ensure_region(&target.shm, u64::from(target.stride) * 4);
        let mut pixels = vec![0u8; target.stride as usize * height as usize * 4];
        if target.shm.read_at(&mut pixels, target.pixels_at).is_err() {
            return;
        }
        let stride = target.stride;
        // OMNI_COMPOSER_TRACE=2: each frame presented, its slot and two pixels (centre, corner).
        static FRAMES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *FRAMES.get_or_init(|| std::env::var("OMNI_COMPOSER_TRACE").as_deref() == Ok("2")) {
            let px = |x: usize, y: usize| {
                let at = (y * stride as usize + x) * 4;
                u32::from_be_bytes(pixels[at..at + 4].try_into().expect("4"))
            };
            eprintln!("[composer] present slot {:?}: centre {:08x} corner {:08x}", st.current_target, px(width as usize / 2, height as usize / 2), px(8, 8));
        }
        drop(st);
        screen.framebuffer.present_frame(&pixels, width, height, stride);
    }
}

/// Layer `l` of the frame as `super::compose` takes it, its buffer's pixels `data`: `None` for a
/// layer not drawn (hidden chrome, no frame, a buffer layer without its pixels or crop).
fn layer_of<'a>(st: &State, l: i64, data: Option<&'a [u8]>) -> Option<super::compose::Layer<'a>> {
    use super::compose::{Blend, Layer, Source};
    let d = st.device.get(&l)?;
    let f = d.frame.as_ref()?;
    if st.hidden.contains(&l) {
        return None;
    }
    let blend = match d.blend {
        Some(common::BlendMode::NONE) => Blend::None,
        Some(common::BlendMode::COVERAGE) => Blend::Coverage,
        _ => Blend::Premultiplied,
    };
    let source = if st.layers.get(&l) == Some(&Composition::SOLID_COLOR) {
        Source::Color(d.color.unwrap_or_default())
    } else {
        let (b, data, c) = (d.slot.and_then(|s| d.buffers.get(&s))?, data?, d.crop.as_ref()?);
        if d.one_to_one() {
            Source::Pixels { data, stride: b.stride as usize, opaque: b.format == RGBX_8888, crop_x: c.left as usize, crop_y: c.top as usize }
        } else {
            Source::Mapped {
                data,
                stride: b.stride as usize,
                rows: b.height as usize,
                opaque: b.format == RGBX_8888,
                bgra: b.format == BGRA_8888,
                crop: (c.left, c.top, c.right, c.bottom),
                transform: d.transform.map_or(0, |t| t.0 as u32),
            }
        }
    };
    Some(Layer { source, frame: (f.left, f.top, f.right, f.bottom), blend, alpha: d.alpha.unwrap_or(1.0) })
}

/// A layer's pixels for [`present_in_place`]: its region's view, borrowed, or (a region with no
/// view) read out as before.
enum LayerPixels<'a> {
    Borrowed(crate::shm::ShmBytes<'a>),
    Read(Vec<u8>),
}

impl LayerPixels<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Borrowed(b) => b,
            Self::Read(v) => v,
        }
    }
}

/// A layer of a frame shown from share images, kept for composing its pixels on the CPU should a
/// reader ask (`Framebuffer::present_external`): its region and how it is drawn.
struct OwnedLayer {
    shm: Arc<Shm>,
    pixels_at: u64,
    stride: usize,
    rows: usize,
    opaque: bool,
    bgra: bool,
    one_to_one: bool,
    crop: (f32, f32, f32, f32),
    frame: (i32, i32, i32, i32),
    blend: super::compose::Blend,
    alpha: f32,
}

/// The CPU's composition of `layers` into `out` (RGBA, `width` x `height`): what the window was
/// shown, from the regions' pixels as they are now.
fn compose_owned(layers: &[OwnedLayer], out: &mut [u8], width: u32, height: u32) {
    use super::compose::{Layer, Source};
    let pixels: Vec<LayerPixels<'_>> = layers
        .iter()
        .map(|l| {
            crate::gpu::native::wait_written(&l.shm, std::time::Duration::from_millis(50));
            crate::gpu::share::ensure_region(&l.shm, l.stride as u64 * 4);
            let need = l.stride * l.rows * 4;
            l.shm.bytes(l.pixels_at, need).map_or_else(
                || {
                    let mut v = vec![0u8; need];
                    let _ = l.shm.read_at(&mut v, l.pixels_at);
                    LayerPixels::Read(v)
                },
                LayerPixels::Borrowed,
            )
        })
        .collect();
    let drawn: Vec<Layer<'_>> = layers
        .iter()
        .zip(&pixels)
        .map(|(l, p)| {
            let data = p.bytes();
            let source = if l.one_to_one {
                Source::Pixels { data, stride: l.stride, opaque: l.opaque, crop_x: l.crop.0 as usize, crop_y: l.crop.1 as usize }
            } else {
                Source::Mapped { data, stride: l.stride, rows: l.rows, opaque: l.opaque, bgra: l.bgra, crop: l.crop, transform: 0 }
            };
            Layer { source, frame: l.frame, blend: l.blend, alpha: l.alpha }
        })
        .collect();
    super::compose::compose_into(out, width as usize, height as usize, &drawn, super::compose::Opts { fast: true, runs: true, bgra: false });
}

/// Frames shown from share images and composed on the CPU instead, while `present_zero` is on,
/// and why the last one was not (for the `[composer] present_zero` line).
static ZERO_FRAMES: [std::sync::atomic::AtomicU64; 2] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];
static ZERO_WHY: Mutex<&str> = Mutex::new("");

/// **A frame shown from the app's share images** (`present_zero`, `crate::gpu::share`): when every
/// visible layer is a buffer layer with a share image holding the generation its region holds, at
/// plane alpha 1 and untransformed, the window's GPU composes them (`sink`) and the framebuffer
/// gets the frame without its pixels -- composed on the CPU only if someone reads them. False, with
/// nothing presented: the CPU composes this frame.
fn present_shared(st: &State, screen: &Screen, order: &[(i32, i64)], sink: &dyn super::framebuffer::ZeroSink) -> bool {
    use crate::gpu::share::{self, ShareBlend, ShareDesc, ShareLayer};
    let why = (|| -> Result<(Vec<ShareLayer>, Vec<OwnedLayer>), &'static str> {
        let (mut shared, mut owned) = (Vec::new(), Vec::new());
        for &(_, l) in order {
            let Some(d) = st.device.get(&l) else { continue };
            let Some(f) = &d.frame else { continue };
            if st.hidden.contains(&l) || f.right <= f.left || f.bottom <= f.top {
                continue;
            }
            let alpha = d.alpha.unwrap_or(1.0);
            if alpha <= 0.0 {
                continue; // drawn by neither path
            }
            if st.layers.get(&l) != Some(&Composition::DEVICE) {
                return Err("a layer that is not a buffer (a solid colour)");
            }
            if alpha < 254.5 / 255.0 {
                return Err("a layer with plane alpha below 1");
            }
            if d.transform.is_some_and(|t| t != common::Transform::NONE) {
                return Err("a transformed layer");
            }
            let (Some(b), Some(c)) = (d.slot.and_then(|s| d.buffers.get(&s)), &d.crop) else { return Err("a layer without its buffer or crop") };
            crate::gpu::native::wait_written(&b.shm, std::time::Duration::from_millis(50));
            let Some(desc) = ShareDesc::read(&b.shm) else { return Err("a buffer with no share image (not the app's Vulkan, or OMNI_PRESENT_ZERO not ready there)") };
            if (desc.width, desc.height) != (b.width, b.height) {
                return Err("a share image of another size than its buffer");
            }
            let mut g = [0u8; 8];
            let _ = b.shm.read_at(&mut g, crate::gpu::native::CONTENT_GENERATION_AT);
            let generation = share::read_generation(&b.shm);
            if generation == 0 || generation != u64::from_le_bytes(g) {
                return Err("a share image not holding its buffer's frame (copied with present_zero off)");
            }
            let blend = match d.blend {
                Some(common::BlendMode::NONE) => (ShareBlend::None, super::compose::Blend::None),
                Some(common::BlendMode::COVERAGE) => (ShareBlend::Coverage, super::compose::Blend::Coverage),
                _ => (ShareBlend::Premultiplied, super::compose::Blend::Premultiplied),
            };
            let crop = (c.left, c.top, c.right, c.bottom);
            let frame = (f.left, f.top, f.right, f.bottom);
            let opaque = b.format == RGBX_8888;
            shared.push(ShareLayer { desc, opaque, crop, frame, blend: blend.0, generation });
            owned.push(OwnedLayer {
                shm: Arc::clone(&b.shm),
                pixels_at: b.pixels_at,
                stride: b.stride as usize,
                rows: b.height as usize,
                opaque,
                bgra: b.format == BGRA_8888,
                one_to_one: d.one_to_one(),
                crop,
                frame,
                blend: blend.1,
                alpha,
            });
        }
        if shared.is_empty() {
            return Err("no layer to draw");
        }
        if shared.len() > crate::gpu::window_present::MAX_LAYERS {
            return Err("more layers than the GPU composition takes");
        }
        Ok((shared, owned))
    })();
    let Mode { width, height, .. } = screen.mode();
    let outcome = why.and_then(|(shared, owned)| {
        sink.present(&shared, (width, height)).map_err(|e| {
            static SAID: AtomicBool = AtomicBool::new(false);
            if !SAID.swap(true, Ordering::Relaxed) {
                eprintln!("[composer] present_zero: the window could not show a frame ({e}); composed on the CPU");
            }
            "the window could not show it"
        })?;
        Ok(owned)
    });
    let done = match outcome {
        Ok(owned) => {
            // Each buffer's frame was taken from its share image: its next release may leave the
            // region stale (`region_lazy`).
            for l in &owned {
                let mut g = [0u8; 8];
                let _ = l.shm.read_at(&mut g, crate::gpu::native::CONTENT_GENERATION_AT);
                crate::gpu::share::mark_taken(&l.shm, u64::from_le_bytes(g));
            }
            screen.framebuffer.present_external(width, height, Arc::new(move |out: &mut [u8]| compose_owned(&owned, out, width, height)));
            true
        }
        Err(why) => {
            *ZERO_WHY.lock() = why;
            false
        }
    };
    let n = ZERO_FRAMES[usize::from(!done)].fetch_add(1, Ordering::Relaxed) + 1;
    let (gpu, cpu) = (ZERO_FRAMES[0].load(Ordering::Relaxed), ZERO_FRAMES[1].load(Ordering::Relaxed));
    if (gpu + cpu) % 600 == 0 || (n == 1 && (gpu + cpu) <= 2) {
        eprintln!("[composer] present_zero: {gpu} frames shown from share images by the window's GPU, {cpu} composed on the CPU (last reason: {})", *ZERO_WHY.lock());
    }
    done
}

/// **The composer's frame without copies** (`compose_zero`, [`super::compose::ZERO`]): each
/// layer's pixels read where they are in its gralloc region, once every layer's release copy has
/// landed (the same wait as the copying path's, and the same moment of reading), composed straight
/// into the framebuffer's next frame by the run path, RGBA or (`bgra`) BGRA (measured in
/// `hal::compose`'s `frame_cost`).
fn present_in_place(st: &State, screen: &Screen, order: &[(i32, i64)], bgra: bool) {
    let buffers: Vec<(i64, &LayerBuffer)> = order
        .iter()
        .filter(|(_, l)| st.layers.get(l) == Some(&Composition::DEVICE) && !st.hidden.contains(l))
        .filter_map(|&(_, l)| {
            let d = st.device.get(&l)?;
            Some((l, d.slot.and_then(|s| d.buffers.get(&s))?))
        })
        .collect();
    // Every copy lands before any buffer is read (as a release fence is waited on).
    for (_, b) in &buffers {
        crate::gpu::native::wait_written(&b.shm, std::time::Duration::from_millis(50));
        crate::gpu::share::ensure_region(&b.shm, u64::from(b.stride) * 4);
    }
    let mut pixels: HashMap<i64, LayerPixels<'_>> = HashMap::new();
    for (l, b) in buffers {
        let need = b.stride as usize * b.height as usize * 4;
        let px = match b.shm.bytes(b.pixels_at, need) {
            Some(bytes) => LayerPixels::Borrowed(bytes),
            None => {
                let mut v = vec![0u8; need];
                if b.shm.read_at(&mut v, b.pixels_at).is_err() {
                    continue;
                }
                LayerPixels::Read(v)
            }
        };
        pixels.insert(l, px);
    }
    let layers: Vec<_> = order.iter().filter_map(|&(_, l)| layer_of(st, l, pixels.get(&l).map(LayerPixels::bytes))).collect();
    let Mode { width, height, .. } = screen.mode();
    let opts = super::compose::Opts { fast: true, runs: true, bgra };
    screen.framebuffer.present_with(width, height, bgra, |out| super::compose::compose_into(out, width as usize, height as usize, &layers, opts));
}

/// `OMNI_COMPOSER_TRACE=layers`: the frame's layers, bottom first, each time their list or their
/// geometry changes -- what each is (its buffer's name), where, and whether the composer can take it.
fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_COMPOSER_TRACE").as_deref() == Ok("layers"))
}

fn trace_layers(st: &mut State) {
    if !trace_on() {
        return;
    }
    let mut order: Vec<(i32, i64)> = st.layers.keys().map(|l| (st.device.get(l).map_or(0, |d| d.z), *l)).collect();
    order.sort_unstable();
    let mut text = String::new();
    for (z, l) in order {
        let c = st.layers[&l];
        let Some(d) = st.device.get(&l) else {
            text.push_str(&format!("  layer {l} z {z} {c:?}: nothing set\n"));
            continue;
        };
        let buffer = d.slot.and_then(|s| d.buffers.get(&s)).map_or_else(
            || {
                let mut held: Vec<_> = d.buffers.keys().collect();
                held.sort_unstable();
                format!("no buffer (slot {:?}; slots held {held:?})", d.slot)
            },
            |b| format!("{:?} {}x{} fmt {:#x} stride {}", b.name, b.width, b.height, b.format, b.stride),
        );
        let frame = d.frame.as_ref().map(|f| format!("[{},{} {},{}]", f.left, f.top, f.right, f.bottom));
        let crop = d.crop.as_ref().map(|c| format!("[{},{} {},{}]", c.left, c.top, c.right, c.bottom));
        text.push_str(&format!(
            "  layer {l} z {z} {c:?} {}{} frame {} crop {} transform {:?} blend {:?} alpha {:?} colour {:?}: {buffer}\n",
            if d.composable(c) { "ours" } else { "NOT OURS" },
            if st.hidden.contains(&l) { " HIDDEN" } else { "" },
            frame.unwrap_or_default(),
            crop.unwrap_or_default(),
            d.transform,
            d.blend,
            d.alpha,
            d.color,
        ));
    }
    if text != st.traced {
        eprint!("[composer] layers ({} frame):\n{text}", if st.device_frame { "composer's" } else { "SurfaceFlinger's" });
        st.traced = text;
    }
}

/// Why the composer cannot compose layer `d` as `c` itself, in a few words.
fn not_ours_because(d: &LayerState, c: Composition) -> String {
    if c != Composition::DEVICE && c != Composition::SOLID_COLOR {
        // SurfaceFlinger's own choice (AOSP 15 `OutputLayer`: rounded corners, a shadow or
        // stretch, a secure layer, an unsupported dataspace or colour transform, a blur).
        return format!("SurfaceFlinger asked for {c:?}");
    }
    let Some(frame) = &d.frame else { return "no display frame".into() };
    if frame.right <= frame.left || frame.bottom <= frame.top {
        return "empty frame".into();
    }
    if c == Composition::SOLID_COLOR {
        return "a solid colour with no colour".into();
    }
    if d.transform.is_some_and(|t| !(0..=7).contains(&t.0)) {
        return format!("transform {:?}", d.transform);
    }
    let Some(buffer) = d.slot.and_then(|s| d.buffers.get(&s)) else { return format!("no buffer in slot {:?}", d.slot) };
    if !matches!(buffer.format, RGBA_8888 | RGBX_8888 | BGRA_8888 | IMPLEMENTATION_DEFINED) {
        return format!("pixel format {:#x}", buffer.format);
    }
    match &d.crop {
        None => "no source crop".into(),
        Some(c) => format!("crop [{},{} {},{}] outside its {}x{} buffer", c.left, c.top, c.right, c.bottom, buffer.width, buffer.height),
    }
}

/// **A frame SurfaceFlinger must compose** (`CLIENT`: RenderEngine on the host GPU through the
/// forwarded GLES, every layer of the frame -- the expensive path): said once for each layer name
/// and reason (`[composer] CLIENT composition: ...`), at most 64 of them a process.
fn log_client_reason(st: &State, hidden: &std::collections::HashSet<i64>) {
    static SAID: std::sync::LazyLock<Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Mutex::default);
    let Some((l, c)) = st.layers.iter().filter(|(l, _)| !hidden.contains(l)).find(|(l, c)| st.device.get(l).is_none_or(|d| !d.composable(**c))) else { return };
    let (name, why) = match st.device.get(l) {
        None => (String::new(), "nothing set on it".to_string()),
        Some(d) => (d.slot.and_then(|s| d.buffers.get(&s)).map_or_else(String::new, |b| b.name.clone()), not_ours_because(d, *c)),
    };
    let key = format!("{name:?}: {why}");
    let mut said = SAID.lock();
    if said.len() < 64 && said.insert(key.clone()) {
        eprintln!("[composer] CLIENT composition (SurfaceFlinger composes the frame, {} layers): layer {l} {key}", st.layers.len());
    }
}

/// Every 600 frames: how many were the composer's own and how many SurfaceFlinger's.
fn log_paths(st: &State) {
    if (st.frames_device + st.frames_client) % 600 == 0 {
        eprintln!("[composer] frames composed here {}, by SurfaceFlinger {}", st.frames_device, st.frames_client);
    }
}

/// A layer's buffer a gralloc handle names.
fn layer_buffer_of(handle: &NativeHandle) -> Option<LayerBuffer> {
    if handle.fds.len() != 1 || handle.ints.len() != HANDLE_INTS || handle.ints[0] as u32 != 0x4247_4d4f {
        return None;
    }
    let Fd(file) = &handle.fds[0];
    let shm = match &*file.kind.lock() {
        FileKind::Shared(m) => Arc::clone(m),
        _ => return None,
    };
    let (width, height, format, stride, pixels_at) = (handle.ints[2] as u32, handle.ints[3] as u32, handle.ints[5], handle.ints[8] as u32, handle.ints[13] as u32 as u64);
    if stride < width || pixels_at != PIXELS_AT {
        return None;
    }
    shm.as_graphics_buffer();
    let mut name = [0u8; 128];
    let _ = shm.read_at(&mut name, NAME_AT);
    let name = String::from_utf8_lossy(&name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())]).into_owned();
    Some(LayerBuffer { shm, format, stride, width, height, pixels_at, name })
}

/// The client target a gralloc handle names (D2's 14 ints and one region).
fn target_of(handle: &NativeHandle) -> Option<Target> {
    if handle.fds.len() != 1 || handle.ints.len() != HANDLE_INTS || handle.ints[0] as u32 != 0x4247_4d4f {
        return None;
    }
    let Fd(file) = &handle.fds[0];
    let shm = match &*file.kind.lock() {
        FileKind::Shared(m) => Arc::clone(m),
        _ => return None,
    };
    let (width, height, format, stride, pixels_at) = (handle.ints[2] as u32, handle.ints[3] as u32, handle.ints[5], handle.ints[8] as u32, handle.ints[13] as u32 as u64);
    let target = (width > 0 && height > 0 && stride >= width && pixels_at == PIXELS_AT).then_some(Target { shm, format, width, height, stride, pixels_at });
    if let Some(t) = &target {
        t.shm.as_graphics_buffer();
    }
    target
}

impl IComposerClientServer for Client {
    fn register_callback(&self, _ctx: &Ctx<'_>, callback: Binder) -> Result<(), Status> {
        let Binder::Handle(handle) = callback else { return Err(Status::ServiceSpecific(EX_BAD_LAYER)) };
        *self.callback.lock() = Some(handle);
        // Every display is connected from the start: SurfaceFlinger's init needs the hotplug to
        // have arrived by the time registerCallback returns. The call goes to the very thread
        // waiting on this one (the broker routes a host service's call to its caller as nested).
        let displays: Vec<i64> = self.screens.lock().keys().copied().collect();
        let proxy = IComposerCallbackProxy::new(Arc::clone(&self.broker), handle);
        for display in displays {
            let _ = proxy.on_hotplug(display, true);
        }
        Ok(())
    }

    fn create_layer(&self, _ctx: &Ctx<'_>, display: i64, _buffer_slot_count: i32) -> Result<i64, Status> {
        let screen = self.screen(display)?;
        let id = {
            let mut next = self.next_layer.lock();
            *next += 1;
            *next
        };
        screen.state.lock().layers.insert(id, Composition::CLIENT);
        Ok(id)
    }

    fn destroy_layer(&self, _ctx: &Ctx<'_>, display: i64, layer: i64) -> Result<(), Status> {
        let screen = self.screen(display)?;
        let mut st = screen.state.lock();
        st.device.remove(&layer);
        st.layers.remove(&layer).map(|_| ()).ok_or(Status::ServiceSpecific(EX_BAD_LAYER))
    }

    fn execute_commands(&self, _ctx: &Ctx<'_>, commands: Vec<DisplayCommand>) -> Result<Vec<CommandResultPayload>, Status> {
        let mut results = Vec::new();
        for (index, cmd) in commands.into_iter().enumerate() {
            let Ok(screen) = self.screen(cmd.display) else {
                results.push(CommandResultPayload::Error(CommandError { command_index: index as i32, error_code: EX_BAD_DISPLAY }));
                continue;
            };
            {
                let mut st = screen.state.lock();
                for layer in &cmd.layers {
                    if let Some(c) = &layer.composition {
                        st.layers.insert(layer.layer, c.composition);
                    }
                    let d = st.device.entry(layer.layer).or_default();
                    // The slots SurfaceFlinger frees first, then the command's buffer: a buffer
                    // cache that frees a slot and fills it again in one command (a swapchain made
                    // anew as a game is joined) means the new buffer. Cleared after it, slot 0
                    // held nothing, and every third frame went to SurfaceFlinger (in-world PS99,
                    // 2026-09-28: "no buffer (slot Some(0); slots held [1, 2])").
                    for slot in layer.buffer_slots_to_clear.iter().flatten() {
                        d.buffers.remove(slot);
                    }
                    if trace_on() && layer.buffer_slots_to_clear.as_ref().is_some_and(|s| !s.is_empty()) {
                        eprintln!(
                            "[composer] layer {}: slots cleared {:?}; this command's buffer: {:?}",
                            layer.layer,
                            layer.buffer_slots_to_clear,
                            layer.buffer.as_ref().map(|b| (b.slot, b.handle.is_some()))
                        );
                    }
                    if let Some(buffer) = &layer.buffer {
                        match buffer.handle.as_ref().map(|h| (h, layer_buffer_of(h))) {
                            Some((_, Some(b))) => {
                                d.buffers.insert(buffer.slot, b);
                            }
                            // A handle the composer cannot take (`OMNI_COMPOSER_TRACE=layers`
                            // names it): the slot then holds nothing, and SurfaceFlinger composes.
                            Some((h, None)) if trace_on() => eprintln!(
                                "[composer] layer {} slot {}: a buffer not taken: {} fds ({}), {} ints {:x?}",
                                layer.layer,
                                buffer.slot,
                                h.fds.len(),
                                h.fds.iter().map(|Fd(f)| String::from_utf8_lossy(&crate::fd::guest_path_of(f)).into_owned()).collect::<Vec<_>>().join(", "),
                                h.ints.len(),
                                h.ints
                            ),
                            _ => {}
                        }
                        d.slot = Some(buffer.slot);
                    }
                    if let Some(f) = &layer.display_frame {
                        d.frame = Some(f.clone());
                    }
                    if let Some(c) = &layer.source_crop {
                        d.crop = Some(c.clone());
                    }
                    if let Some(b) = &layer.blend_mode {
                        d.blend = Some(b.blend_mode);
                    }
                    if let Some(a) = &layer.plane_alpha {
                        d.alpha = Some(a.alpha);
                    }
                    if let Some(z) = &layer.z {
                        d.z = z.z;
                    }
                    if let Some(t) = &layer.transform {
                        d.transform = Some(t.transform);
                    }
                    if let Some(c) = &layer.color {
                        d.color = Some([c.r, c.g, c.b, c.a]);
                    }
                }
                if let Some(ct) = &cmd.client_target {
                    if let Some(handle) = &ct.buffer.handle {
                        match target_of(handle) {
                            Some(t) => {
                                st.targets.insert(ct.buffer.slot, t);
                            }
                            None => results.push(CommandResultPayload::Error(CommandError { command_index: index as i32, error_code: EX_UNSUPPORTED })),
                        }
                    }
                    st.current_target = Some(ct.buffer.slot);
                }
            }
            if cmd.validate_display || cmd.present_or_validate_display {
                // The composer's own composition when it can take every layer.
                static DEVICE_OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                let off = *DEVICE_OFF.get_or_init(|| std::env::var("OMNI_COMPOSER_DEVICE").as_deref() == Ok("0"));
                let hide = !self.show_chrome.load(Ordering::Relaxed);
                let device = {
                    let mut st = screen.state.lock();
                    // The chrome left out: layers the composer was asked to draw itself.
                    let hidden: std::collections::HashSet<i64> = if hide {
                        st.layers
                            .iter()
                            .filter(|(l, c)| **c == Composition::DEVICE && st.device.get(l).and_then(|d| d.slot.and_then(|s| d.buffers.get(&s))).is_some_and(|b| is_chrome(&b.name)))
                            .map(|(l, _)| *l)
                            .collect()
                    } else {
                        std::collections::HashSet::new()
                    };
                    let all = !off && !st.layers.is_empty() && st.layers.iter().filter(|(l, _)| !hidden.contains(l)).all(|(l, c)| st.device.get(l).is_some_and(|d| d.composable(*c)));
                    if !all && !off {
                        log_client_reason(&st, &hidden);
                    }
                    st.hidden = hidden;
                    st.device_frame = all;
                    trace_layers(&mut st);
                    all
                };
                if device {
                    if cmd.present_or_validate_display && !cmd.present_display && skip_validate() {
                        // Nothing to change: presented now, as a hardware composer does.
                        self.present_answering(&screen, cmd.display, &mut results);
                        results.push(CommandResultPayload::PresentOrValidateResult(PresentOrValidate { display: cmd.display, result: PresentOrValidate_Result::Presented }));
                        continue;
                    }
                    if cmd.present_or_validate_display {
                        results.push(CommandResultPayload::PresentOrValidateResult(PresentOrValidate { display: cmd.display, result: PresentOrValidate_Result::Validated }));
                    }
                    if cmd.present_display {
                        self.present_answering(&screen, cmd.display, &mut results);
                    }
                    continue;
                }
                // Every layer is composed by the client -- but the chrome left out, which stays the
                // composer's (and is not drawn).
                let changed: Vec<ChangedCompositionLayer> = {
                    let mut st = screen.state.lock();
                    let State { layers, hidden, .. } = &mut *st;
                    let mut changed = Vec::new();
                    for (l, c) in layers.iter_mut() {
                        if *c != Composition::CLIENT && !hidden.contains(l) {
                            changed.push(ChangedCompositionLayer { layer: *l, composition: Composition::CLIENT });
                            *c = Composition::CLIENT;
                        }
                    }
                    changed
                };
                if !changed.is_empty() {
                    results.push(CommandResultPayload::ChangedCompositionTypes(ChangedCompositionTypes { display: cmd.display, layers: changed }));
                }
                if cmd.present_or_validate_display {
                    results.push(CommandResultPayload::PresentOrValidateResult(PresentOrValidate { display: cmd.display, result: PresentOrValidate_Result::Validated }));
                }
            }
            if cmd.present_display {
                self.present_answering(&screen, cmd.display, &mut results);
            }
        }
        Ok(results)
    }

    fn get_active_config(&self, _ctx: &Ctx<'_>, display: i64) -> Result<i32, Status> {
        self.screen(display).map(|s| s.mode().config)
    }

    fn get_color_modes(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<ColorMode>, Status> {
        self.screen(display).map(|_| vec![ColorMode::NATIVE])
    }

    fn get_dataspace_saturation_matrix(&self, _ctx: &Ctx<'_>, _dataspace: common::Dataspace) -> Result<Vec<f32>, Status> {
        Ok((0..16).map(|i| if i % 5 == 0 { 1.0 } else { 0.0 }).collect())
    }

    fn get_display_attribute(&self, _ctx: &Ctx<'_>, display: i64, config: i32, attribute: DisplayAttribute) -> Result<i32, Status> {
        let screen = self.screen(display)?;
        let mode = screen.mode();
        if config != mode.config {
            return Err(Status::ServiceSpecific(1)); // EX_BAD_CONFIG
        }
        Ok(match attribute {
            DisplayAttribute::WIDTH => mode.width as i32,
            DisplayAttribute::HEIGHT => mode.height as i32,
            DisplayAttribute::VSYNC_PERIOD => vsync_period_ns(),
            DisplayAttribute::DPI_X | DisplayAttribute::DPI_Y => (DPI * 1000.0) as i32,
            DisplayAttribute::CONFIG_GROUP => 0,
            _ => return Err(Status::ServiceSpecific(4)), // EX_BAD_PARAMETER
        })
    }

    fn get_display_capabilities(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<DisplayCapability>, Status> {
        self.screen(display).map(|_| Vec::new())
    }

    fn get_display_configs(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<i32>, Status> {
        self.screen(display).map(|s| vec![s.mode().config])
    }

    fn get_display_configurations(&self, _ctx: &Ctx<'_>, display: i64, _max_frame_interval_ns: i32) -> Result<Vec<DisplayConfiguration>, Status> {
        let screen = self.screen(display)?;
        let mode = screen.mode();
        Ok(vec![DisplayConfiguration {
            config_id: mode.config,
            width: mode.width as i32,
            height: mode.height as i32,
            dpi: Some(DisplayConfiguration_Dpi { x: DPI, y: DPI }),
            config_group: 0,
            vsync_period: vsync_period_ns(),
            vrr_config: None,
        }])
    }

    fn get_display_connection_type(&self, _ctx: &Ctx<'_>, display: i64) -> Result<DisplayConnectionType, Status> {
        // Display 0 is the device's own; the rest are attached to it, which is what SurfaceFlinger
        // expects of a second display and what keeps display 0 the primary.
        self.screen(display).map(|_| if display == DISPLAY { DisplayConnectionType::INTERNAL } else { DisplayConnectionType::EXTERNAL })
    }

    fn get_display_identification_data(&self, _ctx: &Ctx<'_>, display: i64) -> Result<super::aidl::android_hardware_graphics_composer3::DisplayIdentification, Status> {
        self.screen(display)?;
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn get_display_name(&self, _ctx: &Ctx<'_>, display: i64) -> Result<String, Status> {
        self.screen(display).map(|_| if display == DISPLAY { "omnidroid".to_string() } else { format!("omnidroid-{display}") })
    }

    fn get_display_vsync_period(&self, _ctx: &Ctx<'_>, display: i64) -> Result<i32, Status> {
        self.screen(display).map(|_| vsync_period_ns())
    }

    fn get_display_physical_orientation(&self, _ctx: &Ctx<'_>, display: i64) -> Result<common::Transform, Status> {
        self.screen(display).map(|_| common::Transform::NONE)
    }

    fn get_hdr_capabilities(&self, _ctx: &Ctx<'_>, display: i64) -> Result<HdrCapabilities, Status> {
        self.screen(display).map(|_| HdrCapabilities::default())
    }

    fn get_max_virtual_display_count(&self, _ctx: &Ctx<'_>) -> Result<i32, Status> {
        Ok(0)
    }

    fn get_per_frame_metadata_keys(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<PerFrameMetadataKey>, Status> {
        self.screen(display).map(|_| Vec::new())
    }

    fn get_render_intents(&self, _ctx: &Ctx<'_>, display: i64, _mode: ColorMode) -> Result<Vec<RenderIntent>, Status> {
        self.screen(display).map(|_| vec![RenderIntent::COLORIMETRIC])
    }

    fn get_supported_content_types(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Vec<ContentType>, Status> {
        self.screen(display).map(|_| Vec::new())
    }

    fn get_display_decoration_support(&self, _ctx: &Ctx<'_>, display: i64) -> Result<Option<common::DisplayDecorationSupport>, Status> {
        self.screen(display).map(|_| None)
    }

    fn set_active_config(&self, _ctx: &Ctx<'_>, display: i64, config: i32) -> Result<(), Status> {
        let screen = self.screen(display)?;
        if config == screen.mode().config { Ok(()) } else { Err(Status::ServiceSpecific(1)) }
    }

    /// The one configuration there is, at once: nothing to wait for and no refresh needed.
    fn set_active_config_with_constraints(&self, _ctx: &Ctx<'_>, display: i64, config: i32, _constraints: VsyncPeriodChangeConstraints) -> Result<VsyncPeriodChangeTimeline, Status> {
        let screen = self.screen(display)?;
        if config != screen.mode().config {
            return Err(Status::ServiceSpecific(1)); // EX_BAD_CONFIG
        }
        let now = crate::sys::monotonic().as_nanos() as i64;
        Ok(VsyncPeriodChangeTimeline { new_vsync_applied_time_nanos: now, refresh_required: false, refresh_time_nanos: 0 })
    }

    fn get_preferred_boot_display_config(&self, _ctx: &Ctx<'_>, display: i64) -> Result<i32, Status> {
        self.screen(display)?;
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn set_auto_low_latency_mode(&self, _ctx: &Ctx<'_>, display: i64, _on: bool) -> Result<(), Status> {
        self.screen(display)?;
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn set_client_target_slot_count(&self, _ctx: &Ctx<'_>, display: i64, _count: i32) -> Result<(), Status> {
        self.screen(display).map(|_| ())
    }

    fn set_color_mode(&self, _ctx: &Ctx<'_>, display: i64, mode: ColorMode, _intent: RenderIntent) -> Result<(), Status> {
        self.screen(display)?;
        if mode == ColorMode::NATIVE { Ok(()) } else { Err(Status::ServiceSpecific(EX_UNSUPPORTED)) }
    }

    fn set_content_type(&self, _ctx: &Ctx<'_>, display: i64, _type: ContentType) -> Result<(), Status> {
        self.screen(display).map(|_| ())
    }

    fn set_power_mode(&self, _ctx: &Ctx<'_>, display: i64, _mode: PowerMode) -> Result<(), Status> {
        self.screen(display).map(|_| ())
    }

    fn set_vsync_enabled(&self, _ctx: &Ctx<'_>, display: i64, enabled: bool) -> Result<(), Status> {
        self.screen(display)?;
        self.vsync.store(enabled, Ordering::Relaxed);
        Ok(())
    }

    fn set_idle_timer_enabled(&self, _ctx: &Ctx<'_>, display: i64, _timeout_ms: i32) -> Result<(), Status> {
        self.screen(display).map(|_| ())
    }

    fn get_overlay_support(&self, _ctx: &Ctx<'_>) -> Result<super::aidl::android_hardware_graphics_composer3::OverlayProperties, Status> {
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn get_hdr_conversion_capabilities(&self, _ctx: &Ctx<'_>) -> Result<Vec<common::HdrConversionCapability>, Status> {
        Err(Status::ServiceSpecific(EX_UNSUPPORTED))
    }

    fn set_refresh_rate_changed_callback_debug_enabled(&self, _ctx: &Ctx<'_>, display: i64, _enabled: bool) -> Result<(), Status> {
        self.screen(display).map(|_| ())
    }

    fn notify_expected_present(&self, _ctx: &Ctx<'_>, display: i64, _t: ClockMonotonicTimestamp, _frame_interval_ns: i32) -> Result<(), Status> {
        self.screen(display).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::is_chrome;
    use super::Pacer;
    use std::time::{Duration, Instant};

    const P: Duration = Duration::from_nanos(16_666_666);

    /// Deadlines are the origin plus whole periods, however long each tick took to wake and work:
    /// 600 ticks, each "done" 1.2 ms after its deadline, end exactly 600 periods on.
    #[test]
    fn vsync_deadlines_do_not_drift_with_the_ticks_own_time() {
        let t0 = Instant::now();
        let mut pacer = Pacer::new(t0, P);
        let mut due = t0;
        for _ in 0..600 {
            let (next, missed) = pacer.next(due + Duration::from_micros(1200), P);
            assert_eq!(missed, 0);
            due = next;
        }
        assert_eq!(due - t0, P * 600);
    }

    /// A tick woken 3.5 periods late fires once, for the latest deadline passed, and the two it
    /// slept through are counted, not fired back to back; the next is a whole period on.
    #[test]
    fn missed_vsyncs_are_dropped_not_burst() {
        let t0 = Instant::now();
        let mut pacer = Pacer::new(t0, P);
        let (first, _) = pacer.next(t0, P);
        assert_eq!(first, t0 + P);
        let (late, missed) = pacer.next(t0 + P * 4 + P / 2, P);
        assert_eq!((late - t0, missed), (P * 4, 2));
        let (next, missed) = pacer.next(t0 + P * 4 + P / 2, P);
        assert_eq!((next - t0, missed), (P * 5, 0));
        // Asked after its deadline but before the one after it: that deadline, at once, nothing
        // missed.
        let (on_time, missed) = pacer.next(t0 + P * 6 + P / 3, P);
        assert_eq!((on_time - t0, missed), (P * 6, 0));
    }

    /// A new period starts from the last deadline: no tick is skipped or doubled at the change.
    #[test]
    fn a_new_vsync_period_counts_from_the_last_deadline() {
        let t0 = Instant::now();
        let mut pacer = Pacer::new(t0, P);
        for _ in 0..3 {
            pacer.next(t0, P);
        }
        let half = P * 2;
        let (due, missed) = pacer.next(t0 + P * 3, half);
        assert_eq!((due - t0, missed), (P * 3 + half, 0));
    }

    /// `composer_skip_validate`: a frame the composer can take is presented at
    /// `presentOrValidateDisplay` and answered `Presented` (one call a frame); off, it is
    /// `Validated` and presented at the next command's `presentDisplay`, as before. And
    /// `composer_fences`: a present fence with each presented frame, signalled. (The levers are
    /// switched here, in one test, as they are process-wide.)
    #[test]
    fn present_or_validate_presents_a_composers_frame_when_skipping_validate() {
        use super::super::aidl::android_hardware_graphics_composer3::{Color, LayerCommand, ParcelableComposition};
        use super::*;
        let fb = Arc::new(Framebuffer::new(64, 32));
        let composer = Composer::new(crate::binder::broker(crate::binder::Context::Binder), Arc::clone(&fb));
        let client = Client::new(Arc::clone(&composer.broker), Arc::clone(&composer.screens), Arc::new(AtomicBool::new(true)));
        let call = crate::binder::HostCall { code: 0, data: Vec::new(), offsets: Vec::new(), fds: Vec::new(), handles: Vec::new(), sender_pid: 1, sender_euid: 1000 };
        let ctx = Ctx { call: &call };
        let layer = client.create_layer(&ctx, DISPLAY, 3).unwrap();
        let frame = |present_or_validate: bool, present: bool| DisplayCommand {
            display: DISPLAY,
            layers: vec![LayerCommand {
                layer,
                composition: Some(ParcelableComposition { composition: Composition::SOLID_COLOR }),
                display_frame: Some(common::Rect { left: 0, top: 0, right: 64, bottom: 32 }),
                color: Some(Color { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }),
                ..Default::default()
            }],
            present_or_validate_display: present_or_validate,
            present_display: present,
            ..Default::default()
        };
        let result_of = |r: &[CommandResultPayload]| {
            r.iter().find_map(|p| match p {
                CommandResultPayload::PresentOrValidateResult(v) => Some(v.result),
                _ => None,
            })
        };
        let fences_in = |r: &[CommandResultPayload]| r.iter().filter(|p| matches!(p, CommandResultPayload::PresentFence(_))).count();

        crate::lever::apply("composer_skip_validate=0").unwrap();
        crate::lever::apply("composer_fences=0").unwrap();
        let before = fb.frames();
        let r = client.execute_commands(&ctx, vec![frame(true, false)]).unwrap();
        assert_eq!(result_of(&r), Some(PresentOrValidate_Result::Validated));
        assert_eq!(fb.frames(), before, "validated, not presented");
        let r = client.execute_commands(&ctx, vec![frame(false, true)]).unwrap();
        assert_eq!((fb.frames(), fences_in(&r)), (before + 1, 0), "presented by presentDisplay, no fence");

        crate::lever::apply("composer_skip_validate=1").unwrap();
        crate::lever::apply("composer_fences=1").unwrap();
        let r = client.execute_commands(&ctx, vec![frame(true, false)]).unwrap();
        assert_eq!(result_of(&r), Some(PresentOrValidate_Result::Presented));
        assert_eq!((fb.frames(), fences_in(&r)), (before + 2, 1), "presented at once, with its fence");
        crate::lever::apply("composer_skip_validate=0").unwrap();
        crate::lever::apply("composer_fences=0").unwrap();
    }

    /// A present fence's time is the first vsync at or after the present, on the vsync's phase.
    #[test]
    fn a_present_fence_is_timed_at_the_next_vsync() {
        let (last, p) = (1_000_000_000, 16_666_667);
        assert_eq!(super::next_vsync(last, p, last), last);
        assert_eq!(super::next_vsync(last, p, last + 1), last + p);
        assert_eq!(super::next_vsync(last, p, last + 2 * p + 5), last + 3 * p);
        assert_eq!(super::next_vsync(last, p, 999), last);
        assert_eq!(super::next_vsync(0, p, 999), 999, "before any vsync: the present itself");
    }

    /// The rate lever's period, rounded to the nearest nanosecond; out of range refused.
    #[test]
    fn the_vsync_rate_sets_the_period() {
        assert_eq!(super::set_vsync_hz(30), Some(33_333_333));
        assert_eq!(super::set_vsync_hz(0), None);
        assert_eq!(super::set_vsync_hz(60), Some(16_666_667));
        super::VSYNC_PERIOD_NS.store(16_666_666, std::sync::atomic::Ordering::Relaxed);
    }

    /// **The measurement behind `VSYNC_PACE`**: the old relative `sleep(period)` loop against the
    /// deadlines, 3 s each, with a 0.3 ms "callback" in every tick. A timing test, so ignored by
    /// default (a loaded host moves it); `cargo test -- --ignored --nocapture vsync_rate` prints both.
    #[test]
    #[ignore = "timing; run by hand"]
    fn vsync_rate_old_loop_against_deadlines() {
        let work = || {
            let t = Instant::now();
            while t.elapsed() < Duration::from_micros(300) {
                std::hint::spin_loop();
            }
        };
        let run = Duration::from_secs(3);
        let t0 = Instant::now();
        let mut ticks = 0u32;
        while t0.elapsed() < run {
            std::thread::sleep(P);
            work();
            ticks += 1;
        }
        let old = f64::from(ticks) / t0.elapsed().as_secs_f64();
        let t0 = Instant::now();
        let mut pacer = Pacer::new(t0, P);
        let mut ticks = 0u32;
        while t0.elapsed() < run {
            let (due, _) = pacer.next(Instant::now(), P);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            work();
            ticks += 1;
        }
        let paced = f64::from(ticks) / t0.elapsed().as_secs_f64();
        eprintln!("[vsync] old sleep loop {old:.3} Hz, deadlines {paced:.3} Hz");
        assert!(paced > 59.5 && paced < 60.5, "deadlines ran at {paced:.3} Hz");
    }

    /// Names as the image's own windows give them (run 2026-09-28: SystemUI, the launcher, Roblox).
    #[test]
    fn the_bars_and_the_taskbar_are_chrome_and_an_app_is_not() {
        for chrome in ["VRI[StatusBar]#0(BLAST Consumer)0", "VRI[Taskbar]#0(BLAST Consumer)0", "VRI[NavigationBar0]#3(BLAST Consumer)3", "VRI[ScreenDecorOverlayBottom]#5(BLAST Consumer)5"] {
            assert!(is_chrome(chrome), "{chrome}");
        }
        for app in [
            "SurfaceView[com.roblox.client/com.roblox.client.ActivityNativeMain]#2(BLAST Consumer)2",
            "VRI[ActivityNativeMain]#1(BLAST Consumer)1",
            "VRI[FallbackHome]#0(BLAST Consumer)0",
            "bbq-adapter#0(BLAST Consumer)0",
            "",
            "StatusBar",
        ] {
            assert!(!is_chrome(app), "{app}");
        }
    }
}
