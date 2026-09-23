//! The physical key a macOS key event names, as the seam's `scancode` carries it.
//!
//! # What the seam asks for, and why macOS has to derive it
//!
//! [`WindowEvent::KeyDown`](crate::window::WindowEvent::KeyDown) carries two numbers: `keycode`,
//! the host's own key number, and `scancode`, the **physical** key. Windows hands the second over
//! for free -- the PC set-1 make code is in bits 16-23 of the message's `LPARAM` -- and the one
//! consumer, `omni_android::jni::keys::evdev_code`, reads it as exactly that: a set-1 make code,
//! with `0xE000` added for an `E0`-prefixed key, which it turns into a Linux input code because
//! Linux codes 1-88 *are* the set-1 make codes.
//!
//! macOS has no set-1 code anywhere. `-[NSEvent keyCode]` is a *virtual key code*, `kVK_*` from
//! HIToolbox's `Events.h` -- but, unlike a Win32 virtual-key code, it names a **position**, not a
//! character: `kVK_ANSI_Q` is the key QWERTY calls `Q` whatever the layout types with it, which
//! is why `Events.h` calls these "independent of the keyboard layout". So the physical key is
//! known exactly, and what is missing is only its spelling in the seam's unit.
//!
//! `keycode` therefore carries the `kVK_*` number (the host's own, raw, as on Windows), and
//! `scancode` is derived in two steps, each a table checked against its source:
//!
//! 1. [`KEYS`]: `kVK_*` -> the Linux input code (`KEY_*`) of the same physical key. The `kVK`
//!    numbers are `Events.h` from the macOS SDK (`HIToolbox.framework/Headers/Events.h`); the
//!    `KEY_*` numbers are Linux `include/uapi/linux/input-event-codes.h`. Where a Mac key has no
//!    PC counterpart of the same name, the row follows the **USB HID usage** the Mac keyboard
//!    sends, which is the number both sides already agree on: `kVK_Help` is HID usage `0x49`
//!    (the Insert position; Linux `hid-input` maps it to `KEY_INSERT`), `kVK_ANSI_KeypadClear` is
//!    `0x53` (Num Lock/Clear, `KEY_NUMLOCK`), `kVK_ISO_Section` is `0x64` (Non-US `\|`,
//!    `KEY_102ND`), `kVK_ContextualMenu` is `0x65` (Application, `KEY_COMPOSE`).
//! 2. [`set1_of_linux`]: the Linux code -> the set-1 code, the **inverse of `evdev_code`** and
//!    of nothing else, so that `evdev_code(scancode) == Some(linux)` holds for every key that
//!    has a set-1 code the consumer reads. `tests/window_keys_macos.rs` asserts exactly that, key
//!    by key, against the consumer itself (VERIFICATION entries 1 and 7: membership, and no
//!    second implementation as the oracle).
//!
//! A key whose Linux code `evdev_code` cannot produce -- F13-F20, the volume keys, keypad `=`,
//! the JIS keys, Fn -- carries `scancode` **0**, the seam's "the host did not say". That is a
//! statement about the consumer's domain, not a guess: the same test proves, for each of those
//! rows, that **no** 16-bit set-1 code maps to that Linux code, so there is nothing truer to put
//! there.

/// `(kVK_*, Linux KEY_*)` for every key `Events.h` names. See the module header for the sources.
///
/// Sorted by `kVK` so that a duplicate or a gap is visible when read.
pub const KEYS: &[(u16, u16)] = &[
    (0x00, 30),  // kVK_ANSI_A -> KEY_A
    (0x01, 31),  // kVK_ANSI_S -> KEY_S
    (0x02, 32),  // kVK_ANSI_D -> KEY_D
    (0x03, 33),  // kVK_ANSI_F -> KEY_F
    (0x04, 35),  // kVK_ANSI_H -> KEY_H
    (0x05, 34),  // kVK_ANSI_G -> KEY_G
    (0x06, 44),  // kVK_ANSI_Z -> KEY_Z
    (0x07, 45),  // kVK_ANSI_X -> KEY_X
    (0x08, 46),  // kVK_ANSI_C -> KEY_C
    (0x09, 47),  // kVK_ANSI_V -> KEY_V
    (0x0A, 86),  // kVK_ISO_Section -> KEY_102ND (HID 0x64)
    (0x0B, 48),  // kVK_ANSI_B -> KEY_B
    (0x0C, 16),  // kVK_ANSI_Q -> KEY_Q
    (0x0D, 17),  // kVK_ANSI_W -> KEY_W
    (0x0E, 18),  // kVK_ANSI_E -> KEY_E
    (0x0F, 19),  // kVK_ANSI_R -> KEY_R
    (0x10, 21),  // kVK_ANSI_Y -> KEY_Y
    (0x11, 20),  // kVK_ANSI_T -> KEY_T
    (0x12, 2),   // kVK_ANSI_1 -> KEY_1
    (0x13, 3),   // kVK_ANSI_2 -> KEY_2
    (0x14, 4),   // kVK_ANSI_3 -> KEY_3
    (0x15, 5),   // kVK_ANSI_4 -> KEY_4
    (0x16, 7),   // kVK_ANSI_6 -> KEY_6
    (0x17, 6),   // kVK_ANSI_5 -> KEY_5
    (0x18, 13),  // kVK_ANSI_Equal -> KEY_EQUAL
    (0x19, 10),  // kVK_ANSI_9 -> KEY_9
    (0x1A, 8),   // kVK_ANSI_7 -> KEY_7
    (0x1B, 12),  // kVK_ANSI_Minus -> KEY_MINUS
    (0x1C, 9),   // kVK_ANSI_8 -> KEY_8
    (0x1D, 11),  // kVK_ANSI_0 -> KEY_0
    (0x1E, 27),  // kVK_ANSI_RightBracket -> KEY_RIGHTBRACE
    (0x1F, 24),  // kVK_ANSI_O -> KEY_O
    (0x20, 22),  // kVK_ANSI_U -> KEY_U
    (0x21, 26),  // kVK_ANSI_LeftBracket -> KEY_LEFTBRACE
    (0x22, 23),  // kVK_ANSI_I -> KEY_I
    (0x23, 25),  // kVK_ANSI_P -> KEY_P
    (0x24, 28),  // kVK_Return -> KEY_ENTER
    (0x25, 38),  // kVK_ANSI_L -> KEY_L
    (0x26, 36),  // kVK_ANSI_J -> KEY_J
    (0x27, 40),  // kVK_ANSI_Quote -> KEY_APOSTROPHE
    (0x28, 37),  // kVK_ANSI_K -> KEY_K
    (0x29, 39),  // kVK_ANSI_Semicolon -> KEY_SEMICOLON
    (0x2A, 43),  // kVK_ANSI_Backslash -> KEY_BACKSLASH
    (0x2B, 51),  // kVK_ANSI_Comma -> KEY_COMMA
    (0x2C, 53),  // kVK_ANSI_Slash -> KEY_SLASH
    (0x2D, 49),  // kVK_ANSI_N -> KEY_N
    (0x2E, 50),  // kVK_ANSI_M -> KEY_M
    (0x2F, 52),  // kVK_ANSI_Period -> KEY_DOT
    (0x30, 15),  // kVK_Tab -> KEY_TAB
    (0x31, 57),  // kVK_Space -> KEY_SPACE
    (0x32, 41),  // kVK_ANSI_Grave -> KEY_GRAVE
    (0x33, 14),  // kVK_Delete (the backspace key) -> KEY_BACKSPACE
    (0x35, 1),   // kVK_Escape -> KEY_ESC
    (0x36, 126), // kVK_RightCommand -> KEY_RIGHTMETA
    (0x37, 125), // kVK_Command -> KEY_LEFTMETA
    (0x38, 42),  // kVK_Shift -> KEY_LEFTSHIFT
    (0x39, 58),  // kVK_CapsLock -> KEY_CAPSLOCK
    (0x3A, 56),  // kVK_Option -> KEY_LEFTALT
    (0x3B, 29),  // kVK_Control -> KEY_LEFTCTRL
    (0x3C, 54),  // kVK_RightShift -> KEY_RIGHTSHIFT
    (0x3D, 100), // kVK_RightOption -> KEY_RIGHTALT
    (0x3E, 97),  // kVK_RightControl -> KEY_RIGHTCTRL
    (0x3F, 464), // kVK_Function -> KEY_FN
    (0x40, 187), // kVK_F17 -> KEY_F17
    (0x41, 83),  // kVK_ANSI_KeypadDecimal -> KEY_KPDOT
    (0x43, 55),  // kVK_ANSI_KeypadMultiply -> KEY_KPASTERISK
    (0x45, 78),  // kVK_ANSI_KeypadPlus -> KEY_KPPLUS
    (0x47, 69),  // kVK_ANSI_KeypadClear -> KEY_NUMLOCK (HID 0x53)
    (0x48, 115), // kVK_VolumeUp -> KEY_VOLUMEUP
    (0x49, 114), // kVK_VolumeDown -> KEY_VOLUMEDOWN
    (0x4A, 113), // kVK_Mute -> KEY_MUTE
    (0x4B, 98),  // kVK_ANSI_KeypadDivide -> KEY_KPSLASH
    (0x4C, 96),  // kVK_ANSI_KeypadEnter -> KEY_KPENTER
    (0x4E, 74),  // kVK_ANSI_KeypadMinus -> KEY_KPMINUS
    (0x4F, 188), // kVK_F18 -> KEY_F18
    (0x50, 189), // kVK_F19 -> KEY_F19
    (0x51, 117), // kVK_ANSI_KeypadEquals -> KEY_KPEQUAL
    (0x52, 82),  // kVK_ANSI_Keypad0 -> KEY_KP0
    (0x53, 79),  // kVK_ANSI_Keypad1 -> KEY_KP1
    (0x54, 80),  // kVK_ANSI_Keypad2 -> KEY_KP2
    (0x55, 81),  // kVK_ANSI_Keypad3 -> KEY_KP3
    (0x56, 75),  // kVK_ANSI_Keypad4 -> KEY_KP4
    (0x57, 76),  // kVK_ANSI_Keypad5 -> KEY_KP5
    (0x58, 77),  // kVK_ANSI_Keypad6 -> KEY_KP6
    (0x59, 71),  // kVK_ANSI_Keypad7 -> KEY_KP7
    (0x5A, 190), // kVK_F20 -> KEY_F20
    (0x5B, 72),  // kVK_ANSI_Keypad8 -> KEY_KP8
    (0x5C, 73),  // kVK_ANSI_Keypad9 -> KEY_KP9
    (0x5D, 124), // kVK_JIS_Yen -> KEY_YEN (HID 0x89)
    (0x5E, 89),  // kVK_JIS_Underscore -> KEY_RO (HID 0x87)
    (0x5F, 121), // kVK_JIS_KeypadComma -> KEY_KPCOMMA (HID 0x85)
    (0x60, 63),  // kVK_F5 -> KEY_F5
    (0x61, 64),  // kVK_F6 -> KEY_F6
    (0x62, 65),  // kVK_F7 -> KEY_F7
    (0x63, 61),  // kVK_F3 -> KEY_F3
    (0x64, 66),  // kVK_F8 -> KEY_F8
    (0x65, 67),  // kVK_F9 -> KEY_F9
    (0x66, 123), // kVK_JIS_Eisu -> KEY_HANJA (HID 0x91, LANG2)
    (0x67, 87),  // kVK_F11 -> KEY_F11
    (0x68, 122), // kVK_JIS_Kana -> KEY_HANGEUL (HID 0x90, LANG1)
    (0x69, 183), // kVK_F13 -> KEY_F13
    (0x6A, 186), // kVK_F16 -> KEY_F16
    (0x6B, 184), // kVK_F14 -> KEY_F14
    (0x6D, 68),  // kVK_F10 -> KEY_F10
    (0x6E, 127), // kVK_ContextualMenu -> KEY_COMPOSE (HID 0x65)
    (0x6F, 88),  // kVK_F12 -> KEY_F12
    (0x71, 185), // kVK_F15 -> KEY_F15
    (0x72, 110), // kVK_Help -> KEY_INSERT (HID 0x49)
    (0x73, 102), // kVK_Home -> KEY_HOME
    (0x74, 104), // kVK_PageUp -> KEY_PAGEUP
    (0x75, 111), // kVK_ForwardDelete -> KEY_DELETE
    (0x76, 62),  // kVK_F4 -> KEY_F4
    (0x77, 107), // kVK_End -> KEY_END
    (0x78, 60),  // kVK_F2 -> KEY_F2
    (0x79, 109), // kVK_PageDown -> KEY_PAGEDOWN
    (0x7A, 59),  // kVK_F1 -> KEY_F1
    (0x7B, 105), // kVK_LeftArrow -> KEY_LEFT
    (0x7C, 106), // kVK_RightArrow -> KEY_RIGHT
    (0x7D, 108), // kVK_DownArrow -> KEY_DOWN
    (0x7E, 103), // kVK_UpArrow -> KEY_UP
];

/// The `E0`-extended set-1 make codes the consumer reads, and the Linux code of each: the
/// inverse of `omni_android::jni::keys::EXTENDED`, restated here because this crate cannot depend
/// on that one (the dependency runs the other way). `tests/window_keys_macos.rs` is what holds
/// the two together.
const EXTENDED: &[(u16, u32)] = &[
    (96, 0xE01C),  // KEY_KPENTER
    (97, 0xE01D),  // KEY_RIGHTCTRL
    (98, 0xE035),  // KEY_KPSLASH
    (99, 0xE037),  // KEY_SYSRQ
    (100, 0xE038), // KEY_RIGHTALT
    (69, 0xE045),  // KEY_NUMLOCK -- extended, as Windows reports it; unextended 0x45 is Pause
    (102, 0xE047), // KEY_HOME
    (103, 0xE048), // KEY_UP
    (104, 0xE049), // KEY_PAGEUP
    (105, 0xE04B), // KEY_LEFT
    (106, 0xE04D), // KEY_RIGHT
    (107, 0xE04F), // KEY_END
    (108, 0xE050), // KEY_DOWN
    (109, 0xE051), // KEY_PAGEDOWN
    (110, 0xE052), // KEY_INSERT
    (111, 0xE053), // KEY_DELETE
    (125, 0xE05B), // KEY_LEFTMETA
    (126, 0xE05C), // KEY_RIGHTMETA
    (127, 0xE05D), // KEY_COMPOSE
];

/// The set-1 code `evdev_code` turns into Linux code `linux`, or `None` when it turns none into
/// it.
///
/// Linux codes 1-83 and 86-88 are their own set-1 make codes (84 is no key, 85 is
/// `KEY_ZENKAKUHANKAKU` whose set-1 code is not 0x55, 69 is Num Lock, which Windows -- and so the
/// consumer -- reports extended); 119 (`KEY_PAUSE`) is unextended `0x45`; the rest is [`EXTENDED`].
#[must_use]
pub fn set1_of_linux(linux: u16) -> Option<u32> {
    match linux {
        69 => Some(0xE045),
        119 => Some(0x45),
        1..=83 | 86..=88 => Some(u32::from(linux)),
        _ => EXTENDED.iter().find(|(code, _)| *code == linux).map(|&(_, set1)| set1),
    }
}

/// The Linux code of the physical key `kvk` names, from [`KEYS`].
#[must_use]
pub fn linux_of_kvk(kvk: u16) -> Option<u16> {
    KEYS.iter().find(|(key, _)| *key == kvk).map(|&(_, linux)| linux)
}

/// What `WindowEvent::KeyDown.scancode` carries for virtual key `kvk`: the set-1 code of its
/// physical key, or 0 when there is none the consumer reads (see the module header).
#[must_use]
pub fn scancode_of(kvk: u16) -> u32 {
    linux_of_kvk(kvk).and_then(set1_of_linux).unwrap_or(0)
}

/// The **device-dependent** modifier bit (`NX_DEVICE*KEYMASK`, `IOKit/hidsystem/IOLLEvent.h`)
/// that says whether modifier key `kvk` is down, in `-[NSEvent modifierFlags]`'s low word.
///
/// A modifier is reported by `flagsChanged:`, not `keyDown:`, and the event does not say whether
/// the key went down or up -- only which key changed and what the flags are now. The
/// device-independent flags (`NSEventModifierFlagShift`) cannot answer it either, because they
/// stay set while *either* Shift is held. The device-dependent bits are one per physical key.
///
/// `None` for Caps Lock, whose flag is the lock's state rather than the key's (see
/// `appkit::flags_changed`), for Fn, whose one flag bit is also set by the arrow keys, and for
/// anything that is not a modifier.
#[must_use]
pub const fn modifier_bit(kvk: u16) -> Option<u64> {
    match kvk {
        0x3B => Some(0x0000_0001), // kVK_Control: NX_DEVICELCTLKEYMASK
        0x38 => Some(0x0000_0002), // kVK_Shift: NX_DEVICELSHIFTKEYMASK
        0x3C => Some(0x0000_0004), // kVK_RightShift: NX_DEVICERSHIFTKEYMASK
        0x37 => Some(0x0000_0008), // kVK_Command: NX_DEVICELCMDKEYMASK
        0x36 => Some(0x0000_0010), // kVK_RightCommand: NX_DEVICERCMDKEYMASK
        0x3A => Some(0x0000_0020), // kVK_Option: NX_DEVICELALTKEYMASK
        0x3D => Some(0x0000_0040), // kVK_RightOption: NX_DEVICERALTKEYMASK
        0x3E => Some(0x0000_2000), // kVK_RightControl: NX_DEVICERCTLKEYMASK
        _ => None,
    }
}

/// `kVK_CapsLock`.
pub const KVK_CAPS_LOCK: u16 = 0x39;
/// `kVK_Function`.
pub const KVK_FUNCTION: u16 = 0x3F;
/// `NSEventModifierFlagFunction` (`1 << 23`, `AppKit/NSEvent.h`).
pub const FLAG_FUNCTION: u64 = 1 << 23;

#[cfg(test)]
mod tests {
    use super::*;

    /// No `kVK` twice and no Linux code twice: two rows naming one physical key would make one of
    /// them a key that can never be told apart from the other.
    #[test]
    fn every_virtual_key_and_every_linux_code_appears_once() {
        for (i, (kvk, linux)) in KEYS.iter().enumerate() {
            for (other_kvk, other_linux) in &KEYS[i + 1..] {
                assert_ne!(kvk, other_kvk, "kVK {kvk:#04x} is listed twice");
                assert_ne!(linux, other_linux, "Linux code {linux} is listed for {kvk:#04x} and {other_kvk:#04x}");
            }
        }
        assert!(KEYS.windows(2).all(|pair| pair[0].0 < pair[1].0), "KEYS is sorted by kVK");
    }

    /// Spot rows whose reason is not obvious, each pinned to its number so that a row edited
    /// by accident fails here as well as in the consumer test.
    #[test]
    fn the_rows_that_follow_the_hid_usage_rather_than_a_name() {
        assert_eq!(scancode_of(0x0C), 0x10, "kVK_ANSI_Q is set-1 0x10, KEY_Q");
        assert_eq!(scancode_of(0x72), 0xE052, "kVK_Help sits where Insert is");
        assert_eq!(scancode_of(0x47), 0xE045, "keypad Clear is the Num Lock position");
        assert_eq!(scancode_of(0x0A), 0x56, "kVK_ISO_Section is KEY_102ND");
        assert_eq!(scancode_of(0x3E), 0xE01D, "right Control is extended");
        assert_eq!(scancode_of(0x3F), 0, "Fn has no set-1 code the consumer reads");
        assert_eq!(scancode_of(0x34), 0, "0x34 is no key in Events.h");
    }
}
