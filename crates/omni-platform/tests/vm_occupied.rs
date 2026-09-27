//! `vm::occupied_ranges`: which parts of a range the host already holds. What a guest address space
//! reserved *around* the host (`GuestSpaceConfig::around_host`) asks before it reserves.
//!
//! Portable: every host has a way to walk its own address space.

use omni_platform::vm;

/// A range nothing holds right now: reserved by the host's choice, then given back.
fn free_range(len: usize) -> usize {
    let probe = vm::reserve_placeholder(len, vm::allocation_granularity()).expect("a probe");
    let base = probe.base();
    vm::release(probe).expect("the probe released");
    base
}

#[test]
fn a_reservation_inside_the_range_is_reported_and_nothing_else_is() {
    let granule = vm::allocation_granularity();
    let len = 64 * granule;
    let base = free_range(len);
    let at = base + 16 * granule;
    let held = vm::reserve_placeholder_at(at, 2 * granule).expect("a host reservation in the middle");

    let occupied = vm::occupied_ranges(base, len).expect("the walk");
    assert_eq!(occupied, vec![(at, at + 2 * granule)], "exactly the reservation, as [start, end)");

    // A range starting inside it is clipped to the range asked about.
    let inner = vm::occupied_ranges(at + granule, 4 * granule).expect("the walk");
    assert_eq!(inner, vec![(at + granule, at + 2 * granule)]);

    vm::release(held).expect("released");
    assert_eq!(vm::occupied_ranges(base, len).expect("the walk"), Vec::new(), "given back, so free");
}

#[test]
fn committed_memory_counts_as_occupied_too() {
    let granule = vm::allocation_granularity();
    let len = 32 * granule;
    let base = free_range(len);
    let at = base + 8 * granule;
    let held = vm::reserve_placeholder_at(at, granule).expect("a host reservation");
    // SAFETY: `[at, at + granule)` is exactly the placeholder just reserved.
    unsafe { vm::commit_placeholder(at as *mut u8, granule, vm::Protection::ReadWrite) }.expect("committed");

    let occupied = vm::occupied_ranges(base, len).expect("the walk");
    assert_eq!(occupied, vec![(at, at + granule)]);

    // SAFETY: the private memory committed above, which nothing refers to.
    unsafe { vm::decommit_to_placeholder(at as *mut u8, granule) }.expect("decommitted");
    vm::release(held).expect("released");
}

#[test]
fn a_zero_length_range_is_refused() {
    assert!(vm::occupied_ranges(0x1000_0000, 0).is_err());
}
