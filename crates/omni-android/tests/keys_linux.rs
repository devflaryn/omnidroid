//! **The Linux window backend's scancodes are the ones `jni::keys` decodes**, for every key it
//! decodes.
//!
//! `keys.rs` is shared code with no `cfg`: it reads [`WindowEvent::KeyDown`]'s `scancode` as the
//! Windows backend fills it -- the set-1 make code, `0xE000` for an extended key -- and turns it
//! into the Linux input code the engine's table is indexed by. The Linux backend starts from the
//! Linux input code (an X keycode minus 8) and has to hand over the set-1 code of the same key,
//! i.e. `keys::evdev_code`'s inverse. This pins the two to each other by **membership**, not by a
//! count (VERIFICATION entry 1): every input code `keys::evdev_code` can produce, each by name.
//!
//! [`WindowEvent::KeyDown`]: omni_platform::window::WindowEvent::KeyDown

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;

use omni_android::jni::keys;
use omni_platform::window::scancode_from_evdev;

/// Every Linux input code `keys::evdev_code` maps some scancode to: its whole image, found by
/// asking it about every scancode shape it accepts (a plain make code, or `0xE000` plus one).
fn decoded_input_codes() -> BTreeSet<u16> {
    (0..=0xFFu32).chain((0..=0xFFu32).map(|make| 0xE000 | make)).filter_map(keys::evdev_code).collect()
}

#[test]
fn every_input_code_keys_decodes_round_trips_through_the_linux_scancode() {
    let decoded = decoded_input_codes();
    // The image is the keyboard, not a handful: the main block and the extended keys.
    for (evdev, name) in [(1, "Esc"), (30, "A"), (57, "Space"), (88, "F12"), (69, "Num Lock"), (119, "Pause"), (97, "right Ctrl"), (103, "Up"), (127, "Menu")] {
        assert!(decoded.contains(&evdev), "keys.rs decodes {name} ({evdev})");
    }
    for evdev in &decoded {
        let scancode = scancode_from_evdev(*evdev);
        assert_eq!(
            keys::evdev_code(scancode),
            Some(*evdev),
            "input code {evdev} -> Linux scancode {scancode:#x} -> keys.rs decodes {:?}",
            keys::evdev_code(scancode)
        );
    }
}

/// The other direction: an input code `keys.rs` has no key for is 0 from the Linux backend, the
/// seam's "the host gave no code", which `keys.rs` counts as unmapped -- never a code that
/// decodes to a **different** key.
#[test]
fn an_input_code_keys_does_not_decode_is_zero_and_never_another_key() {
    let decoded = decoded_input_codes();
    for evdev in 0..=u16::MAX {
        if decoded.contains(&evdev) {
            continue;
        }
        assert_eq!(scancode_from_evdev(evdev), 0, "input code {evdev}");
    }
    assert_eq!(keys::evdev_code(0), None, "and 0 is no key to keys.rs");
}
