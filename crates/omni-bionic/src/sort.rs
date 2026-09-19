//! `qsort` and `bsearch` over guest memory.
//!
//! The comparator is a **guest function pointer** — the control transfer to it belongs to
//! the thunk boundary (unreviewed), so this module sorts against a
//! [`crate::guestcmp::GuestCompare`] callback the future adapter supplies. Sorting and
//! searching themselves are pure computation over guest memory, per the task's decoupling
//! rule.
//!
//! Signatures implemented:
//!
//! | C signature | here |
//! |---|---|
//! | `void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *))` | [`qsort`] |
//! | `void *bsearch(const void *key, const void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *))` | [`bsearch`] |
//!
//! `qsort`'s output for equal elements is unspecified by C, so any correct ordering
//! satisfies the contract; the implementation is heapsort (in-place, deterministic,
//! no allocation — libroblox.so imports no allocator for the guest, and this crate makes
//! no guest allocations at all).

use crate::error::BionicError;
use crate::guestcmp::GuestCompare;
use crate::memory::{checked_range, Fault, GuestMemory};

/// `void qsort(void *base, size_t nmemb, size_t size, compar)` — heapsort over guest
/// elements, swapping through guest memory in element-sized chunks.
///
/// Errors: `Fault` when the element array is unmapped or overflows, or a comparator call
/// faults; `InvalidArgument` when `size == 0` and `nmemb != 0` (nothing comparable — C
/// gives UB; the error names the function). `nmemb == 0` succeeds without access (valid C
/// for any base, including null).
pub fn qsort(
    mem: &mut impl GuestMemory,
    cmp: &mut impl GuestCompare,
    base: u64,
    nmemb: u64,
    size: u64,
) -> Result<(), BionicError> {
    if nmemb == 0 {
        return Ok(()); // no elements: nothing to compare or touch
    }
    if size == 0 {
        return Err(BionicError::InvalidArgument("qsort: zero element size"));
    }
    let bytes = nmemb.checked_mul(size).ok_or(Fault(base))?;
    checked_range(base, bytes)?;
    if nmemb == 1 {
        return Ok(());
    }
    // Sift-down heapsort on element indices. The comparator receives element addresses.
    let n = nmemb;
    // Heapify (bottom-up).
    let mut i = n / 2;
    while i > 0 {
        i -= 1;
        sift_down(mem, cmp, base, size, i, n)?;
    }
    // Extract max repeatedly.
    let mut end = n;
    while end > 1 {
        end -= 1;
        swap_elems(mem, base, base + end * size, size)?;
        sift_down(mem, cmp, base, size, 0, end)?;
    }
    Ok(())
}

/// Restore the max-heap property for the subtree rooted at `root`, within `heap[..n]`.
fn sift_down(
    mem: &mut impl GuestMemory,
    cmp: &mut impl GuestCompare,
    base: u64,
    size: u64,
    root: u64,
    n: u64,
) -> Result<(), Fault> {
    let mut root = root;
    loop {
        let child = 2 * root + 1;
        if child >= n {
            return Ok(());
        }
        // Pick the larger child (by the comparator).
        let mut swap = child;
        if child + 1 < n {
            let c = cmp.compare(mem, base + child * size, base + (child + 1) * size)?;
            if c < 0 {
                swap = child + 1;
            }
        }
        if cmp.compare(mem, base + root * size, base + swap * size)? >= 0 {
            return Ok(());
        }
        swap_elems(mem, base + root * size, base + swap * size, size)?;
        root = swap;
    }
}

/// Swap the `size`-byte elements at `a` and `b` through guest memory (chunked).
fn swap_elems(mem: &mut impl GuestMemory, a: u64, b: u64, size: u64) -> Result<(), Fault> {
    let mut buf_a = [0u8; 256];
    let mut buf_b = [0u8; 256];
    let mut done = 0u64;
    while done < size {
        let chunk = (size - done).min(256) as usize;
        mem.read(a + done, &mut buf_a[..chunk])?;
        mem.read(b + done, &mut buf_b[..chunk])?;
        mem.write(a + done, &buf_b[..chunk])?;
        mem.write(b + done, &buf_a[..chunk])?;
        done += chunk as u64;
    }
    Ok(())
}

/// `void *bsearch(const void *key, const void *base, size_t nmemb, size_t size, compar)`
///
/// Binary search of `nmemb` elements each `size` bytes; returns the address of a matching
/// element or guest `0` (`NULL`). Which match is returned when elements compare equal is
/// unspecified by C; this returns the first found by the standard bisection.
pub fn bsearch(
    mem: &mut impl GuestMemory,
    cmp: &mut impl GuestCompare,
    key: u64,
    base: u64,
    nmemb: u64,
    size: u64,
) -> Result<u64, BionicError> {
    if nmemb == 0 {
        return Ok(0);
    }
    if size == 0 {
        return Err(BionicError::InvalidArgument("bsearch: zero element size"));
    }
    let bytes = nmemb.checked_mul(size).ok_or(Fault(base))?;
    checked_range(base, bytes)?;
    let mut lo: u64 = 0;
    let mut hi: u64 = nmemb;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let elem = base + mid * size;
        let c = cmp.compare(mem, key, elem)?;
        if c == 0 {
            return Ok(elem);
        } else if c < 0 {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(0)
}
