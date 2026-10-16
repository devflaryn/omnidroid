//! **The OS's own lock**, for the few locks hundreds of guest threads take at once (the binder
//! broker's state, the poll queues). `parking_lot`'s waits spin with `sched_yield` before they
//! park, and on macOS a yield is a system call (`swtch_pri`) the scheduler makes a thread pay for:
//! with ~250 binder threads in the system's host process, yields were the largest single item of
//! its CPU in a game (M4, 2026-10-16, `sample`). `std::sync::Mutex` is `os_unfair_lock` there (a
//! wait in the kernel, the lock handed to the next waiter), a futex on Linux, an SRW lock on
//! Windows.
use std::sync::{MutexGuard, PoisonError};

/// A mutex over the OS's lock, never poisoned (a panic while holding it leaves the data as it is,
/// as `parking_lot`'s does).
#[derive(Default)]
pub(crate) struct OsMutex<T>(std::sync::Mutex<T>);

impl<T> OsMutex<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
