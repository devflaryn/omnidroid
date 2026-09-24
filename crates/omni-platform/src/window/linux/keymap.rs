//! **Which physical key**: an X keycode as [`WindowEvent::KeyDown`](super::super::WindowEvent)'s
//! `scancode`.
//!
//! # What the seam's consumer decodes, and why this backend speaks Windows' number
//!
//! The field is documented as the host's own number for the physical key, and on Windows that is
//! the PC **set-1** make code, with `0xE000` for an `E0`-extended key. `omni-android`'s keyboard
//! path (`jni/keys.rs`, shared code with no `cfg`) decodes exactly that and nothing else: it turns
//! the set-1 code back into the Linux input code the engine's own table is indexed by. So this
//! backend, which *starts* from the Linux input code, has to hand over the set-1 code of the same
//! key -- and the relation is the one `keys.rs` inverts:
//!
//! * **Linux input codes 1-88 are the set-1 make codes of the same keys** (`KEY_ESC` 1 is make
//!   `0x01`, `KEY_A` 30 is `0x1E`, `KEY_F12` 88 is `0x58`), which is how `linux/input-event-codes.h`
//!   numbered them in the first place. Two of those are not keys a set-1 code can name here: 84 is
//!   no key, and 85 (`KEY_ZENKAKUHANKAKU`) is make `0x76`, not `0x55`.
//! * **Num Lock and Pause are crossed.** Windows reports Num Lock as the *extended* `0x45` and
//!   Pause as the plain `0x45` (the tail of Pause's `E1 1D 45` sequence), and `keys.rs` follows
//!   Windows. So `KEY_NUMLOCK` (69) becomes `0xE045` and `KEY_PAUSE` (119) becomes `0x45`.
//! * **The extended keys** -- right Ctrl and Alt, the six-key and arrow clusters, the keypad's `/`
//!   and Enter, Print Screen and the three Windows-key-row keys -- are the `E0` table below, which
//!   is `keys.rs`'s `EXTENDED` read the other way.
//!
//! Every other Linux input code is **0**, the seam's spelling of "the host gave no set-1 code":
//! those are exactly the keys `keys.rs` has no decoding for, so a guessed code would reach the same
//! `Unmapped` it reaches as 0, and a guess nobody can check is the thing VERIFICATION entry 7
//! warns against. `omni-android/tests/keys_linux.rs` holds the two sides together: every Linux
//! input code `keys.rs` can produce round-trips through [`scancode_from_evdev`] and back.
//!
//! # Where the Linux input code comes from
//!
//! An X keycode is a number the server's keymap assigns, and on every server that uses the
//! `evdev` keycodes -- Xorg with evdev or libinput, Xwayland, and Xvfb's default keymap -- it is
//! the Linux input code plus 8 (the X protocol reserves keycodes below 8). A server with another
//! keycode set (the old `xfree86` one) numbers keys differently, and there the backend reports 0
//! for every key rather than a code that names the wrong one; see [`evdev_of_keycode`].

/// The set-1 make code, with `0xE000` for an `E0`-extended key, of the key whose Linux input code
/// is `evdev` -- or 0 for a key with no code here. See this module's header for the table's
/// derivation and why everything else is 0.
#[must_use]
pub const fn scancode_from_evdev(evdev: u16) -> u32 {
    match evdev {
        // Crossed with Pause, as Windows reports them (see the header).
        69 => 0xE045,
        119 => 0x45,
        // The block where the two numberings are the same key.
        1..=83 | 86..=88 => evdev as u32,
        // The `E0` keys.
        96 => 0xE01C,  // KEY_KPENTER
        97 => 0xE01D,  // KEY_RIGHTCTRL
        98 => 0xE035,  // KEY_KPSLASH
        99 => 0xE037,  // KEY_SYSRQ (Print Screen)
        100 => 0xE038, // KEY_RIGHTALT
        102 => 0xE047, // KEY_HOME
        103 => 0xE048, // KEY_UP
        104 => 0xE049, // KEY_PAGEUP
        105 => 0xE04B, // KEY_LEFT
        106 => 0xE04D, // KEY_RIGHT
        107 => 0xE04F, // KEY_END
        108 => 0xE050, // KEY_DOWN
        109 => 0xE051, // KEY_PAGEDOWN
        110 => 0xE052, // KEY_INSERT
        111 => 0xE053, // KEY_DELETE
        125 => 0xE05B, // KEY_LEFTMETA
        126 => 0xE05C, // KEY_RIGHTMETA
        127 => 0xE05D, // KEY_COMPOSE (the Menu key)
        _ => 0,
    }
}

/// The Linux input code of X keycode `keycode`, when the server's keycodes are the `evdev` set
/// (`evdev_keycodes`), and `None` otherwise -- including for a keycode below the protocol's 8,
/// which no key has.
#[must_use]
pub const fn evdev_of_keycode(keycode: u32, evdev_keycodes: bool) -> Option<u16> {
    // The X protocol's keycodes are 8..=255.
    if !evdev_keycodes || keycode < 8 || keycode > 255 {
        return None;
    }
    Some((keycode - 8) as u16)
}

/// The seam's `scancode` for an X keycode: [`evdev_of_keycode`] then [`scancode_from_evdev`], 0
/// when either has no answer.
#[must_use]
pub const fn scancode_of_keycode(keycode: u32, evdev_keycodes: bool) -> u32 {
    match evdev_of_keycode(keycode, evdev_keycodes) {
        Some(evdev) => scancode_from_evdev(evdev),
        None => 0,
    }
}

/// Whether an XKB keycodes name (`xkb_keycodes { include "evdev+aliases(qwerty)" }` is named
/// `evdev+aliases(qwerty)`) is the `evdev` set, in which a keycode is the Linux input code plus 8.
#[must_use]
pub fn is_evdev_keycodes(name: &str) -> bool {
    name.starts_with("evdev")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot checks, one per rule of the table: the shared block, the two crossed keys, the `E0`
    /// block, and the codes that have no set-1 number here.
    #[test]
    fn the_scancode_is_the_set_1_code_of_the_same_physical_key() {
        for (evdev, scancode, key) in [
            (1, 0x01, "Esc"),
            (17, 0x11, "W"),
            (30, 0x1E, "A"),
            (29, 0x1D, "left Ctrl"),
            (57, 0x39, "Space"),
            (88, 0x58, "F12"),
            (86, 0x56, "102nd"),
            (69, 0xE045, "Num Lock"),
            (119, 0x45, "Pause"),
            (97, 0xE01D, "right Ctrl"),
            (100, 0xE038, "right Alt"),
            (96, 0xE01C, "keypad Enter"),
            (103, 0xE048, "Up"),
            (105, 0xE04B, "Left"),
            (106, 0xE04D, "Right"),
            (108, 0xE050, "Down"),
            (111, 0xE053, "Delete"),
            (127, 0xE05D, "Menu"),
        ] {
            assert_eq!(scancode_from_evdev(evdev), scancode, "{key} (evdev {evdev})");
        }
        for evdev in [0, 84, 85, 89, 101, 112, 113, 120, 128, 183, 240, 767, u16::MAX] {
            assert_eq!(scancode_from_evdev(evdev), 0, "evdev {evdev} has no set-1 code here");
        }
    }

    /// No two physical keys share a scancode: the table is injective on everything it names.
    #[test]
    fn no_two_keys_share_a_scancode() {
        let mut seen = std::collections::HashMap::new();
        for evdev in 0..=u16::MAX {
            let scancode = scancode_from_evdev(evdev);
            if scancode != 0 {
                if let Some(other) = seen.insert(scancode, evdev) {
                    panic!("evdev {other} and {evdev} both map to {scancode:#x}");
                }
            }
        }
    }

    /// An X keycode is the Linux input code plus 8, on an `evdev` keymap only.
    #[test]
    fn an_x_keycode_is_the_input_code_plus_eight_on_an_evdev_keymap() {
        assert_eq!(evdev_of_keycode(38, true), Some(30), "X keycode 38 is KEY_A");
        assert_eq!(scancode_of_keycode(38, true), 0x1E);
        assert_eq!(scancode_of_keycode(111, true), 0xE048, "X keycode 111 is Up");
        assert_eq!(scancode_of_keycode(105, true), 0xE01D, "X keycode 105 is right Ctrl");
        assert_eq!(scancode_of_keycode(38, false), 0, "not an evdev keymap: no claim");
        assert_eq!(evdev_of_keycode(7, true), None, "below the protocol's first keycode");
        assert_eq!(evdev_of_keycode(0, true), None, "an input method's committed text");
        assert!(is_evdev_keycodes("evdev+aliases(qwerty)"));
        assert!(!is_evdev_keycodes("xfree86+aliases(qwerty)"));
    }
}
