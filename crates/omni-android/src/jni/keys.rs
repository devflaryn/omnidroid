//! **Keyboard input**: the Java side's `vk.g`, run by the host, so that a key on the host's
//! keyboard reaches the engine the way a hardware keyboard's does on a device.
//!
//! # The path a device takes, decoded from `classes2.dex`
//!
//! * The `SurfaceView`'s `OnKeyListener` is `vk.e$c` (`vk.e.G`, `0x0005`). It consumes keys from
//!   gamepads, joysticks and D-pads and returns `false` for a keyboard's, so those go on to the
//!   activity.
//! * `MainGameActivity.onKeyDown` is `vk.g.a.e(event)` (`0x0007`), and `onKeyUp` is `vk.g.a.f`.
//!   Each passes the key on and consumes it when `vk.g.a(getKeyCode())`, and otherwise falls back
//!   to `GameActivity`'s own `onKeyDownNative`/`onKeyUpNative`.
//! * `vk.g.a(keyCode)` is true when **`vk.g.b` is set** and the key is not `BACK` (4),
//!   `VOLUME_DOWN` (25) or `VOLUME_UP` (24). `vk.g.b` is set by `vk.g.c` (`MainGameActivity.onCreate`
//!   `0x00dd`) and `vk.g.d` (`onConfigurationChanged`) exactly when `Configuration.keyboard ==
//!   KEYBOARD_QWERTY` (2) and `Configuration.hardKeyboardHidden == HARDKEYBOARDHIDDEN_NO` (1): a
//!   hardware keyboard, attached.
//! * What is passed is `NativeGLInterface.nativePassKeyEvent(down, getScanCode(), getKeyCode(),
//!   getRepeatCount() > 0)`, `(ZIIZ)V`, a static native the engine exports
//!   ([`PASS_KEY_EVENT_SYMBOL`], `0x02baebdc`).
//!
//! # What the native reads, decoded at `0x02baebdc`
//!
//! ```text
//! mov w0, w3 ; mov w19, w5 ; mov w20, w2   ; scan code, repeat, down
//! bl  0x2e4eca8   ; scan code -> USB HID usage: the 128-entry table at 0x6e6414, 0 past 0x7f
//! bl  0x2324654   ; the input singleton
//! bl  0x2e4ecc8   ; HID usage -> Roblox KeyCode: the table at 0x6e6b38 ('a' 97, 'w' 119, ' ' 32)
//! bl  0x2e4dff0   ; (input, down, usage, keycode, repeat)
//! ```
//!
//! **`w4`, the Android key code, is never read.** The engine keys everything on the **scan code**,
//! and the table at `0x6e6414` is indexed by Linux input code: index 30 (`KEY_A`) holds usage 4
//! (`a`), index 17 (`KEY_W`) holds 26 (`w`), 103 (`KEY_UP`) holds 82 (Up Arrow). So the host has to
//! say **which physical key**, as a Linux input code, and the window seam carries the host's own
//! number for that ([`WindowEvent::KeyDown`]'s `scancode`), not the layout's.
//!
//! # From the host's key to the guest's
//!
//! * **The scan code.** Windows reports the set-1 make code, with `0xE000` for an `E0`-extended
//!   key ([`WindowEvent::KeyDown`]). Linux's input codes 1-88 **are** the set-1 make codes of the
//!   same keys -- `KEY_ESC` 1 is make `0x01`, `KEY_A` 30 is `0x1E`, `KEY_F12` 88 is `0x58` -- and
//!   the extended keys are [`EXTENDED`]. `the_scan_codes_reach_the_usages_the_engine_expects` in
//!   `tests/gameactivity.rs` composes this with the engine's own table and checks the result
//!   against the USB HID usage of each key.
//! * **The key code.** What a device's `KeyEvent.getKeyCode()` answers for that input code on a
//!   keyboard with no layout file of its own: [`KEY_LAYOUT`], from AOSP `android13-release`'s
//!   `data/keyboards/Generic.kl`, the `AKEYCODE_*` numbers from `include/android/keycodes.h` of the
//!   same release (SDK 33, what the host tells the engine it is). The native does not read it;
//!   `vk.g.a` does.
//!
//! # A hardware keyboard is the embedding's statement, and both sides must hear it
//!
//! `vk.g` passes keys only on a device with a hardware keyboard, and the engine reads the same two
//! `Configuration` fields itself (`GetFieldID` `keyboard` and `hardKeyboardHidden`,
//! `jni-surface-lists.txt`). The layer's declared defaults are a phone's -- `KEYBOARD_NOKEYS`,
//! hidden -- so [`declare_hardware_keyboard`] is how an embedding says otherwise, **before the
//! engine reads its configuration**, and [`KeyInput::new`] refuses until it has: a Java side that
//! passed keys while the engine's `Configuration` said there was no keyboard would be a device
//! that does not exist.
//!
//! **Whose statement it is, measured statically:** the engine's own reads of those two fields land
//! only in AGDK's copy of the configuration (`gConfiguration` at `0x68376a8`, written by
//! `0x285c920`-`0x285c96c`), which nothing else in `libroblox.so` references. The engine learns of
//! a keyboard from its keys: `UserInputService`'s `LastInputType` setter (around `0x4779700`) turns
//! `KeyboardEnabled` and `MouseEnabled` on for a `Keyboard` input before it fires
//! `LastInputTypeChanged` (see [`super::mouse`]). So the declaration is `vk.g`'s gate, and the
//! first key is the engine's.
//!
//! # Keys held when the focus leaves
//!
//! Windows sends no key-up to a window that no longer has the focus, so a key held through an
//! `Alt+Tab` would stay down in the engine for good -- a character that never stops walking.
//! Android does not let that happen: when the focus leaves a window, its `InputDispatcher`
//! synthesises a cancelling `ACTION_UP` (`FLAG_CANCELED`) for every key still down, and
//! `MainGameActivity.onKeyUp` hands it to `vk.g.f` like any other release. [`KeyInput`] does the
//! same: it keeps the keys it has passed down, and a [`WindowEvent::FocusChanged`] to unfocused
//! releases each of them, repeat `false` (a cancel carries a repeat count of 0).

use std::sync::Arc;

use omni_cpu::GuestCpu;
use omni_mem::GuestAddr;
use omni_platform::window::WindowEvent;

use crate::boundary::{Boundary, GuestArg};
use crate::error::{AbiError, AbiResult};

use super::classes::Answer;
use super::Jni;

/// The class the key native is declared on.
pub const KEY_CLASS: &str = "com/roblox/engine/jni/NativeGLInterface";

/// The native a hardware key reaches.
pub const PASS_KEY_EVENT: &str = "nativePassKeyEvent";

/// Its descriptor: `(down, scanCode, keyCode, repeat)`.
pub const PASS_KEY_EVENT_DESCRIPTOR: &str = "(ZIIZ)V";

/// Its exported symbol, the short mangling of [`KEY_CLASS`] and [`PASS_KEY_EVENT`].
pub const PASS_KEY_EVENT_SYMBOL: &str =
    "Java_com_roblox_engine_jni_NativeGLInterface_nativePassKeyEvent";

/// `Configuration.KEYBOARD_QWERTY`: what `vk.g.c` requires of `Configuration.keyboard`.
pub const KEYBOARD_QWERTY: i32 = 2;

/// `Configuration.HARDKEYBOARDHIDDEN_NO`: what `vk.g.c` requires of `hardKeyboardHidden`.
pub const HARDKEYBOARDHIDDEN_NO: i32 = 1;

/// The class both fields are read from.
const CONFIGURATION: &str = "android/content/res/Configuration";

/// The key codes `vk.g.a` withholds: `BACK`, `VOLUME_DOWN`, `VOLUME_UP` (`0x0006`-`0x000f`).
pub const WITHHELD_KEY_CODES: [i32; 3] = [4, 25, 24];

/// The `E0`-extended set-1 make codes and the Linux input code of the same key.
///
/// Every other extended code has no key here. Windows marks exactly these as extended: the right
/// Ctrl and Alt, the six-key and arrow clusters, Num Lock, Print Screen, the keypad's `/` and
/// Enter, and the three Windows-key-row keys.
pub const EXTENDED: &[(u32, u16)] = &[
    (0x1C, 96),  // keypad Enter -> KEY_KPENTER
    (0x1D, 97),  // right Ctrl -> KEY_RIGHTCTRL
    (0x35, 98),  // keypad / -> KEY_KPSLASH
    (0x37, 99),  // Print Screen -> KEY_SYSRQ
    (0x38, 100), // right Alt -> KEY_RIGHTALT
    (0x45, 69),  // Num Lock -> KEY_NUMLOCK
    (0x47, 102), // Home -> KEY_HOME
    (0x48, 103), // Up -> KEY_UP
    (0x49, 104), // Page Up -> KEY_PAGEUP
    (0x4B, 105), // Left -> KEY_LEFT
    (0x4D, 106), // Right -> KEY_RIGHT
    (0x4F, 107), // End -> KEY_END
    (0x50, 108), // Down -> KEY_DOWN
    (0x51, 109), // Page Down -> KEY_PAGEDOWN
    (0x52, 110), // Insert -> KEY_INSERT
    (0x53, 111), // Delete -> KEY_DELETE
    (0x5B, 125), // left Windows -> KEY_LEFTMETA
    (0x5C, 126), // right Windows -> KEY_RIGHTMETA
    (0x5D, 127), // Menu -> KEY_COMPOSE
];

/// `Generic.kl` (AOSP `android13-release`), for every input code [`evdev_code`] produces:
/// `(Linux input code, AKEYCODE)`. Generated from that file and `keycodes.h` rather than typed.
pub const KEY_LAYOUT: &[(u16, i32)] = &[
    (1, 111),   // KEY_ESC -> ESCAPE
    (2, 8),     // KEY_1 -> 1
    (3, 9),     // KEY_2 -> 2
    (4, 10),    // KEY_3 -> 3
    (5, 11),    // KEY_4 -> 4
    (6, 12),    // KEY_5 -> 5
    (7, 13),    // KEY_6 -> 6
    (8, 14),    // KEY_7 -> 7
    (9, 15),    // KEY_8 -> 8
    (10, 16),   // KEY_9 -> 9
    (11, 7),    // KEY_0 -> 0
    (12, 69),   // KEY_MINUS -> MINUS
    (13, 70),   // KEY_EQUAL -> EQUALS
    (14, 67),   // KEY_BACKSPACE -> DEL
    (15, 61),   // KEY_TAB -> TAB
    (16, 45),   // KEY_Q -> Q
    (17, 51),   // KEY_W -> W
    (18, 33),   // KEY_E -> E
    (19, 46),   // KEY_R -> R
    (20, 48),   // KEY_T -> T
    (21, 53),   // KEY_Y -> Y
    (22, 49),   // KEY_U -> U
    (23, 37),   // KEY_I -> I
    (24, 43),   // KEY_O -> O
    (25, 44),   // KEY_P -> P
    (26, 71),   // KEY_LEFTBRACE -> LEFT_BRACKET
    (27, 72),   // KEY_RIGHTBRACE -> RIGHT_BRACKET
    (28, 66),   // KEY_ENTER -> ENTER
    (29, 113),  // KEY_LEFTCTRL -> CTRL_LEFT
    (30, 29),   // KEY_A -> A
    (31, 47),   // KEY_S -> S
    (32, 32),   // KEY_D -> D
    (33, 34),   // KEY_F -> F
    (34, 35),   // KEY_G -> G
    (35, 36),   // KEY_H -> H
    (36, 38),   // KEY_J -> J
    (37, 39),   // KEY_K -> K
    (38, 40),   // KEY_L -> L
    (39, 74),   // KEY_SEMICOLON -> SEMICOLON
    (40, 75),   // KEY_APOSTROPHE -> APOSTROPHE
    (41, 68),   // KEY_GRAVE -> GRAVE
    (42, 59),   // KEY_LEFTSHIFT -> SHIFT_LEFT
    (43, 73),   // KEY_BACKSLASH -> BACKSLASH
    (44, 54),   // KEY_Z -> Z
    (45, 52),   // KEY_X -> X
    (46, 31),   // KEY_C -> C
    (47, 50),   // KEY_V -> V
    (48, 30),   // KEY_B -> B
    (49, 42),   // KEY_N -> N
    (50, 41),   // KEY_M -> M
    (51, 55),   // KEY_COMMA -> COMMA
    (52, 56),   // KEY_DOT -> PERIOD
    (53, 76),   // KEY_SLASH -> SLASH
    (54, 60),   // KEY_RIGHTSHIFT -> SHIFT_RIGHT
    (55, 155),  // KEY_KPASTERISK -> NUMPAD_MULTIPLY
    (56, 57),   // KEY_LEFTALT -> ALT_LEFT
    (57, 62),   // KEY_SPACE -> SPACE
    (58, 115),  // KEY_CAPSLOCK -> CAPS_LOCK
    (59, 131),  // KEY_F1 -> F1
    (60, 132),  // KEY_F2 -> F2
    (61, 133),  // KEY_F3 -> F3
    (62, 134),  // KEY_F4 -> F4
    (63, 135),  // KEY_F5 -> F5
    (64, 136),  // KEY_F6 -> F6
    (65, 137),  // KEY_F7 -> F7
    (66, 138),  // KEY_F8 -> F8
    (67, 139),  // KEY_F9 -> F9
    (68, 140),  // KEY_F10 -> F10
    (69, 143),  // KEY_NUMLOCK -> NUM_LOCK
    (70, 116),  // KEY_SCROLLLOCK -> SCROLL_LOCK
    (71, 151),  // KEY_KP7 -> NUMPAD_7
    (72, 152),  // KEY_KP8 -> NUMPAD_8
    (73, 153),  // KEY_KP9 -> NUMPAD_9
    (74, 156),  // KEY_KPMINUS -> NUMPAD_SUBTRACT
    (75, 148),  // KEY_KP4 -> NUMPAD_4
    (76, 149),  // KEY_KP5 -> NUMPAD_5
    (77, 150),  // KEY_KP6 -> NUMPAD_6
    (78, 157),  // KEY_KPPLUS -> NUMPAD_ADD
    (79, 145),  // KEY_KP1 -> NUMPAD_1
    (80, 146),  // KEY_KP2 -> NUMPAD_2
    (81, 147),  // KEY_KP3 -> NUMPAD_3
    (82, 144),  // KEY_KP0 -> NUMPAD_0
    (83, 158),  // KEY_KPDOT -> NUMPAD_DOT
    (86, 73),   // KEY_102ND -> BACKSLASH
    (87, 141),  // KEY_F11 -> F11
    (88, 142),  // KEY_F12 -> F12
    (96, 160),  // KEY_KPENTER -> NUMPAD_ENTER
    (97, 114),  // KEY_RIGHTCTRL -> CTRL_RIGHT
    (98, 154),  // KEY_KPSLASH -> NUMPAD_DIVIDE
    (99, 120),  // KEY_SYSRQ -> SYSRQ
    (100, 58),  // KEY_RIGHTALT -> ALT_RIGHT
    (102, 122), // KEY_HOME -> MOVE_HOME
    (103, 19),  // KEY_UP -> DPAD_UP
    (104, 92),  // KEY_PAGEUP -> PAGE_UP
    (105, 21),  // KEY_LEFT -> DPAD_LEFT
    (106, 22),  // KEY_RIGHT -> DPAD_RIGHT
    (107, 123), // KEY_END -> MOVE_END
    (108, 20),  // KEY_DOWN -> DPAD_DOWN
    (109, 93),  // KEY_PAGEDOWN -> PAGE_DOWN
    (110, 124), // KEY_INSERT -> INSERT
    (111, 112), // KEY_DELETE -> FORWARD_DEL
    (119, 121), // KEY_PAUSE -> BREAK
    (125, 117), // KEY_LEFTMETA -> META_LEFT
    (126, 118), // KEY_RIGHTMETA -> META_RIGHT
    (127, 82),  // KEY_COMPOSE -> MENU
];

/// The Linux input code of the host key `scancode` names, as [`WindowEvent::KeyDown`] carries it,
/// or `None` for a key this has no code for -- zero (the host did not say), a make code past the
/// 88 that are the same number on both sides, an extended code outside [`EXTENDED`], or stray
/// high bits.
///
/// One exception inside the 88: Windows reports **Pause** as make `0x45` unextended -- Num Lock is
/// the extended `0x45` -- so that one is `KEY_PAUSE` (119) rather than `KEY_NUMLOCK`.
#[must_use]
pub fn evdev_code(scancode: u32) -> Option<u16> {
    let make = scancode & 0xFF;
    match scancode & !0xFF {
        0 => match make {
            0x45 => Some(119),
            // 84 is no key, and 85 is `KEY_ZENKAKUHANKAKU`, whose set-1 code is not 0x55.
            0x01..=0x53 | 0x56..=0x58 => Some(make as u16),
            _ => None,
        },
        0xE000 => EXTENDED.iter().find(|(code, _)| *code == make).map(|&(_, evdev)| evdev),
        _ => None,
    }
}

/// What a device's `KeyEvent.getKeyCode()` answers for Linux input code `evdev`, from
/// [`KEY_LAYOUT`].
#[must_use]
pub fn android_key_code(evdev: u16) -> Option<i32> {
    KEY_LAYOUT.iter().find(|(code, _)| *code == evdev).map(|&(_, key)| key)
}

/// `vk.g.a(keyCode)` with `vk.g.b` set: every key but the three [`WITHHELD_KEY_CODES`].
#[must_use]
pub fn passes(key_code: i32) -> bool {
    !WITHHELD_KEY_CODES.contains(&key_code)
}

/// One `nativePassKeyEvent` call, with the arguments `vk.g.e`/`vk.g.f` pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassKeyEvent {
    /// `true` from `onKeyDown`, `false` from `onKeyUp`.
    pub down: bool,
    /// `getScanCode()`: the Linux input code of the physical key.
    pub scan_code: i32,
    /// `getKeyCode()`: its `AKEYCODE_*`.
    pub key_code: i32,
    /// `getRepeatCount() > 0`: an auto-repeat, which an up never is.
    pub repeat: bool,
}

/// What one window event is, to the keyboard path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// A key the engine is passed.
    Pass(PassKeyEvent),
    /// A host key with no Linux input code here; the host scan code, for the report.
    Unmapped(u32),
    /// A key `vk.g.a` withholds, by its key code.
    Withheld(i32),
}

/// The keyboard path's reading of `event`: `None` for anything that is not a key.
#[must_use]
pub fn translate(event: &WindowEvent) -> Option<KeyOutcome> {
    let (down, scancode, repeat) = match *event {
        WindowEvent::KeyDown { scancode, repeat, .. } => (true, scancode, repeat),
        WindowEvent::KeyUp { scancode, .. } => (false, scancode, false),
        _ => return None,
    };
    let Some(evdev) = evdev_code(scancode) else {
        return Some(KeyOutcome::Unmapped(scancode));
    };
    let Some(key_code) = android_key_code(evdev) else {
        return Some(KeyOutcome::Unmapped(scancode));
    };
    if !passes(key_code) {
        return Some(KeyOutcome::Withheld(key_code));
    }
    Some(KeyOutcome::Pass(PassKeyEvent { down, scan_code: i32::from(evdev), key_code, repeat }))
}

/// The arguments of one `nativePassKeyEvent`: `x0` the `JNIEnv*`, `x1` the `jclass`, then
/// `(Z I I Z)` in `w2, w3, w4, w5`.
#[must_use]
pub fn pass_key_event_args(env: GuestAddr, class: u64, call: &PassKeyEvent) -> [GuestArg; 6] {
    let int = |value: i32| GuestArg::Int(i64::from(value) as u64);
    [
        GuestArg::Pointer(env),
        GuestArg::Int(class),
        GuestArg::Int(u64::from(call.down)),
        int(call.scan_code),
        int(call.key_code),
        GuestArg::Int(u64::from(call.repeat)),
    ]
}

/// **Say that the device has a hardware keyboard, attached** -- `Configuration.keyboard =
/// KEYBOARD_QWERTY`, `hardKeyboardHidden = HARDKEYBOARDHIDDEN_NO` -- so that the engine's own reads
/// of those fields and `vk.g`'s agree.
///
/// Call it before the engine reads its configuration (`initializeNativeCode`), and only for a host
/// that has one: it is a statement about the host, and the engine acts on it.
///
/// # Errors
///
/// Whatever [`Jni::define_field`] refuses for, naming the field.
pub fn declare_hardware_keyboard(jni: &Jni) -> AbiResult<()> {
    jni.define_field(CONFIGURATION, "keyboard", "I", false, Answer::Int(KEYBOARD_QWERTY))?;
    jni.define_field(
        CONFIGURATION,
        "hardKeyboardHidden",
        "I",
        false,
        Answer::Int(HARDKEYBOARDHIDDEN_NO),
    )
}

/// What this instance answers for the two fields `vk.g.c` reads, `(keyboard, hardKeyboardHidden)`.
fn declared_keyboard(jni: &Jni) -> (Option<Answer>, Option<Answer>) {
    jni.with_registry(|registry| {
        let Some(class) = registry.find(CONFIGURATION) else {
            return (None, None);
        };
        let answer = |name: &str| {
            registry
                .field(class, name, "I", false)
                .and_then(|field| registry.field_member(field))
                .map(|member| member.answer)
        };
        (answer("keyboard"), answer("hardKeyboardHidden"))
    })
}

/// **The embedding seam**: host key events in, `nativePassKeyEvent` calls out.
#[derive(Debug)]
pub struct KeyInput {
    target: GuestAddr,
    /// One `jclass` for [`KEY_CLASS`], taken once, for the reason `TouchInput`'s is.
    class: u64,
    /// The keys passed down and not yet up, oldest first, one entry per physical key.
    held: Vec<PassKeyEvent>,
    delivered: u64,
    unmapped: u64,
    withheld: u64,
    cancelled: u64,
}

impl KeyInput {
    /// A seam over the engine's `nativePassKeyEvent`, found through `resolve`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] until the embedding has called [`declare_hardware_keyboard`]: `vk.g`
    /// passes no key otherwise, and the engine's `Configuration` would say there is no keyboard.
    /// [`AbiError::JniRefused`] naming [`PASS_KEY_EVENT_SYMBOL`] when `resolve` does not know it.
    pub fn new(jni: &Jni, resolve: &dyn Fn(&str) -> Option<GuestAddr>) -> AbiResult<Self> {
        let declared = declared_keyboard(jni);
        if declared
            != (Some(Answer::Int(KEYBOARD_QWERTY)), Some(Answer::Int(HARDKEYBOARDHIDDEN_NO)))
        {
            return Err(AbiError::Refused {
                symbol: "KeyInput::new".to_string(),
                address: 0,
                why: format!(
                    "`vk.g` passes keys only with Configuration.keyboard == KEYBOARD_QWERTY (2) and \
                     hardKeyboardHidden == HARDKEYBOARDHIDDEN_NO (1), and this instance answers \
                     (keyboard, hardKeyboardHidden) = {declared:?}; call \
                     `declare_hardware_keyboard` before the engine reads its configuration"
                ),
            });
        }
        let Some(target) = resolve(PASS_KEY_EVENT_SYMBOL) else {
            return Err(AbiError::JniRefused {
                function: PASS_KEY_EVENT_SYMBOL.to_string(),
                address: 0,
                detail: format!(
                    "`{KEY_CLASS}.{PASS_KEY_EVENT}{PASS_KEY_EVENT_DESCRIPTOR}` is exported by \
                     libroblox.so on a device (`0x02baebdc`) and nothing resolved it here, so no \
                     key can reach the engine"
                ),
            });
        };
        jni.with_registry(|registry| {
            if registry.find(KEY_CLASS).is_some() {
                return Ok(());
            }
            registry
                .declare(&super::classes::ClassSpec {
                    name: KEY_CLASS,
                    tier: super::classes::Tier::Support,
                    methods: &[],
                    fields: &[],
                })
                .map(|_| ())
        })?;
        let class = jni.class_reference(KEY_CLASS)?;
        Ok(Self {
            target,
            class,
            held: Vec::new(),
            delivered: 0,
            unmapped: 0,
            withheld: 0,
            cancelled: 0,
        })
    }

    /// Deliver one host window event, with `thread`'s `JNIEnv` on `cpu`: the calls made -- one
    /// for a key the engine is passed, one release per held key when the window loses the focus
    /// (see this module's "Keys held when the focus leaves"), none otherwise. Anything else is
    /// counted or ignored.
    ///
    /// **The caller holds the activations**, as for `TouchInput::deliver`.
    ///
    /// # Errors
    ///
    /// The guest's own failure when a call does not return. The event's remaining calls are not
    /// made.
    pub fn deliver(
        &mut self,
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        event: &WindowEvent,
    ) -> AbiResult<Vec<PassKeyEvent>> {
        let calls = match (event, translate(event)) {
            (WindowEvent::FocusChanged { focused: false }, _) => {
                let released: Vec<PassKeyEvent> = self
                    .held
                    .iter()
                    .map(|key| PassKeyEvent { down: false, repeat: false, ..*key })
                    .collect();
                self.cancelled += released.len() as u64;
                released
            }
            (_, Some(KeyOutcome::Unmapped(_))) => {
                self.unmapped += 1;
                Vec::new()
            }
            (_, Some(KeyOutcome::Withheld(_))) => {
                self.withheld += 1;
                Vec::new()
            }
            (_, Some(KeyOutcome::Pass(call))) => vec![call],
            (_, None) => Vec::new(),
        };
        for call in &calls {
            let args = pass_key_event_args(jni.env_for(thread), self.class, call);
            boundary.call_guest(
                cpu,
                "NativeGLInterface.nativePassKeyEvent (vk.g)",
                self.target,
                &args,
                super::input::PER_EVENT,
            )?;
            self.delivered += 1;
            // Held from any down -- a repeat whose press came before the focus did included --
            // until its up.
            self.held.retain(|key| key.scan_code != call.scan_code);
            if call.down {
                self.held.push(*call);
            }
        }
        Ok(calls)
    }

    /// The keys passed down and not up since, oldest first.
    #[must_use]
    pub fn held(&self) -> &[PassKeyEvent] {
        &self.held
    }

    /// How many releases a lost focus has sent for keys that were held.
    #[must_use]
    pub fn cancelled(&self) -> u64 {
        self.cancelled
    }

    /// How many `nativePassKeyEvent` calls returned.
    #[must_use]
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// How many host keys had no Linux input code here.
    #[must_use]
    pub fn unmapped(&self) -> u64 {
        self.unmapped
    }

    /// How many keys `vk.g.a` withheld.
    #[must_use]
    pub fn withheld(&self) -> u64 {
        self.withheld
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_mem::GuestSpace;

    /// The main block is the same number on both sides; the extended keys are the table; Pause and
    /// Num Lock share a make code and are told apart by the flag.
    #[test]
    fn the_host_scan_code_is_the_linux_input_code_of_the_same_key() {
        for (scancode, evdev, key) in [
            (0x11, 17, "W"),
            (0x1E, 30, "A"),
            (0x1F, 31, "S"),
            (0x20, 32, "D"),
            (0x39, 57, "Space"),
            (0x2A, 42, "left Shift"),
            (0x1D, 29, "left Ctrl"),
            (0x01, 1, "Esc"),
            (0x58, 88, "F12"),
            (0xE01D, 97, "right Ctrl"),
            (0xE048, 103, "Up"),
            (0xE04B, 105, "Left"),
            (0xE04D, 106, "Right"),
            (0xE050, 108, "Down"),
            (0xE045, 69, "Num Lock"),
            (0x45, 119, "Pause"),
        ] {
            assert_eq!(evdev_code(scancode), Some(evdev), "{key} ({scancode:#x})");
        }
        for scancode in [0, 0x54, 0x55, 0x59, 0x7F, 0xE099, 0xE000, 0x1_001E, 0xE11D] {
            assert_eq!(evdev_code(scancode), None, "{scancode:#x} is no key here");
        }
    }

    /// Spot checks against `Generic.kl`, and the coverage that matters: every code the host side
    /// can produce has a key code.
    #[test]
    fn every_input_code_the_host_produces_has_a_key_code() {
        assert_eq!(android_key_code(30), Some(29), "KEY_A -> AKEYCODE_A");
        assert_eq!(android_key_code(17), Some(51), "KEY_W -> AKEYCODE_W");
        assert_eq!(android_key_code(57), Some(62), "KEY_SPACE -> AKEYCODE_SPACE");
        assert_eq!(android_key_code(103), Some(19), "KEY_UP -> AKEYCODE_DPAD_UP");
        assert_eq!(android_key_code(1), Some(111), "KEY_ESC -> AKEYCODE_ESCAPE");
        let scancodes = (0..=0xFFu32).chain((0..=0xFFu32).map(|make| 0xE000 | make));
        for scancode in scancodes {
            if let Some(evdev) = evdev_code(scancode) {
                assert!(android_key_code(evdev).is_some(), "{scancode:#x} -> {evdev} has no key code");
            }
        }
    }

    /// A press, a repeat and a release of `W`: `(down, KEY_W, AKEYCODE_W, repeat)`.
    #[test]
    fn a_key_is_its_input_code_its_key_code_and_its_direction() {
        let press = WindowEvent::KeyDown { keycode: 0x57, scancode: 0x11, repeat: false };
        let repeat = WindowEvent::KeyDown { keycode: 0x57, scancode: 0x11, repeat: true };
        let release = WindowEvent::KeyUp { keycode: 0x57, scancode: 0x11 };
        let key = |down, repeat| {
            Some(KeyOutcome::Pass(PassKeyEvent { down, scan_code: 17, key_code: 51, repeat }))
        };
        assert_eq!(translate(&press), key(true, false));
        assert_eq!(translate(&repeat), key(true, true));
        assert_eq!(translate(&release), key(false, false));
        // The virtual-key code is the layout's and is not what decides the key: the same physical
        // key reported with AZERTY's `Z` is still `KEY_W`.
        let azerty = WindowEvent::KeyDown { keycode: 0x5A, scancode: 0x11, repeat: false };
        assert_eq!(translate(&azerty), key(true, false));
        // Not a key, and a key the host did not name.
        assert_eq!(translate(&WindowEvent::FocusChanged { focused: true }), None);
        let unnamed = WindowEvent::KeyDown { keycode: 0x41, scancode: 0, repeat: false };
        assert_eq!(translate(&unnamed), Some(KeyOutcome::Unmapped(0)));
    }

    /// `vk.g.a` withholds exactly BACK and the two volume keys.
    #[test]
    fn back_and_volume_are_withheld_and_nothing_else_is() {
        for key_code in [4, 24, 25] {
            assert!(!passes(key_code), "{key_code}");
        }
        for &(_, key_code) in KEY_LAYOUT {
            assert!(passes(key_code), "{key_code} is on a keyboard and must pass");
        }
    }

    /// The arguments are `(env, class, down, scan, key, repeat)` in declaration order.
    #[test]
    fn the_arguments_are_in_declaration_order() {
        let call = PassKeyEvent { down: true, scan_code: 17, key_code: 51, repeat: false };
        assert_eq!(
            pass_key_event_args(0x1000, 0x2000, &call),
            [
                GuestArg::Pointer(0x1000),
                GuestArg::Int(0x2000),
                GuestArg::Int(1),
                GuestArg::Int(17),
                GuestArg::Int(51),
                GuestArg::Int(0),
            ]
        );
    }

    #[test]
    fn the_symbol_is_the_short_mangling() {
        assert_eq!(PASS_KEY_EVENT_SYMBOL, super::super::script::mangle(KEY_CLASS, PASS_KEY_EVENT));
    }

    /// **No keyboard declared, no seam**: the layer's defaults are a phone's, and a Java side
    /// passing keys the engine's `Configuration` says cannot exist is refused by name. Declared,
    /// it is built.
    #[test]
    fn the_seam_is_refused_until_a_hardware_keyboard_is_declared() {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let jni = Jni::new(space).expect("a JNI instance");
        let resolve = |symbol: &str| (symbol == PASS_KEY_EVENT_SYMBOL).then_some(0x1000);
        let error = KeyInput::new(&jni, &resolve).expect_err("no keyboard declared");
        assert!(error.to_string().contains("declare_hardware_keyboard"), "{error}");
        declare_hardware_keyboard(&jni).expect("the fields are declared");
        assert!(KeyInput::new(&jni, &resolve).is_ok());
        let error = KeyInput::new(&jni, &|_| None).expect_err("nothing exports it");
        assert!(error.to_string().contains(PASS_KEY_EVENT_SYMBOL), "{error}");
    }
}
