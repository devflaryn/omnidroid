//! **The host's pointer as a mouse whose position is absolute**: each pointer packet of the live
//! window ([`crate::window_input::Out::Pointer`]: `ABS_X`/`ABS_Y` in display pixels, `BTN_*`,
//! `REL_WHEEL`/`REL_HWHEEL` in notches) handed to Android's input dispatcher as a mouse's
//! `MotionEvent`s, through `IInputManager.injectInputEvent` -- the way scrcpy drives a device's
//! mouse, made by the host itself, as a binder client of system_server.
//!
//! Android has no absolute mouse device: `InputReader`'s `CursorInputMapper` takes only relative
//! motion and moves its own pointer by it, through its own acceleration, so the pointer would chase
//! the host's instead of being where it is. An injected event carries its position.
//!
//! # What one packet becomes
//!
//! What `CursorInputMapper::sync` (AOSP, `frameworks/native/services/inputflinger/reader/mapper/`)
//! makes of one, in its order: an `ACTION_BUTTON_RELEASE` for each button released; then `DOWN`
//! or `UP` when the pointer went down or up (only primary, secondary and tertiary make it down),
//! `MOVE` while down, `HOVER_MOVE` otherwise; an `ACTION_BUTTON_PRESS` for each button pressed; a
//! `HOVER_MOVE` after an `UP`; an `ACTION_SCROLL` for the wheels. `SOURCE_MOUSE`,
//! `TOOL_TYPE_MOUSE`, the cursor position the pointer's (the dispatcher sends a mouse's events to
//! the window under its cursor), the event time the moment it is sent on the instance's
//! `CLOCK_MONOTONIC` -- which `crate::input_channel` then times to the app's answer.
//!
//! # The transaction
//!
//! `IInputManager.injectInputEvent(in InputEvent ev, int mode)`, transaction 11 of the image's
//! `IInputManager$Stub` (read from its `getDefaultTransactionName`), `mode` 0
//! (`INJECT_INPUT_EVENT_MODE_ASYNC`: the dispatcher queues it and nothing waits for the app). The
//! event is the parcel `MotionEvent::writeToParcel` (AOSP 15, `libs/input/Input.cpp`) writes after
//! Java's `PARCEL_TOKEN_MOTION_EVENT`. The first event of a session is sent and answered, so a
//! refusal is seen (`[inject]`); the rest go one way, in order, and nothing on the host waits.
//! The host's binder identity is the system's (uid 1000), which holds `INJECT_EVENTS`.
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::binder::{Broker, Parcel};
use crate::evdev::{ABS_X, ABS_Y, BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, EV_ABS, EV_KEY, EV_REL, REL_HWHEEL, REL_WHEEL};

const INPUT_MANAGER: &str = "android.hardware.input.IInputManager";
/// `IInputManager$Stub.TRANSACTION_injectInputEvent` in the image.
const INJECT_INPUT_EVENT: u32 = 11;
/// Java's `InputEvent.PARCEL_TOKEN_MOTION_EVENT`.
const PARCEL_TOKEN_MOTION_EVENT: i32 = 1;
const SOURCE_MOUSE: i32 = 0x2002;
const TOOL_TYPE_MOUSE: i32 = 3;
const BUTTON_PRIMARY: i32 = 1;
const BUTTON_SECONDARY: i32 = 2;
const BUTTON_TERTIARY: i32 = 4;
const BUTTON_BACK: i32 = 8;
const BUTTON_FORWARD: i32 = 16;
const DOWN_BUTTONS: i32 = BUTTON_PRIMARY | BUTTON_SECONDARY | BUTTON_TERTIARY;

/// `MotionEvent` actions.
pub const ACTION_DOWN: i32 = 0;
pub const ACTION_UP: i32 = 1;
pub const ACTION_MOVE: i32 = 2;
pub const ACTION_HOVER_MOVE: i32 = 7;
pub const ACTION_SCROLL: i32 = 8;
pub const ACTION_BUTTON_PRESS: i32 = 11;
pub const ACTION_BUTTON_RELEASE: i32 = 12;

/// `MotionEvent` axes, by their bit in `PointerCoords::bits`.
const AXIS_X: u32 = 0;
const AXIS_Y: u32 = 1;
const AXIS_PRESSURE: u32 = 2;
const AXIS_VSCROLL: u32 = 9;
const AXIS_HSCROLL: u32 = 10;
const AXIS_RELATIVE_X: u32 = 27;
const AXIS_RELATIVE_Y: u32 = 28;

/// One mouse event, before it is a parcel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    pub action: i32,
    pub action_button: i32,
    pub buttons: i32,
    pub x: f32,
    pub y: f32,
    pub dx: f32,
    pub dy: f32,
    pub vscroll: f32,
    pub hscroll: f32,
    pub pressure: f32,
    pub down_time: i64,
    pub event_time: i64,
}

/// The mouse's state: where the pointer is, what is down.
#[derive(Debug, Default)]
pub struct Mouse {
    x: f32,
    y: f32,
    buttons: i32,
    down_time: i64,
}

fn button(code: u16) -> i32 {
    match code {
        BTN_LEFT => BUTTON_PRIMARY,
        BTN_RIGHT => BUTTON_SECONDARY,
        BTN_MIDDLE => BUTTON_TERTIARY,
        BTN_SIDE => BUTTON_BACK,
        BTN_EXTRA => BUTTON_FORWARD,
        _ => 0,
    }
}

impl Mouse {
    /// `CursorInputMapper::sync` for one packet, at `now` (ns on the instance's CLOCK_MONOTONIC).
    pub fn sync(&mut self, packet: &[(u16, u16, i32)], now: i64) -> Vec<Motion> {
        let (mut x, mut y) = (self.x, self.y);
        let (mut pressed, mut released, mut wheel, mut hwheel) = (0, 0, 0, 0);
        for &(kind, code, value) in packet {
            match (kind, code) {
                (EV_ABS, ABS_X) => x = value as f32,
                (EV_ABS, ABS_Y) => y = value as f32,
                (EV_KEY, c) if value != 0 => pressed |= button(c),
                (EV_KEY, c) => released |= button(c),
                (EV_REL, REL_WHEEL) => wheel += value,
                (EV_REL, REL_HWHEEL) => hwheel += value,
                _ => {}
            }
        }
        let (dx, dy) = (x - self.x, y - self.y);
        (self.x, self.y) = (x, y);
        let last = self.buttons;
        let current = (last & !released) | pressed;
        self.buttons = current;
        let (was_down, down) = (last & DOWN_BUTTONS != 0, current & DOWN_BUTTONS != 0);
        let down_changed = was_down != down;
        let moved = dx != 0.0 || dy != 0.0;
        let (buttons_released, buttons_pressed) = (last & !current, current & !last);
        let scrolled = wheel != 0 || hwheel != 0;
        if !(down_changed || moved || scrolled || buttons_released != 0 || buttons_pressed != 0) {
            return Vec::new();
        }
        if down && !was_down {
            self.down_time = now;
        }
        let action = if down_changed {
            if down { ACTION_DOWN } else { ACTION_UP }
        } else if down {
            ACTION_MOVE
        } else {
            ACTION_HOVER_MOVE
        };
        let base = Motion {
            action,
            action_button: 0,
            buttons: current,
            x,
            y,
            dx,
            dy,
            vscroll: 0.0,
            hscroll: 0.0,
            pressure: if down { 1.0 } else { 0.0 },
            down_time: if down || down_changed { self.down_time } else { now },
            event_time: now,
        };
        let mut out = Vec::new();
        let mut state = last;
        for b in (0..5).map(|i| 1 << i).filter(|b| buttons_released & b != 0) {
            state &= !b;
            out.push(Motion { action: ACTION_BUTTON_RELEASE, action_button: b, buttons: state, ..base });
        }
        out.push(base);
        for b in (0..5).map(|i| 1 << i).filter(|b| buttons_pressed & b != 0) {
            state |= b;
            out.push(Motion { action: ACTION_BUTTON_PRESS, action_button: b, buttons: state, ..base });
        }
        // "Send hover move after UP to tell the application that the mouse is hovering now."
        if action == ACTION_UP {
            out.push(Motion { action: ACTION_HOVER_MOVE, down_time: now, ..base });
        }
        if scrolled {
            out.push(Motion { action: ACTION_SCROLL, vscroll: wheel as f32, hscroll: hwheel as f32, ..base });
        }
        out
    }
}

/// `injectInputEvent(ev, INJECT_INPUT_EVENT_MODE_ASYNC)`'s parcel for `m`: `MotionEvent::writeToParcel`
/// (AOSP 15) after the typed object's marker and Java's token.
#[must_use]
pub fn parcel(m: &Motion, id: i32) -> Vec<u8> {
    let mut p = Parcel::with_interface_token(INPUT_MANAGER);
    p.i32(1); // writeTypedObject: not null
    p.i32(PARCEL_TOKEN_MOTION_EVENT);
    p.i32(1); // pointerCount
    p.i32(1); // sampleCount
    p.i32(id);
    p.i32(0); // deviceId
    p.i32(SOURCE_MOUSE);
    p.i32(0); // displayId
    p.byte_vector(&[0; 32]); // hmac
    p.i32(m.action);
    p.i32(m.action_button);
    p.i32(0); // flags
    p.i32(0); // edgeFlags
    p.i32(0); // metaState
    p.i32(m.buttons);
    p.i32(0); // classification (writeByte)
    let identity = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0]; // dsdx, dtdx, tx, dtdy, dsdy, ty
    for v in identity {
        p.f32(v);
    }
    p.f32(1.0); // xPrecision
    p.f32(1.0); // yPrecision
    p.f32(m.x); // raw cursor position: a mouse's events go where its cursor is
    p.f32(m.y);
    for v in identity {
        p.f32(v);
    }
    p.i64(m.down_time);
    p.i32(0); // pointer id
    p.i32(TOOL_TYPE_MOUSE);
    p.i64(m.event_time);
    // PointerCoords: the axes present, as a `BitSet64` -- whose bit n is the n-th from the top
    // (`valueForBit(n)` is `0x8000000000000000 >> n`) -- then their values in axis order.
    let axes = [(AXIS_X, m.x), (AXIS_Y, m.y), (AXIS_PRESSURE, m.pressure), (AXIS_VSCROLL, m.vscroll), (AXIS_HSCROLL, m.hscroll), (AXIS_RELATIVE_X, m.dx), (AXIS_RELATIVE_Y, m.dy)];
    let present: Vec<(u32, f32)> = axes.into_iter().filter(|&(a, v)| a <= AXIS_Y || v != 0.0).collect();
    p.i64(present.iter().fold(0u64, |bits, &(a, _)| bits | (0x8000_0000_0000_0000 >> a)) as i64);
    for (_, v) in &present {
        p.f32(*v);
    }
    p.i32(0); // isResampled
    p.i32(0); // mode: INJECT_INPUT_EVENT_MODE_ASYNC
    p.bytes
}

/// The injector: the mouse's state and the input service's handle, once there is one.
pub struct Injector {
    broker: Arc<Broker>,
    inner: Mutex<Inner>,
}

struct Inner {
    mouse: Mouse,
    handle: Option<u32>,
    looked: Option<Instant>,
    /// Whether an injection has been answered (the first one is sent and answered).
    confirmed: bool,
    next_id: i32,
}

impl Injector {
    #[must_use]
    pub fn new(broker: Arc<Broker>) -> Self {
        Self { broker, inner: Mutex::new(Inner { mouse: Mouse::default(), handle: None, looked: None, confirmed: false, next_id: 0x0100_0000 }) }
    }

    /// Hand one pointer packet to the input dispatcher. Dropped while system_server has no input
    /// service yet (the window's input before boot).
    pub fn send(&self, packet: &[(u16, u16, i32)]) {
        let mut inner = self.inner.lock();
        let now = crate::sys::monotonic().as_nanos() as i64;
        let motions = inner.mouse.sync(packet, now);
        if motions.is_empty() {
            return;
        }
        let Some(handle) = self.handle(&mut inner) else { return };
        for m in motions {
            inner.next_id = inner.next_id.wrapping_add(1);
            let data = parcel(&m, inner.next_id);
            if inner.confirmed {
                if let Err(e) = self.broker.host_transact_oneway(handle, INJECT_INPUT_EVENT, data, &[]) {
                    eprintln!("[inject] the input service is gone (errno {}): looking it up again", e.0);
                    inner.handle = None;
                    return;
                }
                continue;
            }
            match self.broker.host_transact(handle, INJECT_INPUT_EVENT, data, &[]) {
                Ok(reply) => {
                    let exception = reply.get(0..4).map_or(-1, |b| i32::from_le_bytes(b.try_into().expect("4")));
                    let result = reply.get(4..8).map_or(0, |b| i32::from_le_bytes(b.try_into().expect("4")));
                    if exception == 0 {
                        inner.confirmed = true;
                        eprintln!("[inject] the input service takes the host's mouse (injectInputEvent answered {result})");
                    } else {
                        eprintln!("[inject] injectInputEvent refused: exception {exception}");
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("[inject] injectInputEvent failed: errno {}", e.0);
                    inner.handle = None;
                    return;
                }
            }
        }
    }

    /// The input service's handle, looked up at most once a second until there is one.
    fn handle(&self, inner: &mut Inner) -> Option<u32> {
        if inner.handle.is_none() && inner.looked.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            inner.looked = Some(Instant::now());
            match self.broker.check_service("input") {
                Ok(Some(h)) => {
                    eprintln!("[inject] the input service is handle {h}");
                    inner.handle = Some(h);
                }
                Ok(None) => {}
                Err(e) => eprintln!("[inject] looking up the input service: {e}"),
            }
        }
        inner.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(x: i32, y: i32) -> Vec<(u16, u16, i32)> {
        vec![(EV_ABS, ABS_X, x), (EV_ABS, ABS_Y, y)]
    }

    fn actions(m: &[Motion]) -> Vec<(i32, i32, i32)> {
        m.iter().map(|m| (m.action, m.action_button, m.buttons)).collect()
    }

    /// CursorInputMapper's order: DOWN then BUTTON_PRESS; BUTTON_RELEASE, UP, HOVER_MOVE; a second
    /// button while one is held is MOVE then BUTTON_PRESS; back alone does not make the pointer down.
    #[test]
    fn a_packet_becomes_what_cursor_input_mapper_makes_of_it() {
        let mut mouse = Mouse::default();
        assert_eq!(actions(&mouse.sync(&abs(10, 20), 5)), [(ACTION_HOVER_MOVE, 0, 0)]);
        let mut press = abs(12, 20);
        press.push((EV_KEY, BTN_LEFT, 1));
        let m = mouse.sync(&press, 6);
        assert_eq!(actions(&m), [(ACTION_DOWN, 0, 1), (ACTION_BUTTON_PRESS, 1, 1)]);
        assert_eq!((m[0].x, m[0].y, m[0].dx, m[0].pressure, m[0].down_time), (12.0, 20.0, 2.0, 1.0, 6));
        assert_eq!(actions(&mouse.sync(&[(EV_KEY, BTN_RIGHT, 1)], 7)), [(ACTION_MOVE, 0, 3), (ACTION_BUTTON_PRESS, 2, 3)]);
        assert_eq!(actions(&mouse.sync(&[(EV_KEY, BTN_RIGHT, 0), (EV_KEY, BTN_LEFT, 0)], 8)), [(ACTION_BUTTON_RELEASE, 1, 2), (ACTION_BUTTON_RELEASE, 2, 0), (ACTION_UP, 0, 0), (ACTION_HOVER_MOVE, 0, 0)]);
        assert_eq!(actions(&mouse.sync(&[(EV_KEY, BTN_SIDE, 1)], 9)), [(ACTION_HOVER_MOVE, 0, 8), (ACTION_BUTTON_PRESS, 8, 8)], "back does not make it down");
        let m = mouse.sync(&[(EV_REL, REL_WHEEL, -1)], 10);
        assert_eq!(actions(&m), [(ACTION_HOVER_MOVE, 0, 8), (ACTION_SCROLL, 0, 8)]);
        assert_eq!(m[1].vscroll, -1.0);
        assert!(mouse.sync(&abs(12, 20), 11).is_empty(), "no change, no event");
    }

    /// The parcel's layout, field by field as MotionEvent::writeToParcel writes it.
    #[test]
    fn the_parcel_is_motion_event_write_to_parcels() {
        let m = Motion { action: ACTION_DOWN, action_button: 0, buttons: 1, x: 400.0, y: 300.0, dx: 0.0, dy: 0.0, vscroll: 0.0, hscroll: 0.0, pressure: 1.0, down_time: 7, event_time: 9 };
        let bytes = parcel(&m, 42);
        let token_len = 12 + 4 + (INPUT_MANAGER.len() + 1) * 2;
        let at = (token_len + 3) & !3;
        let i32_at = |o: usize| i32::from_le_bytes(bytes[at + o..at + o + 4].try_into().unwrap());
        let f32_at = |o: usize| f32::from_le_bytes(bytes[at + o..at + o + 4].try_into().unwrap());
        let i64_at = |o: usize| i64::from_le_bytes(bytes[at + o..at + o + 8].try_into().unwrap());
        assert_eq!([i32_at(0), i32_at(4), i32_at(8), i32_at(12), i32_at(16)], [1, 1, 1, 1, 42], "non-null, token, 1 pointer, 1 sample, id");
        assert_eq!([i32_at(24), i32_at(28), i32_at(32)], [SOURCE_MOUSE, 0, 32], "source, display, hmac length");
        let after_hmac = 36 + 32;
        assert_eq!([i32_at(after_hmac), i32_at(after_hmac + 20)], [ACTION_DOWN, 1], "action, buttons");
        let cursor = after_hmac + 28 + 24 + 8;
        assert_eq!((f32_at(cursor), f32_at(cursor + 4)), (400.0, 300.0), "the cursor where the pointer is");
        let down = cursor + 8 + 24;
        assert_eq!(i64_at(down), 7);
        assert_eq!([i32_at(down + 8), i32_at(down + 12)], [0, TOOL_TYPE_MOUSE]);
        assert_eq!(i64_at(down + 16), 9, "the sample's time");
        assert_eq!(i64_at(down + 24) as u64, 0xE000_0000_0000_0000, "X, Y, pressure: bits 0, 1, 2 from the top");
        assert_eq!((f32_at(down + 32), f32_at(down + 36), f32_at(down + 40)), (400.0, 300.0, 1.0));
        assert_eq!([i32_at(down + 44), i32_at(down + 48)], [0, 0], "not resampled; mode ASYNC");
        assert_eq!(bytes.len(), at + down + 52);
    }
}
