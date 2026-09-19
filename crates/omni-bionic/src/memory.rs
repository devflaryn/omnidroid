//! Guest memory access. The whole crate touches guest memory only through the
//! [`GuestMemory`] trait, so it runs unchanged against any emulator backend.

/// A guest memory access failed at this guest address.
///
/// Deliberately minimal: an emulator backend adds cause/protection detail at the boundary,
/// not here. `Fault` implements `Eq`/`Hash` so tests can assert exact faulting addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fault(pub u64);

impl Fault {
    /// The faulting guest address.
    pub fn addr(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "fault at guest address {:#x}", self.0)
    }
}

impl std::error::Error for Fault {}

/// Checked byte range covering the addresses `addr ..= addr+len-1` with **no wraparound**.
///
/// The invariant is that every byte address in the range is representable: the *last* byte's
/// address `addr + len - 1` must fit in `u64`. (An exclusive-end `addr + len` check would
/// wrongly reject a range ending at `u64::MAX`, which is a representable guest address.)
///
/// C semantics: a zero-length access to `NULL` is defined behaviour (`memcpy(NULL, x, 0)`,
/// `memcmp(NULL, y, 0)` are valid C), so those are accepted. A non-empty range at null, or
/// any range whose last byte would exceed `u64::MAX`, is a fault at `addr`.
///
/// Returns `(start, len)` on success.
pub fn checked_range(addr: u64, len: u64) -> Result<(u64, u64), Fault> {
    if len == 0 {
        // Zero-length: nothing is accessed, valid C at any address (including NULL).
        Ok((addr, 0))
    } else if addr == 0 {
        Err(Fault(0))
    } else {
        match addr.checked_add(len - 1) {
            // Overflow: the last byte would wrap the 64-bit address space.
            None => Err(Fault(addr)),
            Some(_) => Ok((addr, len)),
        }
    }
}

/// Guest memory as seen by the implemented bionic functions.
///
/// Every access is fallible; a guest pointer that leaves mapped memory produces a
/// [`Fault`] at (or within) the accessed range, never a host crash. Implementations should
/// honour `len == 0` as a no-op `Ok(())` regardless of address.
///
/// Bounds are checked by the *implementation* of this trait (the mock in `tests/` and the
/// later emulator adapter), and *additionally* by [`checked_range`] at the call sites in
/// this crate, so a buggy backend cannot turn a wraparound into a wraparound.
pub trait GuestMemory {
    /// Read `buf.len()` bytes at `addr`.
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault>;
    /// Write `buf.len()` bytes at `addr`.
    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault>;
}
