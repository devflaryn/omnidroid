//! **A touch, through real translated ARM64, in the registers the engine reads.**
//!
//! `jni::input`'s own tests assert what the listener *computes*. These assert where it *lands*: a
//! hand-assembled function stands in for `Java_..._nativePassInput` and stores every argument
//! register it was called with, and each test reads them back. The registers are the ones the
//! real native reads, decoded at `0x02bbba88` (see `jni::input`'s module documentation, and
//! `the_touch_native_reads_the_registers_the_seam_writes` in `tests/gameactivity.rs`, which reads
//! them out of the real binary): `w2` the pointer id, `s0`/`s1` x and y, `w3` the state.
//!
//! A test that only checked that `deliver` returned `Ok` would pass with a seam that put x in
//! `s1`, or the state where the pointer id goes.
//!
//! The same probe stands in for `nativePassKeyEvent` (`jni::keys`), whose `(Z I I Z)` land in the
//! same four integer registers; and probes like it, which also keep `s2`/`s3` and a tag, stand in
//! for the four mouse natives (`jni::mouse`), one of them answering the lock query.
//!
//! ```text
//! cargo test -p omni-android --test input
//! ```

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, Guest};
use omni_android::jni::input::{
    PassInput, TouchInput, INPUT_CLASS, PASS_INPUT_SYMBOL, STATE_BEGAN, STATE_ENDED, STATE_MOVED,
};
use omni_android::jni::Jni;
use omni_android::Boundary;
use omni_cpu::GuestAddr;
use omni_platform::window::{PointerButton, WindowEvent};

/// 144 DPI: density 1.5. Not this host's 1.0, at which a seam that skipped the division would pass.
const DENSITY: f32 = 1.5;
/// The view, in pixels: 1280x720 dp at [`DENSITY`].
const VIEW: (u32, u32) = (1920, 1080);

/// Where the probe writes, as offsets into the harness's data region.
mod slot {
    pub const X0: usize = 0;
    pub const X1: usize = 8;
    /// `w2` in the low half, `w3` in the high.
    pub const W2_W3: usize = 16;
    /// `w4` in the low half, `w5` in the high.
    pub const W4_W5: usize = 24;
    /// `s0` in the low half, `s1` in the high.
    pub const S0_S1: usize = 32;
    pub const CALLS: usize = 40;
    /// `s2` in the low half, `s3` in the high: the mouse probes only.
    pub const S2_S3: usize = 48;
    /// Which mouse probe was called last: [`super::MOVE_TAG`], `BUTTON_TAG` or `WHEEL_TAG`.
    pub const TAG: usize = 56;
    /// What the lock probe answers in `x0`.
    pub const LOCKED: usize = 64;
    /// How many times the lock probe was called.
    pub const QUERIES: usize = 72;
}

/// The mouse probes' tags, so a test can tell which native was called last.
const MOVE_TAG: u64 = 0x4d4f;
const BUTTON_TAG: u64 = 0x4254;
const WHEEL_TAG: u64 = 0x5748;

/// What one call left in the probe's slots.
#[derive(Debug, PartialEq)]
struct Seen {
    env: u64,
    class: u64,
    pointer_id: u32,
    state: u32,
    width: u32,
    height: u32,
    x: f32,
    y: f32,
}

struct Fixture {
    guest: Guest,
    boundary: Arc<Boundary>,
    jni: Arc<Jni>,
    probe: GuestAddr,
}

impl Fixture {
    fn new() -> Fixture {
        let guest = Guest::new();
        let boundary = guest.boundary(16).finish();
        let jni = Jni::new(Arc::clone(&guest.space)).expect("a JNI instance");
        let data = guest.data as u64;
        let mut program = mov64(9, data);
        program.extend([
            str_imm(0, 9, slot::X0 as u32),
            str_imm(1, 9, slot::X1 as u32),
            str_w(2, 9, slot::W2_W3 as u32),
            str_w(3, 9, slot::W2_W3 as u32 + 4),
            str_w(4, 9, slot::W4_W5 as u32),
            str_w(5, 9, slot::W4_W5 as u32 + 4),
            str_s(0, 9, slot::S0_S1 as u32),
            str_s(1, 9, slot::S0_S1 as u32 + 4),
            ldr_imm(10, 9, slot::CALLS as u32),
            add_imm(10, 10, 1),
            str_imm(10, 9, slot::CALLS as u32),
            ret(30),
        ]);
        let probe = guest.load(&program);
        Fixture { guest, boundary, jni, probe }
    }

    /// A seam whose export table knows exactly one symbol -- the real one's name -- at the probe.
    fn seam(&self) -> TouchInput {
        let probe = self.probe;
        TouchInput::new(&self.jni, &|symbol| (symbol == PASS_INPUT_SYMBOL).then_some(probe), DENSITY)
            .expect("a seam over the probe")
    }

    fn deliver(&self, seam: &mut TouchInput, event: WindowEvent) -> Vec<PassInput> {
        let mut cpu = self.guest.thread(&self.boundary);
        seam.deliver(&self.jni, &self.boundary, &mut cpu, 0, &event, VIEW)
            .unwrap_or_else(|error| panic!("{event:?} was not delivered: {error}"))
    }

    fn calls(&self) -> u64 {
        self.guest.read_u64(self.guest.data + slot::CALLS)
    }

    /// What `x1` named on the last call.
    fn class_of_last_call(&self) -> String {
        self.jni.describe(self.seen().class).expect("a live handle")
    }

    fn seen(&self) -> Seen {
        let at = |offset: usize| self.guest.read_u64(self.guest.data + offset);
        let (ints, sizes, floats) = (at(slot::W2_W3), at(slot::W4_W5), at(slot::S0_S1));
        Seen {
            env: at(slot::X0),
            class: at(slot::X1),
            pointer_id: ints as u32,
            state: (ints >> 32) as u32,
            width: sizes as u32,
            height: (sizes >> 32) as u32,
            x: f32::from_bits(floats as u32),
            y: f32::from_bits((floats >> 32) as u32),
        }
    }
}

fn down(x: i32, y: i32) -> WindowEvent {
    WindowEvent::PointerDown { button: PointerButton::Primary, x, y }
}

fn up(x: i32, y: i32) -> WindowEvent {
    WindowEvent::PointerUp { button: PointerButton::Primary, x, y }
}

/// **Press, drag, release: `w2` the pointer, `s0`/`s1` the position in dp, `w3` the state,
/// `w4`/`w5` the view in dp** -- every value distinct from the one beside it, so a swap of any two
/// neighbours is a wrong number in a named register.
#[test]
fn a_touch_reaches_the_native_in_the_registers_the_engine_reads() {
    let _serial = serialized();
    let fixture = Fixture::new();
    let mut seam = fixture.seam();
    seam.set_surface_alive(true);
    let class = |seen: &Seen| fixture.jni.describe(seen.class).expect("a live handle");

    let made = fixture.deliver(&mut seam, down(300, 150));
    assert_eq!(made.len(), 1, "{made:?}");
    let seen = fixture.seen();
    assert_eq!(seen.env, fixture.jni.env_for(0) as u64, "x0 is this thread's JNIEnv*");
    assert_eq!(class(&seen), format!("class {INPUT_CLASS}"), "x1 is the jclass of the declaring class");
    assert_eq!(
        (seen.pointer_id, seen.x, seen.y, seen.state, seen.width, seen.height),
        (0, 200.0, 100.0, STATE_BEGAN as u32, 1280, 720),
        "a press: {seen:?}"
    );

    fixture.deliver(&mut seam, WindowEvent::PointerMoved { x: 450, y: 330 });
    let seen = fixture.seen();
    assert_eq!(
        (seen.pointer_id, seen.x, seen.y, seen.state, seen.width, seen.height),
        (0, 300.0, 220.0, STATE_MOVED as u32, 1280, 720),
        "a drag: {seen:?}"
    );

    fixture.deliver(&mut seam, up(450, 330));
    let seen = fixture.seen();
    assert_eq!(
        (seen.pointer_id, seen.x, seen.y, seen.state, seen.width, seen.height),
        (0, 300.0, 220.0, STATE_ENDED as u32, 1280, 720),
        "a release: {seen:?}"
    );
    assert_eq!(fixture.calls(), 3, "one call for each of the three");
    assert_eq!(seam.delivered(), 3);
}

/// **Nothing reaches the native until the surface is alive**: the seam starts where `jk.o0`
/// starts, `false`. Held-back sends are counted and dropped, and the first one after the surface
/// arrives is a move -- the Java side's behaviour, not a replay of the press.
#[test]
fn nothing_reaches_the_native_until_the_surface_is_alive() {
    let _serial = serialized();
    let fixture = Fixture::new();
    let mut seam = fixture.seam();
    assert!(!seam.surface_alive(), "the surface starts dead");

    fixture.deliver(&mut seam, down(300, 150));
    fixture.deliver(&mut seam, WindowEvent::PointerMoved { x: 330, y: 150 });
    assert_eq!(fixture.calls(), 0, "the native was called before the surface existed");
    assert!(seam.held_back() >= 1, "the held-back sends are counted: {}", seam.held_back());

    seam.set_surface_alive(true);
    fixture.deliver(&mut seam, WindowEvent::PointerMoved { x: 360, y: 150 });
    assert_eq!(fixture.calls(), 1);
    let seen = fixture.seen();
    assert_eq!((seen.state, seen.x), (STATE_MOVED as u32, 240.0), "{seen:?}");
}

/// **A hover and the other buttons never reach the native.** The host's pointer is one finger,
/// and only the primary button puts it down.
#[test]
fn a_hover_and_the_other_buttons_never_reach_the_native() {
    let _serial = serialized();
    let fixture = Fixture::new();
    let mut seam = fixture.seam();
    seam.set_surface_alive(true);
    fixture.deliver(&mut seam, WindowEvent::PointerMoved { x: 10, y: 10 });
    for button in [PointerButton::Secondary, PointerButton::Middle] {
        fixture.deliver(&mut seam, WindowEvent::PointerDown { button, x: 10, y: 10 });
        fixture.deliver(&mut seam, WindowEvent::PointerMoved { x: 20, y: 20 });
        fixture.deliver(&mut seam, WindowEvent::PointerUp { button, x: 20, y: 20 });
    }
    assert_eq!(fixture.calls(), 0);
}

/// **A key, in the registers `nativePassKeyEvent` reads**: `w2` down, `w3` the Linux input code,
/// `w4` the Android key code, `w5` repeat -- decoded at `0x02baebdc`, where `w3` is what the
/// engine turns into a key and `w4` is never read, so a scan code in `w4` would be a keyboard the
/// engine never hears.
#[test]
fn a_key_reaches_the_native_in_the_registers_the_engine_reads() {
    use omni_android::jni::keys::{declare_hardware_keyboard, KeyInput, PASS_KEY_EVENT_SYMBOL};
    let _serial = serialized();
    let fixture = Fixture::new();
    declare_hardware_keyboard(&fixture.jni).expect("a hardware keyboard");
    let probe = fixture.probe;
    let mut keys = KeyInput::new(&fixture.jni, &|symbol| {
        (symbol == PASS_KEY_EVENT_SYMBOL).then_some(probe)
    })
    .expect("a key seam over the probe");
    let mut deliver = |event: WindowEvent| {
        let mut cpu = fixture.guest.thread(&fixture.boundary);
        keys.deliver(&fixture.jni, &fixture.boundary, &mut cpu, 0, &event)
            .unwrap_or_else(|error| panic!("{event:?} was not delivered: {error}"))
    };
    let registers = || {
        let seen = fixture.seen();
        (seen.pointer_id, seen.state, seen.width, seen.height)
    };

    // `W`, make code 0x11: KEY_W 17, AKEYCODE_W 51.
    deliver(WindowEvent::KeyDown { keycode: 0x57, scancode: 0x11, repeat: false });
    assert_eq!(registers(), (1, 17, 51, 0), "(w2 down, w3 scan, w4 key, w5 repeat)");
    assert_eq!(fixture.class_of_last_call(), "class com/roblox/engine/jni/NativeGLInterface");
    deliver(WindowEvent::KeyDown { keycode: 0x57, scancode: 0x11, repeat: true });
    assert_eq!(registers(), (1, 17, 51, 1), "an auto-repeat");
    deliver(WindowEvent::KeyUp { keycode: 0x57, scancode: 0x11 });
    assert_eq!(registers(), (0, 17, 51, 0), "a release");
    // The Up arrow, extended make code 0x48: KEY_UP 103, AKEYCODE_DPAD_UP 19.
    deliver(WindowEvent::KeyDown { keycode: 0x26, scancode: 0xE048, repeat: false });
    assert_eq!(registers(), (1, 103, 19, 0));
    // A pointer event is not a key, and a key with no code reaches nothing.
    deliver(down(10, 10));
    deliver(WindowEvent::KeyDown { keycode: 0x41, scancode: 0, repeat: false });
    assert_eq!(fixture.calls(), 4);
    assert_eq!((keys.delivered(), keys.unmapped()), (4, 1));
}

/// **An engine that does not export the native is refused by name**, not answered with a seam
/// that silently delivers nothing.
#[test]
fn a_missing_export_is_refused_by_name() {
    let _serial = serialized();
    let fixture = Fixture::new();
    let error = TouchInput::new(&fixture.jni, &|_| None, DENSITY).expect_err("nothing exports it");
    assert!(error.to_string().contains(PASS_INPUT_SYMBOL), "{error}");
    let error = MouseInput::new(&fixture.jni, &|_| None, DENSITY).expect_err("nothing exports them");
    assert!(error.to_string().contains(PASS_MOUSE_MOVE_SYMBOL), "{error}");
}

// ---- the mouse: `jni::mouse`, through real translated code ------------------------------------

use omni_android::jni::mouse::{
    MouseInput, MOUSE_LOCKED_CENTER_SYMBOL, PASS_MOUSE_BUTTON_SYMBOL, PASS_MOUSE_MOVE_SYMBOL,
    PASS_MOUSE_WHEEL_SYMBOL,
};

/// The mouse natives' stand-ins: three recording probes, each leaving its tag, and a lock probe
/// that answers whatever [`slot::LOCKED`] holds and counts its calls.
struct Mouse {
    fixture: Fixture,
    move_at: GuestAddr,
    button_at: GuestAddr,
    wheel_at: GuestAddr,
    locked_at: GuestAddr,
}

/// What a mouse probe was last called with: `(tag, s0, s1, s2, s3, w2, w3)`.
type MouseSeen = (u64, f32, f32, f32, f32, u32, u32);

impl Mouse {
    fn new() -> Mouse {
        let fixture = Fixture::new();
        let data = fixture.guest.data as u64;
        let recording = |tag: u64| {
            let mut program = mov64(9, data);
            program.extend([
                str_imm(0, 9, slot::X0 as u32),
                str_imm(1, 9, slot::X1 as u32),
                str_w(2, 9, slot::W2_W3 as u32),
                str_w(3, 9, slot::W2_W3 as u32 + 4),
                str_s(0, 9, slot::S0_S1 as u32),
                str_s(1, 9, slot::S0_S1 as u32 + 4),
                str_s(2, 9, slot::S2_S3 as u32),
                str_s(3, 9, slot::S2_S3 as u32 + 4),
            ]);
            program.extend(mov64(10, tag));
            program.extend([
                str_imm(10, 9, slot::TAG as u32),
                ldr_imm(10, 9, slot::CALLS as u32),
                add_imm(10, 10, 1),
                str_imm(10, 9, slot::CALLS as u32),
                ret(30),
            ]);
            fixture.guest.load(&program)
        };
        let move_at = recording(MOVE_TAG);
        let button_at = recording(BUTTON_TAG);
        let wheel_at = recording(WHEEL_TAG);
        let mut lock = mov64(9, data);
        lock.extend([
            ldr_imm(10, 9, slot::QUERIES as u32),
            add_imm(10, 10, 1),
            str_imm(10, 9, slot::QUERIES as u32),
            ldr_imm(0, 9, slot::LOCKED as u32),
            ret(30),
        ]);
        let locked_at = fixture.guest.load(&lock);
        Mouse { fixture, move_at, button_at, wheel_at, locked_at }
    }

    /// A seam whose export table knows the four real names, at the probes.
    fn seam(&self) -> MouseInput {
        let table = [
            (PASS_MOUSE_MOVE_SYMBOL, self.move_at),
            (PASS_MOUSE_BUTTON_SYMBOL, self.button_at),
            (PASS_MOUSE_WHEEL_SYMBOL, self.wheel_at),
            (MOUSE_LOCKED_CENTER_SYMBOL, self.locked_at),
        ];
        MouseInput::new(
            &self.fixture.jni,
            &|symbol| table.iter().find(|(name, _)| *name == symbol).map(|&(_, at)| at),
            DENSITY,
        )
        .expect("a mouse seam over the probes")
    }

    fn deliver(&self, seam: &mut MouseInput, event: WindowEvent) -> omni_android::jni::mouse::MouseDelivery {
        let fixture = &self.fixture;
        let mut cpu = fixture.guest.thread(&fixture.boundary);
        seam.deliver(&fixture.jni, &fixture.boundary, &mut cpu, 0, &event)
            .unwrap_or_else(|error| panic!("{event:?} was not delivered: {error}"))
    }

    fn set_locked(&self, locked: bool) {
        self.fixture.guest.write_u64(self.fixture.guest.data + slot::LOCKED, u64::from(locked));
    }

    fn queries(&self) -> u64 {
        self.fixture.guest.read_u64(self.fixture.guest.data + slot::QUERIES)
    }

    fn seen(&self) -> MouseSeen {
        let at = |offset: usize| self.fixture.guest.read_u64(self.fixture.guest.data + offset);
        let (ints, low, high) = (at(slot::W2_W3), at(slot::S0_S1), at(slot::S2_S3));
        let float = |bits: u64| f32::from_bits(bits as u32);
        (
            at(slot::TAG),
            float(low),
            float(low >> 32),
            float(high),
            float(high >> 32),
            ints as u32,
            (ints >> 32) as u32,
        )
    }
}

/// **A move, a press and the wheel, each in the registers its native reads** -- decoded at
/// `0x02bbbcf4` (`s0`-`s3` x, y, dx, dy), `0x02bbbd78` (`s0` x, `s1` y, `w2` down, `w3` the
/// button) and `0x02bbbe00` (`s0` x, `s1` y, `s2` the delta); `x0` the `JNIEnv*` and `x1` the
/// `NativeInputInterface` class. The lock is asked before each generic event, as `vk.e$e` asks.
#[test]
fn a_mouse_move_press_and_wheel_reach_their_natives_in_the_registers_the_engine_reads() {
    let _serial = serialized();
    let mouse = Mouse::new();
    let mut seam = mouse.seam();

    let made = mouse.deliver(&mut seam, WindowEvent::PointerMoved { x: 300, y: 150 });
    assert_eq!(made.calls.len(), 1, "{made:?}");
    assert_eq!(mouse.seen(), (MOVE_TAG, 200.0, 100.0, 200.0, 100.0, 0, 0), "a hover, in dp, with its motion");
    let seen = mouse.fixture.seen();
    assert_eq!(seen.env, mouse.fixture.jni.env_for(0) as u64, "x0 is this thread's JNIEnv*");
    assert_eq!(mouse.fixture.class_of_last_call(), format!("class {INPUT_CLASS}"));
    assert_eq!(mouse.queries(), 1, "the hover asked the engine about the lock");

    mouse.deliver(&mut seam, WindowEvent::PointerDown { button: PointerButton::Secondary, x: 300, y: 150 });
    let (tag, x, y, _, _, down, button) = mouse.seen();
    assert_eq!((tag, x, y, down, button), (BUTTON_TAG, 200.0, 100.0, 1, 1), "a right press");

    mouse.deliver(&mut seam, WindowEvent::PointerMoved { x: 330, y: 120 });
    let (tag, x, y, dx, dy, _, _) = mouse.seen();
    assert_eq!((tag, x, y, dx, dy), (MOVE_TAG, 220.0, 80.0, 20.0, -20.0), "a right drag: the camera's motion");

    mouse.deliver(&mut seam, WindowEvent::PointerUp { button: PointerButton::Secondary, x: 330, y: 120 });
    mouse.deliver(&mut seam, WindowEvent::Wheel { x: 330, y: 120, dx: 0, dy: -120 });
    let (tag, x, y, delta, _, _, _) = mouse.seen();
    assert_eq!((tag, x, y, delta), (WHEEL_TAG, 220.0, 80.0, -1.0), "a notch towards the user");
    // A left press: down 1 and button 0, so the two integers cannot trade places unseen.
    mouse.deliver(&mut seam, WindowEvent::PointerDown { button: PointerButton::Primary, x: 330, y: 120 });
    let (tag, _, _, _, _, down, button) = mouse.seen();
    assert_eq!((tag, down, button), (BUTTON_TAG, 1, 0), "a left press: w2 down, w3 the button");
    let counts = seam.counts();
    // Moves: the hover, the drag, and the hover after the release; buttons: right down, right up,
    // left down.
    assert_eq!((counts.moves, counts.buttons, counts.wheels), (3, 3, 1), "{counts:?}");
}

/// **Locked, the listener asks for the capture and passes nothing; captured, the motion is
/// relative and the position accumulates it; unlocked, the capture is given back.** A capture the
/// window lost is taken as lost.
#[test]
fn a_locked_engine_asks_for_the_capture_and_gets_relative_motion() {
    let _serial = serialized();
    let mouse = Mouse::new();
    let mut seam = mouse.seam();
    mouse.set_locked(true);

    let asked = mouse.deliver(&mut seam, WindowEvent::PointerMoved { x: 10, y: 10 });
    assert_eq!((asked.calls.len(), asked.capture), (0, Some(true)), "{asked:?}");
    assert_eq!(mouse.fixture.calls(), 0);
    seam.set_pointer_capture(true);

    mouse.deliver(&mut seam, WindowEvent::PointerMotion { dx: 3, dy: -6 });
    let (tag, x, y, dx, dy, _, _) = mouse.seen();
    assert_eq!((tag, x, y, dx, dy), (MOVE_TAG, 2.0, -4.0, 2.0, -4.0));
    mouse.deliver(&mut seam, WindowEvent::PointerMotion { dx: 3, dy: 3 });
    let (_, x, y, dx, dy, _, _) = mouse.seen();
    assert_eq!((x, y, dx, dy), (4.0, -2.0, 2.0, 2.0), "the position accumulates the motion");

    mouse.set_locked(false);
    let released = mouse.deliver(&mut seam, WindowEvent::PointerMotion { dx: 9, dy: 9 });
    assert_eq!((released.calls.len(), released.capture), (0, Some(false)), "{released:?}");

    seam.set_pointer_capture(true);
    mouse.deliver(&mut seam, WindowEvent::PointerCaptureLost);
    assert!(!seam.has_pointer_capture(), "a lost capture is lost");
}

/// **Keys held when the focus leaves are released**: `W` and then `A` held, the focus lost, and
/// each reaches `nativePassKeyEvent` as a release -- `(0, scan, key, 0)` -- oldest first; a second
/// loss releases nothing.
#[test]
fn keys_held_when_the_focus_leaves_are_released() {
    use omni_android::jni::keys::{declare_hardware_keyboard, KeyInput, PASS_KEY_EVENT_SYMBOL};
    let _serial = serialized();
    let fixture = Fixture::new();
    declare_hardware_keyboard(&fixture.jni).expect("a hardware keyboard");
    let probe = fixture.probe;
    let mut keys = KeyInput::new(&fixture.jni, &|symbol| (symbol == PASS_KEY_EVENT_SYMBOL).then_some(probe))
        .expect("a key seam over the probe");
    let mut deliver = |event: WindowEvent| {
        let mut cpu = fixture.guest.thread(&fixture.boundary);
        keys.deliver(&fixture.jni, &fixture.boundary, &mut cpu, 0, &event)
            .unwrap_or_else(|error| panic!("{event:?} was not delivered: {error}"))
    };
    deliver(WindowEvent::KeyDown { keycode: 0x57, scancode: 0x11, repeat: false });
    deliver(WindowEvent::KeyDown { keycode: 0x57, scancode: 0x11, repeat: true });
    deliver(WindowEvent::KeyDown { keycode: 0x41, scancode: 0x1E, repeat: false });
    deliver(WindowEvent::KeyDown { keycode: 0x53, scancode: 0x1F, repeat: false });
    deliver(WindowEvent::KeyUp { keycode: 0x53, scancode: 0x1F });
    assert_eq!(fixture.calls(), 5);
    let released = deliver(WindowEvent::FocusChanged { focused: false });
    assert_eq!(
        released.iter().map(|key| (key.down, key.scan_code, key.key_code, key.repeat)).collect::<Vec<_>>(),
        [(false, 17, 51, false), (false, 30, 29, false)],
        "W then A, released; S was already up"
    );
    assert_eq!(fixture.calls(), 7, "each release is a call");
    let seen = fixture.seen();
    assert_eq!((seen.pointer_id, seen.state, seen.width, seen.height), (0, 30, 29, 0), "A's release, last");
    assert!(deliver(WindowEvent::FocusChanged { focused: false }).is_empty());
    assert!(deliver(WindowEvent::FocusChanged { focused: true }).is_empty());
    assert_eq!((fixture.calls(), keys.cancelled()), (7, 2));
}
