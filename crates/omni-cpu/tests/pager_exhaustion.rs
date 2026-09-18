//! **A backend that cannot own guest paging is refused, not carried on with.**
//!
//! Its own test binary, and therefore its own process, because the thing it does is fill the
//! process-wide vectored-handler table: run it beside anything else and that thing would find the
//! table full too.
//!
//! # The defect this pins
//!
//! `DynarmicBackend::new` used to install the demand pager with `.ok()`, which silently accepted
//! `HandlerTableFull` as if it were "this platform has no vectored handler". The two are not the
//! same thing at all. Without a pager, every guest fault reaches dynarmic's own frame-based
//! handler, which recompiles the block with fastmem off **permanently** — 30-49x slower, correct
//! results, no error anywhere (D4, D10). That is the same silent-degradation shape the per-slice
//! callback invariant exists for, arriving through configuration rather than through execution.
//!
//! It was not hypothetical: `omni-cpu`'s own suites build one guest address space per test and
//! libtest runs them in parallel, so with the old capacity of 8 the ninth backend in a binary ran
//! with no pager, intermittently, depending on scheduling.

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

use std::sync::Arc;

use omni_cpu::dynarmic::{DynarmicBackend, DynarmicOptions};
use omni_cpu::CpuError;
use omni_mem::{DemandPager, GuestSpace};
use omni_platform::fault::MAX_HANDLERS;

#[test]
fn a_backend_that_cannot_install_a_demand_pager_is_refused() {
    // Fill every slot with pagers of our own. Each needs a space, and D10 measured a pure
    // reservation at **0 bytes** of commit charge, so this costs address space and nothing else.
    let mut held = Vec::with_capacity(MAX_HANDLERS);
    for i in 0..MAX_HANDLERS {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        held.push(DemandPager::install(space).unwrap_or_else(|e| {
            panic!("slot {i} of {MAX_HANDLERS} could not be filled: {e}")
        }));
    }
    assert_eq!(held.len(), MAX_HANDLERS);

    // Now a backend cannot get one, and must say so rather than running without it.
    let space = Arc::new(GuestSpace::new().expect("one more guest address space"));
    match DynarmicBackend::new(space, DynarmicOptions::default()) {
        Err(CpuError::Backend { operation, detail, .. }) => {
            assert_eq!(operation, "install the guest demand pager");
            assert!(
                detail.contains("handler slot") || detail.contains("slots are in use"),
                "the refusal must name the resource that ran out: {detail}"
            );
            assert!(
                detail.contains("30-49x"),
                "and it must say what carrying on would have cost, or nobody acts on it: {detail}"
            );
            // **M1's defect class, pinned.** A lost line continuation in a multi-line string
            // literal does not fail to compile: it silently folds the next line's indentation into
            // the message, so the operator reads this sentence with a 26-space hole in the middle
            // of it. Nothing else in the suite would notice, because every `contains` check still
            // passes. Thirteen literals across the workspace carried it, including this one and
            // `CpuError::DegradedMemoryPath`'s -- the two messages a human reads when D10's and
            // D4's central requirements fail.
            assert!(
                !detail.contains("  "),
                "the refusal contains a run of spaces, which is what a lost line continuation \
                 looks like at runtime: {detail:?}"
            );
        }
        Err(other) => panic!("expected a pager refusal, got {other}"),
        Ok(backend) => panic!(
            "a backend was created with no demand pager and reported owns_guest_paging = {}. \
             Every guest fault would go to dynarmic's own handler and recompile its block onto \
             the callback path, 30-49x slower, with correct results and no error",
            backend.owns_guest_paging()
        ),
    }

    // And with a slot freed it succeeds again, so the refusal is about the resource rather than
    // about something permanent.
    held.pop();
    let space = Arc::new(GuestSpace::new().expect("a guest address space"));
    let backend = DynarmicBackend::new(space, DynarmicOptions::default())
        .expect("a slot is free again, so the backend must come up");
    assert!(backend.owns_guest_paging());
    assert!(backend.slice_invariant_armed());
}
