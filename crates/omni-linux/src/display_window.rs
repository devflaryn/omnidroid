//! **The display in a host window**, live: every frame the host composer presents
//! ([`Framebuffer`]) is shown in a native window as it arrives, and the window's size is the
//! display's -- drag the window's border and Android's display follows it, as a resized external
//! display does ([`Composer::set_display_size`]), so the running app gets a configuration change
//! and lays itself out again at the new size.
//!
//! A debugging aid first: the guest watched live instead of through screenshots. It holds no
//! platform code. The window is `omni_platform::window`'s (Win32, Xlib or AppKit, chosen by that
//! crate), and everything here goes through that seam: a [`Presenter`] for the frames and the
//! size, [`Window::set_client_size`] for a scripted resize, [`WindowEvent::CloseRequested`].
//!
//! # Two threads
//!
//! The **window thread** makes the window and pumps it (the host delivers a window's messages to
//! the thread that made it), and runs the control file. The **present thread** does the rest,
//! through the window's [`Presenter`]: it shows each frame as it arrives and watches the window's
//! size. They are two because the window thread is not always its own: on Windows a border drag
//! holds it in the host's modal loop until the button is released. The present thread goes on
//! through the drag -- the modal loop paints what it presents, and [`Presenter::client_size`]
//! follows the border -- so the frames stay live and the display follows the drag.
//!
//! # The window is the display (and not a scaled view of it)
//!
//! One size for both: a window resized to `w` x `h` physical pixels makes a `w` x `h` display
//! (at least [`MIN_SIDE`] each way). Between the resize and the first frame of the new size the
//! window shows the last frame stretched to it -- `present_rgba`'s contract -- which is the only
//! time the two sizes differ.
//!
//! **A size must hold for [`SETTLE`] before the display follows it.** A border drag is hundreds
//! of sizes, and each display change is a reconnect in SurfaceFlinger and a configuration change
//! in every app; only a size the user stops at is worth one. So a drag shows the last frame
//! stretched while the border moves, and a pause of [`SETTLE`] -- mid-drag or at its end --
//! resizes the display to it, and the app redraws at that size while the border is still held.
//!
//! # Scripted resizes
//!
//! [`Options::control`] names a file of commands, one a line, read as it grows (every
//! [`CONTROL_EVERY`]): `size <w>x<h>` resizes the **window** as a drag would -- the display then
//! follows by the same path as a user's resize, which is what makes that path testable. Unknown
//! lines are reported and skipped.
//!
//! # What it says
//!
//! Every [`REPORT_EVERY`] a `[window]` line: frames presented to the window, the framebuffer's
//! frame count and size, and the window's size. Frames arriving faster than the window takes them
//! are coalesced to the newest (the window shows frames, not a queue of them), so the first count
//! can trail the second by the frames skipped, never lead it. Closing the window ends the window,
//! not the system: the display goes on, headless.
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicBool, Ordering};

use omni_platform::window::{Presenter, Window, WindowDesc, WindowEvent};

use crate::hal::composer::{Composer, MIN_SIDE};
use crate::hal::framebuffer::Framebuffer;

/// How long a window size must hold before the display is resized to it.
pub const SETTLE: Duration = Duration::from_millis(300);
/// How long the present thread waits for a frame before it looks at the window's size again.
const FRAME_WAIT: Duration = Duration::from_millis(10);
/// How long the window thread waits for a message before it looks at the control file and the
/// present thread again.
const PUMP_WAIT: Duration = Duration::from_millis(50);
/// How often the control file is read.
pub const CONTROL_EVERY: Duration = Duration::from_millis(250);
/// How often the `[window]` line is written.
pub const REPORT_EVERY: Duration = Duration::from_secs(5);

/// What the window is to be.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The title bar's text.
    pub title: String,
    /// A file of commands (see this module's "Scripted resizes"), if any.
    pub control: Option<PathBuf>,
}

/// Show the display in a window of its own, on threads of their own (see this module's "Two
/// threads"). They end when the window is closed or cannot be made; the returned handle is the
/// window thread's.
///
/// # Errors
/// The thread could not be started.
pub fn spawn(framebuffer: Arc<Framebuffer>, composer: Arc<Composer>, options: Options) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name("omni-display-window".into()).spawn(move || run(framebuffer, composer, &options))
}

/// A command from the control file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Size(u32, u32),
}

/// One line of the control file: `None` for a line that is not a command (reported by the caller).
fn parse(line: &str) -> Option<Command> {
    let mut words = line.split_whitespace();
    match (words.next()?, words.next(), words.next()) {
        ("size", Some(size), None) => {
            let (w, h) = size.split_once('x')?;
            Some(Command::Size(w.parse().ok()?, h.parse().ok()?))
        }
        _ => None,
    }
}

/// The lines of the control file past the first `seen`, and how many lines it has now. A last line
/// without its newline is not taken yet: it may still be being written.
fn new_lines(text: &str, seen: usize) -> (Vec<String>, usize) {
    let complete: Vec<&str> = text.split_inclusive('\n').filter(|l| l.ends_with('\n')).collect();
    let fresh = complete.iter().skip(seen).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    (fresh, complete.len().max(seen))
}

/// The window thread: make the window, start the present thread, pump, run the control file.
fn run(framebuffer: Arc<Framebuffer>, composer: Arc<Composer>, options: &Options) {
    let (width, height) = composer.display_size();
    let mut window = match Window::new(&WindowDesc::new(&options.title, width, height)) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[window] no window: {e}");
            return;
        }
    };
    window.show();
    eprintln!("[window] {width}x{height}: the display, live (a window resize resizes the display)");
    let stop = Arc::new(AtomicBool::new(false));
    let presenter = window.presenter();
    let present = {
        let stop = Arc::clone(&stop);
        std::thread::Builder::new().name("omni-display-present".into()).spawn(move || present(&presenter, &framebuffer, &composer, &stop))
    };
    let present = match present {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("[window] no present thread: {e}");
            return;
        }
    };
    let (mut control_seen, mut control_read) = (0usize, Instant::now());
    loop {
        window.wait(PUMP_WAIT);
        if window.poll_events().any(|e| e == WindowEvent::CloseRequested) {
            break;
        }
        if present.is_finished() {
            eprintln!("[window] the present thread ended; closing the window");
            break;
        }
        if let Some(path) = &options.control {
            if control_read.elapsed() >= CONTROL_EVERY {
                control_read = Instant::now();
                if let Ok(text) = std::fs::read_to_string(path) {
                    let (lines, seen) = new_lines(&text, control_seen);
                    control_seen = seen;
                    for line in lines {
                        match parse(&line) {
                            Some(Command::Size(w, h)) => match window.set_client_size(w, h) {
                                Ok(()) => eprintln!("[window] control: size {w}x{h}"),
                                Err(e) => eprintln!("[window] control: size {w}x{h}: {e}"),
                            },
                            None => eprintln!("[window] control: not a command: {line:?}"),
                        }
                    }
                }
            }
        }
    }
    stop.store(true, Ordering::Release);
    let _ = present.join();
    eprintln!("[window] closed; the display goes on without it");
}

/// The present thread: each frame to the window, and the window's size to the display.
fn present(presenter: &Presenter, framebuffer: &Framebuffer, composer: &Composer, stop: &AtomicBool) {
    // The framebuffer frame last shown, and how many frames were shown.
    let (mut shown, mut presented) = (0u64, 0u64);
    let mut refused = false;
    // The window's size as last seen, and a size it took and when: the display follows it once it
    // has held for `SETTLE`.
    let mut window_size = presenter.client_size().unwrap_or_default();
    let mut pending: Option<((u32, u32), Instant)> = None;
    let mut report = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if framebuffer.wait_frame(shown + 1, FRAME_WAIT) {
            let (n, fw, fh, pixels) = framebuffer.frame();
            if n > shown {
                shown = n;
                match presenter.present_rgba(&pixels, fw, fh) {
                    Ok(()) => presented += 1,
                    Err(e) if !refused => {
                        refused = true;
                        eprintln!("[window] a frame was not shown: {e}");
                    }
                    Err(_) => {}
                }
            }
        }
        let Some(size) = presenter.client_size() else { return };
        // A minimised window (0x0) keeps the display as it was.
        if size != window_size && size.0 > 0 && size.1 > 0 {
            window_size = size;
            pending = Some((size, Instant::now()));
        }
        if let Some(((w, h), at)) = pending {
            if at.elapsed() >= SETTLE {
                pending = None;
                let want = (w.max(MIN_SIDE), h.max(MIN_SIDE));
                if want != composer.display_size() {
                    match composer.set_display_size(w, h) {
                        Ok((dw, dh)) => eprintln!("[window] window {w}x{h}: display {dw}x{dh}"),
                        Err(e) => eprintln!("[window] window {w}x{h}: the display could not follow: {e}"),
                    }
                }
            }
        }
        if report.elapsed() >= REPORT_EVERY {
            report = Instant::now();
            let (fw, fh) = framebuffer.size();
            eprintln!(
                "[window] {presented} frames presented to the window; framebuffer {} frames, {fw}x{fh}; window {}x{}",
                framebuffer.frames(),
                window_size.0,
                window_size.1
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_command_is_parsed_and_anything_else_is_not() {
        assert_eq!(parse("size 960x600"), Some(Command::Size(960, 600)));
        assert_eq!(parse("  size   1600x900  "), Some(Command::Size(1600, 900)));
        for bad in ["size", "size 960", "size 960x", "size x600", "size 9x6 extra", "resize 1x1", "", "size -1x5"] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    /// Lines are taken once each, in order, and a line still being written (no newline yet) waits.
    #[test]
    fn the_control_file_is_read_as_it_grows() {
        let (lines, seen) = new_lines("size 1x1\nsize 2x2\nsize 3", 0);
        assert_eq!((lines, seen), (vec!["size 1x1".to_string(), "size 2x2".to_string()], 2));
        let (lines, seen) = new_lines("size 1x1\nsize 2x2\nsize 3x3\n\n", seen);
        assert_eq!((lines, seen), (vec!["size 3x3".to_string()], 4));
        let (lines, seen) = new_lines("size 1x1\nsize 2x2\nsize 3x3\n\n", seen);
        assert_eq!((lines, seen), (Vec::<String>::new(), 4));
    }
}
