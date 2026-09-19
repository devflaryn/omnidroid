//! Guest comparator for `qsort`/`bsearch`.
//!
//! The comparator is a **guest function pointer**: calling it is a control transfer into
//! emulated code, which belongs to the thunk boundary (unreviewed), not here. Sorting and
//! searching are pure computation over guest memory, so this crate implements them against
//! a callback trait the future adapter supplies.

use crate::memory::GuestMemory;

/// Host-side stand-in for a guest `int (*compar)(const void *, const void *)`.
///
/// The adapter implements this by performing the guest call with the two element addresses.
/// A comparator that itself faults guest memory must be reported as a `Fault` (return it in
/// the `Err` half), not silently turned into an ordering.
pub trait GuestCompare {
    /// Compare the elements at guest addresses `a` and `b`, C-style: negative / zero /
    /// positive as `a < / == / > b`.
    fn compare(&mut self, mem: &impl GuestMemory, a: u64, b: u64) -> Result<i32, crate::memory::Fault>;
}
