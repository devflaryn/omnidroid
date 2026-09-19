//! Phase 4b tests: `qsort`/`bsearch` over guest memory.
//!
//! Oracles: hand-reasoned C semantics; Rust's `sort_by`/`binary_search_by` on a host-side
//! snapshot as the reference ordering (a genuinely independent implementation). The
//! comparator here is a host closure reading u64 elements from guest memory — standing in
//! for the adapter's guest-call closure.

use omni_bionic::guestcmp::GuestCompare;
use omni_bionic::memory::GuestMemory;
use omni_bionic::mock::MockMemory;
use omni_bionic::sort::{bsearch, qsort};

/// Comparator over little-endian u64 elements.
struct U64Cmp;
impl GuestCompare for U64Cmp {
    fn compare(&mut self, mem: &impl GuestMemory, a: u64, b: u64) -> Result<i32, omni_bionic::memory::Fault> {
        let mut ba = [0u8; 8];
        let mut bb = [0u8; 8];
        mem.read(a, &mut ba)?;
        mem.read(b, &mut bb)?;
        let (va, vb) = (u64::from_le_bytes(ba), u64::from_le_bytes(bb));
        Ok(match va.cmp(&vb) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        })
    }
}

fn map_elems(mem: &mut MockMemory, elems: &[u64], at: u64) {
    let mut bytes = Vec::new();
    for &e in elems {
        bytes.extend_from_slice(&e.to_le_bytes());
    }
    mem.map(at, &bytes);
}

fn read_elems(mem: &MockMemory, at: u64, n: usize) -> Vec<u64> {
    let mut bytes = vec![0u8; n * 8];
    mem.read(at, &mut bytes).unwrap();
    bytes.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

#[test]
fn qsort_sorts_u64_elements() {
    let mut mem = MockMemory::new();
    map_elems(&mut mem, &[5, 3, 9, 1, 7, 3], 0x1000);
    let mut cmp = U64Cmp;
    qsort(&mut mem, &mut cmp, 0x1000, 6, 8).unwrap();
    assert_eq!(read_elems(&mem, 0x1000, 6), [1, 3, 3, 5, 7, 9]);
}

#[test]
fn qsort_matches_rust_reference_on_larger_input() {
    // 100 pseudo-random u64s; reference ordering from Rust's sort (independent impl).
    let mut state = 0x12345678u32;
    let mut elems = Vec::new();
    for _ in 0..100 {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        elems.push(state as u64);
    }
    let mut mem = MockMemory::new();
    map_elems(&mut mem, &elems, 0x1000);
    let mut cmp = U64Cmp;
    qsort(&mut mem, &mut cmp, 0x1000, 100, 8).unwrap();
    let mut reference = elems.clone();
    reference.sort_unstable();
    assert_eq!(read_elems(&mem, 0x1000, 100), reference);
}

#[test]
fn qsort_zero_elements_is_noop_at_null() {
    let mut mem = MockMemory::new();
    let mut cmp = U64Cmp;
    assert!(qsort(&mut mem, &mut cmp, 0, 0, 8).is_ok());
}

#[test]
fn qsort_zero_size_with_elements_is_named_error() {
    let mut mem = MockMemory::new();
    let mut cmp = U64Cmp;
    let err = qsort(&mut mem, &mut cmp, 0x1000, 4, 0).unwrap_err();
    assert!(err.to_string().contains("qsort"));
}

#[test]
fn bsearch_finds_present_and_absent() {
    let mut mem = MockMemory::new();
    map_elems(&mut mem, &[1, 3, 5, 7, 9], 0x1000);
    let mut cmp = U64Cmp;
    // key "at" 0x2000 with value 5.
    map_elems(&mut mem, &[5], 0x2000);
    assert_eq!(bsearch(&mut mem, &mut cmp, 0x2000, 0x1000, 5, 8), Ok(0x1000 + 16));
    map_elems(&mut mem, &[6], 0x2000);
    assert_eq!(bsearch(&mut mem, &mut cmp, 0x2000, 0x1000, 5, 8), Ok(0));
}

#[test]
fn bsearch_zero_elements_returns_null() {
    let mut mem = MockMemory::new();
    let mut cmp = U64Cmp;
    assert_eq!(bsearch(&mut mem, &mut cmp, 0x2000, 0, 0, 8), Ok(0));
}

#[test]
fn qsort_comparator_fault_propagates() {
    // Element array extends past the mapping: the heapify reads must fault, not hang.
    let mut mem = MockMemory::new();
    map_elems(&mut mem, &[9, 8, 7, 6, 5, 4, 3, 2], 0x1000); // 8 elems but claim 10
    let mut cmp = U64Cmp;
    let res = qsort(&mut mem, &mut cmp, 0x1000, 10, 8);
    assert!(res.is_err());
}
