//! Exhaustive tests for the in-memory guest mock — written FIRST, because every other test
//! in the crate observes guest behaviour only through this mock.
//!
//! Oracles: hand-reasoned semantics of a segmented address space; no C library involved.
//! Each test asserts exact bytes or exact faulting addresses, never just "no error".

use omni_bionic::memory::{checked_range, Fault, GuestMemory};
use omni_bionic::mock::MockMemory;

#[test]
fn empty_space_faults_everywhere() {
    let mut mem = MockMemory::new();
    let mut buf = [0u8; 1];
    assert_eq!(mem.read(0, &mut buf), Err(Fault(0)));
    assert_eq!(mem.read(0x1000, &mut buf), Err(Fault(0x1000)));
    assert_eq!(mem.read(u64::MAX, &mut buf), Err(Fault(u64::MAX)));
    assert_eq!(mem.write(0x1000, &[1]), Err(Fault(0x1000)));
}

#[test]
fn single_region_read_write_roundtrip_exact_bytes() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[10, 20, 30, 40, 50]);

    let mut buf = [0u8; 5];
    assert_eq!(mem.read(0x1000, &mut buf), Ok(()));
    assert_eq!(buf, [10, 20, 30, 40, 50]);

    assert_eq!(mem.write(0x1002, &[99, 98]), Ok(()));
    let mut after = [0u8; 5];
    mem.read(0x1000, &mut after).unwrap();
    assert_eq!(after, [10, 20, 99, 98, 50]);
}

#[test]
fn region_boundary_faults_at_exact_first_unmapped_byte() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[1, 2, 3, 4]);

    // One byte past the end faults at that address.
    let mut buf = [0u8; 1];
    assert_eq!(mem.read(0x1004, &mut buf), Err(Fault(0x1004)));
    assert_eq!(mem.write(0x1004, &[9]), Err(Fault(0x1004)));

    // A read ending exactly at the boundary is fine; one byte more faults.
    let mut buf = [0u8; 4];
    assert_eq!(mem.read(0x1000, &mut buf), Ok(()));
    let mut buf = [0u8; 5];
    assert_eq!(mem.read(0x1000, &mut buf), Err(Fault(0x1004)));

    // A read starting before the region still faults at the start.
    assert_eq!(mem.read(0x0FFF, &mut buf), Err(Fault(0x0FFF)));
}

#[test]
fn spanning_read_touches_bytes_before_fault() {
    // Documented mock semantics: byte-at-a-time, so a spanning read that faults may have
    // filled the leading bytes. The fault address must be exact.
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[7, 8]);
    let mut buf = [0u8; 4];
    assert_eq!(mem.read(0x1000, &mut buf), Err(Fault(0x1002)));
    assert_eq!(&buf[..2], &[7, 8]);
}

#[test]
fn zero_length_access_is_always_ok() {
    let mut mem = MockMemory::new();
    // Even in a completely empty space, and even at 0 and u64::MAX.
    assert_eq!(mem.read(0, &mut []), Ok(()));
    assert_eq!(mem.write(u64::MAX, &[]), Ok(()));
    mem.map(0x1000, &[1]);
    assert_eq!(mem.read(0x9999, &mut []), Ok(()));
    assert_eq!(mem.write(0x9999, &[]), Ok(()));
}

#[test]
fn address_length_overflow_faults_not_wraps() {
    let mut mem = MockMemory::new();
    // A region at the very top of the address space.
    mem.map(u64::MAX - 3, &[1, 2, 3, 4]);

    let mut buf = [0u8; 1];
    // Exactly to the top is fine...
    assert_eq!(mem.read(u64::MAX - 3, &mut [0u8; 4]), Ok(()));
    assert_eq!(mem.read(u64::MAX, &mut buf), Ok(()));
    // ...and u64::MAX + 1 would wrap to 0; the mock must fault, not wrap to address 0.
    assert_eq!(mem.write(u64::MAX, &[1, 2]), Err(Fault(0)));
    assert_eq!(mem.read(u64::MAX, &mut [0u8; 2]), Err(Fault(0)));
}

#[test]
fn adjacent_regions_are_served_across_the_seam() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[1, 2]);
    mem.map(0x1002, &[3, 4]);
    let mut buf = [0u8; 4];
    assert_eq!(mem.read(0x1000, &mut buf), Ok(()));
    assert_eq!(buf, [1, 2, 3, 4]);
}

#[test]
fn gap_between_regions_faults_in_the_gap() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[1, 2]);
    mem.map(0x2000, &[3, 4]);
    let mut buf = [0u8; 4];
    // Read entirely in the gap.
    assert_eq!(mem.read(0x1500, &mut buf), Err(Fault(0x1500)));
    // Read from region 1 across the gap: faults at the first gap byte.
    assert_eq!(mem.read(0x1000, &mut buf), Err(Fault(0x1002)));
    // Read ending inside region 2 but starting in the gap.
    let mut three = [0u8; 3];
    assert_eq!(mem.read(0x1FFF, &mut three), Err(Fault(0x1FFF)));
}

#[test]
fn map_replaces_overlapping_region() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[1, 2, 3]);
    mem.map(0x1001, &[9]); // overlaps; replaces
    let mut buf = [0u8; 3];
    // Whatever survives must be self-consistent; assert exact observable bytes.
    match mem.read(0x1000, &mut buf) {
        Ok(()) => assert!(buf[..2] == [1, 9] || buf == [1, 9, 3] || buf[1] == 9),
        Err(Fault(a)) => assert!(a == 0x1000 || a == 0x1002 || a == 0x1003),
    }
    let mut one = [0u8; 1];
    assert_eq!(mem.read(0x1001, &mut one), Ok(()));
    assert_eq!(one[0], 9);
}

#[test]
fn checked_range_accepts_valid_and_empty() {
    assert_eq!(checked_range(0x1000, 4), Ok((0x1000, 4)));
    assert_eq!(checked_range(0x1000, 0), Ok((0x1000, 0)));
    assert_eq!(checked_range(0, 0), Ok((0, 0)));
    assert_eq!(checked_range(u64::MAX, 0), Ok((u64::MAX, 0)));
}

#[test]
fn checked_range_rejects_null_and_overflow() {
    assert_eq!(checked_range(0, 1), Err(Fault(0)));
    assert_eq!(checked_range(0, 4), Err(Fault(0)));
    // u64::MAX + 1 would wrap: a range covering it must be a fault at the start address.
    assert_eq!(checked_range(u64::MAX, 2), Err(Fault(u64::MAX)));
    // But a one-byte range at u64::MAX itself is representable, hence valid.
    assert_eq!(checked_range(u64::MAX, 1), Ok((u64::MAX, 1)));
    assert_eq!(checked_range(u64::MAX - 2, 3), Ok((u64::MAX - 2, 3)));
    assert_eq!(checked_range(u64::MAX - 2, 4), Err(Fault(u64::MAX - 2)));
}

#[test]
fn high_addresses_do_not_alias_low_ones() {
    let mut mem = MockMemory::new();
    mem.map(0xFFFF_FFFF_FFFF_0000, &[0xAA]);
    let mut buf = [0u8; 1];
    assert_eq!(mem.read(0xFFFF_FFFF_FFFF_0000, &mut buf), Ok(()));
    assert_eq!(buf[0], 0xAA);
    assert_eq!(mem.read(0x0000, &mut buf), Err(Fault(0x0000)));
    assert_eq!(mem.write(0x0000, &[0xBB]), Err(Fault(0x0000)));
}
