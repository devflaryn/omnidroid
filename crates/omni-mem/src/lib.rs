//! Guest address-space manager: reservation, lazy commit, decommit, placeholder mapping and the
//! JIT code arena, built on top of [`omni-platform`]'s virtual-memory seam.
//!
//! Empty by design: this crate is created by Task 1 so later tasks have somewhere to land. It
//! must compile for all five targets without `cfg` (Global Constraint 4).
#![forbid(unsafe_code)]
