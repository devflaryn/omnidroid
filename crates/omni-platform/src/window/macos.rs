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
//! * **A window created while the main thread is unavailable is refused**, with
//!   [`WindowError::MainThreadUnavailable`] naming why; see [`main_thread`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{RawWindow, WindowDesc, WindowError, WindowEvent, WindowResult};

mod appkit;
pub mod keys;
mod main_thread;

use main_thread::{on_main, Status};

/// The queue between the AppKit thread, which fills it, and the proxy's thread, which drains it.
pub(super) struct Shared {
    events: Mutex<Vec<WindowEvent>>,
    arrived: Condvar,
    /// Whether the pointer capture is held. Written on the AppKit thread; read from both.
    captured: AtomicBool,
}

impl Shared {
    fn new() -> Shared {
        Shared { events: Mutex::new(Vec::new()), arrived: Condvar::new(), captured: AtomicBool::new(false) }
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
    pub(super) fn poll(&mut self, sink: &mut Vec<WindowEvent>) {
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

    pub(super) fn raw(&self) -> RawWindow {
        self.raw
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
