//! The atomic capabilities the synchronization primitives need: a compare-exchange,
//! and an ordered load and store.
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
//!
//! ## Atomicity is not enough: the ordering, on a weakly ordered host
//!
//! A mutex is two promises, and the CAS above keeps only the first. Mutual
//! exclusion needs the state change to be one atomic step; **publication** needs
//! every store the previous owner made inside its critical section to be visible
//! to the next owner once it has the lock. That is a *release* on the word that
//! hands the lock over and an *acquire* on the word that takes it.
//!
//! `GuestMemory::write` gives neither. It is a byte copy, and on an x86-64 host it
//! happens to be enough, because x86 (TSO) never lets a store overtake an earlier
//! store or a load overtake an earlier load. On an **arm64** host (macOS) it is
//! not: a plain `STR` of "unlocked" can become visible to another core before the
//! critical section's own stores do, and that core's CAS then takes the lock and
//! reads the previous owner's data stale. Found by audit after macOS run m9, where
//! the engine's DataModel write-lock tracker asserted "lock owned by another fiber"
//! on the Mac only: ERRORCHECK and RECURSIVE `pthread_mutex_unlock`, and
//! `pthread_once`'s DONE, published their word with a plain `write`.
//!
//! So a word that **hands something over** is published with
//! [`GuestAtomic::store_u32_release`], and a word whose value alone lets a caller
//! **proceed without a CAS** (`pthread_once` seeing DONE) is read with
//! [`GuestAtomic::load_u32_acquire`]. A plain read that only *guides* a CAS (the
//! "is it free?" peek before `cas_u32`) stays plain: a stale value costs a failed
//! CAS and a retry, never a wrong answer, and the CAS itself is the acquire.
//!
//! Since the guest's own code runs on the same host thread and its loads and
//! stores are the host's (D4), a host release store orders the guest's
//! critical-section stores exactly as the guest's own `STLR` would on a device.

use crate::memory::{Fault, GuestMemory};

/// The atomic operations over guest memory the synchronization layer needs.
///
/// `cas_u32(addr, expect, new)`:
/// * reads the 32-bit LE word at `addr` and writes `new` **atomically** iff it
///   currently equals `expect`; returns `Ok(true)` if the swap happened,
///   `Ok(false)` if the value did not match (no write),
/// * `Err(Fault)` if the address is unmapped (a faulting CAS writes nothing).
///
/// bionic's mutex/cond/rwlock fast paths are CAS loops over one control word,
/// and `sem_post`/`sem_wait` likewise. The release store and acquire load exist
/// for the words that are handed over or observed without a CAS (see the module
/// docs). Everything else stays plain [`GuestMemory`] traffic.
///
/// # The ordering every implementation must give
///
/// | operation | at least |
/// |---|---|
/// | `cas_u32`, when it swaps | acquire **and** release (`AcqRel`) |
/// | `cas_u32`, when it does not | acquire |
/// | `load_u32_acquire` | acquire |
/// | `store_u32_release` | release |
///
/// A CAS that swaps is both at once because the same primitive takes a lock (the
/// acquire the new owner needs) and gives one back (the release the NORMAL
/// `unlock` and every rwlock/sem release rely on). An implementation over a lock
/// (the mock's inner host mutex) has all of these for free; one over host atomics
/// (the adapter's `GuestView`) must ask for them.
///
/// None of the three has a default. A default would have to be built from plain
/// [`GuestMemory`] traffic, which is exactly the unordered access these exist to
/// replace, and would compile silently for the next implementation.
pub trait GuestAtomic {
    /// Atomically swap `new` into `addr` iff the word equals `expect`.
    fn cas_u32(&self, addr: u64, expect: u32, new: u32) -> Result<bool, Fault>;

    /// Read the 32-bit LE word at `addr` with **acquire** ordering: every store
    /// another thread made before its release of this word is visible after it.
    fn load_u32_acquire(&self, addr: u64) -> Result<u32, Fault>;

    /// Write the 32-bit LE word at `addr` with **release** ordering: every load
    /// and store this thread made before it is visible to a thread that acquires
    /// the word and sees this value. One atomic store, never torn.
    fn store_u32_release(&self, addr: u64, value: u32) -> Result<(), Fault>;
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
