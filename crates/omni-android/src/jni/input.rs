//! **Touch input**: the Java side's `vk.e.onTouch`, run by the host, so that a pointer on the
//! host's window reaches the engine the way a finger on a device's screen does.
//!
//! # Which path a device uses -- decoded, because the binary carries two
//!
//! `libroblox.so` has both AGDK's `GameActivity.onTouchEventNative` (one of the 24 natives its
//! `RegisterNatives` binds) and Roblox's own `NativeInputInterface.nativePassInput`. The dex says
//! which one a touch on the screen reaches (`classes2.dex`):
//!
//! * `GameActivity.onCreate` sets `GameActivity$c` -- whose `onTouch` is `N1`, and so
//!   `onTouchEventNative` -- as the `SurfaceView`'s touch listener (`0x0125`).
//! * `MainGameActivity.onCreate` calls `super.onCreate` (`0x00cb`) and **then**
//!   `NativeHelper.n0` (`0x00f1`), which builds a `vk.e` and calls
//!   `surfaceView.setOnTouchListener(vk.e)` (`0x0021`). A `View` holds one `OnTouchListener`, so
//!   this *replaces* AGDK's.
//! * `vk.e.onTouch` returns `true`, consuming the event, and neither activity overrides
//!   `onTouchEvent` -- so nothing on the screen reaches `onTouchEventNative`.
//!
//! The live path is therefore `vk.e.onTouch` -> `NativeInputInterface.nativePassInput(IFFIII)V`,
//! a static native the engine **exports** under its short mangling ([`PASS_INPUT_SYMBOL`],
//! `0x02bbba88`; `jni-surface-lists.txt` Section G tags it `SHORT`). MEASURED on the gate: the
//! engine calls `RegisterNatives` exactly once, for GameActivity's 24, so nothing rebinds it.
//!
//! # What the native reads, decoded at `0x02bbba88`
//!
//! ```text
//! mov  w19, w3        ; state
//! fmov s8,  s1        ; y
//! fmov s9,  s0        ; x
//! mov  w20, w2        ; pointerId
//! bl   0x2324654      ; the input singleton (w0 = 4)
//! fmov s0, s9 ; fmov s1, s8 ; sxtw x1, w20 ; mov w2, w19
//! bl   0x2e4e68c      ; (input, pointerId, state, x, y)
//! ```
//!
//! AAPCS64 numbers the integer and the floating-point arguments separately, so
//! `(JNIEnv*, jclass, int, float, float, int, int, int)` is `x0, x1, w2, s0, s1, w3, w4, w5`.
//! **`w4` and `w5` -- the width and height -- are never read by this build.** They are passed
//! anyway, as `vk.e` computes them, because the descriptor has them and the next build may not
//! ignore them.
//!
//! `0x2e4e68c` does not act on the event: it builds a closure (vtable `0x639ebe8`) and posts it to
//! the engine's own queue. That is what makes calling it from the **UI thread** correct -- which
//! is where a device calls it, because `onTouch` is a `View` callback, and where an embedding
//! already calls the lifecycle natives from.
//!
//! # The model: `vk.e.onTouch`, for a touch-source event with no history
//!
//! [`TouchListener`] is that method, transcribed. Per pointer it keeps a `vk.e$h` -- a position
//! and a state, each with the value it had before its last set (`e(F)`, `f(F)` and `d(I)` all
//! shift the current value into the previous one before storing):
//!
//! * `ACTION_DOWN`/`ACTION_POINTER_DOWN` -> a new record at `getX/getY(actionIndex) / density`,
//!   state [`STATE_BEGAN`] (`0x01f1`-`0x020b`).
//! * `ACTION_MOVE` -> every tracked pointer moves to its new position, state [`STATE_MOVED`]
//!   (`0x00ff`-`0x012e`).
//! * `ACTION_UP`/`ACTION_CANCEL`/`ACTION_POINTER_UP` -> state [`STATE_ENDED`], **position
//!   unchanged** (`0x01bd`).
//!
//! Then, at `0x0228`, every tracked pointer is considered, in id order (a `SparseArray`), and
//! passed to the engine when its state is [`STATE_ENDED`], or its position changed, or its state
//! changed to anything but a first move -- and only if `D.b()`, the surface, is alive (see
//! [`TouchInput::set_surface_alive`]). Sending records the position as the last one sent. Ended
//! pointers are then forgotten, **whether or not they were sent**.
//!
//! **Not modelled, each for a stated reason:**
//!
//! * `nativePassInputBatch` sends a move's *history*. The host's window seam coalesces moves to
//!   their newest (`omni_platform::window`'s `push_event`), so an event here has no history, and
//!   the Java side sends nothing for one with none. It is also behind the Java flag
//!   `AndroidProcessHistoricalTouchEvents`.
//! * `nativePassTouchEndVelocity` is sent only while `vk.e.F` is true, which is the Java flag
//!   `AndroidSendTouchEndVelocity`: its default is `Boolean.FALSE` (`di/a.<init>` `0x130a`, the
//!   register loaded at `0x0067`), and nothing on this host's Java side sets it.
//! * The gesture detectors `vk.e` feeds afterwards (`GestureDetector` -> tap, long press, fling;
//!   `vk.h`, `vk.i` -> pinch, rotate, pan) are Android framework timing and slop rules, not
//!   Roblox's. The long-press id they would set starts at `-1` (`vk.e$g.<init>`), so the move path
//!   never sends one here. **Missing, not faked**: nothing calls `nativePassTapGesture`.
//! * Mouse-source events (`getSource() & 8194`) take `vk.e.y`, not this path: decoded and
//!   transcribed in [`super::mouse`]. This module is the **phone configuration**, where the host
//!   presents its pointer as a finger -- see [`HostFinger`]; an embedding whose host has a mouse
//!   uses [`super::mouse::MouseInput`] instead, and a device with no touch screen sends nothing
//!   here.

use std::collections::BTreeMap;
use std::sync::Arc;

use omni_cpu::{GuestCpu, RunLimit};
use omni_mem::GuestAddr;
use omni_platform::window::{PointerButton, WindowEvent};

use crate::boundary::{Boundary, GuestArg};
use crate::error::{AbiError, AbiResult};

use super::Jni;

/// The class the input natives are declared on.
pub const INPUT_CLASS: &str = "com/roblox/engine/jni/NativeInputInterface";

/// The native a touch reaches.
pub const PASS_INPUT: &str = "nativePassInput";

/// Its descriptor: `(pointerId, x, y, state, width, height)`.
pub const PASS_INPUT_DESCRIPTOR: &str = "(IFFIII)V";

/// Its exported symbol, the short mangling of [`INPUT_CLASS`] and [`PASS_INPUT`]. Spelled out,
/// and checked against [`super::script::mangle`] by this module's tests, because this is the
/// string the embedding's export table is searched for.
pub const PASS_INPUT_SYMBOL: &str = "Java_com_roblox_engine_jni_NativeInputInterface_nativePassInput";

/// Guest instructions one `nativePassInput` is allowed.
///
/// The native posts to a queue and returns, so a figure this large is only ever reached by the
/// first call, which also constructs the input singleton behind `0x2324654`. Counted rather than
/// `Unlimited` for D16's reason, and the same budget [`super::script::PER_DOWNCALL`] uses.
pub const PER_EVENT: RunLimit = RunLimit::Instructions(200_000_000);

/// `vk.e$h`'s state for a pointer that has just touched: `d(0)` at `0x020b`, for `ACTION_DOWN` and
/// `ACTION_POINTER_DOWN`.
pub const STATE_BEGAN: i32 = 0;

/// The state for a pointer that moved: `d(1)` at `0x012e`, for `ACTION_MOVE`.
pub const STATE_MOVED: i32 = 1;

/// The state for a pointer that lifted or was cancelled: `d(2)` at `0x01bd`, for `ACTION_UP`,
/// `ACTION_CANCEL` and `ACTION_POINTER_UP`.
pub const STATE_ENDED: i32 = 2;

/// The pointer id Android gives the first finger to touch, and the only one [`HostFinger`] makes.
pub const PRIMARY_FINGER: i32 = 0;

/// `MotionEvent.getActionMasked()`, the values `vk.e.onTouch` branches on (`0x006d`-`0x007a`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `ACTION_DOWN` (0): the first pointer touched.
    Down,
    /// `ACTION_UP` (1): the last pointer lifted.
    Up,
    /// `ACTION_MOVE` (2): pointers moved.
    Move,
    /// `ACTION_CANCEL` (3): the gesture was taken away.
    Cancel,
    /// `ACTION_POINTER_DOWN` (5): another pointer touched.
    PointerDown,
    /// `ACTION_POINTER_UP` (6): a pointer other than the last lifted.
    PointerUp,
}

/// A touch-source `MotionEvent`, reduced to what `vk.e.onTouch` reads from one with no history.
#[derive(Debug, Clone, PartialEq)]
pub struct MotionEvent {
    /// `getActionMasked()`.
    pub action: Action,
    /// `getPointerId(getActionIndex())`: the pointer the action is about.
    pub pointer: i32,
    /// Every pointer the event carries, as `(id, getX, getY)`, in the view's **pixels**. A device
    /// includes every pointer that is down in every event, and so must a caller.
    pub pointers: Vec<(i32, f32, f32)>,
}

impl MotionEvent {
    /// `getX/getY(findPointerIndex(id))`.
    fn position(&self, id: i32) -> Option<(f32, f32)> {
        self.pointers.iter().find(|(pointer, _, _)| *pointer == id).map(|&(_, x, y)| (x, y))
    }
}

/// One `nativePassInput` call, with the arguments `vk.e.onTouch` computes (`0x02b5`-`0x02e5`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PassInput {
    /// The pointer's id.
    pub pointer_id: i32,
    /// x in density-independent pixels: `getX / density`.
    pub x: f32,
    /// y in density-independent pixels.
    pub y: f32,
    /// [`STATE_BEGAN`], [`STATE_MOVED`] or [`STATE_ENDED`].
    pub state: i32,
    /// `(int)(view.getWidth() / density)`.
    pub width: i32,
    /// `(int)(view.getHeight() / density)`.
    pub height: i32,
}

/// What one event produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Touched {
    /// The calls to make, in order.
    pub calls: Vec<PassInput>,
    /// Pointers that would have been sent had the surface been alive. The Java side logs
    /// `nativePassInput not ready or already passed event` for these (`0x02eb`) and **does not
    /// queue them**.
    pub held_back: usize,
}

/// One `vk.e$h`. Every setter keeps the value it replaces, as `d(I)`, `e(F)` and `f(F)` do.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Pointer {
    /// `a`.
    x: f32,
    /// `b`.
    y: f32,
    /// `c`.
    state: i32,
    /// `d`: the x before the last set, or the x last sent.
    prev_x: f32,
    /// `e`.
    prev_y: f32,
    /// `f`: the state before the last set.
    prev_state: i32,
}

impl Pointer {
    fn set_x(&mut self, x: f32) {
        self.prev_x = self.x;
        self.x = x;
    }

    fn set_y(&mut self, y: f32) {
        self.prev_y = self.y;
        self.y = y;
    }

    fn set_state(&mut self, state: i32) {
        self.prev_state = self.state;
        self.state = state;
    }
}

/// `vk.e.onTouch` for touch-source events. See this module's documentation for the decoding.
#[derive(Debug)]
pub struct TouchListener {
    /// `nl.a.e(activity)`, which is `DisplayMetrics.density`.
    density: f32,
    /// `vk.e.g`, a `SparseArray` -- iterated in key order, which a `BTreeMap` also is.
    pointers: BTreeMap<i32, Pointer>,
}

impl TouchListener {
    /// A listener over a display of `density`: **the same figure the embedding answers for
    /// `DisplayMetrics.density`**, because `vk.e.q()` is `nl.a.e(activity)`, which reads exactly
    /// that field. A second source for it would let the engine's layout and its touches disagree.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] for a density that is not a positive, finite number. The Java side
    /// divides every coordinate by it; MEASURED what a zero density did one field over -- the
    /// renderer divided by it and sized a texture from the infinity.
    pub fn new(density: f32) -> AbiResult<Self> {
        if !(density.is_finite() && density > 0.0) {
            return Err(AbiError::Refused {
                symbol: "TouchListener::new".to_string(),
                address: 0,
                why: format!(
                    "a display density of {density} was given; `vk.e.onTouch` divides every \
                     coordinate by it, so the engine would be handed a touch at infinity or NaN"
                ),
            });
        }
        Ok(Self { density, pointers: BTreeMap::new() })
    }

    /// The density coordinates are divided by.
    #[must_use]
    pub fn density(&self) -> f32 {
        self.density
    }

    /// Run one event through the listener, over a view `view` pixels in size, with the surface
    /// `ready` (`D.b()`) or not.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the event does not carry a position for a pointer the Java side
    /// would read one for -- the action pointer of a down, or any tracked pointer of a move. On a
    /// device that is `getX(-1)`, which throws; here it is a caller that built an event a device
    /// never produces, and a position invented for it would be a touch nobody made.
    pub fn on_touch(
        &mut self,
        event: &MotionEvent,
        view: (u32, u32),
        ready: bool,
    ) -> AbiResult<Touched> {
        let scale = self.density;
        let at = |id: i32| -> AbiResult<(f32, f32)> {
            event.position(id).ok_or_else(|| AbiError::Refused {
                symbol: "vk.e.onTouch".to_string(),
                address: 0,
                why: format!(
                    "a {:?} event carries no position for pointer {id}, and `vk.e.onTouch` reads \
                     one; a device includes every pointer that is down in every event",
                    event.action
                ),
            })
        };
        match event.action {
            Action::Down | Action::PointerDown => {
                let (x, y) = at(event.pointer)?;
                let mut pointer = Pointer::default();
                pointer.set_x(x / scale);
                pointer.set_y(y / scale);
                pointer.set_state(STATE_BEGAN);
                self.pointers.insert(event.pointer, pointer);
            }
            Action::Move => {
                // Every position first, so an event missing one changes nothing.
                let mut moved = Vec::with_capacity(self.pointers.len());
                for &id in self.pointers.keys() {
                    moved.push((id, at(id)?));
                }
                for (id, (x, y)) in moved {
                    if let Some(pointer) = self.pointers.get_mut(&id) {
                        pointer.set_x(x / scale);
                        pointer.set_y(y / scale);
                        pointer.set_state(STATE_MOVED);
                    }
                }
            }
            Action::Up | Action::PointerUp | Action::Cancel => {
                // The position is NOT updated: `0x01bc` sets the state and nothing else, so an up
                // is reported where the pointer last moved to.
                if let Some(pointer) = self.pointers.get_mut(&event.pointer) {
                    pointer.set_state(STATE_ENDED);
                }
            }
        }

        // `0x02b5`-`0x02c1`: `(int)((float)view.getWidth() / scale)`. Java's `float-to-int`
        // truncates toward zero and saturates, and so does `as`.
        let width = (view.0 as f32 / scale) as i32;
        let height = (view.1 as f32 / scale) as i32;
        let mut touched = Touched::default();
        let mut ended = Vec::new();
        for (&id, pointer) in &mut self.pointers {
            // `cmpl-float` then `if-nez`: unequal, or either side NaN, is a change.
            let changed = pointer.x != pointer.prev_x || pointer.y != pointer.prev_y;
            let send = if pointer.state == STATE_ENDED {
                ended.push(id);
                true
            } else if pointer.state == pointer.prev_state {
                changed
            } else if pointer.state != STATE_MOVED || pointer.prev_state != STATE_BEGAN {
                true
            } else {
                changed
            };
            if !send {
                continue;
            }
            if ready {
                pointer.prev_x = pointer.x;
                pointer.prev_y = pointer.y;
                touched.calls.push(PassInput {
                    pointer_id: id,
                    x: pointer.x,
                    y: pointer.y,
                    state: pointer.state,
                    width,
                    height,
                });
            } else {
                touched.held_back += 1;
            }
        }
        // `0x02f6`: ended pointers are forgotten whether or not they were sent.
        for id in ended {
            self.pointers.remove(&id);
        }
        Ok(touched)
    }
}

/// **The host's pointer as one finger** -- this embedding's decision for the phone configuration,
/// and the whole of it. With a mouse declared, [`super::mouse`] is the pointer instead.
///
/// A mobile Roblox build is played by touch: an on-screen thumbstick, a jump button, and a drag to
/// turn the camera. The host has a mouse. So the **primary button is contact**: pressing it puts
/// finger [`PRIMARY_FINGER`] down where the pointer is, dragging moves it, and releasing lifts it.
///
/// * **Nothing else touches.** The other buttons have no counterpart on a touchscreen, and a hover
///   (a move with no button held) is not a touch -- a touchscreen does not report one.
/// * **A release where the pointer is not where it last moved to is a move, then the release.**
///   `vk.e` ignores an up's own position (it reports the pointer where it last moved to), so
///   without the move the engine would see the finger lift somewhere it had left.
/// * **Losing focus mid-press cancels the touch.** The window seam captures the pointer while a
///   button is held, but a capture something else takes (`WM_CAPTURECHANGED`) ends with no
///   button-up at all, and a finger that never lifts is a thumbstick held forever.
#[derive(Debug, Default)]
pub struct HostFinger {
    /// Where the finger is, in the view's pixels, while the primary button is held.
    at: Option<(i32, i32)>,
}

impl HostFinger {
    /// The `MotionEvent`s a window event is, in order. Most window events are none.
    pub fn translate(&mut self, event: &WindowEvent) -> Vec<MotionEvent> {
        let touch = |action: Action, x: i32, y: i32| MotionEvent {
            action,
            pointer: PRIMARY_FINGER,
            pointers: vec![(PRIMARY_FINGER, x as f32, y as f32)],
        };
        match *event {
            WindowEvent::PointerDown { button: PointerButton::Primary, x, y } => {
                if self.at.is_some() {
                    // Already down: the seam cannot deliver this without an up in between, and a
                    // second contact at the same place is not one a finger can make.
                    return Vec::new();
                }
                self.at = Some((x, y));
                vec![touch(Action::Down, x, y)]
            }
            WindowEvent::PointerMoved { x, y } => match self.at {
                Some(_) => {
                    self.at = Some((x, y));
                    vec![touch(Action::Move, x, y)]
                }
                None => Vec::new(),
            },
            WindowEvent::PointerUp { button: PointerButton::Primary, x, y } => match self.at.take() {
                Some(last) if last != (x, y) => {
                    vec![touch(Action::Move, x, y), touch(Action::Up, x, y)]
                }
                Some(_) => vec![touch(Action::Up, x, y)],
                None => Vec::new(),
            },
            WindowEvent::FocusChanged { focused: false } => match self.at.take() {
                Some((x, y)) => vec![touch(Action::Cancel, x, y)],
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// Whether the finger is down.
    #[must_use]
    pub fn is_down(&self) -> bool {
        self.at.is_some()
    }
}

/// The arguments of one `nativePassInput`, in declaration order: `x0` the `JNIEnv*`, `x1` the
/// `jclass`, then `(I F F I I I)`, which AAPCS64 places in `w2, s0, s1, w3, w4, w5`.
#[must_use]
pub fn pass_input_args(env: GuestAddr, class: u64, call: &PassInput) -> [GuestArg; 8] {
    // A `jint` in a `W` register; the callee reads the low half (`sxtw x1, w20` at
    // `0x02bbbad0`), and the upper half is what a compiler's own sign extension would leave.
    let int = |value: i32| GuestArg::Int(i64::from(value) as u64);
    [
        GuestArg::Pointer(env),
        GuestArg::Int(class),
        int(call.pointer_id),
        GuestArg::Float(call.x),
        GuestArg::Float(call.y),
        int(call.state),
        int(call.width),
        int(call.height),
    ]
}

/// Declare [`INPUT_CLASS`], memberless, if it is not declared yet -- the same shape as
/// [`super::script::declare_script_classes`]: a class a static native is called on, which the
/// host needs a `jclass` for and nothing looks members up on.
pub(super) fn declare_input_class(jni: &Jni) -> AbiResult<()> {
    jni.with_registry(|registry| {
        if registry.find(INPUT_CLASS).is_some() {
            return Ok(());
        }
        registry
            .declare(&super::classes::ClassSpec {
                name: INPUT_CLASS,
                tier: super::classes::Tier::Support,
                methods: &[],
                fields: &[],
            })
            .map(|_| ())
    })
}

/// **The embedding seam**: host window events in, `nativePassInput` calls out, on the thread the
/// embedding calls the lifecycle natives from.
///
/// Built once the engine is loaded, fed every [`WindowEvent`] the host window produces through
/// [`TouchInput::deliver`], and told when the surface lives and dies through
/// [`TouchInput::set_surface_alive`].
#[derive(Debug)]
pub struct TouchInput {
    finger: HostFinger,
    listener: TouchListener,
    /// `MainGameActivity.Y`, a `jk.o0`: `D.b()` in `vk.e.onTouch`. See
    /// [`TouchInput::set_surface_alive`].
    surface_alive: bool,
    /// The engine's `nativePassInput`.
    target: GuestAddr,
    /// One `jclass` for [`INPUT_CLASS`], taken once. The native never reads it (`x1` is not used
    /// before `0x2324654` clobbers it), and one per call would spend a reference per touch, which
    /// no frame ever frees here.
    class: u64,
    delivered: u64,
    held_back: u64,
}

impl TouchInput {
    /// A seam over the engine's `nativePassInput`, found through `resolve` -- the embedding's
    /// export table, as [`super::script::run`] takes it -- with coordinates divided by `density`.
    ///
    /// **The surface starts dead**: `jk.o0.a` is a `boolean` field, `false` until
    /// `MainGameActivity.surfaceCreated` sets it, so nothing is delivered until the embedding says
    /// the surface exists.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] for a density [`TouchListener::new`] refuses;
    /// [`AbiError::JniRefused`] naming [`PASS_INPUT_SYMBOL`] when `resolve` does not know it, and
    /// whatever declaring [`INPUT_CLASS`] or taking its `jclass` refuses for.
    pub fn new(
        jni: &Jni,
        resolve: &dyn Fn(&str) -> Option<GuestAddr>,
        density: f32,
    ) -> AbiResult<Self> {
        let listener = TouchListener::new(density)?;
        let Some(target) = resolve(PASS_INPUT_SYMBOL) else {
            return Err(AbiError::JniRefused {
                function: PASS_INPUT_SYMBOL.to_string(),
                address: 0,
                detail: format!(
                    "`{INPUT_CLASS}.{PASS_INPUT}{PASS_INPUT_DESCRIPTOR}` is exported by \
                     libroblox.so on a device (Section G tags it SHORT, `0x02bbba88`) and nothing \
                     resolved it here, so no touch can reach the engine"
                ),
            });
        };
        declare_input_class(jni)?;
        let class = jni.class_reference(INPUT_CLASS)?;
        Ok(Self {
            finger: HostFinger::default(),
            listener,
            surface_alive: false,
            target,
            class,
            delivered: 0,
            held_back: 0,
        })
    }

    /// Say whether the surface is alive: `true` once `onSurfaceCreatedNative` has returned, `false`
    /// before `onSurfaceDestroyedNative` is sent.
    ///
    /// That is the Java side's own order: `MainGameActivity.surfaceCreated` calls
    /// `super.surfaceCreated` (which is `onSurfaceCreatedNative`) and then `Y.a(!isDestroyed())`;
    /// `surfaceDestroyed` calls `Y.a(false)` and then `super.surfaceDestroyed`. `vk.e.onTouch`
    /// sends nothing while it is false, and does not keep what it did not send.
    pub fn set_surface_alive(&mut self, alive: bool) {
        self.surface_alive = alive;
    }

    /// Whether the surface is alive.
    #[must_use]
    pub fn surface_alive(&self) -> bool {
        self.surface_alive
    }

    /// The engine's `nativePassInput`, as resolved.
    #[must_use]
    pub fn target(&self) -> GuestAddr {
        self.target
    }

    /// Deliver one host window event: translate it ([`HostFinger`]), run it through `vk.e`'s
    /// model ([`TouchListener`]) over a view of `view` pixels, and make every call that produces,
    /// with `thread`'s `JNIEnv` on `cpu`. Returns the calls made.
    ///
    /// **The caller holds the activations** -- bionic, JNI and NDK published to this thread --
    /// exactly as it does around [`super::script::run`]: the native reaches imports.
    ///
    /// # Errors
    ///
    /// Whatever the listener refuses, and the first call that does not return: the guest's own
    /// failure, naming the import it died in. The event's remaining calls are not made.
    pub fn deliver(
        &mut self,
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        event: &WindowEvent,
        view: (u32, u32),
    ) -> AbiResult<Vec<PassInput>> {
        let mut made = Vec::new();
        for motion in self.finger.translate(event) {
            let touched = self.listener.on_touch(&motion, view, self.surface_alive)?;
            self.held_back += touched.held_back as u64;
            for call in touched.calls {
                let args = pass_input_args(jni.env_for(thread), self.class, &call);
                boundary.call_guest(
                    cpu,
                    "NativeInputInterface.nativePassInput (vk.e.onTouch)",
                    self.target,
                    &args,
                    PER_EVENT,
                )?;
                self.delivered += 1;
                made.push(call);
            }
        }
        Ok(made)
    }

    /// How many `nativePassInput` calls returned.
    #[must_use]
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// How many sends the dead surface held back -- and so dropped, as the Java side does.
    #[must_use]
    pub fn held_back(&self) -> u64 {
        self.held_back
    }

    /// Whether the host's finger is down.
    #[must_use]
    pub fn finger_down(&self) -> bool {
        self.finger.is_down()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 144 DPI, the host's 150% scale: density 1.5. **Not 1.0**, which is this host's own and at
    /// which pixels and density-independent pixels are the same number -- a listener that forgot
    /// to divide would pass every test written at 1.0.
    const DENSITY: f32 = 1.5;
    const VIEW: (u32, u32) = (1920, 1080);

    fn event(action: Action, x: f32, y: f32) -> MotionEvent {
        MotionEvent { action, pointer: PRIMARY_FINGER, pointers: vec![(PRIMARY_FINGER, x, y)] }
    }

    fn call(x: f32, y: f32, state: i32) -> PassInput {
        // 1920x1080 px at density 1.5 is 1280x720 dp.
        PassInput { pointer_id: PRIMARY_FINGER, x, y, state, width: 1280, height: 720 }
    }

    /// **The three states, in density-independent pixels, with the view in them too.** A press,
    /// a drag and a release are `0`, `1`, `2` at `px / density` -- the state values `vk.e$h.d`
    /// is handed at `0x020b`, `0x012e` and `0x01bd`, and the division at `0x01fe`.
    #[test]
    fn a_press_drag_and_release_are_began_moved_ended_in_dp() {
        let mut listener = TouchListener::new(DENSITY).expect("a density");
        let down = listener.on_touch(&event(Action::Down, 300.0, 150.0), VIEW, true).expect("down");
        assert_eq!(down.calls, vec![call(200.0, 100.0, STATE_BEGAN)]);
        let moved = listener.on_touch(&event(Action::Move, 450.0, 300.0), VIEW, true).expect("move");
        assert_eq!(moved.calls, vec![call(300.0, 200.0, STATE_MOVED)]);
        let up = listener.on_touch(&event(Action::Up, 450.0, 300.0), VIEW, true).expect("up");
        assert_eq!(up.calls, vec![call(300.0, 200.0, STATE_ENDED)]);
        assert_eq!(up.held_back, 0);
        // A cancel ends a touch exactly as an up does.
        listener.on_touch(&event(Action::Down, 30.0, 30.0), VIEW, true).expect("down");
        let cancelled =
            listener.on_touch(&event(Action::Cancel, 30.0, 30.0), VIEW, true).expect("cancel");
        assert_eq!(cancelled.calls, vec![call(20.0, 20.0, STATE_ENDED)]);
    }

    /// **The view is truncated to whole dp**, as Java's `float-to-int` does (`0x02c1`): 1921 px at
    /// 1.5 is 1280.67 dp and is sent as 1280, not rounded to 1281.
    #[test]
    fn the_view_is_truncated_to_whole_dp() {
        let mut listener = TouchListener::new(DENSITY).expect("a density");
        let down = listener
            .on_touch(&event(Action::Down, 300.0, 150.0), (1921, 1081), true)
            .expect("down");
        assert_eq!(
            down.calls.iter().map(|c| (c.width, c.height)).collect::<Vec<_>>(),
            vec![(1280, 720)]
        );
    }

    /// **An up is reported where the pointer last moved to, not where the up event says.**
    /// `0x01bc` sets the state and nothing else.
    #[test]
    fn an_up_keeps_the_position_the_pointer_last_moved_to() {
        let mut listener = TouchListener::new(DENSITY).expect("a density");
        listener.on_touch(&event(Action::Down, 150.0, 150.0), VIEW, true).expect("down");
        let up = listener.on_touch(&event(Action::Up, 900.0, 600.0), VIEW, true).expect("up");
        assert_eq!(up.calls, vec![call(100.0, 100.0, STATE_ENDED)]);
    }

    /// **Nothing is sent while the surface is dead, and nothing is kept for later.** `D.b()`
    /// false sends nothing (`0x02b3`); an up while it is false still forgets the pointer
    /// (`0x02f6`), so the engine never hears of a touch that began and ended then.
    #[test]
    fn a_dead_surface_sends_nothing_and_keeps_nothing() {
        let mut listener = TouchListener::new(DENSITY).expect("a density");
        let down = listener.on_touch(&event(Action::Down, 300.0, 150.0), VIEW, false).expect("down");
        assert!(down.calls.is_empty(), "sent while not ready: {:?}", down.calls);
        assert_eq!(down.held_back, 1);
        let up = listener.on_touch(&event(Action::Up, 300.0, 150.0), VIEW, false).expect("up");
        assert!(up.calls.is_empty(), "sent while not ready: {:?}", up.calls);
        // Alive now, and the pointer is gone: a move names no tracked pointer, so nothing is sent
        // -- a listener that queued the held-back sends, or kept the ended pointer, sends here.
        let after = listener.on_touch(&event(Action::Move, 600.0, 300.0), VIEW, true).expect("move");
        assert!(after.calls.is_empty(), "replayed or resent: {:?}", after.calls);
    }

    /// **A touch that began while the surface was dead reaches the engine as a move**, from the
    /// first event after it comes alive -- the Java side's own behaviour, not a replay.
    #[test]
    fn a_touch_that_began_before_the_surface_arrives_as_a_move() {
        let mut listener = TouchListener::new(DENSITY).expect("a density");
        listener.on_touch(&event(Action::Down, 300.0, 150.0), VIEW, false).expect("down");
        let moved = listener.on_touch(&event(Action::Move, 450.0, 300.0), VIEW, true).expect("move");
        assert_eq!(moved.calls, vec![call(300.0, 200.0, STATE_MOVED)]);
    }

    /// **A move that does not move is not sent**, and a first move after a press that stays put is
    /// not either (`0x0273`-`0x028f`); a move that does move is.
    #[test]
    fn a_move_that_does_not_move_is_not_sent() {
        let mut listener = TouchListener::new(DENSITY).expect("a density");
        listener.on_touch(&event(Action::Down, 300.0, 150.0), VIEW, true).expect("down");
        let still = listener.on_touch(&event(Action::Move, 300.0, 150.0), VIEW, true).expect("move");
        assert!(still.calls.is_empty(), "{:?}", still.calls);
        let still = listener.on_touch(&event(Action::Move, 300.0, 150.0), VIEW, true).expect("move");
        assert!(still.calls.is_empty(), "{:?}", still.calls);
        let moved = listener.on_touch(&event(Action::Move, 303.0, 150.0), VIEW, true).expect("move");
        assert_eq!(moved.calls, vec![call(202.0, 100.0, STATE_MOVED)]);
    }

    /// **A press at exactly the top-left corner is not sent until it moves or lifts** -- the one
    /// place `vk.e`'s dedup reaches a press: a fresh `vk.e$h` is all zeros, so a press at `(0, 0)`
    /// changes neither position nor state. Transcribed rather than smoothed over, because a
    /// listener that "always sends a press" is a different listener.
    #[test]
    fn a_press_at_the_origin_waits_for_its_first_change() {
        let mut listener = TouchListener::new(DENSITY).expect("a density");
        let down = listener.on_touch(&event(Action::Down, 0.0, 0.0), VIEW, true).expect("down");
        assert!(down.calls.is_empty(), "{:?}", down.calls);
        let up = listener.on_touch(&event(Action::Up, 0.0, 0.0), VIEW, true).expect("up");
        assert_eq!(up.calls, vec![call(0.0, 0.0, STATE_ENDED)]);
    }

    /// Two pointers: each is sent when it changes, in id order, and ending one leaves the other.
    #[test]
    fn a_second_pointer_is_its_own_record() {
        let mut listener = TouchListener::new(DENSITY).expect("a density");
        listener.on_touch(&event(Action::Down, 150.0, 150.0), VIEW, true).expect("down");
        let second = MotionEvent {
            action: Action::PointerDown,
            pointer: 1,
            pointers: vec![(0, 150.0, 150.0), (1, 600.0, 300.0)],
        };
        let touched = listener.on_touch(&second, VIEW, true).expect("pointer down");
        assert_eq!(
            touched.calls,
            vec![PassInput { pointer_id: 1, x: 400.0, y: 200.0, state: STATE_BEGAN, width: 1280, height: 720 }],
            "only the new pointer changed"
        );
        let lift = MotionEvent {
            action: Action::PointerUp,
            pointer: 0,
            pointers: vec![(0, 150.0, 150.0), (1, 600.0, 300.0)],
        };
        let touched = listener.on_touch(&lift, VIEW, true).expect("pointer up");
        assert_eq!(touched.calls, vec![call(100.0, 100.0, STATE_ENDED)]);
        let moved = MotionEvent { action: Action::Move, pointer: 1, pointers: vec![(1, 630.0, 300.0)] };
        let touched = listener.on_touch(&moved, VIEW, true).expect("move");
        assert_eq!(
            touched.calls,
            vec![PassInput { pointer_id: 1, x: 420.0, y: 200.0, state: STATE_MOVED, width: 1280, height: 720 }]
        );
    }

    /// A move missing a tracked pointer's position is refused rather than guessed at.
    #[test]
    fn a_move_without_a_tracked_pointer_is_refused() {
        let mut listener = TouchListener::new(DENSITY).expect("a density");
        listener.on_touch(&event(Action::Down, 150.0, 150.0), VIEW, true).expect("down");
        let wrong = MotionEvent { action: Action::Move, pointer: 1, pointers: vec![(1, 1.0, 1.0)] };
        let error = listener.on_touch(&wrong, VIEW, true).expect_err("pointer 0 has no position");
        assert!(error.to_string().contains("pointer 0"), "{error}");
    }

    #[test]
    fn a_density_that_is_not_a_positive_number_is_refused() {
        for density in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let error = TouchListener::new(density).expect_err("refused");
            assert_eq!(error.symbol(), Some("TouchListener::new"), "{error}");
        }
        assert!(TouchListener::new(1.0).is_ok());
    }

    /// **The primary button is one finger, and nothing else touches.**
    #[test]
    fn the_host_primary_button_is_one_finger_and_nothing_else_touches() {
        let mut finger = HostFinger::default();
        // A hover is not a touch, and neither is another button.
        assert!(finger.translate(&WindowEvent::PointerMoved { x: 5, y: 5 }).is_empty());
        for button in PointerButton::ALL.into_iter().filter(|b| *b != PointerButton::Primary) {
            assert!(finger.translate(&WindowEvent::PointerDown { button, x: 5, y: 5 }).is_empty());
            assert!(finger.translate(&WindowEvent::PointerUp { button, x: 5, y: 5 }).is_empty());
        }
        let key = WindowEvent::KeyDown { keycode: 0x41, scancode: 0x1E, repeat: false };
        assert!(finger.translate(&key).is_empty());

        let down = finger.translate(&WindowEvent::PointerDown {
            button: PointerButton::Primary,
            x: 10,
            y: 20,
        });
        assert_eq!(down, vec![event(Action::Down, 10.0, 20.0)]);
        assert!(finger.is_down());
        let moved = finger.translate(&WindowEvent::PointerMoved { x: 30, y: 40 });
        assert_eq!(moved, vec![event(Action::Move, 30.0, 40.0)]);
        // A release where the pointer is: just the up.
        let up = finger.translate(&WindowEvent::PointerUp { button: PointerButton::Primary, x: 30, y: 40 });
        assert_eq!(up, vec![event(Action::Up, 30.0, 40.0)]);
        assert!(!finger.is_down());
        // After the release, moving is hovering again.
        assert!(finger.translate(&WindowEvent::PointerMoved { x: 50, y: 50 }).is_empty());
    }

    /// **A release somewhere else is a move there, then the release**, so the engine sees the
    /// finger lift where it lifted.
    #[test]
    fn a_release_somewhere_else_moves_there_first() {
        let mut finger = HostFinger::default();
        finger.translate(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 10, y: 20 });
        let up = finger.translate(&WindowEvent::PointerUp { button: PointerButton::Primary, x: -5, y: 900 });
        assert_eq!(up, vec![event(Action::Move, -5.0, 900.0), event(Action::Up, -5.0, 900.0)]);
    }

    /// **Losing focus mid-press cancels the touch**; losing it with nothing pressed does nothing.
    #[test]
    fn losing_focus_mid_press_cancels_the_touch() {
        let mut finger = HostFinger::default();
        assert!(finger.translate(&WindowEvent::FocusChanged { focused: false }).is_empty());
        finger.translate(&WindowEvent::PointerDown { button: PointerButton::Primary, x: 10, y: 20 });
        let lost = finger.translate(&WindowEvent::FocusChanged { focused: false });
        assert_eq!(lost, vec![event(Action::Cancel, 10.0, 20.0)]);
        assert!(!finger.is_down());
        assert!(finger.translate(&WindowEvent::FocusChanged { focused: true }).is_empty());
    }

    /// The symbol is the short mangling of the class and the member, which is what Section G says
    /// the export is called.
    #[test]
    fn the_symbol_is_the_short_mangling() {
        assert_eq!(PASS_INPUT_SYMBOL, super::super::script::mangle(INPUT_CLASS, PASS_INPUT));
    }

    /// The argument list is `(env, class, id, x, y, state, width, height)` -- and so, by AAPCS64's
    /// separate counters, `x0 x1 w2 s0 s1 w3 w4 w5`. Asserted here on the list; the end-to-end
    /// version, through real translated code, is `tests/input.rs`.
    #[test]
    fn the_arguments_are_in_declaration_order() {
        let args = pass_input_args(0x1000, 0x2000, &PassInput {
            pointer_id: 7,
            x: 1.5,
            y: 2.5,
            state: STATE_MOVED,
            width: 1280,
            height: 720,
        });
        assert_eq!(
            args,
            [
                GuestArg::Pointer(0x1000),
                GuestArg::Int(0x2000),
                GuestArg::Int(7),
                GuestArg::Float(1.5),
                GuestArg::Float(2.5),
                GuestArg::Int(1),
                GuestArg::Int(1280),
                GuestArg::Int(720),
            ]
        );
    }
}
