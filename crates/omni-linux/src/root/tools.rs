//! The magisk tool binary (`su`, `magisk`, `resetprop` are all this one file).
//! PLACEHOLDER until Task 6 replaces the body with `include_bytes!` of the real committed binary.

/// The bytes of the tool binary.
#[must_use]
pub fn magisk_binary() -> &'static [u8] {
    b"\x7fELF-omni-magisk-placeholder"
}
