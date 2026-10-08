//! **The host's current keyboard layout, live** (`omni_platform::keyboard`): on a layout whose `A`
//! key is the Latin `a` (US/ABC, Turkish Q and F, German, ...), `KEY_A` reads `a`, `A` with Shift
//! and `A` with Caps Lock, and Space reads a space. A layout that types something else with that
//! key (Dvorak, Cyrillic, ...) is not asserted on, and a session without a current layout (a CI
//! machine with no login session) only says so: the seam's refusal is its contract there.

#![cfg(target_os = "macos")]

use omni_platform::keyboard::{current_layout, level, KeyOutput, ThirdLevel};

const KEY_A: u16 = 30;
const KEY_SPACE: u16 = 57;

#[test]
fn the_current_layout_reads_the_a_key_as_the_host_types_it() {
    let layout = match current_layout() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("skipped: no current keyboard layout here ({e})");
            return;
        }
    };
    eprintln!("the host's layout: {}, {} keys", layout.name, layout.keys.len());
    assert_eq!(layout.third_level, ThirdLevel::EitherAlt);
    let key = |code| layout.keys.iter().find(|k| k.code == code).unwrap_or_else(|| panic!("KEY {code} is read"));
    assert_eq!(key(KEY_SPACE).out[0], KeyOutput::Text(" ".into()));
    let a = key(KEY_A);
    if a.out[0] != KeyOutput::Text("a".into()) {
        eprintln!("skipped: this layout's A key types {:?}", a.out[0]);
        return;
    }
    assert_eq!(a.out[level::SHIFT], KeyOutput::Text("A".into()));
    assert_eq!(a.out[level::CAPS], KeyOutput::Text("A".into()));
}
