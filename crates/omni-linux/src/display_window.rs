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
//! **Up to a size, past which the display is scaled down** ([`display_for_window`]): a window
//! larger than [`DISPLAY_MAX_PIXELS`] (1600x900's area; `OMNI_DISPLAY_MAX=WxH` sets another,
//! `OMNI_DISPLAY_MAX=0` none) gets a display of its shape and that many pixels, stretched to it.
//! A fullscreen window on a Retina screen is 2940x1646 physical pixels, 5.25x 1280x720's -- and
//! every one of them was drawn by the app, read back, composed and copied to the window each
//! frame, while what the screen shows of them is its size in points (1470x823). Capped, the app
//! draws ~3x fewer pixels at fullscreen (run 2026-10-07: 23.6 fps at 2940x1646 against 28.7 at
//! 1280x720 in the same session); a window within the cap is still its display, pixel for pixel.
//!
//! **A size must hold for [`SETTLE`] before the display follows it.** A border drag is hundreds
//! of sizes, and each display change is a reconnect in SurfaceFlinger and a configuration change
//! in every app; only a size the user stops at is worth one. So a drag shows the last frame
//! stretched while the border moves, and a pause of [`SETTLE`] -- mid-drag or at its end --
//! resizes the display to it, and the app redraws at that size while the border is still held.
//!
//! # Keyboard and mouse
//!
//! With [`Options::input`], the window's keyboard and mouse are the device's, translated by
//! [`crate::window_input`]: a keyboard and a relative mouse (`crate::evdev`, `/dev/input/event0`
//! and `event1`, registered before system_server's `InputReader` scans) and the absolute pointer
//! (`crate::inject`: a mouse's events handed to the input dispatcher). Keys whenever the window has the
//! focus; the mouse **free and absolute** -- the host's cursor over the window is the app's pointer
//! -- and **held** (the host's cursor captured, raw motion to the relative mouse) only while the app
//! holds Android's pointer capture, as [`crate::input_channel::capture`] reports it. Moves are
//! coalesced ([`crate::window_input::MOVE_EVERY`]). `OMNI_HOST_CURSOR=hide` hides the host's cursor
//! over the window, `show` shows it; by default **the app decides**: the host's cursor is hidden while
//! the pointer icon the app asks Android for over its view is `TYPE_NULL` (it draws its own, as
//! Roblox's engine does over its surface), heard from its `setPointerIcon` to the input service
//! ([`crate::inject::Injector::pointer_icon`]), and shown when it asks for a system pointer.
//!
//! # Scripted resizes and input
//!
//! [`Options::control`] names a file of commands, one a line, read as it grows (every
//! [`CONTROL_EVERY`]): `size <w>x<h>` resizes the **window** as a drag would -- the display then
//! follows by the same path as a user's resize, which is what makes that path testable. `key
//! <scancode> [down|up]` (a set-1 scancode, `0x`-hex or decimal, `0xE0..` for an extended key;
//! both halves when neither is named), `click <x> <y> [primary|secondary|middle]` and `point <x>
//! <y>` (window pixels, as the window's own press and release, or move, there), `move <dx> <dy>`
//! (the relative mouse, as captured motion), `wheel <notches>` (the absolute pointer, where it is)
//! and `flood <hz> <seconds>` (window moves around a circle at that rate -- a mouse's report rate,
//! through the same coalescing) drive the input devices as the window's own events would. `chrome show` / `chrome
//! hide` present the system's bars and taskbar with the app, or the app alone (the default,
//! [`Composer::set_show_chrome`]). Unknown lines are reported and skipped.
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

use omni_platform::window::PointerButton;

use crate::evdev::{Device, Spec, EV_REL, REL_WHEEL, REL_X, REL_Y};
use crate::window_input::move_every_from_env;
use crate::hal::composer::{Composer, MIN_SIDE};
use crate::hal::framebuffer::Framebuffer;
use crate::window_input::{Input, Out, TITLE_FREE};

/// How long a window size must hold before the display is resized to it.
pub const SETTLE: Duration = Duration::from_millis(300);
/// The most pixels a display is given ([`display_for_window`]): 1600x900's.
pub const DISPLAY_MAX_PIXELS: u64 = 1600 * 900;

/// The cap on the display's pixels: `OMNI_DISPLAY_MAX=WxH` (its area), `0` for none, else
/// [`DISPLAY_MAX_PIXELS`].
fn display_max_pixels() -> u64 {
    match std::env::var("OMNI_DISPLAY_MAX").as_deref() {
        Ok("0") => u64::MAX,
        Ok(v) => v.split_once('x').and_then(|(w, h)| Some(w.trim().parse::<u64>().ok()? * h.trim().parse::<u64>().ok()?)).filter(|&n| n > 0).unwrap_or(DISPLAY_MAX_PIXELS),
        Err(_) => DISPLAY_MAX_PIXELS,
    }
}

/// The display a `w` x `h` window is given: the window's own size, or -- when that is more than
/// `max` pixels -- its shape scaled down to `max` pixels (even sides, at least [`MIN_SIDE`]).
#[must_use]
pub fn display_for_window(w: u32, h: u32, max: u64) -> (u32, u32) {
    let pixels = u64::from(w) * u64::from(h);
    if pixels <= max || w == 0 || h == 0 {
        return (w, h);
    }
    let k = (max as f64 / pixels as f64).sqrt();
    let side = |n: u32| ((f64::from(n) * k) as u32 & !1).max(MIN_SIDE);
    (side(w), side(h))
}

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
    /// A file of commands (see this module's "Scripted resizes and input"), if any.
    pub control: Option<PathBuf>,
    /// The window's keyboard and mouse as the device's (see this module's "Keyboard and mouse").
    pub input: bool,
}

/// The input devices: the keyboard, the relative mouse, the absolute pointer.
struct Devices {
    keyboard: Arc<Device>,
    mouse: Arc<Device>,
    pointer: crate::inject::Injector,
}

/// Show the display in a window of its own, on threads of their own (see this module's "Two
/// threads"). They end when the window is closed or cannot be made; the returned handle is the
/// window thread's.
///
/// # Errors
/// The thread could not be started.
pub fn spawn(framebuffer: Arc<Framebuffer>, composer: Arc<Composer>, options: Options) -> std::io::Result<JoinHandle<()>> {
    // The devices now, on the caller's thread: they must exist before `InputReader` scans.
    let devices = options.input.then(|| Devices {
        keyboard: crate::evdev::register(Spec::keyboard("omnidroid keyboard")),
        mouse: crate::evdev::register(Spec::mouse("omnidroid mouse")),
        pointer: crate::inject::Injector::new(crate::binder::broker(crate::binder::Context::Binder)),
    });
    if let Some(d) = &devices {
        eprintln!("[window] input: /dev/input/event{} keyboard, /dev/input/event{} mouse; the pointer injected", d.keyboard.number, d.mouse.number);
    }
    std::thread::Builder::new().name("omni-display-window".into()).spawn(move || run(framebuffer, composer, &options, devices.as_ref()))
}

/// A command from the control file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Size(u32, u32),
    /// A key by its set-1 scancode: down, up, or (`None`) both.
    Key(u32, Option<bool>),
    Click(i32, i32, PointerButton),
    /// The pointer to a window position.
    Point(i32, i32),
    Move(i32, i32),
    /// Window moves at `hz` for `seconds`.
    Flood(u32, u32),
    Wheel(i32),
    /// Present the system's chrome with the app (`chrome show`) or the app alone (`chrome hide`).
    Chrome(bool),
}

/// One line of the control file: `None` for a line that is not a command (reported by the caller).
fn parse(line: &str) -> Option<Command> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let int = |s: &str| -> Option<i64> {
        match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(hex) => i64::from_str_radix(hex, 16).ok(),
            None => s.parse().ok(),
        }
    };
    match words.as_slice() {
        ["size", size] => {
            let (w, h) = size.split_once('x')?;
            Some(Command::Size(w.parse().ok()?, h.parse().ok()?))
        }
        ["key", code] => Some(Command::Key(u32::try_from(int(code)?).ok()?, None)),
        ["key", code, "down"] => Some(Command::Key(u32::try_from(int(code)?).ok()?, Some(true))),
        ["key", code, "up"] => Some(Command::Key(u32::try_from(int(code)?).ok()?, Some(false))),
        ["click", x, y, rest @ ..] => {
            let button = match rest {
                [] | ["primary"] => PointerButton::Primary,
                ["secondary"] => PointerButton::Secondary,
                ["middle"] => PointerButton::Middle,
                _ => return None,
            };
            Some(Command::Click(i32::try_from(int(x)?).ok()?, i32::try_from(int(y)?).ok()?, button))
        }
        ["move", dx, dy] => Some(Command::Move(i32::try_from(int(dx)?).ok()?, i32::try_from(int(dy)?).ok()?)),
        ["point", x, y] => Some(Command::Point(i32::try_from(int(x)?).ok()?, i32::try_from(int(y)?).ok()?)),
        ["flood", hz, secs] => Some(Command::Flood(u32::try_from(int(hz)?).ok().filter(|&h| h > 0)?, u32::try_from(int(secs)?).ok()?)),
        ["wheel", n] => Some(Command::Wheel(i32::try_from(int(n)?).ok()?)),
        ["chrome", "show"] => Some(Command::Chrome(true)),
        ["chrome", "hide"] => Some(Command::Chrome(false)),
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

/// Do what the input translation asks: send to a device, take or give back the capture, retitle.
fn perform(outs: Vec<Out>, window: &mut Window, input: &mut Input, devices: &Devices) {
    let mut queue: std::collections::VecDeque<Out> = outs.into();
    while let Some(out) = queue.pop_front() {
        match out {
            Out::Keyboard(packet) => devices.keyboard.send(&packet),
            Out::Mouse(packet) => devices.mouse.send(&packet),
            Out::Pointer(packet) => devices.pointer.send(&packet),
            Out::Capture(take) => {
                let held = window.set_pointer_capture(take).unwrap_or_else(|e| {
                    eprintln!("[window] pointer capture: {e}");
                    false
                });
                // Said, so a run's log shows when the app held the mouse.
                let why = if input.dragging() { "a camera drag" } else { "the app holds the pointer capture" };
                eprintln!("[window] mouse {}", if !take { "free".to_string() } else if held { format!("held ({why})") } else { "not held: the window has no focus".to_string() });
                if take {
                    // What follows the answer comes before anything else queued.
                    for o in input.captured(held).into_iter().rev() {
                        queue.push_front(o);
                    }
                }
            }
            Out::Title(title) => {
                let _ = window.set_title(title);
            }
            Out::Center => {
                if let Ok((w, h)) = window.client_size() {
                    let _ = window.warp_pointer(i32::try_from(w / 2).unwrap_or(0), i32::try_from(h / 2).unwrap_or(0));
                }
            }
        }
    }
}

/// Who decides whether the host's cursor shows over the window (`OMNI_HOST_CURSOR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostCursor {
    /// The app (the default): hidden while the pointer icon it asks Android for over its view is
    /// `TYPE_NULL` -- it draws its own, as Roblox's engine surface does -- shown otherwise, so one
    /// cursor shows, never two.
    App,
    /// Always shown (`show`).
    Show,
    /// Always hidden (`hide`).
    Hide,
}

impl HostCursor {
    fn from_env() -> Self {
        match std::env::var("OMNI_HOST_CURSOR").as_deref() {
            Ok("show") => Self::Show,
            Ok("hide") => Self::Hide,
            _ => Self::App,
        }
    }
}

/// Display pixels per window pixel, each way.
fn scale(window: &Window, composer: &Composer) -> (f64, f64) {
    let (dw, dh) = composer.display_size();
    match window.client_size() {
        Ok((w, h)) if w > 0 && h > 0 => (f64::from(dw) / f64::from(w), f64::from(dh) / f64::from(h)),
        _ => (1.0, 1.0),
    }
}

/// The window thread: make the window, start the present thread, pump, run the control file.
fn run(framebuffer: Arc<Framebuffer>, composer: Arc<Composer>, options: &Options, devices: Option<&Devices>) {
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
    let mut input = Input::new(move_every_from_env());
    // The host's cursor over the window: the app's word by default (`OMNI_HOST_CURSOR=show|hide`
    // decides instead), and whether it is hidden now.
    let cursor = HostCursor::from_env();
    let mut cursor_hidden = false;
    if devices.is_some() {
        let _ = window.set_title(TITLE_FREE);
        if cursor == HostCursor::Hide {
            let _ = window.set_cursor_hidden(true);
            cursor_hidden = true;
        }
    }
    // The app's pointer capture as last seen (`input_channel::capture`'s counter).
    let mut capture_seen = u64::MAX;
    // A scripted flood: (until, interval, next, step).
    let mut flood: Option<(Instant, Duration, Instant, u32)> = None;
    let stop = Arc::new(AtomicBool::new(false));
    let presenter = window.presenter();
    let present = {
        let stop = Arc::clone(&stop);
        let composer = Arc::clone(&composer);
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
        // Woken by input, else when a coalesced move or a flood's next step is due.
        let mut wait = PUMP_WAIT;
        for due in [input.due(), flood.map(|f| f.2)].into_iter().flatten() {
            wait = wait.min(due.saturating_duration_since(Instant::now()));
        }
        window.wait(wait);
        let events: Vec<WindowEvent> = window.poll_events().collect();
        // A copy in the guest is shared with the host's clipboard only while a window is in use.
        crate::clipboard::window_focus(window.has_focus());
        if events.contains(&WindowEvent::CloseRequested) {
            break;
        }
        if let Some(devices) = devices {
            let now = Instant::now();
            let s = scale(&window, &composer);
            let display = composer.display_size();
            let (wanted, count) = crate::input_channel::capture();
            if count != capture_seen {
                capture_seen = count;
                let outs = input.guest_capture(wanted == Some(true));
                perform(outs, &mut window, &mut input, devices);
            }
            for event in &events {
                let outs = input.event(event, now, s, display);
                perform(outs, &mut window, &mut input, devices);
            }
            if let Some((until, every, next, step)) = &mut flood {
                if now >= *until {
                    flood = None;
                    eprintln!("[window] flood done");
                } else if now >= *next {
                    // A circle of 200 window pixels around the middle, a step a report.
                    let (w, h) = window.client_size().unwrap_or((1280, 720));
                    let a = f64::from(*step) * 0.05;
                    let (x, y) = ((f64::from(w) / 2.0 + 200.0 * a.cos()) as i32, (f64::from(h) / 2.0 + 200.0 * a.sin()) as i32);
                    *step += 1;
                    *next += *every;
                    let outs = input.event(&WindowEvent::PointerMoved { x, y }, now, s, display);
                    perform(outs, &mut window, &mut input, devices);
                }
            }
            let outs = input.tick(now);
            perform(outs, &mut window, &mut input, devices);
            let own_pointer = devices.pointer.pointer_icon() == Some(crate::inject::POINTER_ICON_NULL);
            input.set_own_pointer(own_pointer);
            if cursor == HostCursor::App {
                let hide = own_pointer;
                if hide != cursor_hidden {
                    cursor_hidden = hide;
                    let _ = window.set_cursor_hidden(hide);
                    eprintln!("[window] host cursor {} over the window: the app {}", if hide { "hidden" } else { "shown" }, if hide { "draws its own" } else { "asks for a system pointer" });
                }
            }
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
                            Some(Command::Chrome(show)) => {
                                eprintln!("[window] control: {line}");
                                composer.set_show_chrome(show);
                            }
                            Some(Command::Flood(hz, secs)) if devices.is_some() => {
                                eprintln!("[window] control: {line}");
                                let now = Instant::now();
                                flood = Some((now + Duration::from_secs(u64::from(secs)), Duration::from_micros(1_000_000 / u64::from(hz)), now, 0));
                            }
                            Some(command) => match devices {
                                Some(devices) => {
                                    eprintln!("[window] control: {line}");
                                    let (now, s) = (Instant::now(), scale(&window, &composer));
                                    let outs = scripted(command, &mut input, now, s, composer.display_size());
                                    perform(outs, &mut window, &mut input, devices);
                                }
                                None => eprintln!("[window] control: {line:?}: no input devices (OMNI_WINDOW_INPUT=0)"),
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

/// An input command from the control file, as the window's own events would be.
fn scripted(command: Command, input: &mut Input, now: Instant, s: (f64, f64), display: (u32, u32)) -> Vec<Out> {
    let key = |down| if down { WindowEvent::KeyDown { keycode: 0, scancode: 0, repeat: false } } else { WindowEvent::KeyUp { keycode: 0, scancode: 0 } };
    let with_code = |e: WindowEvent, code: u32| match e {
        WindowEvent::KeyDown { repeat, .. } => WindowEvent::KeyDown { keycode: 0, scancode: code, repeat },
        _ => WindowEvent::KeyUp { keycode: 0, scancode: code },
    };
    match command {
        Command::Key(code, Some(down)) => input.event(&with_code(key(down), code), now, s, display),
        Command::Key(code, None) => {
            let mut out = input.event(&with_code(key(true), code), now, s, display);
            out.extend(input.event(&with_code(key(false), code), now, s, display));
            out
        }
        Command::Click(x, y, button) => input.scripted_click(x, y, button, now, s, display),
        Command::Point(x, y) => {
            let mut out = input.event(&WindowEvent::PointerMoved { x, y }, now, s, display);
            // Sent now, not at the next tick: a script's move is one, not a stream.
            out.extend(input.flush(now));
            out
        }
        // Straight to the relative mouse: what captured motion is.
        Command::Move(dx, dy) => vec![Out::Mouse(vec![(EV_REL, REL_X, dx), (EV_REL, REL_Y, dy)])],
        Command::Wheel(n) => vec![Out::Pointer(vec![(EV_REL, REL_WHEEL, n)])],
        Command::Size(..) | Command::Chrome(_) | Command::Flood(..) => Vec::new(),
    }
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
    let max_pixels = display_max_pixels();
    while !stop.load(Ordering::Acquire) {
        if framebuffer.wait_frame(shown + 1, FRAME_WAIT) {
            let (n, fw, fh, pixels, bgra) = framebuffer.frame_raw();
            if n > shown {
                shown = n;
                // A BGRA frame (`present_bgra`) is the window's own format: handed over shared.
                let shown_now = if bgra { presenter.present_bgra(pixels, fw, fh) } else { presenter.present_rgba(&pixels, fw, fh) };
                match shown_now {
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
                let (dw, dh) = display_for_window(w, h, max_pixels);
                let want = (dw.max(MIN_SIDE), dh.max(MIN_SIDE));
                if want != composer.display_size() {
                    match composer.set_display_size(dw, dh) {
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
    fn a_large_window_gets_a_display_of_its_shape_scaled_down() {
        assert_eq!(display_for_window(1280, 720, DISPLAY_MAX_PIXELS), (1280, 720), "within the cap: the window's own");
        assert_eq!(display_for_window(1600, 900, DISPLAY_MAX_PIXELS), (1600, 900));
        let (w, h) = display_for_window(2940, 1646, DISPLAY_MAX_PIXELS);
        assert!(u64::from(w) * u64::from(h) <= DISPLAY_MAX_PIXELS && w % 2 == 0 && h % 2 == 0, "{w}x{h}");
        assert!((f64::from(w) / f64::from(h) - 2940.0 / 1646.0).abs() < 0.01, "the window's shape: {w}x{h}");
        assert_eq!(display_for_window(2940, 1646, u64::MAX), (2940, 1646), "no cap");
    }

    #[test]
    fn a_size_command_is_parsed_and_anything_else_is_not() {
        assert_eq!(parse("size 960x600"), Some(Command::Size(960, 600)));
        assert_eq!(parse("  size   1600x900  "), Some(Command::Size(1600, 900)));
        assert_eq!(parse("key 0x1E"), Some(Command::Key(0x1E, None)));
        assert_eq!(parse("key 0xE048 down"), Some(Command::Key(0xE048, Some(true))));
        assert_eq!(parse("key 30 up"), Some(Command::Key(30, Some(false))));
        assert_eq!(parse("click 400 300"), Some(Command::Click(400, 300, PointerButton::Primary)));
        assert_eq!(parse("click 5 6 secondary"), Some(Command::Click(5, 6, PointerButton::Secondary)));
        assert_eq!(parse("move -20 15"), Some(Command::Move(-20, 15)));
        assert_eq!(parse("wheel -2"), Some(Command::Wheel(-2)));
        assert_eq!(parse("point 7 8"), Some(Command::Point(7, 8)));
        assert_eq!(parse("flood 1000 5"), Some(Command::Flood(1000, 5)));
        assert_eq!(parse("chrome show"), Some(Command::Chrome(true)));
        assert_eq!(parse("chrome hide"), Some(Command::Chrome(false)));
        for bad in ["chrome", "chrome on", "size","size 960", "size 960x", "size x600", "size 9x6 extra", "resize 1x1", "", "size -1x5", "key", "key zz", "key 1 sideways", "click 1", "click 1 2 left", "move 1", "flood 0 5", "point 1"] {
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
