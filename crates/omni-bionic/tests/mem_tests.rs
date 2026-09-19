//! Phase 2 tests: the `mem*` family and the `_chk` variants, through the mock.
//!
//! Oracles:
//! * hand-reasoned C99/POSIX semantics of memcpy/memmove/memset/memcmp/memchr;
//! * bionic's FORTIFY contract for the `_chk` forms (destination overflow must fail loudly);
//! * a Rust-side trivially-correct reference (`memmove_ref`) for overlap cases;
//! * sign-only checks for `memcmp` per the C standard.
//!
//! Hostile inputs per function: null pointer with nonzero length, unmapped destination or
//! source, ranges running past a region end, `addr + len` overflowing `u64`, zero length,
//! and overlap in both directions. Every case must give an exact result or a `Fault` —
//! never a panic.

use omni_bionic::error::{BionicError, BionicResult};
use omni_bionic::mem::{memcpy, memcpy_chk, memchr, memcmp, memmove, memset, memset_chk};
use omni_bionic::memory::{Fault, GuestMemory};
use omni_bionic::mock::MockMemory;

/// Trivially-correct reference `memmove` used as the oracle for overlap semantics: copies
/// through a full host-side snapshot of the source range.
fn memmove_ref(mem: &mut MockMemory, dst: u64, src: u64, n: usize) -> Result<(), Fault> {
    let mut snap = vec![0u8; n];
    mem.read(src, &mut snap)?;
    mem.write(dst, &snap)
}

fn mapped() -> MockMemory {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0u8; 64]);
    mem.map(0x2000, &[0u8; 64]);
    mem
}

// ---------------------------------------------------------------- memcpy

#[test]
fn memcpy_forward_copy_exact_bytes_and_returns_dst() {
    let mut mem = MockMemory::new();
    mem.map(0x2000, &[1, 2, 3, 4, 5]);
    mem.map(0x1000, &[0u8; 5]);
    assert_eq!(memcpy(&mut mem, 0x1000, 0x2000, 5), Ok(0x1000));
    let mut out = [0u8; 5];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [1, 2, 3, 4, 5]);
}

#[test]
fn memcpy_zero_length_is_null_safe_no_op() {
    let mut mem = mapped();
    // Valid C for any pointers, including NULL.
    assert_eq!(memcpy(&mut mem, 0, 0x2000, 0), Ok(0));
    assert_eq!(memcpy(&mut mem, 0x1000, 0, 0), Ok(0x1000));
    assert_eq!(memcpy(&mut mem, 0, 0, 0), Ok(0));
    // Memory untouched.
    let mut out = [0u8; 4];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [0, 0, 0, 0]);
}

#[test]
fn memcpy_null_with_nonzero_length_faults_at_zero() {
    let mut mem = mapped();
    assert_eq!(memcpy(&mut mem, 0, 0x2000, 4), Err(Fault(0)));
    assert_eq!(memcpy(&mut mem, 0x1000, 0, 4), Err(Fault(0)));
}

#[test]
fn memcpy_unmapped_ranges_fault_before_any_write() {
    let mut mem = mapped();
    // Source unmapped.
    assert_eq!(memcpy(&mut mem, 0x1000, 0x5000, 4), Err(Fault(0x5000)));
    // Destination unmapped.
    assert_eq!(memcpy(&mut mem, 0x5000, 0x1000, 4), Err(Fault(0x5000)));
    // Source range runs past the region end.
    assert_eq!(memcpy(&mut mem, 0x1000, 0x103E, 4), Err(Fault(0x1040)));
    // Nothing was written: destination still zero.
    let mut out = [0xFF; 4];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [0, 0, 0, 0]);
}

#[test]
fn memcpy_range_overflow_faults_not_wraps() {
    let mut mem = mapped();
    // dst + n would exceed u64::MAX by wrapping to a low address.
    assert_eq!(
        memcpy(&mut mem, u64::MAX - 2, 0x1000, 4),
        Err(Fault(u64::MAX - 2))
    );
    assert_eq!(
        memcpy(&mut mem, 0x1000, u64::MAX - 2, 4),
        Err(Fault(u64::MAX - 2))
    );
    // A 2-byte write at u64::MAX-1 is a representable range (last byte u64::MAX), so it
    // passes range validation and then faults at the first unmapped byte (u64::MAX-1) —
    // NOT at a wrapped low address, and not a silent success.
    assert_eq!(
        memcpy(&mut mem, u64::MAX - 1, 0x1000, 2),
        Err(Fault(u64::MAX - 1))
    );
}

#[test]
fn memcpy_chunked_copy_survives_large_n() {
    // 1000 bytes exercises the 256-byte chunking loop.
    let mut mem = MockMemory::new();
    let data: Vec<u8> = (0..1000).map(|i| (i % 251) as u8).collect();
    mem.map(0x3000, &data);
    mem.map(0x5000, &[0u8; 1000]);
    assert_eq!(memcpy(&mut mem, 0x5000, 0x3000, 1000), Ok(0x5000));
    let mut out = vec![0u8; 1000];
    mem.read(0x5000, &mut out).unwrap();
    assert_eq!(out, data);
}

#[test]
fn memcpy_self_copy_is_identity() {
    let mut mem = mapped();
    mem.write(0x1000, &[9, 8, 7]).unwrap();
    assert_eq!(memcpy(&mut mem, 0x1000, 0x1000, 3), Ok(0x1000));
    let mut out = [0u8; 3];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [9, 8, 7]);
}

// ------------------------------------------------------- memcpy overlap (documented)

#[test]
fn memcpy_overlapping_forward_bias_documented_semantics() {
    // Overlap is UB in C; the documented decision is a forward-biased byte copy. The test
    // pins that decision so a future change is a conscious one.
    let mut mem = mapped();
    mem.write(0x1000, &[1, 2, 3, 4, 5, 0, 0, 0]).unwrap();
    // dst = src + 2, n = 4: forward copy moves 1..4 to 3..6.
    assert_eq!(memcpy(&mut mem, 0x1002, 0x1000, 4), Ok(0x1002));
    let mut out = [0u8; 8];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [1, 2, 1, 2, 3, 4, 0, 0]);
}

// ---------------------------------------------------------------- memmove

#[test]
fn memmove_forward_nonoverlap_matches_reference() {
    let mut mem = mapped();
    mem.write(0x1000, &[1, 2, 3, 4, 5]).unwrap();
    assert_eq!(memmove(&mut mem, 0x1020, 0x1000, 5), Ok(0x1020));
    let mut out = [0u8; 5];
    mem.read(0x1020, &mut out).unwrap();
    assert_eq!(out, [1, 2, 3, 4, 5]);
}

#[test]
fn memmove_backward_overlap_matches_reference() {
    // dst > src with overlap: back-to-front copy must equal the snapshot reference.
    let mut mem = mapped();
    mem.write(0x1000, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    assert_eq!(memmove(&mut mem, 0x1002, 0x1000, 6), Ok(0x1002));
    let mut out = [0u8; 8];
    mem.read(0x1000, &mut out).unwrap();
    let mut reference = mapped();
    reference.write(0x1000, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    memmove_ref(&mut reference, 0x1002, 0x1000, 6).unwrap();
    let mut ref_out = [0u8; 8];
    reference.read(0x1000, &mut ref_out).unwrap();
    assert_eq!(out, ref_out);
    // And the exact expected bytes: [1,2,1,2,3,4,5,6].
    assert_eq!(out, [1, 2, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn memmove_forward_overlap_matches_reference() {
    // dst < src with overlap: forward copy; source must not be clobbered first.
    let mut mem = mapped();
    mem.write(0x1000, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    assert_eq!(memmove(&mut mem, 0x1000, 0x1002, 6), Ok(0x1000));
    let mut out = [0u8; 8];
    mem.read(0x1000, &mut out).unwrap();
    let mut reference = mapped();
    reference.write(0x1000, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    memmove_ref(&mut reference, 0x1000, 0x1002, 6).unwrap();
    let mut ref_out = [0u8; 8];
    reference.read(0x1000, &mut ref_out).unwrap();
    assert_eq!(out, ref_out);
    // Exact expected bytes: [3,4,5,6,7,8,7,8].
    assert_eq!(out, [3, 4, 5, 6, 7, 8, 7, 8]);
}

#[test]
fn memmove_exact_overlap_is_identity() {
    let mut mem = mapped();
    mem.write(0x1000, &[4, 5, 6]).unwrap();
    assert_eq!(memmove(&mut mem, 0x1000, 0x1000, 3), Ok(0x1000));
    let mut out = [0u8; 3];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [4, 5, 6]);
}

#[test]
fn memmove_hostile_null_and_faults() {
    let mut mem = mapped();
    assert_eq!(memmove(&mut mem, 0, 0x1000, 3), Err(Fault(0)));
    assert_eq!(memmove(&mut mem, 0x1000, 0, 3), Err(Fault(0)));
    assert_eq!(memmove(&mut mem, 0, 0, 0), Ok(0)); // valid C
    assert_eq!(memmove(&mut mem, 0x1040, 0x1000, 4), Err(Fault(0x1040)));
    assert_eq!(memmove(&mut mem, u64::MAX - 1, 0x1000, 4), Err(Fault(u64::MAX - 1)));
}

// ---------------------------------------------------------------- memset

#[test]
fn memset_fills_low_byte_and_returns_dst() {
    let mut mem = mapped();
    assert_eq!(memset(&mut mem, 0x1000, 0xAB, 4), Ok(0x1000));
    let mut out = [0u8; 4];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [0xAB; 4]);
    // c is converted to unsigned char: only the low byte counts.
    assert_eq!(memset(&mut mem, 0x1000, 0x1FF, 2), Ok(0x1000));
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [0xFF, 0xFF, 0xAB, 0xAB]);
}

#[test]
fn memset_zero_length_and_null_hostile() {
    let mut mem = mapped();
    assert_eq!(memset(&mut mem, 0, 0x41, 0), Ok(0));
    assert_eq!(memset(&mut mem, 0, 0x41, 1), Err(Fault(0)));
    assert_eq!(memset(&mut mem, 0x103E, 0x41, 4), Err(Fault(0x1040)));
}

#[test]
fn memset_chunked_fill() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0u8; 1000]);
    assert_eq!(memset(&mut mem, 0x1000, 7, 1000), Ok(0x1000));
    let mut out = vec![0u8; 1000];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, vec![7u8; 1000]);
}

// ---------------------------------------------------------------- memcmp

#[test]
fn memcmp_sign_only_equal_less_greater() {
    let mut mem = mapped();
    mem.write(0x1000, &[1, 2, 3, 4]).unwrap();
    mem.write(0x1020, &[1, 2, 3, 4]).unwrap();
    mem.write(0x2000, &[1, 2, 3, 5]).unwrap();
    mem.write(0x2020, &[1, 2, 3, 3]).unwrap();

    assert_eq!(memcmp(&mem, 0x1000, 0x1020, 4), Ok(0));
    // Sign, not magnitude.
    let less = memcmp(&mem, 0x1000, 0x2000, 4).unwrap();
    let greater = memcmp(&mem, 0x1000, 0x2020, 4).unwrap();
    assert!(less < 0);
    assert!(greater > 0);
}

#[test]
fn memcmp_unsigned_char_ordering() {
    let mut mem = mapped();
    // 0xFF must compare GREATER than 0x01 (unsigned char), not less (signed).
    mem.write(0x1000, &[0xFF]).unwrap();
    mem.write(0x1020, &[0x01]).unwrap();
    assert!(memcmp(&mem, 0x1000, 0x1020, 1).unwrap() > 0);
    assert!(memcmp(&mem, 0x1020, 0x1000, 1).unwrap() < 0);
}

#[test]
fn memcmp_zero_length_is_zero_even_at_null() {
    let mem = mapped();
    assert_eq!(memcmp(&mem, 0, 0, 0), Ok(0));
    assert_eq!(memcmp(&mem, 0x1000, 0x1030, 0), Ok(0));
}

#[test]
fn memcmp_unmapped_and_overflow_fault() {
    let mem = mapped();
    assert_eq!(memcmp(&mem, 0x5000, 0x1000, 1), Err(Fault(0x5000)));
    assert_eq!(memcmp(&mem, 0x1000, 0x5000, 1), Err(Fault(0x5000)));
    assert_eq!(memcmp(&mem, 0x103E, 0x1000, 4), Err(Fault(0x1040)));
    assert_eq!(memcmp(&mem, u64::MAX, 0x1000, 2), Err(Fault(u64::MAX)));
    // Null with nonzero length.
    assert_eq!(memcmp(&mem, 0, 0x1000, 1), Err(Fault(0)));
}

// ---------------------------------------------------------------- memchr

#[test]
fn memchr_finds_first_match_returns_address() {
    let mut mem = mapped();
    mem.write(0x1000, &[1, 2, 3, 2, 9]).unwrap();
    assert_eq!(memchr(&mem, 0x1000, 2, 5), Ok(0x1001));
    assert_eq!(memchr(&mem, 0x1000, 9, 5), Ok(0x1004));
    assert_eq!(memchr(&mem, 0x1000, 3, 5), Ok(0x1002));
}

#[test]
fn memchr_no_match_returns_null() {
    let mut mem = mapped();
    mem.write(0x1000, &[1, 2, 3]).unwrap();
    assert_eq!(memchr(&mem, 0x1000, 4, 3), Ok(0));
}

#[test]
fn memchr_zero_length_returns_null_without_touching_memory() {
    let mem = mapped();
    // Valid C at any address, including NULL.
    assert_eq!(memchr(&mem, 0, 0x41, 0), Ok(0));
    assert_eq!(memchr(&mem, 0x5000, 0x41, 0), Ok(0));
}

#[test]
fn memchr_scans_only_n_bytes() {
    let mut mem = mapped();
    mem.write(0x1000, &[0, 0, 0, 7]).unwrap();
    // 7 exists at 0x1003 but only 3 bytes are scanned.
    assert_eq!(memchr(&mem, 0x1000, 7, 3), Ok(0));
    assert_eq!(memchr(&mem, 0x1000, 7, 4), Ok(0x1003));
}

#[test]
fn memchr_low_byte_conversion() {
    let mut mem = mapped();
    mem.write(0x1000, &[0x41]).unwrap();
    assert_eq!(memchr(&mem, 0x1000, 0x141, 1), Ok(0x1000));
}

#[test]
fn memchr_unmapped_and_overflow_fault() {
    let mem = mapped();
    assert_eq!(memchr(&mem, 0x5000, 0x41, 4), Err(Fault(0x5000)));
    assert_eq!(memchr(&mem, 0x103E, 0x41, 4), Err(Fault(0x1040)));
    assert_eq!(memchr(&mem, u64::MAX - 1, 0x41, 3), Err(Fault(u64::MAX - 1)));
}

// ---------------------------------------------------------------- _chk variants

fn chk(res: BionicResult<u64>) -> Result<u64, BionicError> {
    res
}

#[test]
fn memcpy_chk_within_bounds_behaves_like_memcpy() {
    let mut mem = mapped();
    mem.map(0x2000, &[1, 2, 3]);
    assert_eq!(chk(memcpy_chk(&mut mem, 0x1000, 0x2000, 3, 8)), Ok(0x1000));
    let mut out = [0u8; 3];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [1, 2, 3]);
}

#[test]
fn memcpy_chk_overflow_returns_named_error() {
    let mut mem = mapped();
    let err = memcpy_chk(&mut mem, 0x1000, 0x2000, 9, 8).unwrap_err();
    assert_eq!(err, BionicError::CheckFailed("__memcpy_chk"));
    assert!(err.to_string().contains("__memcpy_chk"));
    // n == dst_size is exactly full: allowed.
    assert_eq!(chk(memcpy_chk(&mut mem, 0x1000, 0x2000, 8, 8)), Ok(0x1000));
}

#[test]
fn memcpy_chk_memory_fault_still_reported_as_fault() {
    let mut mem = mapped();
    let err = memcpy_chk(&mut mem, 0x1000, 0x5000, 2, 8).unwrap_err();
    assert_eq!(err, BionicError::Memory(Fault(0x5000)));
}

#[test]
fn memset_chk_overflow_returns_named_error() {
    let mut mem = mapped();
    let err = memset_chk(&mut mem, 0x1000, 0x41, 65, 64).unwrap_err();
    assert_eq!(err, BionicError::CheckFailed("__memset_chk"));
    // Full-size fill is fine.
    assert_eq!(chk(memset_chk(&mut mem, 0x1000, 0x41, 64, 64)), Ok(0x1000));
    let mut out = [0u8; 1];
    mem.read(0x103F, &mut out).unwrap();
    assert_eq!(out[0], 0x41);
}

#[test]
fn memset_chk_zero_size_destination_rejects_any_fill() {
    let mut mem = mapped();
    // A zero-sized object can never be filled: n=0 is a no-op that still passes the check.
    assert_eq!(chk(memset_chk(&mut mem, 0x1000, 0x41, 0, 0)), Ok(0x1000));
    assert_eq!(
        memset_chk(&mut mem, 0x1000, 0x41, 1, 0).unwrap_err(),
        BionicError::CheckFailed("__memset_chk")
    );
}
