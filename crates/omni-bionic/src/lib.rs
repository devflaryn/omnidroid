//! Host-side implementations of the **pure** subset of bionic (Android libc/libm) functions
//! that `libroblox.so` imports: functions that need no operating-system access and are pure
//! computation over guest memory.
//!
//! Everything here is written against the [`memory::GuestMemory`] trait, never against a
//! concrete emulator: a thin adapter written later connects these functions to the thunk
//! boundary in `omni-android`. If that boundary changes under review, this crate does not.
//!
//! Design rules (see `docs/research/bionic-pure-report.md` for the full argument):
//!
//! * **Guest pointers are untrusted.** Every access goes through [`memory::GuestMemory`] and
//!   can fail. A failure is a returned error — never a host panic, never a crash.
//! * **Address arithmetic is checked everywhere.** A guest range that overflows `u64` is a
//!   fault, not a wraparound.
//! * **The guest ABI is Android arm64 (LP64)**, not the host's: `long` is 64-bit, `wchar_t`
//!   is 32-bit, and `errno` numbers are Linux values. The host C library is never an oracle
//!   for anything those types touch.
//! * **No plausible stubs.** A function that cannot be implemented correctly returns an error
//!   naming itself, never a believable wrong answer.
//! * **No OS access.** The crate has zero dependencies, no `#[cfg(target_os)]`, and compiles
//!   unchanged on every host.
//!
//! Scanning loops are designed so that each iteration reads at least one byte through the
//! memory trait; every scan therefore terminates at a terminator or a fault, and no test run
//! can hang on an unterminated guest string.

//! ## Coverage, and where this crate deliberately stops
//!
//! Of the **51** thread / synchronisation / TLS symbols the 3,594 initializers statically reach
//! (`docs/research/init-reachable-imports.txt`):
//!
//! * **42 are implemented here.** Note `pthread_cond_timedwait` is [`cond::wait_end`] with
//!   `timeout: Some(..)` — the C symbol is not spelled on a function of its own.
//! * **1 is explicitly excluded**: `pthread_sigmask`, which needs the guest's real signal state.
//! * **8 are NOT here, and cannot be**, because each needs either host → guest re-entry or the
//!   operating system, and this crate has neither by design:
//!
//!   | symbol | what it needs |
//!   |---|---|
//!   | `pthread_create` | spawn a host thread **and** re-enter guest code at the start routine |
//!   | `pthread_join` / `pthread_detach` | host thread lifetime |
//!   | `pthread_exit` | unwind a guest thread through the boundary |
//!   | `pthread_getattr_np` | the live thread's real stack bounds |
//!   | `pthread_attr_setschedparam`, `pthread_getschedparam`, `pthread_setschedparam` | host scheduling policy |
//!
//! Those eight belong to the **adapter**, not here. This is a scope boundary, not an unfinished
//! edge: a zero-dependency crate with no OS access and no way to call back into the guest cannot
//! implement any of them, and a stub that pretended to would be exactly the "plausible stub" the
//! design rules above forbid.
//!
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(clippy::all)]

pub mod atomics;
pub mod atexit;
pub mod cond;
pub mod context;
pub mod ctype;
pub mod errno;
pub mod error;
pub mod guestcmp;
pub mod guard;
pub mod layouts;
pub mod libm;
pub mod locale;
pub mod mem;
pub mod memory;
pub mod metadata;
pub mod mock;
pub mod mock_threads;
pub mod mutex;
pub mod numerics;
pub mod once;
pub mod shared_mem;
pub mod printf;
pub mod rwlock;
pub mod sem;
pub mod sort;
pub mod string;
pub mod threads;
pub mod tls;
pub mod wide;

pub use context::GuestContext;
pub use error::BionicError;
pub use memory::{Fault, GuestMemory};

/// Timing tolerance for the unit tests that assert a wait really blocked.
///
/// A timed wait may return marginally EARLY. The host's timer has finite
/// granularity and the deadline is rounded to it, so `wait_timeout(150ms)` can
/// return at 149.96 ms — which is exactly what was observed here, on Windows,
/// against an `elapsed >= 150ms` assertion. Six such assertions in this crate had
/// zero headroom and failed intermittently under load; two others already allowed
/// slack, so the hazard was known but applied inconsistently.
///
/// The property these tests are for is "it blocked for about the timeout rather
/// than returning immediately". [`TIMER_SLACK`] is small enough that no immediate
/// return can pass and large enough to absorb host timer rounding.
#[cfg(test)]
pub(crate) mod timing {
    use core::time::Duration;

    /// Slack allowed below a timed wait's nominal duration. Covers host timer
    /// granularity and deadline rounding, not scheduling delay — a wait that
    /// returns early by more than this is a real defect.
    pub(crate) const TIMER_SLACK: Duration = Duration::from_millis(5);

    /// Assert `elapsed` shows a genuine block of about `target`, allowing
    /// [`TIMER_SLACK`] for the host timer returning marginally early.
    #[track_caller]
    pub(crate) fn assert_blocked_for(elapsed: Duration, target: Duration, what: &str) {
        let floor = target.saturating_sub(TIMER_SLACK);
        assert!(
            elapsed >= floor,
            "{what}: expected a block of about {target:?} (floor {floor:?} after {TIMER_SLACK:?} timer slack), but it returned after {elapsed:?}",
        );
    }
}
