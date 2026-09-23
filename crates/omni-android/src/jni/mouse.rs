//! **Mouse input**: the host's mouse, delivered the way an Android device with a mouse delivers
//! one to the Java side's `vk.e`, and from there to the engine's mouse natives -- a mouse, not a
//! finger.
//!
//! # Which listener a mouse event reaches, decoded from `classes2.dex`
//!
//! `NativeHelper.n0` builds a `vk.e` over the game's `SurfaceView` (`0x001c`) and makes it the
//! view's touch listener (`0x0021`); `vk.e.G` makes `vk.e$c` its key listener (`0x0005`),
//! **`vk.e$d` its captured-pointer listener** (`0x000d`) and **`vk.e$e` its generic-motion
//! listener** (`0x002b`). Android routes a mouse's `MotionEvent`s by action (AOSP `ViewRootImpl`:
//! `MotionEvent.isTouchEvent()` sends a pointer event to `dispatchTouchEvent`, everything else to
//! `dispatchGenericMotionEvent`, and a captured pointer's to `dispatchCapturedPointerEvent`):
//!
//! | action | route | listener |
//! |---|---|---|
//! | `DOWN` 0, `MOVE` 2, `UP` 1 (a button held) | [`Route::Touch`] | `vk.e.onTouch` |
//! | `HOVER_MOVE` 7, `SCROLL` 8, `BUTTON_PRESS` 11, `BUTTON_RELEASE` 12 | [`Route::Generic`] | `vk.e$e.onGenericMotion` |
//! | anything, while the view has the pointer captured | [`Route::Captured`] | `vk.e$d.onCapturedPointer` |
//!
//! What the host's mouse produces, and in which order, is Android's `CursorInputMapper::sync`
//! (AOSP, `frameworks/native/services/inputflinger/reader/mapper`): a press is `DOWN` then
//! `BUTTON_PRESS`; a release is `BUTTON_RELEASE`, then `UP`, then a `HOVER_MOVE` "to tell the
//! application that the mouse is hovering now"; a second button while one is held is `MOVE` then
//! `BUTTON_PRESS`; only the primary, secondary and tertiary buttons make the pointer "down" -- back
//! and forward alone are a `HOVER_MOVE` and the press. [`MouseDevice`] is that, over the window
//! seam's events. **This ordering is AOSP's, known and not measured on a device here.**
//!
//! # The three listeners
//!
//! * **`vk.e.onTouch`** (`0x0004`-`0x0050`): a mouse is `(getSource() & 0x2002) == 0x2002`
//!   ([`SOURCE_MOUSE`]); with a finger tool it is a touchpad (`vk.e.o`, `0x0012`-`0x001d`). For a
//!   mouse: no button held (`getButtonState() == 0`) turns the event's own history into a scroll
//!   (`0x0023`-`0x004f`) -- an `UP` has none, so nothing -- and a button held is `vk.e.y(event)`.
//! * **`vk.e$e.onGenericMotion`** (`0x0000`-`0x0014`, then `0x00b0`): a mouse or a touchpad
//!   (`0x100008`) asks the engine **`nativeGetMainWindowIsMouseLockedCenter()`**; locked, and the
//!   view without the capture, is **`requestPointerCapture()`** and the event dropped
//!   (`0x00b0`-`0x00c3`); otherwise `vk.e.y(event)` (`0x00c4`).
//! * **`vk.e$d.onCapturedPointer`** (`0x0000`-`0x001b`): **not** locked, and the view holding the
//!   capture, is **`releasePointerCapture()`** and the event dropped; otherwise `vk.e.z(event)`.
//!
//! # What reaches the engine: `vk.e.y` and `vk.e.z`
//!
//! `vk.e.m` and `vk.e.n` are the pointer's last position in density-independent pixels, both 0
//! from the constructor (`<init>` `0x0023`-`0x0026`). `q()` is `nl.a.e(activity)`, the display's
//! `DisplayMetrics.density` -- the same figure [`super::input::TouchListener`] divides by.
//!
//! * `y` (`0x0000`-`0x008b`) -- returns at once while `vk.e.p` is set, which only a touch pinch or
//!   pan sets (`vk.e.a(vk.i)` `0x0015`, `vk.e$g`); a host with no touch screen never sets it.
//!   * `HOVER_MOVE` or `MOVE` (`0x006a`): `x = getX()/q`, `y = getY()/q`, `dx = x - m`,
//!     `dy = y - n`, then `m = x`, `n = y`, and **`nativePassMouseMove(x, y, dx, dy)`**.
//!   * `BUTTON_PRESS` (`0x001d`) / `BUTTON_RELEASE` (`0x0033`): **`nativePassMouseButton(m, n,
//!     down, getActionButton() - 1)`** -- at the **last move's** position, not the event's, and the
//!     button as `BUTTON_PRIMARY` 1 -> 0, `SECONDARY` 2 -> 1, **`TERTIARY` 4 -> 3**, `BACK` 8 -> 7.
//!   * `SCROLL` (`0x0048`-`0x0061`): **`nativePassMouseWheel(m > 0 ? m : 0, n > 0 ? n : 0,
//!     getAxisValue(AXIS_VSCROLL))`**. The horizontal axis is not read.
//!   * `DOWN` answers `true` and passes nothing; everything else answers `false`.
//! * `z` (`0x0000`-`0x0046`) -- `HOVER_MOVE` or `MOVE`: `dx = getAxisValue(AXIS_RELATIVE_X)/q`,
//!   `dy = getAxisValue(AXIS_RELATIVE_Y)/q`; `m += dx`, `n += dy` unless `ci.i.H()` -- the Java flag
//!   `AndroidMouseLockButtonFix`, whose compiled default is `Boolean.FALSE` (`di/a.<init>`
//!   `0x0c14`, the register loaded at `0x0067`), and nothing on this host's Java side sets it; then
//!   **`nativePassMouseMove(m, n, dx, dy)`**. Anything else is `y(event)`.
//!
//! # What the natives do with it, decoded from `libroblox.so`
//!
//! All four are static natives on `NativeInputInterface`, exported under their short manglings,
//! and all of them take the input singleton (`0x2324654`) and **queue** -- which is what makes the
//! UI thread, where a device calls them from `View` callbacks, the right thread here too.
//!
//! * `nativePassMouseMove` (`0x02bbbcf4`): `s0`-`s3` are x, y, dx, dy; on to `0x2e4d9d4`, which
//!   queues them for the main window.
//! * `nativePassMouseButton` (`0x02bbbd78`): `s0` x, `s1` y (truncated to int, `fcvtzs`), `w2`
//!   down, `w3` the button; on to `0x2e4d080`. Buttons 0, 1, 2 are the masks 1, 2, 4 (the table at
//!   `0x6e9940`) -- MouseButton1, 2, 3 -- and **anything past 2 becomes `UserInputType` 22,
//!   `None`**. So on a device **the middle button never reaches the engine as MouseButton3**: the
//!   Java side sends 3. Transcribed, not repaired. A release of a button the engine does not hold is
//!   dropped there (`0x2e4d1b0`-`0x2e4d1dc`).
//! * `nativePassMouseWheel` (`0x02bbbe00`): `s0` x, `s1` y, `s2` the delta, which it scales by an
//!   engine fast-int over 100 (the value at `0x683d480`, set at run time, not decoded here).
//! * `nativeGetMainWindowIsMouseLockedCenter` (`0x02bbbca0` -> `0x2e4ec10`): `true` exactly when
//!   the main window's lock state (`[[input+0xb00]+0x88]`) is 1. The engine writes that state only
//!   **while `UserInputService.MouseEnabled`** (`0x2732808`): `MouseBehavior.LockCenter` -> 1,
//!   `LockCurrentPosition` -> 2, otherwise 0 (`0x2733ce4`-`0x2733d48`). So shift-lock and
//!   first-person capture the pointer; **the right-drag camera (`LockCurrentPosition`) does not** --
//!   it turns on ordinary moves' `dx`/`dy`.
//!
//! # How the engine learns there is a mouse (and a keyboard)
//!
//! From the input, not from `Configuration` (whose `keyboard`/`touchscreen` only AGDK's own copy,
//! `0x68376a8`, ever reads). `UserInputService`'s `LastInputType` setter (around `0x4779700`)
//! turns `MouseEnabled` and `KeyboardEnabled` on for a key **before** it fires
//! `LastInputTypeChanged`, and a mouse button, the wheel or a key put it in a sticky mode that
//! keeps both on (`0x24c36b8` with 3) and is remembered for the next launch (`0x24c3450`). A bare
//! move does not (unless the fast-int at `0x6c4a0d0` is 4). **Nothing is declared for the mouse**:
//! the first click is how a device says it has one, and so is it here.
//!
//! # What this embedding decides
//!
//! * **A press where the pointer is not is a move there first.** A real pointer cannot teleport,
//!   and `vk.e.y` presses at the last *move's* position; a caller whose press arrives without its
//!   move (a synthetic tap) would otherwise click wherever the pointer last was.
//! * **Losing the focus releases every held button.** Android keeps a mouse's stream to the
//!   window it pressed on; the host does not -- a release after the focus has gone goes to another
//!   window, and a button the engine believes held for ever is a camera that never stops turning.
//! * **The capture itself is the window's** ([`omni_platform::window::Window::set_pointer_capture`]):
//!   [`MouseInput::deliver`] says when the listener asked for or gave back the capture, and the
//!   embedding reports what the window did with [`MouseInput::set_pointer_capture`].
//!
//! Not modelled, each for a stated reason: a touchpad as a touchpad (the host reports it as a
//! mouse); `HOVER_ENTER`/`HOVER_EXIT`, which `y` answers `false` to without a call (their only
//! effect would be a capture request the next `HOVER_MOVE` makes anyway); the `KEYCODE_BACK`/
//! `FORWARD` key events Android also synthesizes for the back and forward buttons (the AGDK key
//! path they would take is not wired); `AXIS_HSCROLL`, which `y` does not read.

use std::sync::Arc;

use omni_cpu::GuestCpu;
use omni_mem::GuestAddr;
use omni_platform::window::{PointerButton, WindowEvent};

use crate::boundary::{Boundary, GuestArg};
use crate::error::{AbiError, AbiResult};

use super::input::{INPUT_CLASS, PER_EVENT};
use super::Jni;

/// `nativePassMouseMove(FFFF)V`: `(x, y, dx, dy)`.
pub const PASS_MOUSE_MOVE_SYMBOL: &str =
    "Java_com_roblox_engine_jni_NativeInputInterface_nativePassMouseMove";

/// `nativePassMouseButton(FFZI)V`: `(x, y, down, button)`.
pub const PASS_MOUSE_BUTTON_SYMBOL: &str =
    "Java_com_roblox_engine_jni_NativeInputInterface_nativePassMouseButton";

/// `nativePassMouseWheel(FFF)V`: `(x, y, delta)`.
pub const PASS_MOUSE_WHEEL_SYMBOL: &str =
    "Java_com_roblox_engine_jni_NativeInputInterface_nativePassMouseWheel";

/// `nativeGetMainWindowIsMouseLockedCenter()Z`.
pub const MOUSE_LOCKED_CENTER_SYMBOL: &str =
    "Java_com_roblox_engine_jni_NativeInputInterface_nativeGetMainWindowIsMouseLockedCenter";

/// Every native this module calls, as `(member, descriptor, symbol)`.
pub const NATIVES: [(&str, &str, &str); 4] = [
    ("nativePassMouseMove", "(FFFF)V", PASS_MOUSE_MOVE_SYMBOL),
    ("nativePassMouseButton", "(FFZI)V", PASS_MOUSE_BUTTON_SYMBOL),
    ("nativePassMouseWheel", "(FFF)V", PASS_MOUSE_WHEEL_SYMBOL),
    ("nativeGetMainWindowIsMouseLockedCenter", "()Z", MOUSE_LOCKED_CENTER_SYMBOL),
];

/// `InputDevice.SOURCE_MOUSE`: the mask `vk.e.onTouch` and `vk.e$e` test (`0x0006`, `8194`).
pub const SOURCE_MOUSE: i32 = 0x2002;

/// `InputDevice.SOURCE_MOUSE_RELATIVE`: a captured mouse's source.
pub const SOURCE_MOUSE_RELATIVE: i32 = 0x0002_0004;

/// `MotionEvent.BUTTON_PRIMARY`.
pub const BUTTON_PRIMARY: i32 = 1;
/// `MotionEvent.BUTTON_SECONDARY`.
pub const BUTTON_SECONDARY: i32 = 2;
/// `MotionEvent.BUTTON_TERTIARY`.
pub const BUTTON_TERTIARY: i32 = 4;
/// `MotionEvent.BUTTON_BACK`.
pub const BUTTON_BACK: i32 = 8;
/// `MotionEvent.BUTTON_FORWARD`.
pub const BUTTON_FORWARD: i32 = 16;

/// The buttons that make a mouse's pointer "down" (AOSP `isPointerDown`): the other two are
/// pressed and released while it hovers.
const POINTER_DOWN_BUTTONS: i32 = BUTTON_PRIMARY | BUTTON_SECONDARY | BUTTON_TERTIARY;

/// The host's wheel units in one notch: Windows' `WHEEL_DELTA`. Android's `AXIS_VSCROLL` is one
/// per notch (AOSP `CursorScrollAccumulator`, a wheel's `REL_WHEEL` of 1 at the default scale), so
/// this is the divisor between the two.
pub const WHEEL_DELTA: f32 = 120.0;

/// The Android button a host button is.
#[must_use]
pub const fn android_button(button: PointerButton) -> i32 {
    match button {
        PointerButton::Primary => BUTTON_PRIMARY,
        PointerButton::Secondary => BUTTON_SECONDARY,
        PointerButton::Middle => BUTTON_TERTIARY,
        PointerButton::Back => BUTTON_BACK,
        PointerButton::Forward => BUTTON_FORWARD,
    }
}

/// `MotionEvent.getActionMasked()` for a mouse, the values `vk.e.y` and `vk.e.z` branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    /// `ACTION_DOWN` (0): the pointer went down (the first of the primary, secondary, tertiary).
    Down,
    /// `ACTION_UP` (1): the last of them came up.
    Up,
    /// `ACTION_MOVE` (2): moved with a button held, or any motion while captured.
    Move,
    /// `ACTION_HOVER_MOVE` (7): moved with no button held.
    HoverMove,
    /// `ACTION_SCROLL` (8): the wheel.
    Scroll,
    /// `ACTION_BUTTON_PRESS` (11): one button went down; `action_button` says which.
    ButtonPress,
    /// `ACTION_BUTTON_RELEASE` (12): one button came up.
    ButtonRelease,
}

/// Where Android dispatches a mouse event, and so which of `vk.e`'s listeners hears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `dispatchTouchEvent`: `vk.e.onTouch`.
    Touch,
    /// `dispatchGenericMotionEvent`: `vk.e$e.onGenericMotion`.
    Generic,
    /// `dispatchCapturedPointerEvent`: `vk.e$d.onCapturedPointer`.
    Captured,
}

/// A mouse `MotionEvent`, reduced to what `vk.e` reads from one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseEvent {
    /// Which listener hears it.
    pub route: Route,
    /// `getActionMasked()`.
    pub action: MouseAction,
    /// `getX()`/`getY()`: the pointer in the view's **pixels**; a captured event's are its motion.
    pub x: f32,
    /// See `x`.
    pub y: f32,
    /// `getButtonState()`: the buttons held after this event.
    pub button_state: i32,
    /// `getActionButton()`: the button a press or release is about, 0 for anything else.
    pub action_button: i32,
    /// `getAxisValue(AXIS_VSCROLL)`: notches, positive away from the user.
    pub vscroll: f32,
    /// `getAxisValue(AXIS_RELATIVE_X)`/`_Y`: a captured mouse's motion, in its counts.
    pub relative: (f32, f32),
}

impl MouseEvent {
    fn new(route: Route, action: MouseAction, at: (f32, f32), button_state: i32) -> Self {
        MouseEvent {
            route,
            action,
            x: at.0,
            y: at.1,
            button_state,
            action_button: 0,
            vscroll: 0.0,
            relative: (0.0, 0.0),
        }
    }
}

/// **The host's mouse as Android's input stack presents one** -- `CursorInputMapper::sync` and the
/// view routing, over the window seam's events. See this module's documentation.
#[derive(Debug, Default)]
pub struct MouseDevice {
    /// `getButtonState()`: the Android buttons held.
    buttons: i32,
    /// Where the pointer last was, in the view's pixels. `None` until the host first says.
    at: Option<(i32, i32)>,
}

impl MouseDevice {
    /// The events one window event is, in order, with the view holding the pointer capture or
    /// not. Most window events are none.
    pub fn translate(&mut self, event: &WindowEvent, captured: bool) -> Vec<MouseEvent> {
        let mut out = Vec::new();
        match *event {
            WindowEvent::PointerMoved { x, y } if !captured => self.move_to(x, y, &mut out),
            WindowEvent::PointerDown { button, x, y } => {
                let bit = android_button(button);
                if self.buttons & bit != 0 {
                    // Already held: the host cannot deliver this without a release between, and a
                    // second press of one button is not one a mouse can make.
                    return out;
                }
                if !captured {
                    self.move_to(x, y, &mut out);
                }
                self.press(bit, captured, &mut out);
            }
            WindowEvent::PointerUp { button, x, y } => {
                let bit = android_button(button);
                if self.buttons & bit == 0 {
                    return out;
                }
                if !captured {
                    self.move_to(x, y, &mut out);
                }
                self.release(bit, captured, &mut out);
            }
            WindowEvent::Wheel { x, y, dy, .. } => {
                let route = if captured {
                    Route::Captured
                } else {
                    self.move_to(x, y, &mut out);
                    Route::Generic
                };
                let at = if captured { (0.0, 0.0) } else { self.position() };
                let mut scroll = MouseEvent::new(route, MouseAction::Scroll, at, self.buttons);
                scroll.vscroll = dy as f32 / WHEEL_DELTA;
                out.push(scroll);
            }
            WindowEvent::PointerMotion { dx, dy } if captured => {
                // A captured mouse is `SOURCE_MOUSE_RELATIVE`, for which `CursorInputMapper`
                // reports `MOVE` whether or not a button is held, and `getX`/`getY` are the motion.
                let motion = (dx as f32, dy as f32);
                let mut moved = MouseEvent::new(Route::Captured, MouseAction::Move, motion, self.buttons);
                moved.relative = motion;
                out.push(moved);
            }
            WindowEvent::FocusChanged { focused: false } => {
                // This embedding's decision (see the module documentation): every held button is
                // released where the pointer is.
                for bit in [BUTTON_PRIMARY, BUTTON_SECONDARY, BUTTON_TERTIARY, BUTTON_BACK, BUTTON_FORWARD] {
                    if self.buttons & bit != 0 {
                        self.release(bit, captured, &mut out);
                    }
                }
            }
            _ => {}
        }
        out
    }

    /// The buttons held, as `getButtonState()`.
    #[must_use]
    pub fn buttons(&self) -> i32 {
        self.buttons
    }

    fn position(&self) -> (f32, f32) {
        let (x, y) = self.at.unwrap_or((0, 0));
        (x as f32, y as f32)
    }

    /// The pointer moved to `(x, y)`: `MOVE` with a button down, `HOVER_MOVE` without. A position
    /// the pointer is already at is no motion and no event.
    fn move_to(&mut self, x: i32, y: i32, out: &mut Vec<MouseEvent>) {
        if self.at == Some((x, y)) {
            return;
        }
        self.at = Some((x, y));
        let (route, action) = if self.buttons & POINTER_DOWN_BUTTONS != 0 {
            (Route::Touch, MouseAction::Move)
        } else {
            (Route::Generic, MouseAction::HoverMove)
        };
        out.push(MouseEvent::new(route, action, self.position(), self.buttons));
    }

    /// `CursorInputMapper::sync` for a press: the motion event (`DOWN` when the pointer went down,
    /// `MOVE` when another button changed a down pointer, `HOVER_MOVE` for back/forward alone), then
    /// `BUTTON_PRESS`.
    fn press(&mut self, bit: i32, captured: bool, out: &mut Vec<MouseEvent>) {
        let was_down = self.buttons & POINTER_DOWN_BUTTONS != 0;
        self.buttons |= bit;
        let down = self.buttons & POINTER_DOWN_BUTTONS != 0;
        let action = if down && !was_down {
            MouseAction::Down
        } else if down || captured {
            MouseAction::Move
        } else {
            MouseAction::HoverMove
        };
        out.push(self.motion(action, captured));
        let mut pressed = self.motion(MouseAction::ButtonPress, captured);
        pressed.action_button = bit;
        out.push(pressed);
    }

    /// `CursorInputMapper::sync` for a release: `BUTTON_RELEASE` first, then the motion event --
    /// `UP` when the pointer came up, followed (uncaptured) by the `HOVER_MOVE` Android sends after
    /// an `UP`.
    fn release(&mut self, bit: i32, captured: bool, out: &mut Vec<MouseEvent>) {
        let was_down = self.buttons & POINTER_DOWN_BUTTONS != 0;
        self.buttons &= !bit;
        let down = self.buttons & POINTER_DOWN_BUTTONS != 0;
        let mut released = self.motion(MouseAction::ButtonRelease, captured);
        released.action_button = bit;
        out.push(released);
        if was_down && !down {
            out.push(self.motion(MouseAction::Up, captured));
            if !captured {
                out.push(self.motion(MouseAction::HoverMove, captured));
            }
        } else {
            let action = if down || captured { MouseAction::Move } else { MouseAction::HoverMove };
            out.push(self.motion(action, captured));
        }
    }

    /// An event at the pointer, routed as Android routes its action.
    fn motion(&self, action: MouseAction, captured: bool) -> MouseEvent {
        let route = if captured {
            Route::Captured
        } else if matches!(action, MouseAction::Down | MouseAction::Move | MouseAction::Up) {
            Route::Touch
        } else {
            Route::Generic
        };
        // A captured event's position is its motion, and a press or a release has none.
        let at = if captured { (0.0, 0.0) } else { self.position() };
        MouseEvent::new(route, action, at, self.buttons)
    }
}

/// One call into the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseCall {
    /// `nativePassMouseMove(x, y, dx, dy)`, all in density-independent pixels.
    Move {
        /// `vk.e.m` after the move.
        x: f32,
        /// `vk.e.n` after the move.
        y: f32,
        /// The motion.
        dx: f32,
        /// The motion.
        dy: f32,
    },
    /// `nativePassMouseButton(x, y, down, button)`.
    Button {
        /// `vk.e.m`: the last move's x.
        x: f32,
        /// `vk.e.n`.
        y: f32,
        /// A press.
        down: bool,
        /// `getActionButton() - 1`.
        button: i32,
    },
    /// `nativePassMouseWheel(x, y, delta)`.
    Wheel {
        /// `vk.e.m`, or 0 if it is not positive.
        x: f32,
        /// `vk.e.n`, or 0 if it is not positive.
        y: f32,
        /// `AXIS_VSCROLL`.
        delta: f32,
    },
}

impl MouseCall {
    /// The native this call is made to.
    #[must_use]
    pub fn symbol(&self) -> &'static str {
        match self {
            MouseCall::Move { .. } => PASS_MOUSE_MOVE_SYMBOL,
            MouseCall::Button { .. } => PASS_MOUSE_BUTTON_SYMBOL,
            MouseCall::Wheel { .. } => PASS_MOUSE_WHEEL_SYMBOL,
        }
    }
}

/// What `vk.e` did with one event: the calls it made, and whether it asked for the pointer
/// capture (`Some(true)`, `requestPointerCapture`) or gave it back (`Some(false)`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Heard {
    /// The calls, in order.
    pub calls: Vec<MouseCall>,
    /// A capture request, if the listener made one.
    pub capture: Option<bool>,
}

/// **`vk.e`'s mouse half**, transcribed: `onTouch`'s mouse branch, `vk.e$e.onGenericMotion`,
/// `vk.e$d.onCapturedPointer`, `y` and `z`. See this module's documentation for the decoding.
#[derive(Debug)]
pub struct MouseListener {
    /// `q()`: `DisplayMetrics.density`.
    density: f32,
    /// `vk.e.m`.
    m: f32,
    /// `vk.e.n`.
    n: f32,
}

impl MouseListener {
    /// A listener over a display of `density` -- the figure the embedding answers for
    /// `DisplayMetrics.density`, for [`super::input::TouchListener::new`]'s reason.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] for a density that is not a positive, finite number.
    pub fn new(density: f32) -> AbiResult<Self> {
        if !(density.is_finite() && density > 0.0) {
            return Err(AbiError::Refused {
                symbol: "MouseListener::new".to_string(),
                address: 0,
                why: format!(
                    "a display density of {density} was given; `vk.e.y` divides every position by \
                     it, so the engine would be handed a pointer at infinity or NaN"
                ),
            });
        }
        Ok(MouseListener { density, m: 0.0, n: 0.0 })
    }

    /// `(vk.e.m, vk.e.n)`: where the engine was last told the pointer is.
    #[must_use]
    pub fn position(&self) -> (f32, f32) {
        (self.m, self.n)
    }

    /// Run one event through the listener its route names, with the view holding the pointer
    /// capture (`has_capture`) or not. `locked` is `nativeGetMainWindowIsMouseLockedCenter`, asked
    /// only where the Java side asks it.
    ///
    /// # Errors
    ///
    /// Whatever `locked` fails with.
    pub fn on_event(
        &mut self,
        event: &MouseEvent,
        has_capture: bool,
        locked: &mut dyn FnMut() -> AbiResult<bool>,
    ) -> AbiResult<Heard> {
        let mut heard = Heard::default();
        match event.route {
            // `vk.e.onTouch`, a mouse with a mouse tool: no button held is the history scroll,
            // which an event with no history -- every one here -- makes nothing of.
            Route::Touch => {
                if event.button_state != 0 {
                    self.y(event, &mut heard.calls);
                }
            }
            // `vk.e$e.onGenericMotion`, `0x00b0`.
            Route::Generic => {
                if locked()? && !has_capture {
                    heard.capture = Some(true);
                } else {
                    self.y(event, &mut heard.calls);
                }
            }
            // `vk.e$d.onCapturedPointer`.
            Route::Captured => {
                if !locked()? && has_capture {
                    heard.capture = Some(false);
                } else {
                    self.z(event, &mut heard.calls);
                }
            }
        }
        Ok(heard)
    }

    /// `vk.e.y`.
    fn y(&mut self, event: &MouseEvent, calls: &mut Vec<MouseCall>) {
        match event.action {
            MouseAction::HoverMove | MouseAction::Move => {
                let x = event.x / self.density;
                let y = event.y / self.density;
                let (dx, dy) = (x - self.m, y - self.n);
                self.m = x;
                self.n = y;
                calls.push(MouseCall::Move { x, y, dx, dy });
            }
            MouseAction::ButtonPress | MouseAction::ButtonRelease => calls.push(MouseCall::Button {
                x: self.m,
                y: self.n,
                down: event.action == MouseAction::ButtonPress,
                button: event.action_button - 1,
            }),
            MouseAction::Scroll => {
                // `cmpl-float` then `if-lez`: not greater than zero -- NaN included -- is zero.
                let positive = |v: f32| if v > 0.0 { v } else { 0.0 };
                calls.push(MouseCall::Wheel {
                    x: positive(self.m),
                    y: positive(self.n),
                    delta: event.vscroll,
                });
            }
            MouseAction::Down | MouseAction::Up => {}
        }
    }

    /// `vk.e.z`. `AndroidMouseLockButtonFix` is off (its compiled default), so the position
    /// accumulates the motion.
    fn z(&mut self, event: &MouseEvent, calls: &mut Vec<MouseCall>) {
        match event.action {
            MouseAction::HoverMove | MouseAction::Move => {
                let dx = event.relative.0 / self.density;
                let dy = event.relative.1 / self.density;
                self.m += dx;
                self.n += dy;
                calls.push(MouseCall::Move { x: self.m, y: self.n, dx, dy });
            }
            _ => self.y(event, calls),
        }
    }
}

/// The arguments of one call: `x0` the `JNIEnv*`, `x1` the `jclass`, then the call's own -- floats
/// in `s0` upwards and integers in `w2` upwards, AAPCS64 counting the two separately.
#[must_use]
pub fn mouse_call_args(env: GuestAddr, class: u64, call: &MouseCall) -> Vec<GuestArg> {
    let mut args = vec![GuestArg::Pointer(env), GuestArg::Int(class)];
    match *call {
        MouseCall::Move { x, y, dx, dy } => args.extend([
            GuestArg::Float(x),
            GuestArg::Float(y),
            GuestArg::Float(dx),
            GuestArg::Float(dy),
        ]),
        MouseCall::Button { x, y, down, button } => args.extend([
            GuestArg::Float(x),
            GuestArg::Float(y),
            GuestArg::Int(u64::from(down)),
            GuestArg::Int(i64::from(button) as u64),
        ]),
        MouseCall::Wheel { x, y, delta } => {
            args.extend([GuestArg::Float(x), GuestArg::Float(y), GuestArg::Float(delta)]);
        }
    }
    args
}

/// What one window event came to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MouseDelivery {
    /// The calls made, in order.
    pub calls: Vec<MouseCall>,
    /// `Some(true)` when the listener asked for the pointer capture, `Some(false)` when it gave it
    /// back: the embedding applies it to the window and reports the outcome with
    /// [`MouseInput::set_pointer_capture`].
    pub capture: Option<bool>,
}

/// Counts of what a [`MouseInput`] has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseCounts {
    /// `nativePassMouseMove` calls that returned.
    pub moves: u64,
    /// `nativePassMouseButton` calls that returned.
    pub buttons: u64,
    /// `nativePassMouseWheel` calls that returned.
    pub wheels: u64,
    /// `nativeGetMainWindowIsMouseLockedCenter` calls that returned `true`.
    pub locked: u64,
    /// `requestPointerCapture` calls the listener made.
    pub capture_requests: u64,
    /// `releasePointerCapture` calls it made.
    pub capture_releases: u64,
}

/// **The embedding seam**: host window events in, the engine's mouse natives out, on the thread
/// the embedding calls the lifecycle natives from.
#[derive(Debug)]
pub struct MouseInput {
    device: MouseDevice,
    listener: MouseListener,
    /// `View.hasPointerCapture()`: what the embedding last reported.
    captured: bool,
    /// The four natives, in [`NATIVES`]' order.
    targets: [GuestAddr; 4],
    /// One `jclass` for [`INPUT_CLASS`], for `TouchInput`'s reason.
    class: u64,
    counts: MouseCounts,
}

impl MouseInput {
    /// A seam over the engine's mouse natives, found through `resolve`, over a display of
    /// `density`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] for a density [`MouseListener::new`] refuses;
    /// [`AbiError::JniRefused`] naming the first of [`NATIVES`] `resolve` does not know; whatever
    /// declaring [`INPUT_CLASS`] or taking its `jclass` refuses for.
    pub fn new(
        jni: &Jni,
        resolve: &dyn Fn(&str) -> Option<GuestAddr>,
        density: f32,
    ) -> AbiResult<Self> {
        let listener = MouseListener::new(density)?;
        let mut targets = [0; 4];
        for (target, (member, descriptor, symbol)) in targets.iter_mut().zip(NATIVES) {
            *target = resolve(symbol).ok_or_else(|| AbiError::JniRefused {
                function: symbol.to_string(),
                address: 0,
                detail: format!(
                    "`{INPUT_CLASS}.{member}{descriptor}` is exported by libroblox.so on a device \
                     and nothing resolved it here, so the host's mouse cannot reach the engine"
                ),
            })?;
        }
        super::input::declare_input_class(jni)?;
        let class = jni.class_reference(INPUT_CLASS)?;
        Ok(MouseInput {
            device: MouseDevice::default(),
            listener,
            captured: false,
            targets,
            class,
            counts: MouseCounts::default(),
        })
    }

    /// Say whether the window now holds the pointer capture: the outcome of applying a
    /// [`MouseDelivery::capture`] to it.
    pub fn set_pointer_capture(&mut self, held: bool) {
        self.captured = held;
    }

    /// Whether the view holds the pointer capture, as last reported.
    #[must_use]
    pub fn has_pointer_capture(&self) -> bool {
        self.captured
    }

    /// Deliver one host window event: through [`MouseDevice`], then each resulting event through
    /// [`MouseListener`], making every call it produces with `thread`'s `JNIEnv` on `cpu`.
    ///
    /// A [`WindowEvent::PointerCaptureLost`] is the window saying the capture has ended, and is
    /// taken as that before anything else.
    ///
    /// **The caller holds the activations**, as for `TouchInput::deliver`.
    ///
    /// # Errors
    ///
    /// The guest's own failure, naming the native, when a call does not return. The event's
    /// remaining calls are not made.
    pub fn deliver(
        &mut self,
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        event: &WindowEvent,
    ) -> AbiResult<MouseDelivery> {
        if matches!(event, WindowEvent::PointerCaptureLost) {
            self.captured = false;
        }
        let env = jni.env_for(thread);
        let class = self.class;
        let [move_at, button_at, wheel_at, locked_at] = self.targets;
        let mut delivery = MouseDelivery::default();
        for motion in self.device.translate(event, self.captured) {
            let counts = &mut self.counts;
            let mut locked = || -> AbiResult<bool> {
                let answer = boundary.call_guest(
                    &mut *cpu,
                    "NativeInputInterface.nativeGetMainWindowIsMouseLockedCenter (vk.e$e/vk.e$d)",
                    locked_at,
                    &[GuestArg::Pointer(env), GuestArg::Int(class)],
                    PER_EVENT,
                )?;
                let locked = answer.x0 & 1 != 0;
                counts.locked += u64::from(locked);
                Ok(locked)
            };
            let heard = self.listener.on_event(&motion, self.captured, &mut locked)?;
            match heard.capture {
                Some(true) => self.counts.capture_requests += 1,
                Some(false) => self.counts.capture_releases += 1,
                None => {}
            }
            if heard.capture.is_some() {
                delivery.capture = heard.capture;
            }
            for call in heard.calls {
                let (target, caller) = match call {
                    MouseCall::Move { .. } => (move_at, "NativeInputInterface.nativePassMouseMove (vk.e.y/z)"),
                    MouseCall::Button { .. } => (button_at, "NativeInputInterface.nativePassMouseButton (vk.e.y)"),
                    MouseCall::Wheel { .. } => (wheel_at, "NativeInputInterface.nativePassMouseWheel (vk.e.y)"),
                };
                boundary.call_guest(cpu, caller, target, &mouse_call_args(env, class, &call), PER_EVENT)?;
                match call {
                    MouseCall::Move { .. } => self.counts.moves += 1,
                    MouseCall::Button { .. } => self.counts.buttons += 1,
                    MouseCall::Wheel { .. } => self.counts.wheels += 1,
                }
                delivery.calls.push(call);
            }
        }
        Ok(delivery)
    }

    /// What this seam has done.
    #[must_use]
    pub fn counts(&self) -> MouseCounts {
        self.counts
    }

    /// The Android buttons held.
    #[must_use]
    pub fn buttons(&self) -> i32 {
        self.device.buttons()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Density 1.5, not this host's 1.0, at which a missing division is invisible.
    const DENSITY: f32 = 1.5;

    fn moved(x: i32, y: i32) -> WindowEvent {
        WindowEvent::PointerMoved { x, y }
    }

    fn down(button: PointerButton, x: i32, y: i32) -> WindowEvent {
        WindowEvent::PointerDown { button, x, y }
    }

    fn up(button: PointerButton, x: i32, y: i32) -> WindowEvent {
        WindowEvent::PointerUp { button, x, y }
    }

    fn kinds(events: &[MouseEvent]) -> Vec<(Route, MouseAction, i32, i32)> {
        events.iter().map(|e| (e.route, e.action, e.button_state, e.action_button)).collect()
    }

    /// Run window events through a device and a listener with the lock answering `locked`,
    /// collecting every call and capture request.
    struct Rig {
        device: MouseDevice,
        listener: MouseListener,
        captured: bool,
        locked: bool,
        queries: u32,
    }

    impl Rig {
        fn new() -> Self {
            Rig {
                device: MouseDevice::default(),
                listener: MouseListener::new(DENSITY).expect("a density"),
                captured: false,
                locked: false,
                queries: 0,
            }
        }

        fn feed(&mut self, event: WindowEvent) -> Heard {
            let mut all = Heard::default();
            for motion in self.device.translate(&event, self.captured) {
                let locked = self.locked;
                let queries = &mut self.queries;
                let mut ask = || {
                    *queries += 1;
                    Ok(locked)
                };
                let heard = self.listener.on_event(&motion, self.captured, &mut ask).expect("heard");
                all.calls.extend(heard.calls);
                if heard.capture.is_some() {
                    all.capture = heard.capture;
                }
            }
            all
        }
    }

    /// **A press is `DOWN` then `BUTTON_PRESS`; a release is `BUTTON_RELEASE`, `UP`, then a
    /// hover** -- `CursorInputMapper`'s order -- and each goes where Android routes its action.
    #[test]
    fn a_click_is_down_press_release_up_and_a_hover_each_on_its_route() {
        let mut device = MouseDevice::default();
        assert_eq!(kinds(&device.translate(&moved(10, 20), false)), [(
            Route::Generic,
            MouseAction::HoverMove,
            0,
            0
        )]);
        let pressed = device.translate(&down(PointerButton::Primary, 10, 20), false);
        assert_eq!(kinds(&pressed), [
            (Route::Touch, MouseAction::Down, BUTTON_PRIMARY, 0),
            (Route::Generic, MouseAction::ButtonPress, BUTTON_PRIMARY, BUTTON_PRIMARY),
        ]);
        assert_eq!(kinds(&device.translate(&moved(15, 20), false)), [(
            Route::Touch,
            MouseAction::Move,
            BUTTON_PRIMARY,
            0
        )]);
        assert_eq!(kinds(&device.translate(&up(PointerButton::Primary, 15, 20), false)), [
            (Route::Generic, MouseAction::ButtonRelease, 0, BUTTON_PRIMARY),
            (Route::Touch, MouseAction::Up, 0, 0),
            (Route::Generic, MouseAction::HoverMove, 0, 0),
        ]);
    }

    /// **A second button while one is held is `MOVE` then its press, and the pointer stays down
    /// until the last of the three comes up.** Back and forward alone never put it down.
    #[test]
    fn a_second_button_is_a_move_and_back_alone_is_a_hover() {
        let mut device = MouseDevice::default();
        device.translate(&moved(5, 5), false);
        device.translate(&down(PointerButton::Secondary, 5, 5), false);
        assert_eq!(kinds(&device.translate(&down(PointerButton::Middle, 5, 5), false)), [
            (Route::Touch, MouseAction::Move, BUTTON_SECONDARY | BUTTON_TERTIARY, 0),
            (Route::Generic, MouseAction::ButtonPress, BUTTON_SECONDARY | BUTTON_TERTIARY, BUTTON_TERTIARY),
        ]);
        assert_eq!(kinds(&device.translate(&up(PointerButton::Secondary, 5, 5), false)), [
            (Route::Generic, MouseAction::ButtonRelease, BUTTON_TERTIARY, BUTTON_SECONDARY),
            (Route::Touch, MouseAction::Move, BUTTON_TERTIARY, 0),
        ]);
        device.translate(&up(PointerButton::Middle, 5, 5), false);
        assert_eq!(kinds(&device.translate(&down(PointerButton::Back, 5, 5), false)), [
            (Route::Generic, MouseAction::HoverMove, BUTTON_BACK, 0),
            (Route::Generic, MouseAction::ButtonPress, BUTTON_BACK, BUTTON_BACK),
        ]);
        // A press of a held button, and a release of one not held, are nothing.
        assert!(device.translate(&down(PointerButton::Back, 5, 5), false).is_empty());
        assert!(device.translate(&up(PointerButton::Forward, 5, 5), false).is_empty());
    }

    /// **The listener**: a hover is a move in dp with its motion from the last one; a press is at
    /// the **last move's** position with the button as `getActionButton() - 1`; a release likewise.
    #[test]
    fn moves_are_dp_with_their_motion_and_presses_are_at_the_last_move() {
        let mut rig = Rig::new();
        assert_eq!(rig.feed(moved(300, 150)).calls, [MouseCall::Move {
            x: 200.0,
            y: 100.0,
            dx: 200.0,
            dy: 100.0
        }]);
        assert_eq!(rig.feed(moved(330, 120)).calls, [MouseCall::Move {
            x: 220.0,
            y: 80.0,
            dx: 20.0,
            dy: -20.0
        }]);
        assert_eq!(rig.feed(down(PointerButton::Secondary, 330, 120)).calls, [MouseCall::Button {
            x: 220.0,
            y: 80.0,
            down: true,
            button: 1
        }]);
        // A drag with the button held: a move, from `onTouch`.
        assert_eq!(rig.feed(moved(360, 120)).calls, [MouseCall::Move {
            x: 240.0,
            y: 80.0,
            dx: 20.0,
            dy: 0.0
        }]);
        // Released: the button, then the hover after the `UP`, which is no motion.
        assert_eq!(rig.feed(up(PointerButton::Secondary, 360, 120)).calls, [
            MouseCall::Button { x: 240.0, y: 80.0, down: false, button: 1 },
            MouseCall::Move { x: 240.0, y: 80.0, dx: 0.0, dy: 0.0 },
        ]);
        assert_eq!(rig.queries, 5, "every generic event asks the engine about the lock");
    }

    /// **The buttons are `getActionButton() - 1`**: primary 0, secondary 1, and the middle 3 --
    /// which the engine turns into `None`, as it does on a device.
    #[test]
    fn the_button_is_the_action_button_less_one_middle_included() {
        let mut rig = Rig::new();
        rig.feed(moved(1, 1));
        for (button, index) in [
            (PointerButton::Primary, 0),
            (PointerButton::Secondary, 1),
            (PointerButton::Middle, 3),
            (PointerButton::Back, 7),
            (PointerButton::Forward, 15),
        ] {
            let heard = rig.feed(down(button, 1, 1));
            assert!(
                heard.calls.contains(&MouseCall::Button {
                    x: 1.0 / DENSITY,
                    y: 1.0 / DENSITY,
                    down: true,
                    button: index
                }),
                "{button}: {:?}",
                heard.calls
            );
            rig.feed(up(button, 1, 1));
        }
    }

    /// **A press that arrives without its move is a move there first**, so the engine is not told
    /// the click happened where the pointer last was.
    #[test]
    fn a_press_without_its_move_moves_there_first() {
        let mut rig = Rig::new();
        rig.feed(moved(30, 30));
        let heard = rig.feed(down(PointerButton::Primary, 600, 300));
        assert_eq!(heard.calls, [
            MouseCall::Move { x: 400.0, y: 200.0, dx: 380.0, dy: 180.0 },
            MouseCall::Button { x: 400.0, y: 200.0, down: true, button: 0 },
        ]);
    }

    /// **The wheel is `AXIS_VSCROLL` -- notches, positive away from the user -- at the last move,
    /// clamped to zero.** The horizontal wheel reaches the engine as a zero.
    #[test]
    fn the_wheel_is_notches_at_the_last_move_clamped_at_zero() {
        let mut rig = Rig::new();
        rig.feed(moved(150, 75));
        assert_eq!(rig.feed(WindowEvent::Wheel { x: 150, y: 75, dx: 0, dy: -240 }).calls, [
            MouseCall::Wheel { x: 100.0, y: 50.0, delta: -2.0 }
        ]);
        assert_eq!(rig.feed(WindowEvent::Wheel { x: 150, y: 75, dx: 0, dy: 60 }).calls, [
            MouseCall::Wheel { x: 100.0, y: 50.0, delta: 0.5 }
        ]);
        assert_eq!(rig.feed(WindowEvent::Wheel { x: 150, y: 75, dx: 120, dy: 0 }).calls, [
            MouseCall::Wheel { x: 100.0, y: 50.0, delta: 0.0 }
        ]);
        // A pointer left of the view is at x 0 to the wheel, not negative.
        rig.feed(moved(-30, 75));
        assert_eq!(rig.feed(WindowEvent::Wheel { x: -30, y: 75, dx: 0, dy: 120 }).calls, [
            MouseCall::Wheel { x: 0.0, y: 50.0, delta: 1.0 }
        ]);
    }

    /// **Locked, a generic event asks for the capture and is dropped; captured and unlocked, a
    /// captured event gives it back and is dropped.** Unlocked and uncaptured, nothing is asked.
    #[test]
    fn the_lock_asks_for_the_capture_and_the_unlock_gives_it_back() {
        let mut rig = Rig::new();
        rig.locked = true;
        let heard = rig.feed(moved(10, 10));
        assert_eq!((heard.calls.as_slice(), heard.capture), (&[][..], Some(true)));
        rig.captured = true;
        // Captured and locked: the motion is relative, in dp, and the position accumulates it.
        let heard = rig.feed(WindowEvent::PointerMotion { dx: 3, dy: -6 });
        assert_eq!(heard.calls, [MouseCall::Move { x: 2.0, y: -4.0, dx: 2.0, dy: -4.0 }]);
        assert_eq!(heard.capture, None);
        let heard = rig.feed(WindowEvent::PointerMotion { dx: 3, dy: 3 });
        assert_eq!(heard.calls, [MouseCall::Move { x: 4.0, y: -2.0, dx: 2.0, dy: 2.0 }]);
        // Unlocked: the next captured event gives the capture back and passes nothing.
        rig.locked = false;
        let heard = rig.feed(WindowEvent::PointerMotion { dx: 30, dy: 30 });
        assert_eq!((heard.calls.as_slice(), heard.capture), (&[][..], Some(false)));
        // Not captured and not locked: moves go through without a request.
        rig.captured = false;
        let heard = rig.feed(moved(40, 40));
        assert_eq!(heard.capture, None);
        assert_eq!(heard.calls.len(), 1);
    }

    /// **While captured, a press is `DOWN` and `BUTTON_PRESS` on the captured route** and reaches
    /// the engine at the accumulated position; absolute moves are not motion then.
    #[test]
    fn captured_buttons_are_at_the_accumulated_position() {
        let mut rig = Rig::new();
        rig.locked = true;
        rig.captured = true;
        rig.feed(WindowEvent::PointerMotion { dx: 15, dy: 30 });
        assert!(rig.feed(moved(500, 500)).calls.is_empty(), "no absolute motion while captured");
        let pressed = rig.device.translate(&down(PointerButton::Primary, 500, 500), true);
        assert_eq!(kinds(&pressed), [
            (Route::Captured, MouseAction::Down, BUTTON_PRIMARY, 0),
            (Route::Captured, MouseAction::ButtonPress, BUTTON_PRIMARY, BUTTON_PRIMARY),
        ]);
        let heard = rig.feed(up(PointerButton::Primary, 500, 500));
        assert_eq!(heard.calls, [MouseCall::Button { x: 10.0, y: 20.0, down: false, button: 0 }]);
    }

    /// **Losing the focus releases every held button**, where the pointer is; losing it with none
    /// held does nothing.
    #[test]
    fn losing_the_focus_releases_every_held_button() {
        let mut rig = Rig::new();
        let lost = WindowEvent::FocusChanged { focused: false };
        assert!(rig.feed(lost.clone()).calls.is_empty());
        rig.feed(moved(30, 30));
        rig.feed(down(PointerButton::Secondary, 30, 30));
        rig.feed(down(PointerButton::Primary, 30, 30));
        let heard = rig.feed(lost);
        // Primary first: its release, and a `MOVE` because the secondary still holds the pointer
        // down; then the secondary's, and the `UP` (no call) and the hover after it.
        assert_eq!(heard.calls, [
            MouseCall::Button { x: 20.0, y: 20.0, down: false, button: 0 },
            MouseCall::Move { x: 20.0, y: 20.0, dx: 0.0, dy: 0.0 },
            MouseCall::Button { x: 20.0, y: 20.0, down: false, button: 1 },
            MouseCall::Move { x: 20.0, y: 20.0, dx: 0.0, dy: 0.0 },
        ]);
        assert_eq!(rig.device.buttons(), 0);
    }

    #[test]
    fn a_density_that_is_not_a_positive_number_is_refused() {
        for density in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(MouseListener::new(density).is_err(), "{density}");
        }
    }

    /// The symbols are the short manglings of the class and each member.
    #[test]
    fn the_symbols_are_the_short_manglings() {
        for (member, _, symbol) in NATIVES {
            assert_eq!(symbol, super::super::script::mangle(INPUT_CLASS, member));
        }
    }

    /// `(env, class, floats..., ints...)`, in declaration order.
    #[test]
    fn the_arguments_are_in_declaration_order() {
        let button = MouseCall::Button { x: 1.5, y: 2.5, down: true, button: 3 };
        assert_eq!(mouse_call_args(0x1000, 0x2000, &button), [
            GuestArg::Pointer(0x1000),
            GuestArg::Int(0x2000),
            GuestArg::Float(1.5),
            GuestArg::Float(2.5),
            GuestArg::Int(1),
            GuestArg::Int(3),
        ]);
        let moved = MouseCall::Move { x: 1.0, y: 2.0, dx: 3.0, dy: 4.0 };
        assert_eq!(mouse_call_args(0x1000, 0x2000, &moved)[2..], [
            GuestArg::Float(1.0),
            GuestArg::Float(2.0),
            GuestArg::Float(3.0),
            GuestArg::Float(4.0),
        ]);
        let wheel = MouseCall::Wheel { x: 1.0, y: 2.0, delta: -1.0 };
        assert_eq!(mouse_call_args(0x1000, 0x2000, &wheel)[2..], [
            GuestArg::Float(1.0),
            GuestArg::Float(2.0),
            GuestArg::Float(-1.0),
        ]);
    }
}
