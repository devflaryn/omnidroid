//! **The live window's keyboard and mouse, as a device's**: host window events
//! ([`WindowEvent`]) turned into input for the guest -- evdev events for a USB-style keyboard and a
//! five-button wheel mouse ([`crate::evdev`]) that Android's own `InputReader` reads, and packets
//! for the absolute pointer that [`crate::inject`] hands to the input dispatcher as a mouse's
//! events. Platform-agnostic; no window here, only what to send and what to ask of the window
//! ([`Out`]), so it is tested without one.
//!
//! # The keyboard
//!
//! Every key the window has the focus for, by its **physical** key ([`evdev_code`] of the
//! scancode), so Android's key layout (`Generic.kl`) and the app see the key a device's keyboard
//! would report. A host auto-repeat is the driver's repeat (value 2). Keys held when the focus goes
//! are released. **The Windows (Meta) keys stay the host's** ([`HOST_KEYS`]): the host acts on them
//! too (the Start menu), and Meta alone opens Android's app list, so passing them on would do two
//! things at once.
//!
//! # The mouse: free and absolute, held only while the app holds the pointer capture
//!
//! **Free** (the default): the host's cursor moves over the window as over any other, and where it
//! is, is where the app's pointer is -- the window position scaled to the display, sent as an
//! absolute position ([`Out::Pointer`], `ABS_X`/`ABS_Y`). [`crate::inject`] makes of it what
//! Android's `CursorInputMapper` makes of a mouse:
//! `SOURCE_MOUSE` hover moves, `DOWN`/`BUTTON_PRESS`/`MOVE`/`BUTTON_RELEASE`/`UP` and `SCROLL`,
//! at that position. Android has no absolute mouse device (its mouse is relative motion it moves its
//! own pointer by, through its own acceleration), so this is the only way the pointer can *be*
//! where the host's is rather than chase it. Nothing is captured and no key is taken for it.
//!
//! **Held** while -- and only while -- **the app holds Android's pointer capture**
//! ([`crate::input_channel::capture`]: the `CAPTURE` message the input dispatcher sends the focused
//! window; Roblox asks for it for its camera lock, `MouseBehavior.LockCenter`). Then the host's
//! pointer is captured too ([`Out::Capture`]: hidden, held still, raw motion) at the window's
//! centre ([`Out::Center`]: where such an app keeps its pointer, so the host's reappears there), and
//! the motion,
//! buttons and wheel go to the relative mouse ([`Out::Mouse`]), which Android reports to the app
//! as `SOURCE_MOUSE_RELATIVE` -- what a captured pointer is on a device. When the app releases the
//! capture, the host's is given back and the pointer is free again, where the host's cursor is.
//! Losing the window's focus gives the host's pointer back as well (Alt+Tab always frees the
//! mouse); it is taken again when the focus returns, if the app still holds the capture.
//!
//! # The camera drag
//!
//! **A secondary-button drag over a view that draws its own pointer holds the host's cursor**
//! (Roblox's right-drag camera, `MouseBehavior.LockCurrentPosition`: the engine keeps its cursor
//! still and turns the camera by the pointer's `dx`/`dy`). The app asks for no system pointer there
//! (`TYPE_NULL`, [`Input::set_own_pointer`]), so the host's is hidden already; without a hold it
//! would still move, leave the window, and be somewhere else at the release. So the press captures
//! the host's cursor where it was pressed ([`Out::Capture`]), the raw motion moves the **app's**
//! pointer (absolute positions kept on the display, as a device's pointer is), and the release --
//! where the app's pointer is -- is followed by a move back to where it was pressed, where the app
//! kept its cursor; the host's cursor is given back there, never having moved. The camera turns as
//! on a device; the cursor stays put, as on the desktop client.
//!
//! # Coalesced, not queued
//!
//! A mouse reports up to 8,000 times a second, and each report sent on is work for the guest's
//! whole input pipeline -- the kernel, `InputReader`, the dispatcher, the app's UI thread -- all of
//! it translated code. A pointer has one position, so **positions are coalesced**: the newest is
//! sent at most every [`MOVE_EVERY`], and a press, release or wheel first sends the position it
//! happened at, so a click lands where it was made. Captured motion is **summed** the same way (it
//! is distance, and all of it counts). Keys are never delayed.
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use omni_platform::window::{evdev_code, PointerButton, WindowEvent};

use crate::evdev::{ABS_X, ABS_Y, BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, EV_ABS, EV_KEY, EV_REL, REL_HWHEEL, REL_WHEEL, REL_X, REL_Y};

/// Keys the host keeps: `KEY_LEFTMETA`, `KEY_RIGHTMETA`.
pub const HOST_KEYS: [u16; 2] = [125, 126];
/// One wheel notch in the window seam's units.
const NOTCH: i32 = 120;
/// **How often a moving pointer is sent**, at most: 60 times a second, as often as the display can
/// show it (a game world here draws at 30-40), far less than a mouse's report rate. Each move is a
/// trip through the input dispatcher and the app's translated UI thread: in PS99 a mouse streaming
/// over the window cost the game ~10% of its frames at 125 a second (run base1) and ~5% at 60 (run
/// F, 2026-09-29), with a click still landing in 2-9 ms. `OMNI_POINTER_HZ` changes it.
pub const MOVE_EVERY: Duration = Duration::from_micros(16_667);

/// Evdev events for one device, as one packet.
pub type Packet = Vec<(u16, u16, i32)>;

/// What to do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// Put the host's cursor at the window's centre (held there while captured).
    Center,
    /// Send to the keyboard.
    Keyboard(Packet),
    /// Send to the relative mouse (the pointer captured).
    Mouse(Packet),
    /// Send to the absolute pointer (the pointer free).
    Pointer(Packet),
    /// Take (`true`) or give back the host's pointer capture; the caller answers whether it is held,
    /// by [`Input::captured`].
    Capture(bool),
    /// The window's title should say this.
    Title(&'static str),
}

/// The title while the mouse is free.
pub const TITLE_FREE: &str = "omnidroid";
/// The title while the app holds the mouse.
pub const TITLE_HELD: &str = "omnidroid \u{2014} the app holds the mouse (Alt+Tab frees it)";

/// The evdev button of a host button.
#[must_use]
pub const fn button_code(button: PointerButton) -> u16 {
    match button {
        PointerButton::Primary => BTN_LEFT,
        PointerButton::Secondary => BTN_RIGHT,
        PointerButton::Middle => BTN_MIDDLE,
        PointerButton::Back => BTN_SIDE,
        PointerButton::Forward => BTN_EXTRA,
    }
}

/// The translation's state.
#[derive(Debug)]
pub struct Input {
    /// The app holds the pointer capture (the guest's word).
    wanted: bool,
    /// The host's pointer capture is held.
    held: bool,
    /// The window has the focus.
    focused: bool,
    keys: BTreeSet<u16>,
    /// Buttons down on the absolute pointer, and on the relative mouse.
    pointer_buttons: BTreeSet<u16>,
    mouse_buttons: BTreeSet<u16>,
    /// Wheel remainders below a notch, (horizontal, vertical).
    wheel: (i32, i32),
    /// The display position last sent, the newest not yet sent, and when one was last sent.
    sent: Option<(i32, i32)>,
    pending: Option<(i32, i32)>,
    last_move: Option<Instant>,
    /// Captured motion not yet sent.
    motion: (i32, i32),
    every: Duration,
    /// The app draws its own pointer over its view (it asked Android for `TYPE_NULL`).
    own_pointer: bool,
    /// A camera drag being held (see "The camera drag" in the module's documentation).
    drag: Option<Drag>,
}

/// A secondary-button drag the host holds the cursor for.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drag {
    /// Where it was pressed, in display pixels: where the pointer goes back to.
    pressed: (i32, i32),
    /// The app's pointer, moved by the raw motion, kept on the display.
    at: (f64, f64),
    /// Display pixels per window pixel, at the press.
    scale: (f64, f64),
}

impl Drag {
    fn at(&self) -> (i32, i32) {
        (self.at.0.round() as i32, self.at.1.round() as i32)
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new(MOVE_EVERY)
    }
}

/// `OMNI_POINTER_HZ`: how often a moving pointer is sent, at most.
#[must_use]
pub fn move_every_from_env() -> Duration {
    std::env::var("OMNI_POINTER_HZ").ok().and_then(|v| v.parse::<u32>().ok()).filter(|&hz| hz > 0).map_or(MOVE_EVERY, |hz| Duration::from_micros(1_000_000 / u64::from(hz)))
}

impl Input {
    /// A translation that sends a moving pointer at most every `every`.
    #[must_use]
    pub fn new(every: Duration) -> Self {
        Self {
            wanted: false,
            held: false,
            focused: true,
            keys: BTreeSet::new(),
            pointer_buttons: BTreeSet::new(),
            mouse_buttons: BTreeSet::new(),
            wheel: (0, 0),
            sent: None,
            pending: None,
            last_move: None,
            motion: (0, 0),
            every,
            own_pointer: false,
            drag: None,
        }
    }

    /// Whether the app draws its own pointer over its view (the pointer icon it asked Android for
    /// is `TYPE_NULL`): a secondary-button drag then holds the host's cursor (a camera drag).
    pub fn set_own_pointer(&mut self, own: bool) {
        self.own_pointer = own;
    }

    /// Whether a camera drag is being held.
    #[must_use]
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Whether the host's pointer capture is held (the app holds Android's).
    #[must_use]
    pub fn held(&self) -> bool {
        self.held
    }

    /// The caller's answer to an [`Out::Capture`]`(true)`: whether the capture is now held.
    pub fn captured(&mut self, held: bool) -> Vec<Out> {
        self.held = held;
        if !held {
            // A camera drag the window could not hold goes on as a free drag.
            self.drag = None;
        }
        if held && self.drag.is_none() { vec![Out::Title(TITLE_HELD)] } else { Vec::new() }
    }

    /// The app took (`true`) or released the pointer capture.
    pub fn guest_capture(&mut self, wanted: bool) -> Vec<Out> {
        if wanted == self.wanted {
            return Vec::new();
        }
        self.wanted = wanted;
        if wanted {
            // What the free pointer has down is let go first: the app's pointer changes source. A
            // camera drag ends with it; its capture is kept for the app's.
            let mut out = self.end_drag(false);
            out.extend(self.release_pointer_buttons());
            self.pending = None;
            if self.focused && !self.held {
                out.push(Out::Capture(true));
            }
            // Held at the centre, where an app that locks its pointer keeps it (Roblox's first
            // person and shift-lock), so it is given back there.
            if self.focused {
                out.push(Out::Center);
            }
            out
        } else {
            let mut out = self.release_mouse_buttons();
            self.motion = (0, 0);
            if self.held {
                self.held = false;
                out.push(Out::Capture(false));
                out.push(Out::Title(TITLE_FREE));
            }
            // The host's cursor shows where it was held; the app's pointer is synced to it by the
            // move the window reports there.
            self.sent = None;
            out
        }
    }

    /// Whether a pointer event is the relative mouse's now: the app holds the capture and so does
    /// the host.
    fn relative(&self) -> bool {
        self.wanted && self.held
    }

    /// End a camera drag: the button released where the app's pointer is (if `release`), the
    /// pointer back where it was pressed, and the host's cursor given back unless the app holds
    /// the capture.
    fn end_drag(&mut self, release: bool) -> Vec<Out> {
        let Some(drag) = self.drag.take() else { return Vec::new() };
        let mut out = Vec::new();
        let now = Instant::now();
        if release {
            let at = drag.at();
            let down: Vec<u16> = std::mem::take(&mut self.pointer_buttons).into_iter().collect();
            if !down.is_empty() {
                let mut packet = self.position(at.0, at.1, now);
                packet.extend(down.into_iter().map(|b| (EV_KEY, b, 0)));
                out.push(Out::Pointer(packet));
            }
        }
        // Back where it was pressed: the app kept its cursor there for the drag.
        let back = self.position(drag.pressed.0, drag.pressed.1, now);
        if !back.is_empty() {
            out.push(Out::Pointer(back));
        }
        self.pending = None;
        if self.held && !self.wanted {
            self.held = false;
            out.push(Out::Capture(false));
        }
        out
    }

    /// One window event. `scale` turns window pixels into display pixels (display / window size);
    /// `display` is the display's size, which positions are kept inside.
    pub fn event(&mut self, event: &WindowEvent, now: Instant, scale: (f64, f64), display: (u32, u32)) -> Vec<Out> {
        let mut out = Vec::new();
        let at = |x: i32, y: i32| -> (i32, i32) {
            let clamp = |v: f64, max: u32| (v.round() as i32).clamp(0, max.saturating_sub(1) as i32);
            (clamp(f64::from(x) * scale.0, display.0), clamp(f64::from(y) * scale.1, display.1))
        };
        match *event {
            WindowEvent::KeyDown { scancode, repeat, .. } => {
                let Some(code) = evdev_code(scancode).filter(|c| !HOST_KEYS.contains(c)) else { return out };
                self.keys.insert(code);
                out.push(Out::Keyboard(vec![(EV_KEY, code, if repeat { 2 } else { 1 })]));
            }
            WindowEvent::KeyUp { scancode, .. } => {
                let Some(code) = evdev_code(scancode).filter(|c| !HOST_KEYS.contains(c)) else { return out };
                if self.keys.remove(&code) {
                    out.push(Out::Keyboard(vec![(EV_KEY, code, 0)]));
                }
            }
            WindowEvent::PointerMoved { x, y } if !self.relative() => {
                self.pending = Some(at(x, y));
                out.extend(self.tick(now));
            }
            WindowEvent::PointerMotion { dx, dy } if self.held && !self.wanted && self.drag.is_some() => {
                let drag = self.drag.as_mut().expect("dragging");
                let clamp = |v: f64, max: u32| v.clamp(0.0, f64::from(max.saturating_sub(1)));
                drag.at = (clamp(drag.at.0 + f64::from(dx) * drag.scale.0, display.0), clamp(drag.at.1 + f64::from(dy) * drag.scale.1, display.1));
                self.pending = Some(drag.at());
                out.extend(self.tick(now));
            }
            WindowEvent::PointerMotion { dx, dy } if self.relative() => {
                self.motion.0 = self.motion.0.saturating_add(dx);
                self.motion.1 = self.motion.1.saturating_add(dy);
                out.extend(self.tick(now));
            }
            WindowEvent::PointerUp { button: PointerButton::Secondary, .. } if self.drag.is_some() => {
                // The last motion first, then the release where the app's pointer is.
                self.last_move = None;
                out.extend(self.tick(now));
                out.extend(self.end_drag(true));
            }
            WindowEvent::PointerDown { button, .. } | WindowEvent::PointerUp { button, .. } if self.drag.is_some() => {
                // Another button during a camera drag: where the app's pointer is.
                let down = matches!(event, WindowEvent::PointerDown { .. });
                let code = button_code(button);
                let changed = if down { self.pointer_buttons.insert(code) } else { self.pointer_buttons.remove(&code) };
                if changed {
                    let at = self.drag.map(|d| d.at()).expect("dragging");
                    self.pending = None;
                    let mut packet = self.position(at.0, at.1, now);
                    packet.push((EV_KEY, code, i32::from(down)));
                    out.push(Out::Pointer(packet));
                }
            }
            WindowEvent::PointerDown { button, x, y } | WindowEvent::PointerUp { button, x, y } => {
                let down = matches!(event, WindowEvent::PointerDown { .. });
                let code = button_code(button);
                if self.relative() {
                    out.extend(self.flush_motion(now));
                    let changed = if down { self.mouse_buttons.insert(code) } else { self.mouse_buttons.remove(&code) };
                    if changed {
                        out.push(Out::Mouse(vec![(EV_KEY, code, i32::from(down))]));
                    }
                } else if down || self.pointer_buttons.contains(&code) {
                    // The press or release where it happened, in the packet that moves there.
                    self.pending = None;
                    let (px, py) = at(x, y);
                    let mut packet = self.position(px, py, now);
                    if down {
                        self.pointer_buttons.insert(code);
                    } else {
                        self.pointer_buttons.remove(&code);
                    }
                    packet.push((EV_KEY, code, i32::from(down)));
                    out.push(Out::Pointer(packet));
                    // A camera drag: the secondary button alone, over a view that draws its own
                    // pointer, the window focused -- the host's cursor is held where it was pressed
                    // and the app's pointer moves by the raw motion.
                    if down && button == PointerButton::Secondary && self.own_pointer && self.focused && !self.wanted && !self.held && self.pointer_buttons.len() == 1 {
                        self.drag = Some(Drag { pressed: (px, py), at: (f64::from(px), f64::from(py)), scale });
                        out.push(Out::Capture(true));
                    }
                } else if self.mouse_buttons.remove(&code) {
                    // Pressed while the pointer was held, released after it was given back.
                    out.push(Out::Mouse(vec![(EV_KEY, code, 0)]));
                }
            }
            WindowEvent::Wheel { x, y, dx, dy } => {
                self.wheel.0 += dx;
                self.wheel.1 += dy;
                let (h, v) = (self.wheel.0 / NOTCH, self.wheel.1 / NOTCH);
                self.wheel = (self.wheel.0 % NOTCH, self.wheel.1 % NOTCH);
                let mut packet = Vec::new();
                if v != 0 {
                    packet.push((EV_REL, REL_WHEEL, v));
                }
                if h != 0 {
                    packet.push((EV_REL, REL_HWHEEL, h));
                }
                if !packet.is_empty() {
                    if self.relative() {
                        out.extend(self.flush_motion(now));
                        out.push(Out::Mouse(packet));
                    } else {
                        self.pending = None;
                        let (px, py) = at(x, y);
                        let mut p = self.position(px, py, now);
                        p.extend(packet);
                        out.push(Out::Pointer(p));
                    }
                }
            }
            WindowEvent::PointerCaptureLost => {
                if self.drag.is_some() {
                    // Taken from the window (the focus went): the drag ends where it is.
                    out.extend(self.end_drag(true));
                }
                self.held = false;
                out.extend(self.release_mouse_buttons());
                out.push(Out::Title(TITLE_FREE));
            }
            WindowEvent::FocusChanged { focused } => {
                self.focused = focused;
                if focused {
                    if self.wanted && !self.held {
                        out.push(Out::Capture(true));
                    }
                } else {
                    out.extend(self.release_keys());
                    out.extend(self.end_drag(true));
                    out.extend(self.release_pointer_buttons());
                    out.extend(self.release_mouse_buttons());
                    if self.held {
                        self.held = false;
                        out.push(Out::Capture(false));
                        out.push(Out::Title(TITLE_FREE));
                    }
                }
            }
            _ => {}
        }
        out
    }

    /// The position packet for `(x, y)`, empty if the guest has it already.
    fn position(&mut self, x: i32, y: i32, now: Instant) -> Packet {
        if self.sent == Some((x, y)) {
            return Vec::new();
        }
        self.sent = Some((x, y));
        self.last_move = Some(now);
        vec![(EV_ABS, ABS_X, x), (EV_ABS, ABS_Y, y)]
    }

    /// When the next coalesced move is due, if one is waiting.
    #[must_use]
    pub fn due(&self) -> Option<Instant> {
        let waiting = self.pending.is_some() || self.motion != (0, 0);
        waiting.then(|| self.last_move.map_or_else(Instant::now, |t| t + self.every))
    }

    /// Time passing: the coalesced move, if one is waiting and due.
    pub fn tick(&mut self, now: Instant) -> Vec<Out> {
        if self.last_move.is_some_and(|t| now < t + self.every) {
            return Vec::new();
        }
        if self.relative() {
            return self.flush_motion(now);
        }
        match self.pending.take() {
            Some((x, y)) => {
                let packet = self.position(x, y, now);
                if packet.is_empty() { Vec::new() } else { vec![Out::Pointer(packet)] }
            }
            None => Vec::new(),
        }
    }

    /// The waiting move now, due or not (a scripted move is one, not a stream).
    pub fn flush(&mut self, now: Instant) -> Vec<Out> {
        self.last_move = None;
        self.tick(now)
    }

    fn flush_motion(&mut self, now: Instant) -> Vec<Out> {
        let (dx, dy) = std::mem::take(&mut self.motion);
        let mut packet = Vec::new();
        if dx != 0 {
            packet.push((EV_REL, REL_X, dx));
        }
        if dy != 0 {
            packet.push((EV_REL, REL_Y, dy));
        }
        if packet.is_empty() {
            return Vec::new();
        }
        self.last_move = Some(now);
        vec![Out::Mouse(packet)]
    }

    /// A scripted click at window position (`x`, `y`): the window's own press and release there.
    pub fn scripted_click(&mut self, x: i32, y: i32, button: PointerButton, now: Instant, scale: (f64, f64), display: (u32, u32)) -> Vec<Out> {
        let mut out = self.event(&WindowEvent::PointerDown { button, x, y }, now, scale, display);
        out.extend(self.event(&WindowEvent::PointerUp { button, x, y }, now, scale, display));
        out
    }

    fn release_keys(&mut self) -> Vec<Out> {
        let packet: Packet = std::mem::take(&mut self.keys).into_iter().map(|k| (EV_KEY, k, 0)).collect();
        if packet.is_empty() { Vec::new() } else { vec![Out::Keyboard(packet)] }
    }

    fn release_pointer_buttons(&mut self) -> Vec<Out> {
        let packet: Packet = std::mem::take(&mut self.pointer_buttons).into_iter().map(|b| (EV_KEY, b, 0)).collect();
        if packet.is_empty() { Vec::new() } else { vec![Out::Pointer(packet)] }
    }

    fn release_mouse_buttons(&mut self) -> Vec<Out> {
        let packet: Packet = std::mem::take(&mut self.mouse_buttons).into_iter().map(|b| (EV_KEY, b, 0)).collect();
        if packet.is_empty() { Vec::new() } else { vec![Out::Mouse(packet)] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: (f64, f64) = (1.0, 1.0);
    const DISPLAY: (u32, u32) = (1280, 720);

    fn key(scancode: u32, down: bool) -> WindowEvent {
        if down { WindowEvent::KeyDown { keycode: 0, scancode, repeat: false } } else { WindowEvent::KeyUp { keycode: 0, scancode } }
    }

    fn abs(x: i32, y: i32) -> Packet {
        vec![(EV_ABS, ABS_X, x), (EV_ABS, ABS_Y, y)]
    }

    #[test]
    fn keys_go_to_the_keyboard_by_their_physical_code_and_repeat_as_the_driver_does() {
        let (mut input, now) = (Input::default(), Instant::now());
        assert_eq!(input.event(&key(0x1E, true), now, ONE, DISPLAY), [Out::Keyboard(vec![(EV_KEY, 30, 1)])], "A is KEY_A");
        assert_eq!(input.event(&WindowEvent::KeyDown { keycode: 0, scancode: 0x1E, repeat: true }, now, ONE, DISPLAY), [Out::Keyboard(vec![(EV_KEY, 30, 2)])]);
        assert_eq!(input.event(&key(0x1E, false), now, ONE, DISPLAY), [Out::Keyboard(vec![(EV_KEY, 30, 0)])]);
        assert!(input.event(&key(0x1E, false), now, ONE, DISPLAY).is_empty(), "a release of a key not down is not passed on");
        assert!(input.event(&key(0xE05B, true), now, ONE, DISPLAY).is_empty(), "the left Windows key is the host's");
        assert_eq!(input.event(&key(0xE01D, true), now, ONE, DISPLAY), [Out::Keyboard(vec![(EV_KEY, 97, 1)])], "Right Ctrl is only a key");
    }

    /// Free: a click is the absolute pointer's, where it was made, with nothing captured.
    #[test]
    fn a_click_lands_where_it_is_made_without_a_capture() {
        let (mut input, t0) = (Input::default(), Instant::now());
        let out = input.event(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 100, y: 50 }, t0, (2.0, 2.0), DISPLAY);
        assert_eq!(out, [Out::Pointer(vec![(EV_ABS, ABS_X, 200), (EV_ABS, ABS_Y, 100), (EV_KEY, BTN_LEFT, 1)])]);
        let out = input.event(&WindowEvent::PointerUp { button: PointerButton::Primary, x: 100, y: 50 }, t0, (2.0, 2.0), DISPLAY);
        assert_eq!(out, [Out::Pointer(vec![(EV_KEY, BTN_LEFT, 0)])], "same place: no move with it");
        assert!(!input.held());
    }

    /// A flood of moves is one position per `MOVE_EVERY`, the newest.
    #[test]
    fn moves_are_coalesced_to_the_newest_and_sent_at_most_every_interval() {
        let (mut input, t0) = (Input::new(Duration::from_millis(8)), Instant::now());
        assert_eq!(input.event(&WindowEvent::PointerMoved { x: 1, y: 1 }, t0, ONE, DISPLAY), [Out::Pointer(abs(1, 1))], "the first at once");
        for i in 2..100 {
            assert!(input.event(&WindowEvent::PointerMoved { x: i, y: i }, t0 + Duration::from_millis(1), ONE, DISPLAY).is_empty(), "held back");
        }
        assert_eq!(input.due(), Some(t0 + Duration::from_millis(8)));
        assert!(input.tick(t0 + Duration::from_millis(7)).is_empty());
        assert_eq!(input.tick(t0 + Duration::from_millis(8)), [Out::Pointer(abs(99, 99))], "the newest");
        assert_eq!(input.due(), None);
        // A press flushes the position it happened at, even inside the interval.
        let out = input.event(&WindowEvent::PointerDown { button: PointerButton::Secondary, x: 5, y: 6 }, t0 + Duration::from_millis(9), ONE, DISPLAY);
        assert_eq!(out, [Out::Pointer(vec![(EV_ABS, ABS_X, 5), (EV_ABS, ABS_Y, 6), (EV_KEY, BTN_RIGHT, 1)])]);
        // Positions stay on the display.
        let out = input.event(&WindowEvent::PointerMoved { x: -40, y: 9000 }, t0 + Duration::from_secs(1), ONE, DISPLAY);
        assert_eq!(out, [Out::Pointer(abs(0, 719))]);
    }

    /// The app's capture holds the host's pointer; motion is then relative, summed, and the
    /// release frees it again.
    #[test]
    fn the_apps_capture_holds_the_mouse_and_its_release_frees_it() {
        let (mut input, t0) = (Input::new(Duration::from_millis(8)), Instant::now());
        assert_eq!(input.guest_capture(true), [Out::Capture(true), Out::Center], "held at the centre");
        assert_eq!(input.captured(true), [Out::Title(TITLE_HELD)]);
        assert_eq!(input.event(&WindowEvent::PointerMotion { dx: 3, dy: -4 }, t0, ONE, DISPLAY), [Out::Mouse(vec![(EV_REL, REL_X, 3), (EV_REL, REL_Y, -4)])]);
        assert!(input.event(&WindowEvent::PointerMotion { dx: 1, dy: 1 }, t0 + Duration::from_millis(1), ONE, DISPLAY).is_empty());
        assert!(input.event(&WindowEvent::PointerMotion { dx: 2, dy: 0 }, t0 + Duration::from_millis(2), ONE, DISPLAY).is_empty());
        // A press sends the motion before it.
        let out = input.event(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 0, y: 0 }, t0 + Duration::from_millis(3), ONE, DISPLAY);
        assert_eq!(out, [Out::Mouse(vec![(EV_REL, REL_X, 3), (EV_REL, REL_Y, 1)]), Out::Mouse(vec![(EV_KEY, BTN_LEFT, 1)])]);
        assert_eq!(input.event(&WindowEvent::Wheel { x: 0, y: 0, dx: 0, dy: 120 }, t0, ONE, DISPLAY), [Out::Mouse(vec![(EV_REL, REL_WHEEL, 1)])]);
        assert!(input.event(&WindowEvent::PointerMoved { x: 9, y: 9 }, t0, ONE, DISPLAY).is_empty(), "no absolute moves while held");
        let out = input.guest_capture(false);
        assert_eq!(out, [Out::Mouse(vec![(EV_KEY, BTN_LEFT, 0)]), Out::Capture(false), Out::Title(TITLE_FREE)]);
        assert!(!input.held());
        assert_eq!(input.event(&WindowEvent::PointerMoved { x: 9, y: 9 }, t0 + Duration::from_secs(1), ONE, DISPLAY), [Out::Pointer(abs(9, 9))], "free again");
    }

    /// Losing the focus frees the host's pointer whatever the app holds; the focus back takes it
    /// again if the app still holds the capture.
    #[test]
    fn the_focus_frees_the_mouse_and_takes_it_back() {
        let (mut input, t0) = (Input::default(), Instant::now());
        input.event(&key(0x11, true), t0, ONE, DISPLAY);
        input.event(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 1, y: 1 }, t0, ONE, DISPLAY);
        let out = input.event(&WindowEvent::FocusChanged { focused: false }, t0, ONE, DISPLAY);
        assert_eq!(out, [Out::Keyboard(vec![(EV_KEY, 17, 0)]), Out::Pointer(vec![(EV_KEY, BTN_LEFT, 0)])]);
        assert!(input.guest_capture(true).is_empty(), "no capture asked for without the focus");
        assert_eq!(input.event(&WindowEvent::FocusChanged { focused: true }, t0, ONE, DISPLAY), [Out::Capture(true)]);
        input.captured(true);
        let out = input.event(&WindowEvent::FocusChanged { focused: false }, t0, ONE, DISPLAY);
        assert_eq!(out, [Out::Capture(false), Out::Title(TITLE_FREE)]);
    }

    /// A secondary drag over a view that draws its own pointer holds the host's cursor: the raw
    /// motion moves the app's pointer (kept on the display), the release is where that pointer is,
    /// and the pointer goes back where it was pressed before the cursor is given back.
    #[test]
    fn a_camera_drag_holds_the_cursor_and_gives_it_back_where_it_was_pressed() {
        let (mut input, t0) = (Input::new(Duration::from_millis(8)), Instant::now());
        let press = WindowEvent::PointerDown { button: PointerButton::Secondary, x: 100, y: 50 };
        // Not over a view drawing its own pointer: an ordinary drag.
        assert_eq!(input.event(&press, t0, ONE, DISPLAY), [Out::Pointer(vec![(EV_ABS, ABS_X, 100), (EV_ABS, ABS_Y, 50), (EV_KEY, BTN_RIGHT, 1)])]);
        input.event(&WindowEvent::PointerUp { button: PointerButton::Secondary, x: 100, y: 50 }, t0, ONE, DISPLAY);
        assert!(!input.dragging());

        input.set_own_pointer(true);
        let t1 = t0 + Duration::from_secs(1);
        let out = input.event(&press, t1, ONE, DISPLAY);
        assert_eq!(out, [Out::Pointer(vec![(EV_KEY, BTN_RIGHT, 1)]), Out::Capture(true)], "pressed where it is, then held");
        assert!(input.captured(true).is_empty(), "no 'the app holds the mouse' title for a drag");
        assert!(input.dragging());
        // Raw motion moves the app's pointer, coalesced; absolute moves are not the pointer now.
        assert_eq!(input.event(&WindowEvent::PointerMotion { dx: 30, dy: -10 }, t1, ONE, DISPLAY), [Out::Pointer(abs(130, 40))]);
        assert!(input.event(&WindowEvent::PointerMotion { dx: -5000, dy: 0 }, t1 + Duration::from_millis(1), ONE, DISPLAY).is_empty(), "coalesced");
        // The release: the last motion (kept on the display), the release there, back to the press.
        let out = input.event(&WindowEvent::PointerUp { button: PointerButton::Secondary, x: 100, y: 50 }, t1 + Duration::from_millis(2), ONE, DISPLAY);
        assert_eq!(
            out,
            [Out::Pointer(abs(0, 40)), Out::Pointer(vec![(EV_KEY, BTN_RIGHT, 0)]), Out::Pointer(abs(100, 50)), Out::Capture(false)]
        );
        assert!(!input.dragging() && !input.held());
        // Losing the focus mid-drag ends it the same way.
        input.event(&press, t1 + Duration::from_secs(1), ONE, DISPLAY);
        input.captured(true);
        let out = input.event(&WindowEvent::FocusChanged { focused: false }, t1 + Duration::from_secs(1), ONE, DISPLAY);
        assert_eq!(out, [Out::Pointer(vec![(EV_KEY, BTN_RIGHT, 0)]), Out::Capture(false)]);
        // The app's capture during a drag ends the drag and keeps the host's capture for itself.
        let mut input = Input::new(Duration::from_millis(8));
        input.set_own_pointer(true);
        input.event(&press, t0, ONE, DISPLAY);
        input.captured(true);
        assert_eq!(input.guest_capture(true), [Out::Pointer(vec![(EV_KEY, BTN_RIGHT, 0)]), Out::Center], "the free pointer's button let go");
        assert!(!input.dragging() && input.held());
    }

    #[test]
    fn wheel_notches_are_whole() {
        let (mut input, t0) = (Input::default(), Instant::now());
        assert!(input.event(&WindowEvent::Wheel { x: 10, y: 10, dx: 0, dy: 60 }, t0, ONE, DISPLAY).is_empty(), "half a notch");
        assert_eq!(input.event(&WindowEvent::Wheel { x: 10, y: 10, dx: 0, dy: 60 }, t0, ONE, DISPLAY), [Out::Pointer(vec![(EV_ABS, ABS_X, 10), (EV_ABS, ABS_Y, 10), (EV_REL, REL_WHEEL, 1)])]);
        assert_eq!(input.event(&WindowEvent::Wheel { x: 10, y: 10, dx: -240, dy: 0 }, t0, ONE, DISPLAY), [Out::Pointer(vec![(EV_REL, REL_HWHEEL, -2)])]);
    }
}
