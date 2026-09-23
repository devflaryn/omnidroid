//! `timerfd_create(2)`: a timer that is a descriptor.
//!
//! # Why this is in the descriptor table
//!
//! [`eventfd`](super::eventfd)'s argument: `epoll`, `poll` and `read` observe one descriptor
//! space, and the engine's transport watches its timer in the same epoll set as its sockets.
//!
//! # The clock, and why it can only be one
//!
//! **`CLOCK_MONOTONIC` only**, read through [`crate::clock::monotonic_now`] -- the same function
//! that answers the guest's own `clock_gettime(CLOCK_MONOTONIC)`. That is what makes
//! `TFD_TIMER_ABSTIME` meaningful: the engine computes an absolute deadline from a time *it* read,
//! and a timer on any other epoch would fire early or late by the difference, every time, with
//! nothing reporting it. `CLOCK_REALTIME` and `CLOCK_BOOTTIME` are refused by name by the caller.
//!
//! # The semantics, and where each comes from
//!
//! | call | behaviour |
//! |---|---|
//! | `settime(value = 0)` | disarm |
//! | `settime(value, interval)` | the first expiry at `now + value` (or `value`, absolute), then every `interval` if it is non-zero |
//! | `read` | 8 bytes: expirations since the last read or `settime`, then zero; `EAGAIN` when none |
//! | `poll` | readable while at least one expiration is unread |
//!
//! **Expirations are computed when asked, not counted by a thread.** Nothing here runs on its
//! own: the count is a function of the arming and the clock, so a timer that is never read costs
//! nothing, and a periodic timer read late reports every period it missed, which is what the
//! kernel's overrun count is.
//!
//! **What it does not do**: wake anybody at the deadline. A waiter learns the deadline through
//! [`TimerFd::deadline`] and caps its own wait there -- `ReadinessSource::Timer` is how the
//! table says so. What *does* raise the readiness gate is arming or disarming, because another
//! thread's `settime` is a change a waiter on this descriptor could otherwise miss.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::error::{FsError, FsErrorKind, FsResult};
use super::pipe::{ReadyGate, Readiness};

/// `TFD_TIMER_ABSTIME`: the value is a deadline on the timer's clock, not a delay.
pub const TFD_TIMER_ABSTIME: i32 = 1;
/// `TFD_NONBLOCK`, which is `O_NONBLOCK`.
pub const TFD_NONBLOCK: i32 = 0o4_000;
/// `TFD_CLOEXEC`, which is `O_CLOEXEC`. Recorded on the descriptor for `fcntl(F_GETFD)` and
/// otherwise inert: there is no `exec` here.
pub const TFD_CLOEXEC: i32 = 0o2_000_000;
/// Every `timerfd_create` flag Linux defines.
pub const KNOWN_FLAGS: i32 = TFD_NONBLOCK | TFD_CLOEXEC;

/// Bytes of one `read`: the expiration count, a `uint64_t`.
pub const TRANSFER_BYTES: usize = 8;

/// When the timer next fires and how often after that.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Arming {
    /// The next expiry on the monotonic clock, or `None` when disarmed.
    deadline: Option<Duration>,
    /// Zero for a one-shot timer.
    interval: Duration,
}

/// One timer.
#[derive(Debug)]
pub struct TimerFd {
    arming: Mutex<Arming>,
    nonblocking: bool,
    gate: Arc<ReadyGate>,
}

impl TimerFd {
    /// A disarmed timer.
    #[must_use]
    pub fn new(nonblocking: bool, gate: Arc<ReadyGate>) -> TimerFd {
        TimerFd { arming: Mutex::new(Arming::default()), nonblocking, gate }
    }

    fn arming(&self) -> std::sync::MutexGuard<'_, Arming> {
        self.arming.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether `O_NONBLOCK` is set.
    #[must_use]
    pub fn nonblocking(&self) -> bool {
        self.nonblocking
    }

    /// Set or clear `O_NONBLOCK`.
    pub fn set_nonblocking(&mut self, nonblocking: bool) {
        self.nonblocking = nonblocking;
    }

    /// The next expiry, on the monotonic clock, or `None` when disarmed.
    #[must_use]
    pub fn deadline(&self) -> Option<Duration> {
        self.arming().deadline
    }

    /// What the timer would report *at* `now`: `(time until the next expiry, interval)`, both
    /// zero when disarmed -- `timerfd_gettime`'s answer, and `settime`'s `old_value`.
    #[must_use]
    pub fn remaining(&self, now: Duration) -> (Duration, Duration) {
        let arming = *self.arming();
        match arming.deadline {
            None => (Duration::ZERO, arming.interval),
            // An expiry already passed and unread reports zero remaining, not a negative one.
            Some(deadline) => (deadline.saturating_sub(now), arming.interval),
        }
    }

    /// Arm or disarm, returning what [`remaining`](TimerFd::remaining) said before.
    ///
    /// `value` zero disarms, whatever `interval` says, which is the kernel's rule. Unread
    /// expirations are discarded: they belonged to the old arming.
    pub fn settime(
        &self,
        absolute: bool,
        value: Duration,
        interval: Duration,
        now: Duration,
    ) -> (Duration, Duration) {
        let old = self.remaining(now);
        {
            let mut arming = self.arming();
            *arming = if value.is_zero() {
                Arming { deadline: None, interval }
            } else if absolute {
                Arming { deadline: Some(value), interval }
            } else {
                Arming { deadline: Some(now.saturating_add(value)), interval }
            };
        }
        self.gate.bump();
        old
    }

    /// Readable once at least one expiration is unread. Never writable -- `write` is `EINVAL`.
    #[must_use]
    pub fn readiness(&self, now: Duration) -> Readiness {
        let expired = self.arming().deadline.is_some_and(|deadline| now >= deadline);
        Readiness { readable: expired, writable: false, hangup: false, error: false }
    }

    /// `read`: the expirations since the last read, as a native-endian `u64`, and the count
    /// consumed -- a one-shot timer disarms, a periodic one moves to its next future expiry.
    pub fn read(&self, buf: &mut [u8], now: Duration) -> FsResult<usize> {
        const OP: &str = "read";
        if buf.len() < TRANSFER_BYTES {
            return Err(FsError::kinded(
                OP,
                "a timerfd",
                FsErrorKind::InvalidInput,
                "a timerfd read is eight bytes or nothing: it consumes the expiration count",
            ));
        }
        let expirations = {
            let mut arming = self.arming();
            let Some(deadline) = arming.deadline.filter(|deadline| now >= *deadline) else {
                return Err(FsError::kinded(
                    OP,
                    "a timerfd",
                    FsErrorKind::WouldBlock,
                    "no expiration since the last read. As `pipe`, the wait belongs to the caller",
                ));
            };
            if arming.interval.is_zero() {
                arming.deadline = None;
                1u64
            } else {
                let late = (now - deadline).as_nanos() / arming.interval.as_nanos();
                let count = u64::try_from(late).unwrap_or(u64::MAX - 1).saturating_add(1);
                let advance = arming.interval.saturating_mul(u32::try_from(count).unwrap_or(u32::MAX));
                arming.deadline = Some(deadline.saturating_add(advance));
                count
            }
        };
        buf[..TRANSFER_BYTES].copy_from_slice(&expirations.to_ne_bytes());
        self.gate.bump();
        Ok(TRANSFER_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer() -> TimerFd {
        TimerFd::new(true, Arc::new(ReadyGate::default()))
    }

    fn read(timer: &TimerFd, now: Duration) -> FsResult<u64> {
        let mut buf = [0u8; 8];
        timer.read(&mut buf, now).map(|_| u64::from_ne_bytes(buf))
    }

    const MS: Duration = Duration::from_millis(1);

    /// One-shot, relative: not readable before, readable at, one expiration, then disarmed.
    #[test]
    fn a_one_shot_timer_fires_once_at_its_deadline() {
        let t = timer();
        let start = Duration::from_secs(100);
        t.settime(false, 5 * MS, Duration::ZERO, start);
        assert!(!t.readiness(start + 4 * MS).readable);
        assert_eq!(read(&t, start + 4 * MS).unwrap_err().kind(), Some(FsErrorKind::WouldBlock));
        assert!(t.readiness(start + 5 * MS).readable, "at the deadline, not after it");
        assert_eq!(read(&t, start + 9 * MS).unwrap(), 1);
        assert_eq!(t.deadline(), None, "a one-shot timer disarms once read");
        assert_eq!(read(&t, start + 20 * MS).unwrap_err().kind(), Some(FsErrorKind::WouldBlock));
    }

    /// Absolute arming is on the same clock the value came from; `remaining` counts down to it.
    #[test]
    fn an_absolute_deadline_is_taken_as_given() {
        let t = timer();
        let now = Duration::from_secs(50);
        t.settime(true, Duration::from_secs(52), Duration::ZERO, now);
        assert_eq!(t.deadline(), Some(Duration::from_secs(52)));
        assert_eq!(t.remaining(now), (Duration::from_secs(2), Duration::ZERO));
        // An absolute deadline already in the past is expired immediately.
        t.settime(true, Duration::from_secs(10), Duration::ZERO, now);
        assert!(t.readiness(now).readable);
    }

    /// A periodic timer read late reports every period it missed, and re-arms in the future.
    #[test]
    fn a_periodic_timer_counts_the_periods_it_missed() {
        let t = timer();
        let start = Duration::from_secs(1);
        t.settime(false, 10 * MS, 10 * MS, start);
        assert_eq!(read(&t, start + 35 * MS).unwrap(), 3, "expiries at 10, 20 and 30 ms");
        assert_eq!(t.deadline(), Some(start + 40 * MS));
        assert!(!t.readiness(start + 39 * MS).readable);
    }

    /// Zero disarms, and re-arming discards what the old arming had not had read.
    #[test]
    fn zero_disarms_and_rearming_discards_unread_expirations() {
        let t = timer();
        let start = Duration::from_secs(1);
        t.settime(false, MS, Duration::ZERO, start);
        let old = t.settime(false, Duration::ZERO, Duration::ZERO, start + 5 * MS);
        assert_eq!(old, (Duration::ZERO, Duration::ZERO), "it had expired, so zero remained");
        assert!(!t.readiness(start + 10 * MS).readable, "disarmed");
    }
}
