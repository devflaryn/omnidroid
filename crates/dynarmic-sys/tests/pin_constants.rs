//! Constants this crate states about the **pin** rather than about a live jit, checked against the
//! vendored source that defines them.
//!
//! A constant copied out of a dependency is a fact with no owner: nothing fails when the dependency
//! moves, and the number quietly becomes a lie. These read the declarations back out of
//! `vendor/dynarmic` and fail if a re-pin changes either of them, which is the only thing that can
//! make [`OD_FIXED_PER_JIT_BYTES`](dynarmic_sys::OD_FIXED_PER_JIT_BYTES) wrong.

use std::path::{Path, PathBuf};

fn vendored(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/dynarmic").join(relative)
}

/// The 16 MiB `FastDispatchEntry` table `A64EmitX64` holds as a member, which is the dominant term
/// in this backend's per-guest-thread cost and is allocated whether or not the optimization that
/// uses it is enabled.
#[test]
fn the_fixed_per_jit_state_is_still_a_sixteen_mebibyte_fast_dispatch_table() {
    let header = vendored("src/dynarmic/backend/x64/a64_emit_x64.h");
    let Ok(text) = std::fs::read_to_string(&header) else {
        // The vendored tree is present in every build that compiles this crate at all, so its
        // absence is a broken checkout rather than a skip worth tolerating silently.
        panic!("the vendored header {} is missing", header.display());
    };

    let has = |needle: &str| text.lines().any(|l| l.trim() == needle);
    assert!(
        has("static_assert(sizeof(FastDispatchEntry) == 0x10);"),
        "the pin no longer asserts sizeof(FastDispatchEntry) == 0x10, so OD_FIXED_PER_JIT_BYTES's \
         first factor is no longer established by the source it came from"
    );
    assert!(
        has("static constexpr size_t fast_dispatch_table_size = 0x100000;"),
        "the pin no longer declares fast_dispatch_table_size = 0x100000, so \
         OD_FIXED_PER_JIT_BYTES's second factor has moved"
    );
    assert!(
        has("std::array<FastDispatchEntry, fast_dispatch_table_size> fast_dispatch_table;"),
        "the table is no longer a by-value member of A64EmitX64, so it may no longer be allocated \
         per jit at all -- which would make the constant an overstatement rather than a floor"
    );

    assert_eq!(
        dynarmic_sys::OD_FIXED_PER_JIT_BYTES,
        0x10 * 0x10_0000,
        "16 MiB: 0x10 bytes per entry times 0x100000 entries"
    );
    assert_eq!(dynarmic_sys::OD_FIXED_PER_JIT_BYTES, 16 * 1024 * 1024);
}

/// The pin itself, so a re-pin cannot slip past the test above by moving the file.
#[test]
fn the_pin_is_the_one_the_decisions_log_names() {
    let pin = vendored("../PIN.txt");
    let text = std::fs::read_to_string(&pin)
        .unwrap_or_else(|e| panic!("{}: {e}", pin.display()));
    assert!(
        text.contains("9d45823"),
        "D5 pins yuzu-mirror/dynarmic@9d45823; PIN.txt says something else:\n{text}"
    );
}
