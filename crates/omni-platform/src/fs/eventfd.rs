//! `eventfd(2)`: a counter that is a descriptor.
//!
//! # Why this is in the descriptor table and not a namespace of its own
//!
//! The same argument [`pipe`](super::pipe) carries one kind along. `poll`, `select` and
//! `ALooper_addFd` observe **one** descriptor space, and a guest that gets an `eventfd` and then
//! polls it alongside a pipe is doing the ordinary thing with it. Two allocators handing out the
//! same number would make that a silent mix-up rather than an error.
//!
//! # The semantics, and where each of them comes from
//!
//! `eventfd` is a 64-bit counter in the kernel, and every operation is defined in terms of it:
//!
//! | call | behaviour |
//! |---|---|
//! | `read` | 8 bytes. The whole counter, then zero — or `1` and a decrement in **semaphore** mode. Blocks while the counter is zero; `EAGAIN` when `EFD_NONBLOCK` |
//! | `write` | 8 bytes, added to the counter. `EINVAL` for `0xffffffffffffffff`. Blocks while the sum would exceed `0xfffffffffffffffe`; `EAGAIN` when `EFD_NONBLOCK` |
//! | `poll` | readable while the counter is non-zero; writable while a write of `1` would not block |
//!
//! A read or a write of **fewer than eight bytes is `EINVAL`**, which is the one part of this that
//! is easy to get wrong in a forgiving direction: a short read that returned what fitted would
//! hand the guest a truncated counter and lose the rest, because the read is destructive.
//!
//! # What this does **not** do, and why that is not a gap
//!
//! There is no blocking here. A zero-counter read on a descriptor without `EFD_NONBLOCK` reports
//! [`FsErrorKind::WouldBlock`], exactly as `pipe` does, and for the reason that module's
//! documentation gives: **the wait belongs to the caller.** D16's runaway-guest defence is built
//! from step budgets a sleeping thread does not consume, so how long a guest may block is the
//! adapter's policy and not this seam's. The readiness gate is shared with `pipe`, so a poll
//! already waits on both kinds at once.

use std::sync::{Arc, Mutex};

use super::error::{FsError, FsErrorKind, FsResult};
use super::pipe::{ReadyGate, Readiness};

/// `EFD_SEMAPHORE`. Linux's value, which is the guest's ABI.
pub const EFD_SEMAPHORE: i32 = 0o1;
/// `EFD_CLOEXEC`, which is `O_CLOEXEC`. Recorded and otherwise inert: this runtime does not
/// `exec`, so there is nothing for close-on-exec to do, and a guest that sets it is asking for a
/// property it cannot observe here.
pub const EFD_CLOEXEC: i32 = 0o2_000_000;
/// `EFD_NONBLOCK`, which is `O_NONBLOCK`.
pub const EFD_NONBLOCK: i32 = 0o4_000;

/// Every flag `eventfd2` defines. A flag outside this set is `EINVAL`, which is what the kernel
/// answers and what keeps a wrong argument from being silently accepted.
pub const KNOWN_FLAGS: i32 = EFD_SEMAPHORE | EFD_CLOEXEC | EFD_NONBLOCK;

/// The largest value the counter may hold, which is `u64::MAX - 1`.
///
/// **Not `u64::MAX`**, and the one below it is load-bearing: `0xffffffffffffffff` is the value a
/// `write` is required to reject with `EINVAL`, so the counter can never legitimately reach it
/// and a write that would is what blocks instead.
pub const MAX_COUNT: u64 = u64::MAX - 1;

/// How many bytes every `read` and `write` on an eventfd must be.
pub const TRANSFER_BYTES: usize = 8;

/// One `eventfd`, as its descriptor sees it.
///
/// The counter is behind its own mutex rather than the table's, for [`pipe`](super::pipe)'s
/// reason: the table lock is held across a read or a write, so the inner lock must be the *later*
/// edge in the one lock order this seam has, and nothing here waits while holding either.
#[derive(Debug)]
pub struct EventFd {
    count: Mutex<u64>,
    semaphore: bool,
    nonblocking: bool,
    gate: Arc<ReadyGate>,
}

impl EventFd {
    /// A new eventfd with `initval` in its counter.
    #[must_use]
    pub fn new(initval: u64, semaphore: bool, nonblocking: bool, gate: Arc<ReadyGate>) -> EventFd {
        EventFd { count: Mutex::new(initval), semaphore, nonblocking, gate }
    }

    fn count(&self) -> std::sync::MutexGuard<'_, u64> {
        self.count.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether `EFD_NONBLOCK` is set.
    #[must_use]
    pub fn nonblocking(&self) -> bool {
        self.nonblocking
    }

    /// Set or clear `EFD_NONBLOCK`, which `fcntl(F_SETFL)` does.
    pub fn set_nonblocking(&mut self, nonblocking: bool) {
        self.nonblocking = nonblocking;
    }

    /// Whether this eventfd is in `EFD_SEMAPHORE` mode.
    #[must_use]
    pub fn semaphore(&self) -> bool {
        self.semaphore
    }

    /// The counter, for a caller that needs to look at it. Diagnostic; the guest reads by `read`.
    #[must_use]
    pub fn value(&self) -> u64 {
        *self.count()
    }

    /// Readiness, by the table in this module's documentation.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        let count = *self.count();
        Readiness {
            readable: count > 0,
            // Writable while a write of **one** would not block, which is how Linux states it:
            // `poll` cannot know what the guest is about to write, so the smallest useful write is
            // the question it answers.
            writable: count < MAX_COUNT,
            hangup: false,
            error: false,
        }
    }

    /// `read(2)` on this eventfd.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::InvalidInput`] for a buffer shorter than eight bytes, and
    /// [`FsErrorKind::WouldBlock`] when the counter is zero.
    pub fn read(&self, buf: &mut [u8]) -> FsResult<usize> {
        const OP: &str = "read";
        if buf.len() < TRANSFER_BYTES {
            return Err(FsError::kinded(
                OP,
                "an eventfd",
                FsErrorKind::InvalidInput,
                "an eventfd read is eight bytes or nothing: the read is destructive, so a short \
                 read that returned what fitted would consume a counter the guest never saw",
            ));
        }
        let taken = {
            let mut count = self.count();
            if *count == 0 {
                return Err(FsError::kinded(
                    OP,
                    "an eventfd",
                    FsErrorKind::WouldBlock,
                    "the counter is zero. As `pipe`, the wait belongs to the caller: this seam has \
                     no opinion about how long a guest may block",
                ));
            }
            if self.semaphore {
                *count -= 1;
                1
            } else {
                std::mem::replace(&mut *count, 0)
            }
        };
        buf[..TRANSFER_BYTES].copy_from_slice(&taken.to_ne_bytes());
        // A read makes the descriptor **writable** where it may not have been, and un-readable
        // where the counter reached zero. Both are readiness changes a poller is entitled to see.
        self.gate.bump();
        Ok(TRANSFER_BYTES)
    }

    /// `write(2)` on this eventfd.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::InvalidInput`] for a buffer shorter than eight bytes or for the value
    /// `0xffffffffffffffff`, and [`FsErrorKind::WouldBlock`] when the sum would exceed
    /// [`MAX_COUNT`].
    pub fn write(&self, buf: &[u8]) -> FsResult<usize> {
        const OP: &str = "write";
        if buf.len() < TRANSFER_BYTES {
            return Err(FsError::kinded(
                OP,
                "an eventfd",
                FsErrorKind::InvalidInput,
                "an eventfd write is eight bytes or nothing",
            ));
        }
        let mut bytes = [0u8; TRANSFER_BYTES];
        bytes.copy_from_slice(&buf[..TRANSFER_BYTES]);
        let value = u64::from_ne_bytes(bytes);
        if value == u64::MAX {
            return Err(FsError::kinded(
                OP,
                "an eventfd",
                FsErrorKind::InvalidInput,
                "0xffffffffffffffff is the one value an eventfd write must reject: it is the \
                 counter's unreachable maximum, and accepting it would make the next read report \
                 a count no write produced",
            ));
        }
        {
            let mut count = self.count();
            // `checked_add` and then the bound, rather than the bound alone: the sum of two
            // values near `u64::MAX` wraps, and a wrapped sum is below the bound.
            match count.checked_add(value) {
                Some(sum) if sum <= MAX_COUNT => *count = sum,
                _ => {
                    return Err(FsError::kinded(
                        OP,
                        "an eventfd",
                        FsErrorKind::WouldBlock,
                        "the counter would pass 0xfffffffffffffffe. As `pipe`, the wait belongs \
                         to the caller",
                    ))
                }
            }
        }
        self.gate.bump();
        Ok(TRANSFER_BYTES)
    }
}
