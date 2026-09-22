//! [`HostWindowSource`]: the one [`WindowSource`] this workspace ships, over a real desktop
//! window.
//!
//! # What it is, and why it is a published cell rather than a query
//!
//! [`ndk::window`](super::window) says the width and the height are facts about the **host's**
//! output, and [`WindowSource`] is how an embedding hands them over. The obvious implementation
//! would hold an `omni_platform::window::Window` and call `client_size` from inside
//! `ANativeWindow_getWidth`. It cannot exist, for a reason that is a property of Win32 rather
//! than a choice anyone made here:
//!
//! * `omni_platform::window::Window` is **`!Send` and `!Sync`**, because window messages are
//!   delivered only to the thread that created the window — the window seam's "Thread affinity"
//!   records that the compiler refusing the move is better than an event queue that is
//!   permanently empty.
//! * `ANativeWindow_getWidth` is serviced on whichever **guest** thread called it. In
//!   `jni-surface.md` §8 that is the game thread `GameActivity_onCreate` spawns, which is not the
//!   thread that owns the window and never can be.
//!
//! So the pull stops one step short of the OS: the window's own thread calls [`sample`] once per
//! frame, which asks the OS and publishes the answer here, and the guest thread reads what was
//! published. That is written **once**, in this file, rather than in every embedding — the
//! alternative is each host inventing its own cell, and a cell that is nearly right is the shape
//! `VERIFICATION.md` entry 6 is full of.
//!
//! [`sample`]: HostWindowSource::sample
//!
//! # Why `sample` asks the OS instead of taking the number from a resize event
//!
//! Because the OS is the only thing that knows. `omni_platform::window::Window::client_size` is
//! documented as asking `GetClientRect` on every call rather than caching what the last
//! `WM_SIZE` said, and the reason is measured: the graphics spike watched this host's surface
//! extent drift **41 times across 5 seconds with the window untouched**
//! (`docs/research/graphics-spike.md` §4, a Parsec virtual-display adapter renegotiating the
//! desktop). A source fed from [`WindowEvent::Resized`](omni_platform::window::WindowEvent)
//! alone would be right after every resize a *user* performed and stale after every one the
//! *display* performed. [`HostWindowSource::publish`] exists anyway, because it is what `sample`
//! is written in terms of and because it is the only way the packing and the no-pixels rule can
//! be tested on a machine with no display at all — but it is not the path a host should build
//! its frame loop on.
//!
//! # Five targets, and nothing fabricated for four of them
//!
//! This file contains no `cfg` and names no OS type. On Linux and macOS
//! `omni_platform::window::Window` is `enum Window {}` — **uninhabited** — so `Window::new`
//! returns [`WindowError::Unsupported`](omni_platform::window::WindowError::Unsupported) naming
//! the API it intends to reach for, and a `&Window` can never be constructed there at all.
//! [`HostWindowSource::watching`] and [`HostWindowSource::sample`] therefore compile on all five
//! targets and are **unreachable** on four of them, which is better than four fabricated
//! failures of their own: the refusal a Linux host gets is the window seam's, at `Window::new`,
//! which is where the missing work actually is. [`HostWindowSource::publish`] has no OS in it,
//! works everywhere, and is what this file's own tests are written against for that reason.
//! Nothing here claims those targets work, and nothing here stops them working when the window
//! seam grows its backends.

use std::sync::Arc;

use omni_platform::window::{RawWindow, Window};
use parking_lot::Mutex;

use crate::error::{AbiError, AbiResult};

use super::window::{WindowGeometry, WindowSource};

/// What has been published, and how many times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Published {
    /// The last client size that was a client size, or `None` for a window with no pixels.
    geometry: Option<WindowGeometry>,
    /// How many times [`HostWindowSource::publish`] has been reached, whatever it published.
    ///
    /// Zero means **nothing has been published yet**, which is a different host state from a
    /// window that reported no pixels and is printed differently by this type's `Debug` — the
    /// refusal `ANativeWindow_getWidth` produces interpolates that `Debug`, so the two are told
    /// apart in the log rather than in a comment.
    samples: u64,
}

/// A [`WindowSource`] fed by a real desktop window's own thread.
///
/// Attach it with [`Ndk::set_window_source`](super::Ndk::set_window_source), keep it alive for as
/// long as the window, and call [`HostWindowSource::sample`] from the window's thread every time
/// round the frame loop. See this module's documentation for why the last part cannot be done
/// from the guest thread instead.
pub struct HostWindowSource {
    published: Mutex<Published>,
    /// The OS handle of the window being sampled, for
    /// [`WindowSource::raw_window`](super::window::WindowSource::raw_window).
    ///
    /// A separate cell from [`Published`] rather than a field of it, because it is published by a
    /// different event: the geometry changes every frame and the handle changes **once**, when a
    /// window is attached. Folding them together would mean every `publish` carried a handle it
    /// had no way to know, and a host feeding this source by hand would have to supply one.
    ///
    /// `RawWindow` is `Copy` and is two integers, so nothing is borrowed across a guest call.
    raw: Mutex<Option<RawWindow>>,
}

impl HostWindowSource {
    /// A source that has already sampled `window`, so it is never asked before it has an answer.
    ///
    /// Taking the window here rather than offering a bare constructor is deliberate: a source
    /// that has been attached to an [`Ndk`](super::Ndk) but never fed answers `None`, and the
    /// guest's first `ANativeWindow_getWidth` then refuses for a reason that has nothing to do
    /// with the window. Seeding at construction makes that state unreachable for every host that
    /// has a window, which is every host this constructor is for.
    ///
    /// A **minimised** window is still a successful construction: it publishes no geometry and
    /// [`WindowSource::geometry`] answers `None` until it is restored, which is the honest
    /// reading of a window that has no pixels.
    ///
    /// # Errors
    ///
    /// Whatever [`HostWindowSource::sample`] refuses for: the window seam's own failure, or a
    /// dimension that does not fit the `int32_t` the NDK returns.
    pub fn watching(window: &Window) -> AbiResult<Arc<HostWindowSource>> {
        let source = HostWindowSource::unpublished();
        // Taken here and not in `sample`, because it does not change: `omni_platform`'s window
        // holds one `HWND` for its whole life and `Window::raw` is a field read. Sampling it every
        // frame would suggest to a reader that it could differ between frames.
        source.set_raw_window(window.raw());
        source.sample(window)?;
        Ok(source)
    }

    /// A source with nothing published yet, for a host that feeds it from somewhere else.
    ///
    /// [`WindowSource::geometry`] answers `None` until [`HostWindowSource::publish`] is called,
    /// and `ANativeWindow_getWidth` refuses while that is true — the refusal interpolates this
    /// type's `Debug`, which says `nothing published yet` rather than naming a size, so an
    /// unfed source is not mistaken in the log for a minimised window.
    #[must_use]
    pub fn unpublished() -> Arc<HostWindowSource> {
        let published = Published { geometry: None, samples: 0 };
        Arc::new(HostWindowSource { published: Mutex::new(published), raw: Mutex::new(None) })
    }

    /// Publish the OS handle of the window this source describes.
    ///
    /// [`HostWindowSource::watching`] does this for you. It is public for the host that feeds a
    /// source through [`HostWindowSource::publish`] and still has a real window — and for the one
    /// that does not, which simply never calls it and whose
    /// [`super::window::WindowSource::raw_window`] stays `None`.
    ///
    /// Stage 3's `vkCreateAndroidSurfaceKHR` is the only consumer there will be; see that trait
    /// method for what a shim does with a `None`.
    pub fn set_raw_window(&self, raw: RawWindow) {
        *self.raw.lock() = Some(raw);
    }

    /// Ask `window` for its client area **now** and publish it. Call this from the window's own
    /// thread, once per frame.
    ///
    /// Returns what was published: `Some` for a window with pixels, `None` for one with none.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] naming this call, for either of two things. The window seam
    /// refused — on Windows that is `GetClientRect` failing — in which case the seam's own
    /// message is carried through verbatim, because it already names the operation, the API and
    /// the code. Or an axis does not fit in an `int32_t`, which is
    /// [`HostWindowSource::publish`]'s refusal.
    ///
    /// The window seam's [`WindowError`](omni_platform::window::WindowError) is **rendered into**
    /// [`AbiError`] rather than returned as itself, and what that costs is
    /// `WindowError::is_unsupported` — a caller can no longer branch on "this target was never
    /// built out" without reading the string. It is worth it here because this is the only
    /// function in the crate that would have a second host-facing error type, and because the
    /// thing a host does with either failure is the same: stop, and print what it says.
    pub fn sample(&self, window: &Window) -> AbiResult<Option<WindowGeometry>> {
        let (width, height) = window.client_size().map_err(|err| AbiError::Refused {
            symbol: "HostWindowSource::sample".to_string(),
            address: 0,
            why: format!(
                "the host window could not be asked for its client size, so there is nothing to \
                 publish and `ANativeWindow_getWidth` would answer a size that is no longer \
                 true: {err}"
            ),
        })?;
        self.publish(width, height)
    }

    /// Publish a client size the host already has, in physical pixels.
    ///
    /// **Zero in either axis publishes "no pixels"**, not a refusal: that is what a minimised
    /// Win32 window reports, `omni_platform::window::Window::client_size` documents it, and it
    /// is an ordinary state a frame loop passes through. [`WindowSource::geometry`] then answers
    /// `None` and `ANativeWindow_getWidth` refuses naming this source — which is the right place
    /// for that to surface, because there genuinely is no surface extent to report and the fix
    /// is the embedding's.
    ///
    /// Returns what was published.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] naming the axis, when a dimension does not fit in the `int32_t`
    /// `ANativeWindow_getWidth` returns. Nothing this seam produces can reach that —
    /// `omni_platform::window::MAX_EXTENT` is 65,535, because `WM_SIZE` carries the client size
    /// in the two 16-bit halves of its `LPARAM` — so this is the arm for a host publishing a
    /// number it did not get from a window, and it refuses rather than truncating, because a
    /// truncated width is a plausible width.
    pub fn publish(&self, width: u32, height: u32) -> AbiResult<Option<WindowGeometry>> {
        let fits = |axis: &str, getter: &str, value: u32| {
            i32::try_from(value).map_err(|_| AbiError::Refused {
                symbol: "HostWindowSource::publish".to_string(),
                address: 0,
                why: format!(
                    "a {axis} of {value} physical pixels was published, and `{getter}` returns \
                     an `int32_t`. Narrowing it would hand the guest a plausible number that is \
                     not the window's"
                ),
            })
        };
        let width = fits("width", "ANativeWindow_getWidth", width)?;
        let height = fits("height", "ANativeWindow_getHeight", height)?;
        // `WindowGeometry::new` is what refuses a non-positive axis, and here that refusal is
        // the *expected* answer rather than an error: a minimised window is published as "no
        // geometry", so the `Err` is discarded and the absence is what is stored.
        let geometry = WindowGeometry::new(width, height).ok();
        let mut published = self.published.lock();
        published.geometry = geometry;
        published.samples += 1;
        Ok(geometry)
    }

    /// How many times this source has been published to, by [`HostWindowSource::sample`] or
    /// directly.
    ///
    /// Diagnostic (Global Constraint 6), and the one that answers the question a stale geometry
    /// raises: a guest reading a size that does not match the window is a source nobody is
    /// feeding, and this is what says so. A count that is not rising while the frame loop runs
    /// is the whole diagnosis.
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.published.lock().samples
    }
}

impl WindowSource for HostWindowSource {
    fn geometry(&self) -> Option<WindowGeometry> {
        self.published.lock().geometry
    }

    fn raw_window(&self) -> Option<RawWindow> {
        *self.raw.lock()
    }
}

impl core::fmt::Debug for HostWindowSource {
    /// Prints what would be answered and how many samples produced it.
    ///
    /// This is not a convenience: `ANativeWindow_getWidth`'s refusal for a source with no
    /// geometry interpolates this, so these words are what a host reads when the guest is
    /// refused. "nothing published yet" and "no pixels" are different host mistakes with
    /// different fixes, and `None` printed for both would be neither.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let published = *self.published.lock();
        let state = match (published.geometry, published.samples) {
            (Some(g), _) => format!("{}x{}", g.width, g.height),
            (None, 0) => "nothing published yet".to_string(),
            (None, _) => "no pixels (a minimised window reports 0x0)".to_string(),
        };
        write!(f, "HostWindowSource {{ {state}, samples {} }}", published.samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A published size is what the source answers, and a zero axis is absence rather than a
    /// number.**
    ///
    /// Written against [`HostWindowSource::publish`] rather than against a window on purpose:
    /// this is the half of the type that has no OS in it, so it is asserted on every target
    /// including the four that have no window backend at all.
    ///
    /// The two axes are published with **different** values, because a source that answered the
    /// width for both would pass a test that used a square (`tools/mutate.py` row `window-A2` is
    /// that mistake one layer down).
    #[test]
    fn a_published_size_is_answered_and_a_zero_axis_is_no_geometry() {
        let source = HostWindowSource::unpublished();
        assert_eq!(source.geometry(), None, "an unfed source has no answer");
        assert_eq!(source.samples(), 0);

        assert_eq!(source.publish(1280, 720).expect("a publishable size"),
            Some(WindowGeometry::new(1280, 720).expect("a positive geometry")));
        assert_eq!(source.geometry().map(|g| (g.width, g.height)), Some((1280, 720)));
        assert_eq!(source.samples(), 1);

        // A resize: the same source, a new answer, no new source.
        assert!(source.publish(1920, 1080).is_ok());
        assert_eq!(source.geometry().map(|g| (g.width, g.height)), Some((1920, 1080)));

        // Minimised. Not an error, and not a size.
        assert_eq!(source.publish(0, 0).expect("a minimised window publishes"), None);
        assert_eq!(source.geometry(), None);
        // One axis is enough: a window can be zero-height without being zero-width.
        assert!(source.publish(1280, 0).expect("a zero axis publishes").is_none());
        assert!(source.publish(0, 720).expect("a zero axis publishes").is_none());
        assert_eq!(source.samples(), 5, "every publish counts, including the ones with no size");
    }

    /// **An axis that does not fit an `int32_t` is refused rather than narrowed**, and the
    /// refusal names the axis and the value.
    #[test]
    fn an_axis_past_int32_is_refused_and_named() {
        let source = HostWindowSource::unpublished();
        let too_wide = u32::try_from(i32::MAX).expect("i32::MAX fits u32") + 1;
        let error = source.publish(too_wide, 720).expect_err("a width past int32_t is refused");
        assert_eq!(error.symbol(), Some("HostWindowSource::publish"));
        let text = error.to_string();
        assert!(text.contains("width"), "{error}");
        assert!(text.contains(&too_wide.to_string()), "the refusal names the value: {error}");

        let error = source.publish(1280, too_wide).expect_err("a height past int32_t is refused");
        assert!(error.to_string().contains("ANativeWindow_getHeight"), "{error}");
        assert_eq!(source.geometry(), None, "a refused publish stores nothing");
    }

    /// **A source with no window answers `None` for the handle, and a published one answers the
    /// handle it was given.**
    ///
    /// Written against [`HostWindowSource::set_raw_window`] rather than against a window, for the
    /// reason the test above is: this half has no OS in it and runs on all five targets, including
    /// the four where a `&Window` cannot be constructed at all. The end-to-end half — that the
    /// handle is the one `GetClientRect` was asked about — belongs to `tests/ndk_host_window.rs`,
    /// where there is a window.
    #[test]
    fn a_source_with_no_window_has_no_raw_handle() {
        let source = HostWindowSource::unpublished();
        assert_eq!(source.raw_window(), None, "an unfed source names no window");
        source.publish(1280, 720).expect("a publishable size");
        assert_eq!(
            source.raw_window(),
            None,
            "publishing a size must not invent a handle: a host compositing into something that \
             is not an OS window has a size and no HWND"
        );

        let handle = RawWindow::Win32 { hwnd: 0x1234, hinstance: 0x5678 };
        source.set_raw_window(handle);
        assert_eq!(source.raw_window(), Some(handle));
        assert_eq!(
            source.geometry().map(|g| (g.width, g.height)),
            Some((1280, 720)),
            "and the geometry is untouched by it"
        );
    }

    /// **The three states print differently**, because the refusal `ANativeWindow_getWidth`
    /// produces for a source with no geometry carries this text and is all a host gets.
    #[test]
    fn the_debug_tells_an_unfed_source_from_a_minimised_one() {
        let source = HostWindowSource::unpublished();
        assert!(format!("{source:?}").contains("nothing published yet"), "{source:?}");
        source.publish(0, 0).expect("a minimised window publishes");
        let minimised = format!("{source:?}");
        assert!(minimised.contains("no pixels"), "{minimised}");
        assert!(!minimised.contains("nothing published yet"), "{minimised}");
        source.publish(800, 600).expect("a publishable size");
        assert!(format!("{source:?}").contains("800x600"), "{source:?}");
    }
}
