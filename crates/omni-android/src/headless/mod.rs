//! **Headless mode: the frame's GPU work dropped where the engine cannot see it, reversibly.**
//!
//! An instance nobody is watching (a farm of clients, a notebook, a container with no display)
//! still has its engine render every frame, and the GPU does all of it for nobody. Headless mode
//! drops the **draws into the frame's own render targets** at the layer the engine records through
//! -- Vulkan at record time ([`crate::vulkan`]), GLES at the call ([`crate::gles`]) -- and forwards
//! everything else: render-pass begins and ends (so load/store ops and layout transitions happen),
//! barriers, copies, uploads, shader compiles, compute, submits, fences, presents and swaps. Every
//! call the engine makes returns what it would have returned, every fence signals and every frame
//! is presented; what is missing is the pixels.
//!
//! * **Which targets** is [`history`]'s rule: the swapchain's, and those rendered in most recent
//!   frames. A target drawn into now and then is kept real, so nothing the engine renders once and
//!   keeps is lost.
//! * **Reversible at runtime**: [`Headless::set`]. Turning it off takes effect for the next
//!   recording; per-frame targets are redrawn by the next frame, which is why only they are dropped.
//! * **A screenshot while headless** ([`Headless::request_screenshot`]) renders for real from the
//!   request on, waits [`WARMUP_FRAMES`] frames so that targets that feed each other (a previous
//!   frame's depth, an exposure) are whole again, and reads the frame being presented back before
//!   it is presented -- that exact frame, not the window's contents.
//!
//! The control channel is the embedding's: [`control`] parses its commands.

pub mod control;
pub mod history;
pub mod png;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

use history::FrameHistory;

/// How many frames a screenshot requested while headless renders for real before the one it saves.
pub const WARMUP_FRAMES: u64 = 3;

/// A screenshot asked for and not yet taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Where the PNG goes.
    pub path: PathBuf,
    /// The first frame (present count) whose present it reads back.
    pub due: u64,
}

/// One renderer layer's headless state: the switch, the frame count, the targets' history and a
/// pending screenshot.
#[derive(Debug, Default)]
pub struct Headless {
    on: AtomicBool,
    /// Presents so far.
    frame: AtomicU64,
    /// Draws dropped, and draws made into targets kept real while on.
    dropped: AtomicU64,
    kept: AtomicU64,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    history: FrameHistory,
    screenshot: Option<Pending>,
}

impl Headless {
    /// Off, with nothing recorded.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Turn headless mode on or off. Takes effect from the next recording.
    pub fn set(&self, on: bool) {
        self.on.store(on, Ordering::Release);
    }

    /// Whether headless mode is on.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// Whether a recording beginning now drops: on, and no screenshot waiting for a real frame.
    #[must_use]
    pub fn drops_now(&self) -> bool {
        self.is_on() && self.inner.lock().screenshot.is_none()
    }

    /// The frame being recorded: presents so far.
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.frame.load(Ordering::Acquire)
    }

    /// A present returned: the next frame begins.
    pub fn presented(&self) {
        self.frame.fetch_add(1, Ordering::AcqRel);
    }

    /// Target `key` was rendered into this frame.
    pub fn rendered(&self, key: u64) {
        let frame = self.frame();
        self.inner.lock().history.rendered(key, frame);
    }

    /// Target `key` is the swapchain's: always per-frame.
    pub fn mark_swapchain_target(&self, key: u64) {
        self.inner.lock().history.mark_always(key);
    }

    /// Target `key` was destroyed.
    pub fn forget_target(&self, key: u64) {
        self.inner.lock().history.forget(key);
    }

    /// Whether target `key` is per-frame now.
    #[must_use]
    pub fn per_frame(&self, key: u64) -> bool {
        let frame = self.frame();
        self.inner.lock().history.per_frame(key, frame)
    }

    /// Count one draw: dropped, or made (into a target kept real while on).
    pub fn count_draw(&self, dropped: bool) {
        if dropped {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        } else if self.is_on() {
            self.kept.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Ask for the frame presented [`WARMUP_FRAMES`] from now (headless) or the next one (not) to
    /// be saved at `path`. A second request before the first is taken replaces it.
    pub fn request_screenshot(&self, path: &Path) {
        let warmup = if self.is_on() { WARMUP_FRAMES } else { 0 };
        let due = self.frame() + warmup;
        self.inner.lock().screenshot = Some(Pending { path: path.to_path_buf(), due });
    }

    /// The screenshot due at the present about to happen, taken: the caller reads that frame back.
    #[must_use]
    pub fn take_due_screenshot(&self) -> Option<Pending> {
        let frame = self.frame();
        let mut inner = self.inner.lock();
        if inner.screenshot.as_ref().is_some_and(|pending| frame >= pending.due) {
            inner.screenshot.take()
        } else {
            None
        }
    }

    /// Put a screenshot back that could not be taken at this present (none to read).
    pub fn defer_screenshot(&self, pending: Pending) {
        let mut inner = self.inner.lock();
        if inner.screenshot.is_none() {
            inner.screenshot = Some(pending);
        }
    }

    /// One line: on or off, frames, targets, draws dropped and kept.
    #[must_use]
    pub fn status(&self) -> String {
        let frame = self.frame();
        let (targets, per_frame) = self.inner.lock().history.census(frame);
        format!(
            "headless {}; {frame} frames presented; {targets} render targets seen, {per_frame} per-frame; \
             {} draws dropped, {} made into targets kept real while headless",
            if self.is_on() { "on" } else { "off" },
            self.dropped.load(Ordering::Relaxed),
            self.kept.load(Ordering::Relaxed)
        )
    }
}

/// Encode `rgba` and write it to `path` on a thread of its own, then say so on stderr:
/// `SCREENSHOT: saved <path> <w>x<h>`, or `SCREENSHOT: failed <path>: <why>`.
pub fn save_png(path: PathBuf, width: u32, height: u32, rgba: Vec<u8>) {
    let spawned = std::thread::Builder::new().name("omni-screenshot".into()).spawn(move || {
        let file = png::encode_rgba(width, height, &rgba);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, file) {
            Ok(()) => eprintln!("SCREENSHOT: saved {} {width}x{height}", path.display()),
            Err(error) => eprintln!("SCREENSHOT: failed {}: {error}", path.display()),
        }
    });
    if let Err(error) = spawned {
        eprintln!("SCREENSHOT: failed: no thread to encode it on: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screenshot_while_headless_waits_for_real_frames_and_stops_the_dropping() {
        let headless = Headless::new();
        headless.set(true);
        assert!(headless.drops_now());
        headless.request_screenshot(Path::new("a.png"));
        assert!(!headless.drops_now(), "the frames before the screenshot are real");
        for _ in 0..WARMUP_FRAMES {
            assert_eq!(headless.take_due_screenshot(), None);
            headless.presented();
        }
        let due = headless.take_due_screenshot().expect("due after the warm-up");
        assert_eq!(due.path, Path::new("a.png"));
        assert!(headless.drops_now(), "dropping again once it is taken");
        assert_eq!(headless.take_due_screenshot(), None);
    }

    #[test]
    fn a_screenshot_while_not_headless_is_the_next_frame() {
        let headless = Headless::new();
        headless.request_screenshot(Path::new("b.png"));
        assert!(headless.take_due_screenshot().is_some());
        assert!(!headless.drops_now());
    }
}
