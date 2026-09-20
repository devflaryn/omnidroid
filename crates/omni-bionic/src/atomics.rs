//! The compare-exchange capability the synchronization primitives need.
//!
//! ## Why GuestMemory alone is not enough
//!
//! `GuestMemory::read`/`write` move bytes; two host threads racing on the same
//! guest word can each read "unlocked" and each write "locked", and *both*
//! believe they hold the mutex — a lost mutual exclusion that no later test can
//! see, only the corrupted data can. A real futex protocol needs the value
//! check and the state change to be **one atomic step** in the guest.
//!
//! The crate cannot synthesize that from byte reads/writes. The adapter's memory
//! layer can: it owns the guest's real memory and can perform the guest's
//! interlocked operation (LDXR/STXR or a host CAS under the memory lock). So the
//! threading layer takes a [`GuestAtomic`] capability alongside the memory.
//!
//! The adapter implements this ONCE over the guest's `LDXR/STXR` emulation (or
//! an equivalent host CAS under its memory lock). `SharedMockMemory` implements
//! it atomically under its inner host lock, which is what makes the concurrency
//! tests real: eight host threads racing a guest mutex resolve through one
//! atomic CAS, not through luck.

use crate::memory::{Fault, GuestMemory};

/// A 32-bit compare-and-swap over guest memory.
///
/// `cas_u32(addr, expect, new)`:
/// * reads the 32-bit LE word at `addr` and writes `new` **atomically** iff it
///   currently equals `expect`; returns `Ok(true)` if the swap happened,
///   `Ok(false)` if the value did not match (no write),
/// * `Err(Fault)` if the address is unmapped (a faulting CAS writes nothing).
///
/// This is the whole surface the sync layer needs: bionic's mutex/cond/rwlock
/// fast paths are CAS loops over one control word, and `sem_post`/`sem_wait`
/// likewise. Everything else stays plain [`GuestMemory`] traffic.
pub trait GuestAtomic {
    /// Atomically swap `new` into `addr` iff the word equals `expect`.
    fn cas_u32(&self, addr: u64, expect: u32, new: u32) -> Result<bool, Fault>;
}

/// Helper: atomically exchange a 32-bit word and return its PREVIOUS value
/// (`Ok(prev)` if the swap happened, `Err` otherwise). Built on `cas_u32` for
/// implementations, but may be overridden to avoid the retry loop.
pub trait GuestAtomicExt: GuestAtomic {
    /// Swap `new` in, returning the previous value; `Err` when the value was
    /// not `expect`.
    fn swap_if_eq(&self, addr: u64, expect: u32, new: u32) -> Result<Result<u32, u32>, Fault> {
        // Default: a single CAS gives us the answer for the boolean case; the
        // previous value needs one extra read AFTER a successful CAS (the word
        // is known to have been `expect` at the CAS moment, so prev == expect).
        if self.cas_u32(addr, expect, new)? {
            Ok(Ok(expect))
        } else {
            Ok(Err(expect))
        }
    }
}

impl<T: GuestAtomic> GuestAtomicExt for T {}

/// Sanity helper used by tests: read a u32 through the plain memory trait.
pub fn read_u32(mem: &impl GuestMemory, addr: u64) -> Result<u32, Fault> {
    let mut b = [0u8; 4];
    mem.read(addr, &mut b)?;
    Ok(u32::from_le_bytes(b))
}
