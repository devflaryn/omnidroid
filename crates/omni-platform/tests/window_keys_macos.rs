//! **The macOS key table against its one consumer, key by key.**
//!
//! `WindowEvent::KeyDown.scancode` is read by exactly one function in the workspace,
//! `omni_android::jni::keys::evdev_code`, which takes it for a PC set-1 make code and answers the
//! Linux input code of the key. The macOS backend derives that set-1 code from the `kVK_*` virtual
//! key through its own table (`window/macos/keys.rs`). This file holds the two together with the
//! consumer as the oracle -- not a second copy of it (VERIFICATION entry 7) -- and by membership,
//! row by row (entry 1):
//!
//! * every `kVK` whose Linux code has a set-1 code comes back out of `evdev_code` as **that** Linux
//!   code;
//! * every `kVK` that carries scancode 0 does so because **no** set-1 code reaches its Linux code
//!   through the consumer -- searched over all 65,536 values -- so 0 is forced, not an omission.

#![cfg(target_os = "macos")]

use omni_android::jni::keys::evdev_code;
use omni_platform::window::macos_keys::{scancode_of, set1_of_linux, KEYS};

#[test]
fn every_virtual_key_reaches_its_linux_code_through_the_consumer() {
    let mut mapped = Vec::new();
    let mut unmapped = Vec::new();
    for &(kvk, linux) in KEYS {
        let scancode = scancode_of(kvk);
        if scancode == 0 {
            unmapped.push((kvk, linux));
            continue;
        }
        assert_eq!(
            evdev_code(scancode),
            Some(linux),
            "kVK {kvk:#04x} carries scancode {scancode:#06x}, which the consumer reads as {:?}, not \
             Linux code {linux}",
            evdev_code(scancode)
        );
        mapped.push(kvk);
    }
    for (kvk, linux) in &unmapped {
        let reaching: Vec<u32> = (0..=0xFFFF_u32).filter(|&s| evdev_code(s) == Some(*linux)).collect();
        assert!(
            reaching.is_empty(),
            "kVK {kvk:#04x} (Linux {linux}) carries scancode 0, but set-1 code(s) {reaching:x?} reach \
             that Linux code through the consumer"
        );
    }
    println!(
        "{} virtual keys round-trip through evdev_code; {} carry 0 because the consumer has no \
         set-1 code for their Linux key: {:x?}",
        mapped.len(),
        unmapped.len(),
        unmapped
    );
    // Membership of the rows the port depends on most, by name.
    for (kvk, name) in [(0x00, "A"), (0x0D, "W"), (0x31, "Space"), (0x24, "Return"), (0x35, "Escape"),
        (0x7E, "Up"), (0x38, "Shift"), (0x3B, "Control"), (0x3A, "Option"), (0x37, "Command")]
    {
        assert!(mapped.contains(&kvk), "kVK_{name} ({kvk:#04x}) must reach the consumer");
    }
}

/// The inverse itself, over the consumer's whole domain: every set-1 code the consumer reads is
/// the one [`set1_of_linux`] answers for its Linux code. A table that round-tripped only the keys
/// a Mac has could still invert some other code wrongly, and the next backend to use it would
/// inherit that.
#[test]
fn set1_of_linux_is_the_inverse_of_the_consumer_over_its_whole_domain() {
    let mut checked = 0;
    for scancode in 0..=0xFFFF_u32 {
        if let Some(linux) = evdev_code(scancode) {
            assert_eq!(set1_of_linux(linux), Some(scancode), "Linux {linux} from set-1 {scancode:#06x}");
            checked += 1;
        }
    }
    for linux in 0..=u16::MAX {
        if let Some(scancode) = set1_of_linux(linux) {
            assert_eq!(evdev_code(scancode), Some(linux), "set1_of_linux({linux}) = {scancode:#06x}");
        }
    }
    println!("{checked} set-1 codes the consumer reads, each inverted exactly");
}
