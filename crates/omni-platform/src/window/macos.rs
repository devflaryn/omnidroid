//! macOS backend for the window seam: an `NSWindow` whose content view is backed by a
//! `CAMetalLayer`, driven from any thread.
//!
//! # The shape, and the one thing about it that is not Win32's
//!
//! AppKit runs on the main thread only, and this seam's callers are not on it (libtest's threads,
//! the gate's test thread). So the main thread is handed to AppKit before `main` runs -- see
//! [`main_thread`] for the mechanism, the alternatives rejected and when it declines -- and a
//! [`Window`] here is a **proxy**: every AppKit call is sent to that thread synchronously
//! (`dispatch_sync_f` to the main queue) and returns its answer, and every event the window
//! produces is pushed, on that thread, into a queue this proxy owns. [`Window::poll`] drains the
//! queue without crossing threads; [`Window::wait`] blocks on the queue's condition variable, not
//! on a run loop.
//!
//! The seam's contract survives unchanged: the proxy is `!Send` like every `Window`, events come
//! out in order with the same coalescing (the shared `push_event`), sizes are physical pixels, the
//! close button is a request. The difference is only *where* the host's work happens.
//!
//! # Pixels
//!
//! AppKit measures in points; a Retina window has two physical pixels per point. Sizes are the
//! view's bounds through `-[NSView convertRectToBacking:]`, positions are points times
//! `backingScaleFactor`, and a size is requested as pixels divided by the window's own scale.
//! [`Window::dpi`] is `96 × backingScaleFactor`: the Windows convention (96 at 100%) applied to
//! the same fact -- the scale the user's display setting puts between logical units and pixels --
//! so that the gate's `density = dpi / 96` is the backing scale, 2.0 on a Retina display.
//!
//! # What this backend cannot do, stated rather than hidden
//!
//! * **Captured motion is accelerated.** AppKit's mouse deltas come after the system's pointer
//!   ballistics; the seam asks for the device's counts. See `appkit::OmniView::motion`.
//! * **The hidden cursor is a cursor rect, so it is AppKit's to apply -- and only for the key
//!   window.** `set_cursor_hidden` puts an invisible `NSCursor` over the view with
//!   `-[NSView addCursorRect:cursor:]`, as winit's invisible cursor does. The seam asks for it over
//!   an inactive window too (the pointer's moves over it are reported, and the game draws its own
//!   cursor there), and AppKit has no public way to give it: cursor rects are applied only in the
//!   key window, and the window server gives the cursor to the frontmost application. What is
//!   done is the one thing public API allows -- the view sets the invisible cursor from the moves
//!   its always-active tracking area still delivers while the window is not key, and the arrow
//!   when the pointer leaves -- and whether the window server honours a background application's
//!   `-[NSCursor set]` is **not known here** (untested: no Mac). The alternative is the private
//!   `CGSSetConnectionProperty(..., "SetsCursorInBackground", true)`, which this backend does not
//!   use. `+[NSCursor hide]` was not used either: one counter for the whole application.
//! * **A window created while the main thread is unavailable is refused**, with
//!   [`WindowError::MainThreadUnavailable`] naming why; see [`main_thread`].

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{RawWindow, WindowDesc, WindowError, WindowEvent, WindowResult};

mod appkit;
pub mod keys;
mod main_thread;

use main_thread::{on_main, Status};

/// The AppKit thread this module owns, for the one other seam that needs it: the web view
/// (`crate::webview`'s macOS backend), whose `WKWebView` is main-thread-only exactly as a window is.
pub(crate) mod appkit_thread {
    pub(crate) use super::appkit::start_application;
    pub(crate) use super::main_thread::{on_main, status, Status};
}

/// The queue between the AppKit thread, which fills it, and the proxy's thread, which drains it.
pub(super) struct Shared {
    events: Mutex<Vec<WindowEvent>>,
    arrived: Condvar,
    /// Whether the pointer capture is held. Written on the AppKit thread; read from both.
    captured: AtomicBool,
    /// Whether `set_cursor_hidden` asks for the cursor to be hidden over the view. Written on the
    /// AppKit thread; read from both.
    hide_cursor: AtomicBool,
    /// Whether the window is the key window: the focus, as `windowDidBecomeKey:` and
    /// `windowDidResignKey:` report it. Written on the AppKit thread; read from both.
    key: AtomicBool,
    /// The seam's buttons reported down and not yet up, by [`button_bit`]: what [`Window::poll`]
    /// asks the host about ("A button reported down is reported up" in `super`).
    buttons_down: AtomicU32,
    /// The pointer's last reported position, `x` in the high half and `y` in the low: where a
    /// release `poll` reports for a lost one is.
    last_pointer: AtomicU64,
}

/// The bit a seam button is tracked by in [`Shared::buttons_down`].
pub(super) const fn button_bit(button: super::PointerButton) -> u32 {
    match button {
        super::PointerButton::Primary => 1,
        super::PointerButton::Secondary => 2,
        super::PointerButton::Middle => 4,
        super::PointerButton::Back => 8,
        super::PointerButton::Forward => 16,
    }
}

/// The `CGMouseButton` a seam button is: left 0, right 1, centre 2, and the others by number, as
/// `-[NSEvent buttonNumber]` numbers them (`appkit::other_button`).
const fn cg_button(button: super::PointerButton) -> u32 {
    match button {
        super::PointerButton::Primary => 0,
        super::PointerButton::Secondary => 1,
        super::PointerButton::Middle => 2,
        super::PointerButton::Back => 3,
        super::PointerButton::Forward => 4,
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// `CGEventSourceButtonState(3)`: whether a mouse button is down now, for a state table.
    /// Callable from any thread.
    fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
}

/// `kCGEventSourceStateCombinedSessionState`: every source in the login session.
const COMBINED_SESSION_STATE: i32 = 0;

impl Shared {
    fn new() -> Shared {
        Shared {
            events: Mutex::new(Vec::new()),
            arrived: Condvar::new(),
            captured: AtomicBool::new(false),
            hide_cursor: AtomicBool::new(false),
            key: AtomicBool::new(false),
            buttons_down: AtomicU32::new(0),
            last_pointer: AtomicU64::new(0),
        }
    }

    /// Queue `event` with the seam's coalescing rule, and wake a waiter.
    fn push(&self, event: WindowEvent) {
        let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        super::push_event(&mut events, event);
        drop(events);
        self.arrived.notify_all();
    }

    fn captured(&self) -> bool {
        self.captured.load(Ordering::Acquire)
    }

    /// Set the capture flag; answers whether it changed.
    fn set_captured(&self, captured: bool) -> bool {
        self.captured.swap(captured, Ordering::AcqRel) != captured
    }

    fn hide_requested(&self) -> bool {
        self.hide_cursor.load(Ordering::Acquire)
    }

    /// Set the hide request; answers whether it changed.
    fn set_hide_requested(&self, hidden: bool) -> bool {
        self.hide_cursor.swap(hidden, Ordering::AcqRel) != hidden
    }

    fn key(&self) -> bool {
        self.key.load(Ordering::Acquire)
    }

    fn set_key(&self, key: bool) {
        self.key.store(key, Ordering::Release);
    }

    /// Record a position a pointer event carried.
    fn set_last_pointer(&self, x: i32, y: i32) {
        self.last_pointer.store((u64::from(x as u32) << 32) | u64::from(y as u32), Ordering::Release);
    }

    fn last_pointer(&self) -> (i32, i32) {
        let packed = self.last_pointer.load(Ordering::Acquire);
        ((packed >> 32) as u32 as i32, packed as u32 as i32)
    }

    /// Record a button going down or up, as reported.
    fn set_button(&self, button: super::PointerButton, down: bool) {
        if down {
            self.buttons_down.fetch_or(button_bit(button), Ordering::AcqRel);
        } else {
            self.buttons_down.fetch_and(!button_bit(button), Ordering::AcqRel);
        }
    }
}

/// A window on the AppKit thread, as the thread that made it sees it.
pub(super) struct Window {
    /// The id `appkit`'s main-thread registry holds the `NSWindow` and its view under.
    id: u64,
    shared: Arc<Shared>,
    raw: RawWindow,
}

impl Window {
    /// Refuse without an AppKit thread; otherwise build the window on it.
    pub(super) fn create(desc: &WindowDesc<'_>) -> WindowResult<Self> {
        let status = main_thread::status();
        if status != Status::Serving {
            return Err(WindowError::MainThreadUnavailable { operation: "create", why: status.why() });
        }
        let shared = Arc::new(Shared::new());
        let (title, width, height) = (desc.title, desc.width, desc.height);
        let for_view = Arc::clone(&shared);
        let (id, raw) = on_main(move |mtm| appkit::create(mtm, title, width, height, for_view))?;
        Ok(Window { id, shared, raw })
    }

    pub(super) fn show(&self) {
        let id = self.id;
        on_main(move |mtm| appkit::show(mtm, id));
    }

    /// Everything queued since the last poll. No thread is crossed: the AppKit thread queued it.
    ///
    /// **A button reported down that the host has up is released first**: while any is down, the
    /// session's button state (`CGEventSourceButtonState`, callable from any thread) is asked, and
    /// a release no event brought is queued behind whatever is queued already.
    pub(super) fn poll(&mut self, sink: &mut Vec<WindowEvent>) {
        let down = self.shared.buttons_down.load(Ordering::Acquire);
        if down != 0 {
            for button in [
                super::PointerButton::Primary,
                super::PointerButton::Secondary,
                super::PointerButton::Middle,
                super::PointerButton::Back,
                super::PointerButton::Forward,
            ] {
                // SAFETY: plain CoreGraphics call with by-value arguments.
                let held = unsafe { CGEventSourceButtonState(COMBINED_SESSION_STATE, cg_button(button)) };
                if down & button_bit(button) != 0 && !held {
                    let before = self.shared.buttons_down.fetch_and(!button_bit(button), Ordering::AcqRel);
                    // Only if no real release got there first.
                    if before & button_bit(button) != 0 {
                        let (x, y) = self.shared.last_pointer();
                        self.shared.push(WindowEvent::PointerUp { button, x, y });
                    }
                }
            }
        }
        let mut events = self.shared.events.lock().unwrap_or_else(PoisonError::into_inner);
        sink.append(&mut events);
    }

    pub(super) fn client_size(&self) -> WindowResult<(u32, u32)> {
        let id = self.id;
        Ok(on_main(move |_| appkit::client_size(id)))
    }

    /// `96 × backingScaleFactor`, rounded: 192 on a Retina display. See this module's "Pixels".
    pub(super) fn dpi(&self) -> WindowResult<u32> {
        let id = self.id;
        let scale = on_main(move |_| appkit::backing_scale(id));
        Ok((96.0 * scale).round() as u32)
    }

    pub(super) fn set_client_size(&self, width: u32, height: u32, operation: &'static str) -> WindowResult<()> {
        let _ = operation;
        let id = self.id;
        on_main(move |_| appkit::set_client_size(id, width, height));
        Ok(())
    }

    pub(super) fn set_minimized(&self, minimized: bool) -> WindowResult<()> {
        let id = self.id;
        on_main(move |_| appkit::set_minimized(id, minimized));
        Ok(())
    }

    pub(super) fn request_close(&self) -> WindowResult<()> {
        let id = self.id;
        on_main(move |_| appkit::request_close(id));
        Ok(())
    }

    pub(super) fn set_pointer_capture(&mut self, captured: bool) -> WindowResult<bool> {
        let id = self.id;
        on_main(move |mtm| appkit::set_pointer_capture(mtm, id, captured))
    }

    pub(super) fn has_pointer_capture(&self) -> bool {
        self.shared.captured()
    }

    pub(super) fn set_cursor_hidden(&mut self, hidden: bool) -> WindowResult<()> {
        let id = self.id;
        on_main(move |_| appkit::set_cursor_hidden(id, hidden));
        Ok(())
    }

    /// Asked for and the key window: what the cursor rect is in force for.
    pub(super) fn cursor_hidden(&self) -> bool {
        self.shared.hide_requested() && self.shared.key()
    }

    /// The key window, as the delegate last heard.
    pub(super) fn has_focus(&self) -> bool {
        self.shared.key()
    }

    pub(super) fn warp_pointer(&mut self, x: i32, y: i32) -> WindowResult<()> {
        let id = self.id;
        on_main(move |mtm| appkit::warp_pointer(mtm, id, x, y));
        Ok(())
    }

    /// Block on the queue's condition until it is non-empty or `timeout` passes.
    pub(super) fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut events = self.shared.events.lock().unwrap_or_else(PoisonError::into_inner);
        while events.is_empty() {
            let left = match deadline {
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => left,
                    _ => return false,
                },
                // A timeout past `Instant`'s range: wait a day at a time, for ever.
                None => Duration::from_secs(86_400),
            };
            events = self.shared.arrived.wait_timeout(events, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }

    /// The image goes to the AppKit thread (a copy: the proxy's caller keeps its buffer), where it
    /// becomes the contents of a layer over the view: `appkit::present_rgba`.
    pub(super) fn present_rgba(&mut self, rgba: &[u8], width: u32, height: u32) -> WindowResult<()> {
        self.presenter().present_rgba(rgba, width, height)
    }

    /// A handle presenting from any thread: the window's id, which every call carries to the
    /// AppKit thread -- as this proxy's own calls do -- and which finds nothing once it is closed.
    pub(super) fn presenter(&self) -> Presenter {
        Presenter { id: self.id }
    }

    pub(super) fn raw(&self) -> RawWindow {
        self.raw
    }
}

/// Presents to a window from any thread: see [`super::Presenter`].
#[derive(Clone)]
pub(super) struct Presenter {
    id: u64,
}

impl Presenter {
    pub(super) fn present_rgba(&self, rgba: &[u8], width: u32, height: u32) -> WindowResult<()> {
        let (id, pixels) = (self.id, rgba.to_vec());
        on_main(move |_| appkit::present_rgba(id, pixels, width, height))
    }

    pub(super) fn client_size(&self) -> Option<(u32, u32)> {
        let id = self.id;
        on_main(move |_| appkit::live_client_size(id))
    }
}

impl Drop for Window {
    /// Closed on the AppKit thread; the view (and its reference to the queue) goes with it.
    fn drop(&mut self) {
        let id = self.id;
        on_main(move |_| appkit::destroy(id));
    }
}

/// Where the Vulkan loader may be on this host, in the order to try: see
/// [`super::vulkan_loader_candidates`].
///
/// MEASURED on this host (Homebrew `vulkan-loader` 1.4.357 and `molten-vk` 1.4.2, nothing in
/// `/usr/local/lib`): `dlopen("libvulkan.dylib")` -- the leaf name `ash::Entry::load` uses -- fails,
/// because dyld's default search is `/usr/lib` and the working directory; `/opt/homebrew/lib/
/// libvulkan.1.dylib` loads, offers `VK_KHR_portability_enumeration`, and without it
/// `vkCreateInstance` answers `VK_ERROR_INCOMPATIBLE_DRIVER`; `/opt/homebrew/lib/libMoltenVK.dylib`
/// loaded directly as the loader creates an instance and enumerates the Apple M1 with no
/// portability flag at all (n=1 each). So the leaf names come first (they find whatever
/// `DYLD_LIBRARY_PATH`/`DYLD_FALLBACK_LIBRARY_PATH` or the LunarG SDK's `/usr/local/lib` puts
/// there), then the loader at Homebrew's two prefixes, then MoltenVK itself as the last resort --
/// usable, measured, but with no layers and no loader in between.
pub(super) const VULKAN_LOADER_CANDIDATES: &[&str] = &[
    "libvulkan.dylib",
    "libvulkan.1.dylib",
    "/opt/homebrew/lib/libvulkan.1.dylib",
    "/usr/local/lib/libvulkan.1.dylib",
    "/opt/homebrew/lib/libMoltenVK.dylib",
    "/usr/local/lib/libMoltenVK.dylib",
];
