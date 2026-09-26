//! **The activity's lifecycle, driven by the host window**: for each thing the host's window
//! system does to the window, the `GameActivity` callbacks a device's framework makes for the same
//! thing, in its order.
//!
//! # Why the host has to do this
//!
//! On a device the engine never meets a window system: it learns what happened to its window
//! through the activity -- `onPause`/`onStop` when it goes to the background, the `SurfaceView`'s
//! `surfaceDestroyed` and then `surfaceCreated`/`surfaceChanged`/`surfaceRedrawNeeded` when its
//! surface is taken away and given back, `surfaceChanged` with a new size when it is resized,
//! `onWindowFocusChanged` when another window takes the input -- and GameActivity's glue turns
//! each into an `APP_CMD_*` for the game thread (`TERM_WINDOW`, `INIT_WINDOW`,
//! `WINDOW_RESIZED`, `PAUSE`, `LOST_FOCUS`, ...). Those, and the surface's own capabilities, are
//! all the engine knows of its window (below).
//!
//! MEASURED why that matters here (w26, Windows, 2026-09-25): 1,470 s into a session a present
//! answered `VK_ERROR_OUT_OF_DATE_KHR`, and for the next two minutes every acquire answered the
//! same (~2,400 of them) and nothing was presented, until the person closed the window. No guest
//! thread died, and the host had delivered no window callback -- the only ones a session made were
//! the startup rows and a resize.
//!
//! # What the engine rebuilds its swapchain on -- DECODED on the modified 2.739.691 build
//!
//! Link addresses below are that build's `libroblox.so`, a different binary from the stock
//! fixture's.
//!
//! * **Out of date is logged, not acted on.** Acquire (`0x28405a4`) logs any non-zero result and
//!   returns -1, and the frame goes to a fallback framebuffer; present (`0x2853d04`) logs a
//!   negative one. Neither sets anything on the default flags (`GraphicsVulkanMtSubmit` and
//!   `GraphicsVulkanMtSubmitResizing` both off).
//! * **Every frame, `0x2790a34` asks the surface's capabilities** (the one caps query per acquire
//!   the census counts) and rebuilds the swapchain only when the current extent or transform
//!   differs from the swapchain's, the framebuffer is gone, or the query fails. A same-size
//!   swapchain the driver calls out of date matches on all three, so **nothing in the engine ever
//!   rebuilds it**, which is w26's loop exactly.
//! * `APP_CMD_WINDOW_RESIZED` and `CONFIG_CHANGED` are only logged (`0x2bed724`); a resize works
//!   because the next frame's capabilities show the new extent. `WINDOW_REDRAW_NEEDED` updates the
//!   insets and the view's metrics and does not reach the swapchain. So **a same-size
//!   `surfaceChanged` does nothing for a stale swapchain.**
//! * **A failed capabilities query does, with no pause**: the check destroys the main framebuffer
//!   on any error (`0x2790b0c`, logging `Vulkan: destroying window frame buffer` once, and nothing
//!   on the queries after), and a null framebuffer is one of its rebuild conditions -- so the next
//!   query that succeeds rebuilds the swapchain, with the old one as `oldSwapchain`. Meanwhile
//!   `beginFrame` (`0x27de770`) returns nothing, and `renderPerform` (`0x27da0f0`) skips the scene's
//!   recording, the acquire, `vkQueueSubmit` and the present (`0x27da27c`); the CPU's scene
//!   preparation and every other job go on. This is what the host's Vulkan layer does for a
//!   minimised window, a display mode change and the safety net (`Vulkan::set_surface_withheld`).
//! * **Left to the driver, a minimised window costs full frames**: Win32 answers a 0x0 extent, on
//!   which the check returns with the framebuffer kept (`0x2790b64`), so every frame is still
//!   recorded and submitted -- into the fallback framebuffer (`dev+0x810`) when the acquire
//!   answers out of date, `0x28250f4` submitting with no present. No sleep or throttle anywhere on
//!   that path: only the render job's frame cap.
//! * **`APP_CMD_TERM_WINDOW` also does, and pauses**: `nativeActivity_onKillSurface` runs
//!   `pauseExperienceOrLuaApp_`, which takes the render job -- and the DataModel stepping in it --
//!   off the scheduler (`0x2ed6d60`) and destroys the view, blocking up to a flag's timeout; after
//!   `APP_CMD_INIT_WINDOW` the next tick resumes it on a new view (state 9 to 10, or 5 to 7). TERM
//!   checks only that the flags arrived and INIT checks nothing, so the pair **needs no PAUSE, STOP,
//!   START or RESUME** around it; a TERM that lands mid-transition (state 6) is not re-armed. That
//!   the new view is a new `VkSurfaceKHR` and swapchain is INFERRED, from the path and from every
//!   run's census. Kept for the safety net's second attempt and the device's own background.
//! * **The game thread ticks only while it has the focus, is started and has a window**
//!   (`0x2bed63c`): `LOST_FOCUS` and `STOP` stop it, `GAINED_FOCUS` and `START` start it again;
//!   `PAUSE` and `RESUME` change nothing there. `INIT_WINDOW` takes the focus from the last focus
//!   event. So a window without the focus stops ticking, as on ChromeOS or DeX, and a new surface
//!   is picked up once it has the focus again.
//!
//! # The mapping: an instance keeps playing, unless the embedding asks for a device's pauses
//!
//! **The default is a product decision** (2026-09-25): Omnidroid runs several instances at once --
//! 3-4 played together, or 30-35 at low quality -- of which one has the focus and many are
//! minimised, and every one must keep playing through both, as desktop Roblox does. A device pauses
//! on both; [`Policy`] turns either on.
//!
//! | host | by default | with the device's pauses ([`Policy`]) |
//! |---|---|---|
//! | minimised (a [`WindowEvent::Resized`] to zero) | no callback: the game keeps ticking, and the surface's capabilities are withheld so it draws nothing | `pause_in_background`: to the background, as Home does -- `onWindowFocusChanged(false)`, `onPause`, `surfaceDestroyed`, `onStop`, `onTrimMemory(UI_HIDDEN)`, then `ProcessLifecycleOwner`'s pause and stop 700 ms later |
//! | restored (a non-zero size while minimised) | the capabilities answered again: the engine rebuilds its swapchain, with no pause (and `surfaceChanged`, `onContentRectChanged` if the size changed) | `pause_in_background`: `onStart`, `onResume` (and the process's resume, if its pause was sent), a new surface, and `onWindowFocusChanged(true)` if the window has the focus |
//! | resized | `surfaceChanged` with the new size, then `onContentRectChanged` | the same |
//! | focus lost or gained | said, not told: the game keeps ticking behind other windows | `follow_focus`: `onWindowFocusChanged`, once each way |
//! | the display's mode changed | one capabilities query withheld: the engine rebuilds its swapchain (while minimised: left to the restore) | the same; paused, the restore brings a new surface anyway |
//! | the scale or the monitor changed, or the window was covered | said only: a size change that comes with it is its own `Resized` | the same |
//! | the driver answered out of date for [`OUT_OF_DATE_BOUND`] with no present, not minimised | the safety net: the engine's rebuild first; if that brings no frames, a new surface (`surfaceDestroyed`, `surfaceCreated`, `surfaceChanged`, `surfaceRedrawNeeded`, no pause or stop), backing off to [`OUT_OF_DATE_BOUND_MAX`] | the same, and only with the focus under `follow_focus` |
//!
//! **Why a withheld capabilities answer and not a new surface for the default rows**: both make the
//! engine rebuild (the decode below), but a new surface pauses the experience while it lasts --
//! `TERM_WINDOW` takes the render job, and the DataModel stepping in it, off the scheduler -- and
//! a withheld answer pauses nothing. A new surface is kept for when a rebuild on the same surface
//! did not bring frames back.
//!
//! Every host event is also said, as a `WINDOW:` line, whether or not it causes a callback: a
//! session that froze behind a window change nobody logged (w26) cannot say which change it was.
//!
//! # What this module is not
//!
//! It makes no call. It is the state machine -- which callbacks, in which order, given what the
//! activity was last told -- and the embedding makes each [`Call`] on the thread that owns the
//! window, which is the UI thread every other lifecycle call is made from. That split is what lets
//! the mapping be tested without a guest (this module's tests).
//!
//! # The process's lifecycle beside the activity's
//!
//! androidx's `ProcessLifecycleOwner` is what the app's `JNIAppLifecycleNativeAdapter` observes
//! ([`ProcessEvent`]), and its rules are copied, not approximated: `activityPaused` posts the
//! process's pause [`PROCESS_PAUSE_DELAY`] later; `activityResumed` cancels that if it has not run
//! and dispatches `ON_RESUME` if it has; `ON_STOP` follows the delayed pause once the activity has
//! stopped. So a restore within 700 ms of a minimise tells the process nothing, as a device does.

use core::fmt;
use std::time::{Duration, Instant};

use omni_platform::window::{DisplayChange, WindowEvent};

use super::script::ProcessEvent;
use crate::GuestArg;

/// `ProcessLifecycleOwner.TIMEOUT_MS`: how long after the last activity paused the process's own
/// pause is dispatched.
pub const PROCESS_PAUSE_DELAY: Duration = Duration::from_millis(700);

/// How long the driver may keep answering `VK_ERROR_OUT_OF_DATE_KHR` with **no present** before
/// the surface is recreated by the safety net. The engine rebuilds a swapchain within a frame when
/// it has been told to, so two seconds without one is a swapchain nobody is going to rebuild.
pub const OUT_OF_DATE_BOUND: Duration = Duration::from_secs(2);

/// The longest the safety net waits between two recreations that did not bring frames back: the
/// bound doubles after each, up to this, so a display another program holds (an exclusive
/// full-screen game) costs one recreation a minute rather than one every two seconds.
pub const OUT_OF_DATE_BOUND_MAX: Duration = Duration::from_secs(60);

/// `ComponentCallbacks2.TRIM_MEMORY_UI_HIDDEN`: what the system tells a process whose UI is no
/// longer visible.
pub const TRIM_MEMORY_UI_HIDDEN: i32 = 20;

/// `PixelFormat.RGBA_8888`, the format every `surfaceChanged` here carries (the startup rows'
/// reason: the one format this runtime presents).
pub const RGBA_8888: u64 = 1;

/// One callback a device's framework would make, as the embedding is to make it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// `onStartNative` (`APP_CMD_START`).
    Start,
    /// `onResumeNative` (`APP_CMD_RESUME`).
    Resume,
    /// `onPauseNative` (`APP_CMD_PAUSE`).
    Pause,
    /// `onStopNative` (`APP_CMD_STOP`).
    Stop,
    /// `onWindowFocusChangedNative` (`APP_CMD_GAINED_FOCUS` / `APP_CMD_LOST_FOCUS`).
    WindowFocusChanged(bool),
    /// `onSurfaceCreatedNative` (`APP_CMD_INIT_WINDOW`): the `SurfaceView`'s `surfaceCreated`.
    SurfaceCreated,
    /// `onSurfaceChangedNative`, format [`RGBA_8888`]: the `SurfaceView`'s `surfaceChanged`. The
    /// glue posts `APP_CMD_WINDOW_RESIZED` when the window's size differs from the last it saw.
    SurfaceChanged {
        /// The surface's width in pixels.
        width: u32,
        /// The surface's height in pixels.
        height: u32,
    },
    /// `onSurfaceRedrawNeededNative` (`APP_CMD_WINDOW_REDRAW_NEEDED`): what starts the engine's
    /// own surface path (`nativeActivity_onSurfaceChanged`; see `tests/gameactivity.rs`).
    SurfaceRedrawNeeded,
    /// `onSurfaceDestroyedNative` (`APP_CMD_TERM_WINDOW`).
    SurfaceDestroyed,
    /// `onContentRectChangedNative` (`APP_CMD_CONTENT_RECT_CHANGED`): `GameActivity.onGlobalLayout`
    /// after a layout that changed the view's rectangle.
    ContentRectChanged {
        /// The content's width in pixels.
        width: u32,
        /// The content's height in pixels.
        height: u32,
    },
    /// `onTrimMemoryNative`.
    TrimMemory(i32),
    /// A process lifecycle event, through `script::process_lifecycle`.
    Process(ProcessEvent),
    /// **Not a callback: the host's own Vulkan layer.** While `true`, the surface's capabilities
    /// answer `VK_ERROR_SURFACE_LOST_KHR` (`Vulkan::set_surface_withheld`): the engine destroys its
    /// framebuffer and draws nothing, the game ticking on; answered again, it rebuilds its
    /// swapchain. Nothing without Vulkan.
    WithholdSurface(bool),
    /// **Not a callback: the host's own Vulkan layer.** One capabilities query withheld
    /// (`Vulkan::withhold_surface_once`): the engine's own swapchain rebuild, with no pause.
    RebuildSwapchain,
}

impl Call {
    /// The `GameActivity` native this is and its JNI descriptor, or `None` for a
    /// [`Call::Process`], which is a static native of another class.
    #[must_use]
    pub const fn native(self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            Call::Start => ("onStartNative", "(J)V"),
            Call::Resume => ("onResumeNative", "(J)V"),
            Call::Pause => ("onPauseNative", "(J)V"),
            Call::Stop => ("onStopNative", "(J)V"),
            Call::WindowFocusChanged(_) => ("onWindowFocusChangedNative", "(JZ)V"),
            Call::SurfaceCreated => ("onSurfaceCreatedNative", "(JLandroid/view/Surface;)V"),
            Call::SurfaceChanged { .. } => ("onSurfaceChangedNative", "(JLandroid/view/Surface;III)V"),
            Call::SurfaceRedrawNeeded => ("onSurfaceRedrawNeededNative", "(JLandroid/view/Surface;)V"),
            Call::SurfaceDestroyed => ("onSurfaceDestroyedNative", "(J)V"),
            Call::ContentRectChanged { .. } => ("onContentRectChangedNative", "(JIIII)V"),
            Call::TrimMemory(_) => ("onTrimMemoryNative", "(JI)V"),
            Call::Process(_) | Call::WithholdSurface(_) | Call::RebuildSwapchain => return None,
        })
    }

    /// The arguments after `(env, thiz, handle)`, where `surface` is the Java `Surface` the
    /// activity's view holds -- the same object for every surface, because the glue releases its
    /// window on `surfaceDestroyed` and so takes the next `surfaceCreated` as a new one.
    #[must_use]
    pub fn tail(self, surface: u64) -> Vec<GuestArg> {
        match self {
            Call::WindowFocusChanged(focused) => vec![GuestArg::Int(u64::from(focused))],
            Call::SurfaceCreated | Call::SurfaceRedrawNeeded => vec![GuestArg::Int(surface)],
            Call::SurfaceChanged { width, height } => vec![
                GuestArg::Int(surface),
                GuestArg::Int(RGBA_8888),
                GuestArg::Int(u64::from(width)),
                GuestArg::Int(u64::from(height)),
            ],
            Call::ContentRectChanged { width, height } => vec![
                GuestArg::Int(0),
                GuestArg::Int(0),
                GuestArg::Int(u64::from(width)),
                GuestArg::Int(u64::from(height)),
            ],
            Call::TrimMemory(level) => vec![GuestArg::Int(level as u32 as u64)],
            Call::Start
            | Call::Resume
            | Call::Pause
            | Call::Stop
            | Call::SurfaceDestroyed
            | Call::Process(_)
            | Call::WithholdSurface(_)
            | Call::RebuildSwapchain => Vec::new(),
        }
    }
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Call::WindowFocusChanged(focused) => write!(f, "onWindowFocusChangedNative({focused})"),
            Call::SurfaceChanged { width, height } => write!(f, "onSurfaceChangedNative({width}x{height})"),
            Call::ContentRectChanged { width, height } => {
                write!(f, "onContentRectChangedNative(0, 0, {width}, {height})")
            }
            Call::TrimMemory(level) => write!(f, "onTrimMemoryNative({level})"),
            Call::Process(event) => write!(f, "ProcessLifecycleOwner {event:?}"),
            Call::WithholdSurface(true) => f.write_str("the surface's capabilities withheld (VK_ERROR_SURFACE_LOST_KHR)"),
            Call::WithholdSurface(false) => f.write_str("the surface's capabilities answered again"),
            Call::RebuildSwapchain => f.write_str("one capabilities query withheld: the engine rebuilds its swapchain"),
            other => match other.native() {
                Some((member, _)) => f.write_str(member),
                None => write!(f, "{other:?}"),
            },
        }
    }
}

/// What one host event, or one turn of the clock, comes to: a line saying what happened, and the
/// callbacks it causes, in order (possibly none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    /// What the host did and what the activity is told, for the `WINDOW:` line.
    pub said: String,
    /// The callbacks, in the order they are to be made.
    pub calls: Vec<Call>,
}

impl Reaction {
    fn new(said: impl Into<String>, calls: Vec<Call>) -> Reaction {
        Reaction { said: said.into(), calls }
    }

    /// Whether this reaction gives the engine something new to draw into -- a new surface, a
    /// rebuilt swapchain, a surface answered again, or the old one at a new size -- after which it
    /// is expected to present again.
    #[must_use]
    pub fn renews_surface(&self) -> bool {
        self.calls.iter().any(|call| {
            matches!(call, Call::SurfaceChanged { .. } | Call::RebuildSwapchain | Call::WithholdSurface(false))
        })
    }

    /// Whether it renews more than the surface's size: a new surface or a rebuilt swapchain.
    #[must_use]
    pub fn renews_more_than_the_size(&self) -> bool {
        self.calls.iter().any(|call| {
            matches!(call, Call::SurfaceCreated | Call::RebuildSwapchain | Call::WithholdSurface(false))
        })
    }
}

/// The safety net's watch over the driver's out-of-date answers.
#[derive(Debug, Clone, Copy)]
struct Watch {
    /// The driver's out-of-date count at the last turn.
    answers: u64,
    /// The present count at the last turn.
    presents: u64,
    /// When the out-of-date run began, if one is running.
    since: Option<Instant>,
    /// How long a run may last before the surface is recreated; doubles after a recreation that
    /// did not bring frames back, and is reset by one that did.
    bound: Duration,
}

/// **What an embedding tells the activity**, beyond what keeps the game running.
///
/// Both default to off, and that default is a product decision (2026-09-25): an Omnidroid instance
/// is one of several -- 3-4 played at once, or 30-35 at low quality -- of which one has the focus
/// and many are minimised, and each must **keep playing** through both, as desktop Roblox does. A
/// device pauses on both; either half of that is here for an embedding that wants it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Policy {
    /// Tell the activity the host's focus changes (`onWindowFocusChanged`). The engine's game
    /// thread stops ticking without the focus (the module header's decode), so this pauses an
    /// instance behind another window. `OMNI_FOLLOW_FOCUS=1` in the gate.
    pub follow_focus: bool,
    /// A minimised window sends the activity to the background as Home does -- focus, pause,
    /// surface destroyed, stop, trim memory, and the process's pause and stop -- and a restore
    /// brings it back. `OMNI_PAUSE_IN_BACKGROUND=1` in the gate.
    pub pause_in_background: bool,
}

/// **The activity's state as the host has told it, and what the next host event makes of it.**
#[derive(Debug, Clone)]
pub struct WindowLifecycle {
    policy: Policy,
    /// Started and resumed: false only in the background [`Policy::pause_in_background`] sends it
    /// to.
    front: bool,
    /// Whether the host's window has no pixels (minimised), whatever the activity was told.
    minimized: bool,
    /// Whether the host says the window has the keyboard focus now.
    host_focused: bool,
    /// What `onWindowFocusChanged` last told the activity.
    focus_told: bool,
    /// The size the activity's surface was last given.
    size: (u32, u32),
    /// When the activity last paused, while `ProcessLifecycleOwner`'s delayed pause is pending.
    paused_at: Option<Instant>,
    /// `ProcessLifecycleOwner.mPauseSent`.
    process_paused: bool,
    /// `ProcessLifecycleOwner.mStopSent`.
    process_stopped: bool,
    /// A display change the activity has not seen, because it came while in the background.
    display_changed_away: bool,
    watch: Watch,
}

impl WindowLifecycle {
    /// The state the startup rows leave: in the front with a surface of `size`, and told it has
    /// the focus (`onWindowFocusChangedNative(true)` is one of those rows).
    #[must_use]
    pub fn new(size: (u32, u32), policy: Policy) -> WindowLifecycle {
        WindowLifecycle {
            policy,
            front: true,
            minimized: false,
            host_focused: true,
            focus_told: true,
            size,
            paused_at: None,
            process_paused: false,
            process_stopped: false,
            display_changed_away: false,
            watch: Watch { answers: 0, presents: 0, since: None, bound: OUT_OF_DATE_BOUND },
        }
    }

    /// Whether the activity is in the front: started and resumed. Only
    /// [`Policy::pause_in_background`] ever takes it out.
    #[must_use]
    pub fn in_front(&self) -> bool {
        self.front
    }

    /// Whether the host's window is minimised now: the game may be playing, and there is nothing
    /// to draw into.
    #[must_use]
    pub fn minimized(&self) -> bool {
        self.minimized
    }

    /// The policy this lifecycle follows.
    #[must_use]
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// What `onWindowFocusChanged` last told the activity.
    #[must_use]
    pub fn focus_told(&self) -> bool {
        self.focus_told
    }

    /// The size the surface was last given.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// When the activity paused, while the process's delayed pause is still to come.
    #[must_use]
    pub fn paused_at(&self) -> Option<Instant> {
        self.paused_at
    }

    /// Whether the process's pause (`ON_PAUSE`, `setInactive`) has been sent.
    #[must_use]
    pub fn process_paused(&self) -> bool {
        self.process_paused
    }

    /// Whether the activity is stopped (it is exactly when it is not in the front).
    #[must_use]
    pub fn stopped(&self) -> bool {
        !self.front
    }

    /// **One host event.** `None` for an event that is not about the window's state (input, a
    /// close request -- the close has its own path); otherwise the line to log and the callbacks,
    /// which may be none.
    pub fn event(&mut self, event: &WindowEvent, now: Instant) -> Option<Reaction> {
        match *event {
            WindowEvent::Resized { width, height } => Some(self.sized(width, height, now, "resized")),
            WindowEvent::FocusChanged { focused } => {
                let gained = if focused { "gained" } else { "lost" };
                self.host_focused = focused;
                if !self.policy.follow_focus {
                    return Some(Reaction::new(
                        format!("focus {gained}: not told, the game keeps playing (OMNI_FOLLOW_FOCUS=1 tells it)"),
                        Vec::new(),
                    ));
                }
                if !self.front {
                    return Some(Reaction::new(
                        format!("focus {gained}, in the background: the activity is told on its return"),
                        Vec::new(),
                    ));
                }
                if focused == self.focus_told {
                    return Some(Reaction::new(format!("focus {gained}, which the activity already has"), Vec::new()));
                }
                self.focus_told = focused;
                // The game thread ticks only with the focus (below), so an out-of-date run that
                // began before is not the engine's to answer while it is away.
                self.watch.since = None;
                Some(Reaction::new(format!("focus {gained}"), vec![Call::WindowFocusChanged(focused)]))
            }
            WindowEvent::Occluded { occluded } => Some(Reaction::new(
                if occluded {
                    "occluded: none of the window can be seen (the game keeps running, as it does behind another window on a desktop)"
                } else {
                    "visible again"
                },
                Vec::new(),
            )),
            WindowEvent::DisplayChanged { change } => Some(self.display_changed(change)),
            _ => None,
        }
    }

    /// **The window's size as the host reports it when asked**, rather than as an event said it:
    /// the same state change as a [`WindowEvent::Resized`] to that size, and `None` when it is the
    /// size the activity already has. For the size a display changes with no event
    /// (`Window::client_size`'s reason for asking the OS every time).
    pub fn sampled(&mut self, width: u32, height: u32, now: Instant) -> Option<Reaction> {
        let zero = width == 0 || height == 0;
        let same = if self.minimized { zero } else { !zero && (width, height) == self.size };
        if same {
            return None;
        }
        Some(self.sized(width, height, now, "sampled at"))
    }

    fn sized(&mut self, width: u32, height: u32, now: Instant, how: &str) -> Reaction {
        let zero = width == 0 || height == 0;
        match (self.minimized, zero) {
            (false, true) => {
                self.minimized = true;
                self.watch.since = None;
                if self.policy.pause_in_background {
                    return self.send_to_background(width, height, now, how);
                }
                Reaction::new(
                    format!(
                        "minimised ({how} {width}x{height}): the game keeps playing and draws nothing until \
                         restored (OMNI_PAUSE_IN_BACKGROUND=1 pauses it instead)"
                    ),
                    vec![Call::WithholdSurface(true)],
                )
            }
            (true, true) => Reaction::new(format!("{how} {width}x{height}, still minimised"), Vec::new()),
            (true, false) => {
                self.minimized = false;
                self.watch.since = None;
                let display = if core::mem::take(&mut self.display_changed_away) {
                    ", after a display change while it was minimised"
                } else {
                    ""
                };
                if !self.front {
                    return self.bring_back_from_background(width, height, how, display);
                }
                // **Still playing, so no start or resume, and no new surface**: the surface's
                // capabilities answered again, and the engine rebuilds the swapchain whose
                // framebuffer the withheld answers destroyed (the module header). A new size is told
                // as a resize is, so the glue and the view learn it too.
                let resized = (width, height) != self.size;
                self.size = (width, height);
                let mut calls = vec![Call::WithholdSurface(false)];
                if resized {
                    calls.extend([Call::SurfaceChanged { width, height }, Call::ContentRectChanged { width, height }]);
                }
                Reaction::new(
                    format!("restored ({how} {width}x{height}{display}): the engine rebuilds its swapchain"),
                    calls,
                )
            }
            (false, false) if (width, height) == self.size => {
                Reaction::new(format!("{how} {width}x{height}, the size the surface has"), Vec::new())
            }
            (false, false) => {
                let (was_w, was_h) = self.size;
                self.size = (width, height);
                self.watch.since = None;
                Reaction::new(
                    format!("{how} {width}x{height} from {was_w}x{was_h}: the surface changes size"),
                    vec![Call::SurfaceChanged { width, height }, Call::ContentRectChanged { width, height }],
                )
            }
        }
    }

    /// [`Policy::pause_in_background`]'s minimise: to the background, in the order the close path
    /// uses.
    fn send_to_background(&mut self, width: u32, height: u32, now: Instant, how: &str) -> Reaction {
        let mut calls = Vec::new();
        if self.focus_told {
            calls.push(Call::WindowFocusChanged(false));
            self.focus_told = false;
        }
        calls.extend([Call::Pause, Call::SurfaceDestroyed, Call::Stop, Call::TrimMemory(TRIM_MEMORY_UI_HIDDEN)]);
        self.front = false;
        // `activityPaused`: the process's pause is posted, unless it was already sent.
        if !self.process_paused {
            self.paused_at = Some(now);
        }
        Reaction::new(
            format!("minimised ({how} {width}x{height}): the activity goes to the background (OMNI_PAUSE_IN_BACKGROUND)"),
            calls,
        )
    }

    /// [`Policy::pause_in_background`]'s restore: back to the front with a new surface.
    fn bring_back_from_background(&mut self, width: u32, height: u32, how: &str, display: &str) -> Reaction {
        let mut calls = vec![Call::Start, Call::Resume];
        // `activityResumed`: the pending pause is cancelled; one already sent is answered.
        self.paused_at = None;
        if self.process_paused {
            calls.push(Call::Process(ProcessEvent::Resume));
            self.process_paused = false;
            self.process_stopped = false;
        }
        calls.extend([Call::SurfaceCreated, Call::SurfaceChanged { width, height }, Call::SurfaceRedrawNeeded]);
        if (width, height) != self.size {
            calls.push(Call::ContentRectChanged { width, height });
        }
        // The engine's `INIT_WINDOW` takes the last focus event's, so a lifecycle that does not
        // follow the host's focus gives back the focus the minimise took.
        if self.host_focused || !self.policy.follow_focus {
            calls.push(Call::WindowFocusChanged(true));
            self.focus_told = true;
        }
        self.front = true;
        self.size = (width, height);
        Reaction::new(
            format!("restored ({how} {width}x{height}{display}): the activity returns with a new surface"),
            calls,
        )
    }

    fn display_changed(&mut self, change: DisplayChange) -> Reaction {
        if self.minimized {
            self.display_changed_away = true;
            return Reaction::new(
                format!("{change} changed while minimised: the restore brings a new surface"),
                Vec::new(),
            );
        }
        match change {
            DisplayChange::Mode => {
                self.watch.since = None;
                Reaction::new(format!("{change} changed: the engine rebuilds its swapchain"), vec![Call::RebuildSwapchain])
            }
            _ => Reaction::new(
                format!("{change} changed (a size change comes as its own event; the safety net watches the rest)"),
                Vec::new(),
            ),
        }
    }

    /// The surface taken away and a new one given, at the size it had: what a `SurfaceView`
    /// does when its surface is recreated under a resumed activity.
    fn new_surface(&self) -> Vec<Call> {
        let (width, height) = self.size;
        vec![
            Call::SurfaceDestroyed,
            Call::SurfaceCreated,
            Call::SurfaceChanged { width, height },
            Call::SurfaceRedrawNeeded,
        ]
    }

    /// **One turn of the UI thread's loop**, with the engine's present count and the driver's
    /// out-of-date count (0 without Vulkan). Two things are due by the clock rather than by an
    /// event: `ProcessLifecycleOwner`'s delayed pause, and the safety net.
    pub fn tick(&mut self, now: Instant, presents: u64, out_of_date: u64) -> Option<Reaction> {
        if let Some(paused) = self.paused_at {
            if !self.front && now.saturating_duration_since(paused) >= PROCESS_PAUSE_DELAY {
                // The delayed runnable: `dispatchPauseIfNeeded`, then `dispatchStopIfNeeded`.
                self.paused_at = None;
                self.process_paused = true;
                let mut calls = vec![Call::Process(ProcessEvent::Pause)];
                if !self.front {
                    self.process_stopped = true;
                    calls.push(Call::Process(ProcessEvent::Stop));
                }
                return Some(Reaction::new("the process's delayed pause is due (700 ms after the activity's)", calls));
            }
        }
        let watch = &mut self.watch;
        let answered = out_of_date > watch.answers;
        let presented = presents > watch.presents;
        watch.answers = out_of_date;
        watch.presents = presents;
        if !self.front || self.minimized || !self.focus_told {
            // Nothing to draw into, or no focus (the engine's game thread is not ticking then; see
            // the module header's decode): no present is expected and none is missed.
            watch.since = None;
            return None;
        }
        if presented && !answered {
            // Frames are flowing: whatever ran is over, and a recreation that led here worked.
            watch.since = None;
            watch.bound = OUT_OF_DATE_BOUND;
            return None;
        }
        if answered && watch.since.is_none() && !presented {
            watch.since = Some(now);
        }
        let since = watch.since?;
        if presented || now.saturating_duration_since(since) < watch.bound {
            return None;
        }
        let waited = now.saturating_duration_since(since);
        // The first time, the engine's own rebuild; when one has not brought frames back, a new
        // surface -- `TERM_WINDOW` then `INIT_WINDOW`, which pauses the experience while it lasts.
        let first = watch.bound == OUT_OF_DATE_BOUND;
        watch.since = None;
        watch.bound = (watch.bound * 2).min(OUT_OF_DATE_BOUND_MAX);
        let next = watch.bound;
        let (what, calls) = if first {
            ("the engine rebuilds its swapchain", vec![Call::RebuildSwapchain])
        } else {
            ("the surface is destroyed and a new one created", self.new_surface())
        };
        Some(Reaction::new(
            format!(
                "the driver has answered VK_ERROR_OUT_OF_DATE_KHR for {:.1}s with no present and no window \
                 event to explain it: {what} (next bound {:.0}s)",
                waited.as_secs_f32(),
                next.as_secs_f32()
            ),
            calls,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD: (u32, u32) = (1280, 720);

    /// Every opt-in on: a device.
    const DEVICE: Policy = Policy { follow_focus: true, pause_in_background: true };
    /// Only the focus followed.
    const FOCUS: Policy = Policy { follow_focus: true, pause_in_background: false };

    fn resized(width: u32, height: u32) -> WindowEvent {
        WindowEvent::Resized { width, height }
    }

    fn calls(reaction: Option<Reaction>) -> Vec<Call> {
        reaction.expect("a window event is always said").calls
    }

    fn new_surface(width: u32, height: u32) -> [Call; 4] {
        [Call::SurfaceDestroyed, Call::SurfaceCreated, Call::SurfaceChanged { width, height }, Call::SurfaceRedrawNeeded]
    }

    /// **By default a minimised instance keeps playing and draws nothing**: the minimise tells the
    /// activity nothing -- no pause, no stop, no surface taken away, no process event, ever -- and
    /// withholds the surface's capabilities, so the engine stops recording and submitting frames;
    /// the restore answers them again, which is what makes the engine rebuild its swapchain, with
    /// no start, resume or new surface.
    #[test]
    fn by_default_a_minimise_keeps_playing_and_draws_nothing_and_a_restore_rebuilds_the_swapchain() {
        let t0 = Instant::now();
        let mut life = WindowLifecycle::new(HD, Policy::default());
        let minimised = life.event(&resized(0, 0), t0).unwrap();
        assert_eq!(minimised.calls, [Call::WithholdSurface(true)]);
        assert!(minimised.said.contains("keeps playing"), "{}", minimised.said);
        assert!(life.minimized() && life.in_front() && !life.stopped() && life.focus_told());
        assert_eq!(life.sampled(0, 0, t0), None);
        for s in 1..60 {
            assert_eq!(life.tick(t0 + Duration::from_secs(s), 0, s), None, "nothing is due while minimised");
        }
        let restored = life.event(&resized(1280, 720), t0 + Duration::from_secs(60)).unwrap();
        assert_eq!(restored.calls, [Call::WithholdSurface(false)]);
        assert!(restored.renews_surface() && restored.renews_more_than_the_size());
        assert!(!life.minimized());
        // Restored at another size: told as a resize is, too.
        assert_eq!(calls(life.event(&resized(0, 0), t0)), [Call::WithholdSurface(true)]);
        assert_eq!(calls(life.sampled(1600, 900, t0)), [
            Call::WithholdSurface(false),
            Call::SurfaceChanged { width: 1600, height: 900 },
            Call::ContentRectChanged { width: 1600, height: 900 },
        ]);
        assert_eq!(life.size(), (1600, 900));
    }

    /// **By default the focus is not told**: the game keeps ticking behind other windows, and the
    /// activity is never told it lost the focus -- so the close's own focus loss is still owed.
    #[test]
    fn by_default_the_focus_is_said_and_not_told() {
        let t0 = Instant::now();
        let mut life = WindowLifecycle::new(HD, Policy::default());
        for focused in [false, true, false] {
            let said = life.event(&WindowEvent::FocusChanged { focused }, t0).unwrap();
            assert!(said.calls.is_empty() && said.said.contains("OMNI_FOLLOW_FOCUS"), "{said:?}");
            assert!(life.focus_told());
        }
        // Minimised and restored without the focus: still nothing about it.
        life.event(&resized(0, 0), t0);
        let back = calls(life.event(&resized(1280, 720), t0));
        assert!(!back.iter().any(|c| matches!(c, Call::WindowFocusChanged(_))), "{back:?}");
    }

    /// **With [`Policy::pause_in_background`], minimise and restore are a device's Home and
    /// return**: to the background in the close path's order, and back with a new surface --
    /// `onStart`, `onResume`, the three surface callbacks and the focus, in a device's order.
    #[test]
    fn paused_in_background_a_minimise_goes_to_the_background_and_a_restore_brings_a_new_surface() {
        let t0 = Instant::now();
        let mut life = WindowLifecycle::new(HD, DEVICE);
        assert_eq!(calls(life.event(&resized(0, 0), t0)), [
            Call::WindowFocusChanged(false),
            Call::Pause,
            Call::SurfaceDestroyed,
            Call::Stop,
            Call::TrimMemory(TRIM_MEMORY_UI_HIDDEN),
        ]);
        assert!(!life.in_front() && life.stopped());
        // Another zero while minimised is nothing.
        assert_eq!(calls(life.event(&resized(0, 0), t0)), []);
        assert_eq!(calls(life.event(&resized(1280, 720), t0 + Duration::from_millis(100))), [
            Call::Start,
            Call::Resume,
            Call::SurfaceCreated,
            Call::SurfaceChanged { width: 1280, height: 720 },
            Call::SurfaceRedrawNeeded,
            Call::WindowFocusChanged(true),
        ]);
        assert!(life.in_front() && life.focus_told());
    }

    /// **The process is told only what `ProcessLifecycleOwner` would tell it**: nothing for a
    /// restore inside 700 ms; its pause and stop once 700 ms have passed in the background; and
    /// its resume, right after the activity's, on the restore that follows.
    #[test]
    fn paused_in_background_the_process_is_paused_700_ms_after_the_activity_and_resumed_only_if_it_was() {
        let t0 = Instant::now();
        let mut life = WindowLifecycle::new(HD, DEVICE);
        life.event(&resized(0, 0), t0);
        assert_eq!(life.tick(t0 + Duration::from_millis(699), 0, 0), None);
        let restored = calls(life.event(&resized(1280, 720), t0 + Duration::from_millis(699)));
        assert!(!restored.iter().any(|c| matches!(c, Call::Process(_))), "{restored:?}");
        assert_eq!(life.tick(t0 + Duration::from_secs(5), 0, 0), None, "the pending pause was cancelled");

        let t1 = t0 + Duration::from_secs(10);
        life.event(&resized(0, 0), t1);
        let due = life.tick(t1 + PROCESS_PAUSE_DELAY, 0, 0).expect("the delayed pause");
        assert_eq!(due.calls, [Call::Process(ProcessEvent::Pause), Call::Process(ProcessEvent::Stop)]);
        assert!(life.process_paused());
        assert_eq!(life.tick(t1 + Duration::from_secs(5), 0, 0), None, "sent once");
        let restored = calls(life.event(&resized(1280, 720), t1 + Duration::from_secs(6)));
        assert_eq!(&restored[..3], [Call::Start, Call::Resume, Call::Process(ProcessEvent::Resume)]);
        assert!(!life.process_paused());
    }

    /// **Paused in the background without following the focus**, the minimise still takes the
    /// focus and the restore gives it back, because the engine's `INIT_WINDOW` takes the last
    /// focus event's.
    #[test]
    fn paused_in_background_without_the_focus_followed_the_background_still_takes_it() {
        let t0 = Instant::now();
        let mut life = WindowLifecycle::new(HD, Policy { follow_focus: false, pause_in_background: true });
        assert_eq!(calls(life.event(&WindowEvent::FocusChanged { focused: false }, t0)), []);
        assert_eq!(calls(life.event(&resized(0, 0), t0))[0], Call::WindowFocusChanged(false));
        assert_eq!(calls(life.event(&resized(1280, 720), t0)).last(), Some(&Call::WindowFocusChanged(true)));
    }

    /// **A resize is the resize path**: `surfaceChanged` with the new size and the content
    /// rectangle -- and the same size again is nothing, as a `WM_SIZE` that names the size the
    /// swapchain already has must be.
    #[test]
    fn a_resize_changes_the_surfaces_size_and_the_same_size_is_nothing() {
        let t0 = Instant::now();
        for policy in [Policy::default(), DEVICE] {
            let mut life = WindowLifecycle::new(HD, policy);
            assert_eq!(calls(life.event(&resized(960, 540), t0)), [
                Call::SurfaceChanged { width: 960, height: 540 },
                Call::ContentRectChanged { width: 960, height: 540 },
            ]);
            assert_eq!(calls(life.event(&resized(960, 540), t0)), []);
            assert_eq!(life.sampled(960, 540, t0), None);
            assert_eq!(life.size(), (960, 540));
            // A restore to a size other than the one the surface had also says the rectangle moved.
            life.event(&resized(0, 0), t0);
            let back = calls(life.event(&resized(1280, 720), t0));
            assert!(back.contains(&Call::ContentRectChanged { width: 1280, height: 720 }), "{back:?}");
        }
    }

    /// **With [`Policy::follow_focus`], the focus follows the host's, once each way** -- and a
    /// minimise does not stop the instance: a focus change while minimised is told at once (the
    /// activity is still resumed), and the restore is the plain new surface.
    #[test]
    fn following_the_focus_it_is_told_once_each_way() {
        let t0 = Instant::now();
        let mut life = WindowLifecycle::new(HD, FOCUS);
        let lost = WindowEvent::FocusChanged { focused: false };
        let gained = WindowEvent::FocusChanged { focused: true };
        assert_eq!(calls(life.event(&lost, t0)), [Call::WindowFocusChanged(false)]);
        assert_eq!(calls(life.event(&lost, t0)), []);
        assert_eq!(calls(life.event(&gained, t0)), [Call::WindowFocusChanged(true)]);
        assert_eq!(calls(life.event(&resized(0, 0), t0)), [Call::WithholdSurface(true)]);
        assert_eq!(calls(life.event(&lost, t0)), [Call::WindowFocusChanged(false)]);
        assert_eq!(calls(life.event(&resized(1280, 720), t0)), [Call::WithholdSurface(false)]);
        assert_eq!(calls(life.event(&gained, t0)), [Call::WindowFocusChanged(true)]);
    }

    /// **With both, the focus waits out the background**: one gained while minimised is told on
    /// the restore, one lost while minimised is not told at all, and a restore without the focus
    /// gives none.
    #[test]
    fn paused_in_background_and_following_the_focus_it_waits_out_the_background() {
        let t0 = Instant::now();
        let mut life = WindowLifecycle::new(HD, DEVICE);
        let lost = WindowEvent::FocusChanged { focused: false };
        let gained = WindowEvent::FocusChanged { focused: true };
        // Unfocused, then minimised: no second focus loss in the background sequence.
        life.event(&lost, t0);
        assert_eq!(calls(life.event(&resized(0, 0), t0))[0], Call::Pause);
        // Restored without the focus: no focus gained.
        let back = calls(life.event(&resized(1280, 720), t0));
        assert!(!back.iter().any(|c| matches!(c, Call::WindowFocusChanged(_))), "{back:?}");
        // The focus arriving later is told then.
        assert_eq!(calls(life.event(&gained, t0)), [Call::WindowFocusChanged(true)]);
        // Minimised, then the focus comes back before the size does: told with the restore.
        life.event(&resized(0, 0), t0);
        assert_eq!(calls(life.event(&gained, t0)), []);
        assert_eq!(calls(life.event(&resized(1280, 720), t0)).last(), Some(&Call::WindowFocusChanged(true)));
    }

    /// **A display mode change rebuilds the swapchain; the rest are said and left**; and one that
    /// comes while minimised is left for the restore, which rebuilds it anyway.
    #[test]
    fn a_mode_change_rebuilds_the_swapchain_and_scale_monitor_and_occlusion_do_not() {
        let t0 = Instant::now();
        let mut life = WindowLifecycle::new(HD, Policy::default());
        let mode = WindowEvent::DisplayChanged { change: DisplayChange::Mode };
        let renewed = life.event(&mode, t0).unwrap();
        assert_eq!(renewed.calls, [Call::RebuildSwapchain]);
        assert!(renewed.renews_surface());
        for quiet in [
            WindowEvent::DisplayChanged { change: DisplayChange::Scale },
            WindowEvent::DisplayChanged { change: DisplayChange::Monitor },
            WindowEvent::Occluded { occluded: true },
            WindowEvent::Occluded { occluded: false },
        ] {
            let said = life.event(&quiet, t0).expect("every window event is said");
            assert!(said.calls.is_empty(), "{quiet:?} -> {said:?}");
            assert!(!said.said.is_empty());
        }
        life.event(&resized(0, 0), t0);
        assert_eq!(calls(life.event(&mode, t0)), []);
        let back = life.event(&resized(1280, 720), t0).unwrap();
        assert!(back.said.contains("display change"), "{}", back.said);
        assert_eq!(back.calls, [Call::WithholdSurface(false)]);
        // Paused in the background, the restore is a new surface whatever changed.
        let mut device = WindowLifecycle::new(HD, DEVICE);
        device.event(&resized(0, 0), t0);
        assert_eq!(calls(device.event(&mode, t0)), []);
        assert!(calls(device.event(&resized(1280, 720), t0)).contains(&Call::SurfaceCreated));
    }

    /// **Input and the close request are not this module's**: no reaction, so no `WINDOW:` line
    /// per key press.
    #[test]
    fn input_and_the_close_request_are_not_window_state() {
        let mut life = WindowLifecycle::new(HD, Policy::default());
        for event in [
            WindowEvent::CloseRequested,
            WindowEvent::PointerMoved { x: 1, y: 2 },
            WindowEvent::KeyDown { keycode: 0x41, scancode: 0x1E, repeat: false },
            WindowEvent::PointerCaptureLost,
        ] {
            assert_eq!(life.event(&event, Instant::now()), None, "{event:?}");
        }
    }

    /// **The safety net**: out-of-date answers with no present for [`OUT_OF_DATE_BOUND`] have the
    /// engine rebuild its swapchain; when that does not bring frames back the next is a new
    /// surface, and the bound doubles; a present resets both. Out-of-date answers that frames
    /// follow are a resize the engine handled, and are left alone; and a minimised window, which
    /// has nothing to present to, is never renewed.
    #[test]
    fn out_of_date_with_no_present_past_the_bound_renews_the_surface_and_backs_off() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut life = WindowLifecycle::new(HD, Policy::default());
        // Presenting: nothing.
        assert_eq!(life.tick(at(0), 100, 0), None);
        assert_eq!(life.tick(at(100), 110, 0), None);
        // One out-of-date answer, then frames again: a resize the engine handled.
        assert_eq!(life.tick(at(200), 110, 1), None);
        assert_eq!(life.tick(at(300), 120, 1), None);
        assert_eq!(life.tick(at(5_000), 130, 1), None);
        // Out of date, and no present from then on.
        assert_eq!(life.tick(at(5_100), 130, 2), None);
        assert_eq!(life.tick(at(6_000), 130, 5), None);
        assert_eq!(life.tick(at(7_000), 130, 7), None, "under the bound");
        let renewed = life.tick(at(7_100), 130, 8).expect("past the bound");
        assert_eq!(renewed.calls, [Call::RebuildSwapchain]);
        // It did not help: the next waits twice as long, and is a new surface.
        assert_eq!(life.tick(at(7_200), 130, 9), None);
        assert_eq!(life.tick(at(10_000), 130, 12), None, "2.8 s: under the doubled bound");
        assert_eq!(life.tick(at(11_300), 130, 14).expect("4.1 s: past it").calls, new_surface(1280, 720));
        // Frames come back: the bound is reset, and the next is the rebuild again.
        assert_eq!(life.tick(at(11_400), 131, 14), None);
        assert_eq!(life.tick(at(11_500), 131, 15), None);
        assert_eq!(life.tick(at(13_600), 131, 16).expect("back to the first bound").calls, [Call::RebuildSwapchain]);
        // Minimised: nothing to present to, and no renewal however long it lasts.
        life.event(&resized(0, 0), at(13_700));
        for ms in (14_000..80_000).step_by(1_000) {
            assert_eq!(life.tick(at(ms), 131, 17 + ms), None);
        }
    }

    /// **Following the focus, the net does not run without it**: the game thread does not tick
    /// then, so no present is due.
    #[test]
    fn following_the_focus_the_net_waits_for_it() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut life = WindowLifecycle::new(HD, FOCUS);
        life.event(&WindowEvent::FocusChanged { focused: false }, at(0));
        for ms in (100..30_000).step_by(500) {
            assert_eq!(life.tick(at(ms), 10, ms), None);
        }
        life.event(&WindowEvent::FocusChanged { focused: true }, at(30_000));
        assert_eq!(life.tick(at(30_100), 10, 40_000), None);
        assert!(life.tick(at(32_200), 10, 40_001).is_some(), "with the focus back, it runs");
    }

    /// Each call names the native the startup rows name, with the descriptor they use, and its
    /// arguments are the ones those rows pass.
    #[test]
    fn each_call_is_the_native_and_arguments_the_startup_rows_use() {
        assert_eq!(Call::Pause.native(), Some(("onPauseNative", "(J)V")));
        assert_eq!(Call::WindowFocusChanged(true).native(), Some(("onWindowFocusChangedNative", "(JZ)V")));
        assert_eq!(
            Call::SurfaceChanged { width: 1, height: 2 }.native(),
            Some(("onSurfaceChangedNative", "(JLandroid/view/Surface;III)V"))
        );
        assert_eq!(Call::Process(ProcessEvent::Pause).native(), None);
        assert_eq!(Call::SurfaceChanged { width: 960, height: 540 }.tail(0x77), [
            GuestArg::Int(0x77),
            GuestArg::Int(1),
            GuestArg::Int(960),
            GuestArg::Int(540),
        ]);
        assert_eq!(Call::WindowFocusChanged(false).tail(0x77), [GuestArg::Int(0)]);
        assert_eq!(Call::TrimMemory(20).tail(0x77), [GuestArg::Int(20)]);
        assert_eq!(Call::SurfaceDestroyed.tail(0x77), []);
        assert_eq!(Call::WithholdSurface(true).native(), None, "the host's own layer, not a callback");
        assert_eq!(Call::RebuildSwapchain.native(), None);
        assert_eq!(Call::SurfaceChanged { width: 960, height: 540 }.to_string(), "onSurfaceChangedNative(960x540)");
    }
}
