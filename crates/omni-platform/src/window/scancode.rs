//! **The physical key a [`super::WindowEvent::KeyDown`] names, as a Linux input code.**
//!
//! Every backend reports the physical key as a set-1 scancode (`0xE000` added for an
//! `E0`-extended key); a guest wants the Linux input code of the same key (`KEY_*`,
//! `linux/input-event-codes.h`) -- `KeyEvent.getScanCode()` on the HLE path, and the codes an
//! evdev keyboard reports on the real-AOSP path. The mapping is the PC keyboard's, fixed: the 88
//! unextended make codes are the same number on both sides (bar Pause), and the extended keys are
//! [`EXTENDED_SCANCODES`]. It moved here from `omni-android`'s `jni::keys`, which re-exports it
//! and whose tests still pin it key by key.

/// The `E0`-extended set-1 make codes and the Linux input code of the same key.
///
/// Every other extended code has no key here. Windows marks exactly these as extended: the right
/// Ctrl and Alt, the six-key and arrow clusters, Num Lock, Print Screen, the keypad's `/` and
/// Enter, and the three Windows-key-row keys.
pub const EXTENDED_SCANCODES: &[(u32, u16)] = &[
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

/// The Linux input code of the host key `scancode` names, as [`super::WindowEvent::KeyDown`] carries it,
/// or `None` for a key this has no code for -- zero (the host did not say), a make code past the
/// 88 that are the same number on both sides, an extended code outside [`EXTENDED_SCANCODES`], or stray
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
        0xE000 => EXTENDED_SCANCODES.iter().find(|(code, _)| *code == make).map(|&(_, evdev)| evdev),
        _ => None,
    }
}

