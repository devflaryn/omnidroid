//! The bindings in `src/lib.rs` are hand-written mirrors of `shim/od_dynarmic.h`.
//! Nothing checks them at link time: a `#[repr(C)]` struct that disagrees with
//! the C one links cleanly and corrupts memory. These tests are the check.

use dynarmic_sys::*;
use std::mem::{align_of, size_of};

#[test]
fn abi_version_matches() {
    // SAFETY: no arguments, no state.
    assert_eq!(unsafe { od_dynarmic_abi_version() }, OD_DYNARMIC_ABI_VERSION);
}

#[test]
fn struct_layouts_match_the_c_header() {
    let mut l = OdAbiLayout::default();
    // SAFETY: `l` is a valid, writable `OdAbiLayout`.
    unsafe { od_dynarmic_abi_layout(&mut l) };

    assert_eq!(l.callbacks_size as usize, size_of::<OdCallbacks>(), "od_callbacks size");
    assert_eq!(l.callbacks_align as usize, align_of::<OdCallbacks>(), "od_callbacks align");
    assert_eq!(l.config_size as usize, size_of::<OdConfig>(), "od_config size");
    assert_eq!(l.config_align as usize, align_of::<OdConfig>(), "od_config align");
    assert_eq!(
        l.effective_config_size as usize,
        size_of::<OdEffectiveConfig>(),
        "od_effective_config size"
    );
    assert_eq!(
        l.effective_config_align as usize,
        align_of::<OdEffectiveConfig>(),
        "od_effective_config align"
    );
    assert_eq!(l.stats_size as usize, size_of::<OdStats>(), "od_stats size");
    assert_eq!(l.stats_align as usize, align_of::<OdStats>(), "od_stats align");
}

#[test]
fn a_callback_table_has_one_slot_per_pointer() {
    // If the Rust struct gained or lost a field without the C one following,
    // the size check above would catch it -- but only if the fields are all
    // pointer-sized, which they are. This states the count directly so the
    // documented surface and the code cannot drift apart silently.
    assert_eq!(
        size_of::<OdCallbacks>() / size_of::<usize>(),
        23,
        "23 callbacks: 1 code fetch, 5 reads, 5 writes, 5 exclusives, \
         interpreter fallback, SVC, exception, icache op, CNTPCT, and 2 tick hooks"
    );
}

#[test]
fn the_shim_reentry_marker_is_not_a_halt_reason() {
    // `od_jit_run` returns this instead of letting dynarmic abort. It must be
    // distinguishable from every real halt bit.
    let real = OD_HALT_STEP
        | OD_HALT_CACHE_INVALIDATION
        | OD_HALT_MEMORY_ABORT
        | OD_HALT_USER1
        | OD_HALT_USER2
        | OD_HALT_USER3
        | OD_HALT_USER4
        | OD_HALT_USER5
        | OD_HALT_USER6
        | OD_HALT_USER7
        | OD_HALT_USER8;
    assert_eq!(OD_HALT_SHIM_REENTERED & real, 0);
    assert_ne!(OD_HALT_SHIM_REENTERED, 0);
}
