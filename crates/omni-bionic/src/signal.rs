//! The one signal function that is pure computation: `sigfillset`.
//!
//! # Why exactly one of the family is here
//!
//! Four signal symbols are in the 188 the initializers reach — `sigfillset`, `sigaction`,
//! `raise` and `pthread_sigmask` — and they split cleanly rather than arbitrarily. Three of
//! them need **the guest's real signal state**: a disposition table, a per-thread mask, and a
//! delivery mechanism that can interrupt translated guest code at an arbitrary instruction.
//! Omnidroid has none of those, inventing one is a design of its own, and the adapter refuses
//! all three by name (`omni_android::bionic::signals` has the argument and the believable wrong
//! answer each one declines to give).
//!
//! `sigfillset` needs none of it. It is `memset(set, 0xff, sizeof(sigset_t))` — a total
//! function of its single argument, with no state behind it at all — so it is implemented, and
//! it is implemented *here*, because a function that touches nothing but guest memory is what
//! this crate is for (D19).
//!
//! That split is deliberate and is the shape the brief for this phase asked for: model what can
//! be modelled exactly, refuse the rest by name, and never produce the third option — a
//! `sigaction` that returns 0 without installing anything, which would tell guest code it will
//! be notified about a fault it will never hear about.
//!
//! # Bionic fills the whole word, and glibc does not
//!
//! glibc's `sigfillset` deliberately leaves the two NPTL-reserved signals (32 and 33) clear, so
//! that a program cannot block the threading implementation's own signals. **Bionic does not do
//! that**: `sigfillset64` is a plain `memset(set, 0xff, sizeof(*set))` and every bit ends up
//! set. This crate follows bionic, which is the same convention `guestcmp` records for
//! `strcmp`'s byte difference — the guest was compiled against bionic, and a glibc-shaped
//! correction here would be this crate deciding to be a different libc.

use crate::layouts::sizes;
use crate::memory::{checked_range, Fault, GuestMemory};

/// The byte every bit of a filled `sigset_t` is set to.
///
/// Named rather than spelled inline because it is the whole observable of this module, and
/// because a mutation that changes it to `0` produces a set that is *empty* while every range
/// check and every return value stays exactly as it was.
pub const FILLED_BYTE: u8 = 0xFF;

/// `int sigfillset(sigset_t *set)`.
///
/// `Ok(Ok(()))` when the set was filled; `Ok(Err(errno))` for the one failure bionic reports,
/// which is a null pointer; `Err(Fault)` when the guest's `set` is not writable memory.
///
/// Bionic reports a null `set` as `EINVAL` through `errno` and returns `-1` — the `<signal.h>`
/// convention, not the pthread one, so the caller sets `errno` rather than returning the number.
///
/// # Errors
///
/// [`Fault`] if the eight bytes at `set` are not writable guest memory. A null pointer is **not**
/// a fault: bionic checks for it first and answers `EINVAL`, and reporting it as a bad access
/// would tell a reader the guest passed a wild pointer when it passed the one value the function
/// is documented to reject.
pub fn fillset(mem: &mut impl GuestMemory, set: u64) -> Result<Result<(), i32>, Fault> {
    if set == 0 {
        return Ok(Err(crate::errno::consts::EINVAL));
    }
    // Checked before the write, so a set whose last byte leaves mapped memory is refused as a
    // whole rather than half-filled. A half-filled set is the worst of the three outcomes: it
    // says "these signals and not those" about a partition nobody chose.
    checked_range(set, sizes::SIGSET_T)?;
    // The size is bounded by the constant, never by anything the guest said: the only argument
    // here is the destination address.
    let bytes = [FILLED_BYTE; sizes::SIGSET_T as usize];
    mem.write(set, &bytes)?;
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errno::consts;
    use crate::mock::MockMemory;

    fn mem_at(start: u64, len: usize) -> MockMemory {
        let mut mem = MockMemory::new();
        mem.map(start, &vec![0u8; len]);
        mem
    }

    /// Every bit of the set, and not one byte past it.
    ///
    /// The trailing guard byte is the assertion that matters: a `sigfillset` that wrote
    /// `sizeof(sigset64_t)` on a host where that was larger would corrupt whatever the guest put
    /// next to its set, and nothing about the return value would say so.
    #[test]
    fn a_filled_set_is_eight_bytes_of_ones_and_nothing_after_them() {
        let mut mem = mem_at(0x1000, 64);
        assert_eq!(fillset(&mut mem, 0x1000), Ok(Ok(())));
        let mut out = [0u8; 16];
        mem.read(0x1000, &mut out).expect("read it back");
        assert_eq!(&out[..8], &[0xFFu8; 8], "bionic fills the whole word");
        assert_eq!(&out[8..], &[0u8; 8], "and writes nothing past sizeof(sigset_t)");
        assert_eq!(sizes::SIGSET_T, 8, "LP64 sigset_t is one unsigned long");
    }

    /// Bit 31 and bit 32 are both set, which is where bionic and glibc differ.
    ///
    /// glibc reserves signals 32 and 33 for NPTL and leaves their bits clear. A guest compiled
    /// against bionic that checked `sigismember(&set, 32)` after a `sigfillset` would get the
    /// wrong answer from a glibc-shaped implementation, and no test of the other 62 bits would
    /// see it.
    #[test]
    fn the_two_signals_glibc_reserves_are_set_here_because_bionic_sets_them() {
        let mut mem = mem_at(0x2000, 32);
        fillset(&mut mem, 0x2000).expect("writable").expect("filled");
        let mut out = [0u8; 8];
        mem.read(0x2000, &mut out).expect("read it back");
        let word = u64::from_le_bytes(out);
        // POSIX numbers signals from 1, and the kernel's set is bit (signo - 1).
        for signo in [1u32, 31, 32, 33, 64] {
            assert!(word & (1u64 << (signo - 1)) != 0, "signal {signo} must be in a filled set");
        }
        assert_eq!(word, u64::MAX);
    }

    /// A null set is `EINVAL`, not a fault: bionic checks it before it touches anything.
    #[test]
    fn a_null_set_is_einval_rather_than_a_bad_access() {
        let mut mem = mem_at(0x1000, 64);
        assert_eq!(fillset(&mut mem, 0), Ok(Err(consts::EINVAL)));
    }

    /// A set outside the guest's memory is a fault, and so is one that straddles the end.
    ///
    /// The mock writes byte at a time, so *this* level only establishes that the access is
    /// refused; the all-or-nothing property belongs to the adapter's `GuestView`, which takes
    /// the whole range through `omni_mem::admit` before it copies anything, and the adapter's
    /// own suite is where it is asserted.
    #[test]
    fn an_unmapped_set_faults() {
        let mut mem = mem_at(0x1000, 64);
        assert!(fillset(&mut mem, 0x9999_0000).is_err());
        assert!(fillset(&mut mem, 0x1000 + 64 - 7).is_err());
        // A `set` whose last byte would wrap the address space is refused before any access.
        assert!(fillset(&mut mem, u64::MAX - 2).is_err());
    }
}
