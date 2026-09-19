//! Thread-identity and lifecycle registry for guest threads.
//!
//! The guest is Android arm64: a `pthread_t` is a 64-bit value (`unsigned long`,
//! LP64) and `pthread_self()` returns it. The host may be LLP64 Windows where a
//! "thread id" is a 32-bit DWORD — so this crate's thread identity is its own
//! newtype, always 64 bits, and the adapter decides what fills it. A host thread
//! id's width must never leak into a guest value.

use core::time::Duration;
use core::fmt;

/// A guest thread identity: always 64 bits, whatever the host uses internally.
///
/// Values are opaque to this crate; the only operation is equality (the whole of
/// `pthread_equal`). The all-zero value is reserved: no real thread has it, which
/// lets registries use `GuestThreadId::NONE` as "no owner / not registered".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GuestThreadId(pub u64);

impl GuestThreadId {
    /// The reserved "no thread" value (0). Never returned by a live registry.
    pub const NONE: GuestThreadId = GuestThreadId(0);

    /// Whether this is the reserved none-value.
    pub fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for GuestThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "guest-thread({:#x})", self.0)
    }
}

/// Identifies the calling guest thread and tracks the live set.
///
/// Implemented by the adapter (and by the test mock). `current` must return a
/// stable identity per host thread for the lifetime of that thread; the lifecycle
/// methods let the TLS layer run key destructors at thread exit and keep the
/// live-set bounded.
pub trait ThreadRegistry {
    /// The calling thread's identity. Must be stable per host thread and must
    /// never be [`GuestThreadId::NONE`].
    fn current(&self) -> GuestThreadId;

    /// Register the calling thread as alive. Returns `false` if it was already
    /// registered (a double-attach is an adapter bug; callers treat it as one).
    fn attach(&self) -> bool;

    /// Mark the calling thread dead and return the list of destructor work the
    /// TLS layer must run: one entry per (destructor, value) pair, in key order.
    /// The crate's TLS sweep logic decides the rounds; see [`crate::tls`].
    fn detach_and_take_destructors(&self) -> Vec<(u64, u64)>;

    /// Whether the calling thread is currently registered.
    fn is_attached(&self) -> bool;

    /// Number of currently-registered threads (test/diagnostic use).
    fn live_count(&self) -> usize;
}

/// The two clocks POSIX timed waits can use.
///
/// Bionic's `pthread_condattr_setclock` accepts `CLOCK_MONOTONIC` and
/// `CLOCK_REALTIME`; a cond's timedwait measures against the cond's own clock. The
/// adapter maps these onto the host's monotonic/wall clocks; the mock maps both to
/// the host instant clock with an optional offset (see `tests`).
pub trait Clock {
    /// `CLOCK_MONOTONIC`: a clock that never jumps backwards.
    fn now_monotonic(&self) -> Duration;
    /// `CLOCK_REALTIME`: the wall clock, which may jump.
    fn now_realtime(&self) -> Duration;
}

/// Outcome of a futex wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResult {
    /// The wait was ended by a wake.
    Woken,
    /// `*addr != expected` at the moment of the wait: return without blocking.
    WouldBlock,
    /// The timeout expired (or the deadline was already in the past).
    TimedOut,
}

impl WaitResult {
    /// Whether the waiter may retry its predicate loop (both Woken and TimedOut
    /// re-check the predicate; WouldBlock means the check already failed).
    pub fn retry_predicate(self) -> bool {
        matches!(self, WaitResult::Woken | WaitResult::TimedOut)
    }
}

impl fmt::Display for WaitResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaitResult::Woken => write!(f, "woken"),
            WaitResult::WouldBlock => write!(f, "would block (value mismatch)"),
            WaitResult::TimedOut => write!(f, "timed out"),
        }
    }
}

/// Blocking primitive keyed by a guest address, modelling Linux futex.
///
/// The contract is the Linux one, which is what bionic's synchronization primitives
/// are built on:
///
/// * `wait(addr, expected, timeout)`: if `*addr == expected`, block until a `wake`
///   targeting `addr` or the timeout expires. The value check must be atomic with
///   the blocking decision (the caller holds its own protocol lock around the
///   value-read + wait pair, exactly as a futex user must).
/// * `wake(addr, count)`: wake up to `count` waiters on `addr`; return how many
///   were woken. Waking with no waiter is not an error and returns 0.
///
/// Spurious wakeups are permitted (POSIX): a wait may return `Woken` without a
/// matching wake. Predicate loops are mandatory for correct callers and tests.
pub trait Futex {
    /// Block the calling thread until woken or timed out, per the contract above.
    fn wait(&self, addr: u64, expected: u32, timeout: Option<Duration>) -> WaitResult;

    /// Wake up to `count` waiters on `addr`; returns the number woken.
    fn wake(&self, addr: u64, count: u32) -> u32;
}
