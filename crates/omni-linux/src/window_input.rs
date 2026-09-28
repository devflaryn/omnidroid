//! **The live window's keyboard and mouse, as a device's**: host window events
//! ([`WindowEvent`]) turned into evdev events for the two input devices the display window makes
//! ([`crate::evdev`]) -- a USB-style keyboard and a five-button wheel mouse -- which Android's own
//! `InputReader` reads. Platform-agnostic; no window here, only what to send and what to ask of the
//! window ([`Out`]), so it is tested without one.
//!
//! # The keyboard
//!
//! Every key the window has the focus for, by its **physical** key ([`evdev_code`] of the
//! scancode), so Android's key layout (`Generic.kl`) and the app see the key a device's keyboard
//! would report. A host auto-repeat is the driver's repeat (value 2). Keys held when the focus goes
//! are released.
//!
//! # The mouse: held on a click, as a virtual machine holds it
//!
//! Android has no absolute mouse: a mouse is relative motion (`REL_X`/`REL_Y`) that Android moves
//! its own pointer by, through its own acceleration, and `SOURCE_MOUSE` -- what Roblox reads a
//! mouse by -- comes from nothing else. So the host's mouse is **held** while it drives Android's:
//! a click in the window takes the pointer capture ([`Out::Capture`]: the host cursor hidden and
//! still, the device's raw motion reported), and from then on the motion, the five buttons and both
//! wheels go to the mouse device. **Right Ctrl gives it back** (and is not passed on); so does the
//! window losing the focus. The keyboard does not need the hold.
//!
//! **The first click lands where it was made.** Android's pointer is somewhere else when the hold
//! begins, and a relative mouse cannot say where to go -- except by the one motion whose scaling is
//! known: the first after the pointer has been still. Its velocity tracker resets at rest, and a
//! first sample, having no velocity, gets the acceleration curve's base gain -- [`PLACE_GAIN`],
//! MEASURED on this image (it uses the curved "new ballistics"). So the hold sends one move far
//! past the top-left corner, which Android clamps to (0, 0) whatever it scales it by; waits
//! [`PLACE_WAIT`]; sends one move of the click's display position divided by that gain; and only
//! then the click. Anything that happens meanwhile waits and follows in order. Motion while the
//! mouse is held is the device's own, undivided: Android's curve on top of it, as on a device with
//! a USB mouse.
use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use omni_platform::window::{evdev_code, PointerButton, WindowEvent};

use crate::evdev::{BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, EV_KEY, EV_REL, REL_HWHEEL, REL_WHEEL, REL_X, REL_Y};

/// How long Android's pointer is left still before the move that places it: past its velocity
/// tracker's 300 ms reset, so the move is not accelerated.
pub const PLACE_WAIT: Duration = Duration::from_millis(400);
/// **The gain Android gives a mouse's first move after a rest**, at the default pointer speed.
/// MEASURED 2026-09-28 (d7 run 1, AOSP 15 arm64 emulator image, `pointer_speed` 0): one move of
/// (400, 300) counts, 400 ms after the pointer was sent home, put it at (817, 612) -- a gain in
/// 2.040..=2.042 on both axes, one figure. The gate places at two far-apart points to hold it to
/// that. A changed pointer speed changes it.
pub const PLACE_GAIN: f64 = 2.04;
/// `KEY_RIGHTCTRL`: the key that gives the mouse back.
pub const RELEASE_KEY: u16 = 97;
/// One wheel notch in the window seam's units.
const NOTCH: i32 = 120;
/// Far enough past the corner for any display: Android clamps its pointer there.
const HOME: i32 = -65_536;

/// Evdev events for one device, as one packet.
pub type Packet = Vec<(u16, u16, i32)>;

/// What to do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// Send to the keyboard.
    Keyboard(Packet),
    /// Send to the mouse.
    Mouse(Packet),
    /// Take (`true`) or give back the pointer capture; the caller answers whether it is held, by
    /// [`Input::captured`].
    Capture(bool),
    /// The window's title should say this.
    Title(&'static str),
}

/// The title while the mouse is free.
pub const TITLE_FREE: &str = "omnidroid \u{2014} click to control with the mouse (keyboard goes to Android)";
/// The title while the mouse is held.
pub const TITLE_HELD: &str = "omnidroid \u{2014} mouse held \u{00b7} Right Ctrl releases it";

/// The move far past the top-left corner that puts Android's pointer at (0, 0).
fn home() -> Out {
    Out::Mouse(vec![(EV_REL, REL_X, HOME), (EV_REL, REL_Y, HOME)])
}

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

/// A placement under way: the pointer sent home at `since`, to be moved to `to` (display pixels),
/// with the click and whatever came after it held until then.
#[derive(Debug)]
struct Placing {
    since: Instant,
    to: (i32, i32),
    after: VecDeque<WindowEvent>,
}

/// The translation's state.
#[derive(Debug, Default)]
pub struct Input {
    held: bool,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u16>,
    /// The release key's own release, not to be passed on.
    swallow: Option<u16>,
    /// Wheel remainders below a notch, (horizontal, vertical).
    wheel: (i32, i32),
    placing: Option<Placing>,
}

impl Input {
    /// Whether the mouse is held.
    #[must_use]
    pub fn held(&self) -> bool {
        self.held
    }

    /// The caller's answer to an [`Out::Capture`]`(true)`: whether the capture is now held. Held:
    /// the placement starts (the pointer sent home now). Not held: it is abandoned and the click
    /// goes nowhere -- the next click asks again.
    pub fn captured(&mut self, held: bool, now: Instant) -> Vec<Out> {
        self.held = held;
        if !held {
            self.placing = None;
            return Vec::new();
        }
        let mut out = vec![Out::Title(TITLE_HELD)];
        if let Some(p) = &mut self.placing {
            p.since = now;
            out.push(home());
        }
        out
    }

    /// One window event. `scale` turns window pixels into display pixels (display / window size).
    pub fn event(&mut self, event: &WindowEvent, now: Instant, scale: (f64, f64)) -> Vec<Out> {
        if let Some(p) = &mut self.placing {
            // Keys are not held back: they have nothing to do with where the pointer is.
            if !matches!(event, WindowEvent::KeyDown { .. } | WindowEvent::KeyUp { .. } | WindowEvent::FocusChanged { .. } | WindowEvent::PointerCaptureLost) {
                p.after.push_back(event.clone());
                return Vec::new();
            }
        }
        let mut out = Vec::new();
        match *event {
            WindowEvent::KeyDown { scancode, repeat, .. } => {
                let Some(code) = evdev_code(scancode) else { return out };
                if code == RELEASE_KEY && self.held && !repeat {
                    self.swallow = Some(code);
                    return self.release();
                }
                self.keys.insert(code);
                out.push(Out::Keyboard(vec![(EV_KEY, code, if repeat { 2 } else { 1 })]));
            }
            WindowEvent::KeyUp { scancode, .. } => {
                let Some(code) = evdev_code(scancode) else { return out };
                if self.swallow == Some(code) {
                    self.swallow = None;
                    return out;
                }
                if self.keys.remove(&code) {
                    out.push(Out::Keyboard(vec![(EV_KEY, code, 0)]));
                }
            }
            WindowEvent::PointerDown { button, x, y } if !self.held => {
                // The click that takes the mouse: capture; once held, the pointer home, then placed,
                // then the click (`captured`, `tick`).
                out.push(Out::Capture(true));
                let to = ((f64::from(x) * scale.0 / PLACE_GAIN).round() as i32, (f64::from(y) * scale.1 / PLACE_GAIN).round() as i32);
                let mut after = VecDeque::new();
                after.push_back(WindowEvent::PointerDown { button, x, y });
                self.placing = Some(Placing { since: now, to, after });
            }
            WindowEvent::PointerDown { button, .. } => {
                let code = button_code(button);
                self.buttons.insert(code);
                out.push(Out::Mouse(vec![(EV_KEY, code, 1)]));
            }
            WindowEvent::PointerUp { button, .. } => {
                let code = button_code(button);
                if self.buttons.remove(&code) {
                    out.push(Out::Mouse(vec![(EV_KEY, code, 0)]));
                }
            }
            WindowEvent::PointerMotion { dx, dy } if self.held => {
                let mut packet = Vec::new();
                if dx != 0 {
                    packet.push((EV_REL, REL_X, dx));
                }
                if dy != 0 {
                    packet.push((EV_REL, REL_Y, dy));
                }
                if !packet.is_empty() {
                    out.push(Out::Mouse(packet));
                }
            }
            WindowEvent::Wheel { dx, dy, .. } if self.held => {
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
                    out.push(Out::Mouse(packet));
                }
            }
            WindowEvent::PointerCaptureLost => {
                self.held = false;
                self.placing = None;
                out.extend(self.release_buttons());
                out.push(Out::Title(TITLE_FREE));
            }
            WindowEvent::FocusChanged { focused: false } => {
                out.extend(self.release_keys());
                out.extend(self.release_buttons());
                self.placing = None;
                if self.held {
                    self.held = false;
                    out.push(Out::Capture(false));
                    out.push(Out::Title(TITLE_FREE));
                }
            }
            _ => {}
        }
        out
    }

    /// Time passing: a placement due now finishes -- the move to the click, then what waited.
    pub fn tick(&mut self, now: Instant, scale: (f64, f64)) -> Vec<Out> {
        let due = self.placing.as_ref().is_some_and(|p| now.duration_since(p.since) >= PLACE_WAIT);
        if !due {
            return Vec::new();
        }
        let p = self.placing.take().expect("checked");
        let mut out = vec![Out::Mouse(vec![(EV_REL, REL_X, p.to.0), (EV_REL, REL_Y, p.to.1)])];
        for event in p.after {
            // The click itself as a held mouse's: `held` may still be false for a scripted click.
            let was = self.held;
            self.held = true;
            out.extend(self.event(&event, now, scale));
            self.held = was;
        }
        out
    }

    /// A scripted click at window position (`x`, `y`): placed and pressed as a real one is, with no
    /// capture asked for (a script has no hand on the mouse).
    pub fn scripted_click(&mut self, x: i32, y: i32, button: PointerButton, now: Instant, scale: (f64, f64)) -> Vec<Out> {
        let was = self.held;
        self.held = false;
        let mut out: Vec<Out> = self.event(&WindowEvent::PointerDown { button, x, y }, now, scale).into_iter().filter(|o| !matches!(o, Out::Capture(_))).collect();
        self.held = was;
        if self.placing.is_some() {
            out.push(home());
        }
        if let Some(p) = &mut self.placing {
            p.after.push_back(WindowEvent::PointerUp { button, x, y });
        } else {
            out.extend(self.event(&WindowEvent::PointerUp { button, x, y }, now, scale));
        }
        out
    }

    /// Give the mouse back: its buttons released, the capture ended.
    fn release(&mut self) -> Vec<Out> {
        self.held = false;
        self.placing = None;
        let mut out = self.release_buttons();
        out.push(Out::Capture(false));
        out.push(Out::Title(TITLE_FREE));
        out
    }

    fn release_keys(&mut self) -> Vec<Out> {
        let packet: Packet = std::mem::take(&mut self.keys).into_iter().map(|k| (EV_KEY, k, 0)).collect();
        if packet.is_empty() { Vec::new() } else { vec![Out::Keyboard(packet)] }
    }

    fn release_buttons(&mut self) -> Vec<Out> {
        let packet: Packet = std::mem::take(&mut self.buttons).into_iter().map(|b| (EV_KEY, b, 0)).collect();
        if packet.is_empty() { Vec::new() } else { vec![Out::Mouse(packet)] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: (f64, f64) = (1.0, 1.0);

    fn key(scancode: u32, down: bool) -> WindowEvent {
        if down { WindowEvent::KeyDown { keycode: 0, scancode, repeat: false } } else { WindowEvent::KeyUp { keycode: 0, scancode } }
    }

    #[test]
    fn keys_go_to_the_keyboard_by_their_physical_code_and_repeat_as_the_driver_does() {
        let (mut input, now) = (Input::default(), Instant::now());
        assert_eq!(input.event(&key(0x1E, true), now, ONE), [Out::Keyboard(vec![(EV_KEY, 30, 1)])], "A is KEY_A");
        assert_eq!(input.event(&WindowEvent::KeyDown { keycode: 0, scancode: 0x1E, repeat: true }, now, ONE), [Out::Keyboard(vec![(EV_KEY, 30, 2)])]);
        assert_eq!(input.event(&key(0x1E, false), now, ONE), [Out::Keyboard(vec![(EV_KEY, 30, 0)])]);
        assert_eq!(input.event(&key(0xE048, true), now, ONE), [Out::Keyboard(vec![(EV_KEY, 103, 1)])], "Up is KEY_UP");
        assert!(input.event(&key(0x1E, false), now, ONE).is_empty(), "a release of a key not down is not passed on");
    }

    /// The click that takes the mouse: capture, home, then -- only after the wait -- the move to the
    /// click and the click; a release that came meanwhile follows it, in order.
    #[test]
    fn the_first_click_captures_places_the_pointer_and_then_clicks() {
        let (mut input, t0) = (Input::default(), Instant::now());
        let out = input.event(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 100, y: 50 }, t0, (2.0, 2.0));
        assert_eq!(out, [Out::Capture(true)], "nothing moves before the capture is held");
        assert_eq!(input.captured(true, t0), [Out::Title(TITLE_HELD), home()]);
        assert!(input.event(&WindowEvent::PointerUp { button: PointerButton::Primary, x: 100, y: 50 }, t0, ONE).is_empty(), "held back");
        assert!(input.tick(t0 + Duration::from_millis(100), ONE).is_empty(), "not yet");
        assert_eq!(
            input.tick(t0 + PLACE_WAIT, ONE),
            [
                Out::Mouse(vec![(EV_REL, REL_X, 98), (EV_REL, REL_Y, 49)]),
                Out::Mouse(vec![(EV_KEY, BTN_LEFT, 1)]),
                Out::Mouse(vec![(EV_KEY, BTN_LEFT, 0)]),
            ],
            "placed at the click's display position (window x2, then over the gain), then pressed and released"
        );
        assert_eq!(input.event(&WindowEvent::PointerMotion { dx: 3, dy: -4 }, t0, ONE), [Out::Mouse(vec![(EV_REL, REL_X, 3), (EV_REL, REL_Y, -4)])]);
    }

    #[test]
    fn right_ctrl_gives_the_mouse_back_and_is_not_passed_on() {
        let (mut input, t0) = (Input::default(), Instant::now());
        input.event(&WindowEvent::PointerDown { button: PointerButton::Secondary, x: 1, y: 1 }, t0, ONE);
        input.captured(true, t0);
        input.tick(t0 + PLACE_WAIT, ONE);
        let out = input.event(&key(0xE01D, true), t0, ONE);
        assert_eq!(out, [Out::Mouse(vec![(EV_KEY, BTN_RIGHT, 0)]), Out::Capture(false), Out::Title(TITLE_FREE)]);
        assert!(input.event(&key(0xE01D, false), t0, ONE).is_empty(), "its release is swallowed too");
        assert!(!input.held());
        assert!(input.event(&WindowEvent::PointerMotion { dx: 5, dy: 5 }, t0, ONE).is_empty(), "free: motion is not passed on");
        assert_eq!(input.event(&key(0xE01D, true), t0, ONE), [Out::Keyboard(vec![(EV_KEY, RELEASE_KEY, 1)])], "while free Right Ctrl is a key");
    }

    #[test]
    fn wheel_notches_are_whole_and_losing_the_focus_releases_everything() {
        let (mut input, t0) = (Input::default(), Instant::now());
        input.event(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 1, y: 1 }, t0, ONE);
        input.captured(true, t0);
        input.tick(t0 + PLACE_WAIT, ONE);
        assert!(input.event(&WindowEvent::Wheel { x: 0, y: 0, dx: 0, dy: 60 }, t0, ONE).is_empty(), "half a notch");
        assert_eq!(input.event(&WindowEvent::Wheel { x: 0, y: 0, dx: 0, dy: 60 }, t0, ONE), [Out::Mouse(vec![(EV_REL, REL_WHEEL, 1)])]);
        assert_eq!(input.event(&WindowEvent::Wheel { x: 0, y: 0, dx: -240, dy: 0 }, t0, ONE), [Out::Mouse(vec![(EV_REL, REL_HWHEEL, -2)])]);
        input.event(&key(0x11, true), t0, ONE);
        let out = input.event(&WindowEvent::FocusChanged { focused: false }, t0, ONE);
        assert_eq!(
            out,
            [
                Out::Keyboard(vec![(EV_KEY, 17, 0)]),
                Out::Mouse(vec![(EV_KEY, BTN_LEFT, 0)]),
                Out::Capture(false),
                Out::Title(TITLE_FREE),
            ]
        );
    }

    #[test]
    fn a_refused_capture_moves_nothing_and_the_next_click_asks_again() {
        let (mut input, t0) = (Input::default(), Instant::now());
        assert_eq!(input.event(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 9, y: 9 }, t0, ONE), [Out::Capture(true)]);
        assert!(input.captured(false, t0).is_empty());
        assert!(input.tick(t0 + PLACE_WAIT, ONE).is_empty(), "no placement left");
        assert_eq!(input.event(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 9, y: 9 }, t0, ONE), [Out::Capture(true)]);
    }

    #[test]
    fn a_scripted_click_is_placed_and_clicked_without_a_capture() {
        let (mut input, t0) = (Input::default(), Instant::now());
        let out = input.scripted_click(40, 30, PointerButton::Primary, t0, ONE);
        assert_eq!(out, [Out::Mouse(vec![(EV_REL, REL_X, HOME), (EV_REL, REL_Y, HOME)])]);
        assert_eq!(
            input.tick(t0 + PLACE_WAIT, ONE),
            [Out::Mouse(vec![(EV_REL, REL_X, 20), (EV_REL, REL_Y, 15)]), Out::Mouse(vec![(EV_KEY, BTN_LEFT, 1)]), Out::Mouse(vec![(EV_KEY, BTN_LEFT, 0)])]
        );
        assert!(!input.held());
    }
}
