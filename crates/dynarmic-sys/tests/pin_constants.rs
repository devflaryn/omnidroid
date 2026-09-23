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

/// The 16 MiB `FastDispatchEntry` table, **allocated only when `FastDispatch` is on** -- patch 0002.
/// Upstream held it by value in every `A64EmitX64`, the dominant term in a guest thread's cost
/// whether or not the optimization that uses it was enabled.
#[test]
fn the_fast_dispatch_table_is_sixteen_mebibytes_and_allocated_only_when_it_is_used() {
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
        has("std::unique_ptr<std::array<FastDispatchEntry, fast_dispatch_table_size>> fast_dispatch_table;"),
        "the table is no longer patch 0002's lazily allocated member -- if it is by value again, \
         every jit pays 16 MiB of written memory for an optimization this runtime disables"
    );
    let emitter = vendored("src/dynarmic/backend/x64/a64_emit_x64.cpp");
    let source = std::fs::read_to_string(&emitter)
        .unwrap_or_else(|e| panic!("the vendored source {}: {e}", emitter.display()));
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let allocation = lines
        .iter()
        .position(|l| l.starts_with("fast_dispatch_table = std::make_unique<"))
        .expect("patch 0002 allocates the table in A64EmitX64's constructor");
    assert_eq!(
        lines[allocation - 1],
        "if (conf.HasOptimization(OptimizationFlag::FastDispatch)) {",
        "the allocation is guarded by the optimization that reads the table, and by nothing else"
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
