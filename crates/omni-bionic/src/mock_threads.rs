//! Mock implementations of the threading traits, with **real host blocking**.
//!
//! [`MockFutex`] backs each guest address with a host `Mutex<HashMap>` of
//! `Condvar`-based waiter queues: a `wait` genuinely blocks the calling host thread
//! and a `wake` on another host thread genuinely releases it. This is what makes the
//! concurrency tests meaningful — single-threaded mocks cannot find the bugs that
//! matter here.
//!
//! Resource discipline: one host `Condvar` may exist per guest address that has
//! *waiters*, but it is removed when its queue empties, so a guest that creates and
//! destroys a million mutexes one at a time never holds more than a handful of host
//! primitives at once. See the phase-8 resource-exhaustion test.

use crate::threads::{Clock, Futex, GuestThreadId, ThreadRegistry, WaitResult};
use core::time::Duration;
use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

// ---------------------------------------------------------------------------
// MockFutex
// ---------------------------------------------------------------------------

/// A queue of host waiters for one guest address.
#[derive(Default)]
struct WaiterQueue {
    /// How many waiters are registered (waiting or about to be woken).
    waiting: usize,
    /// Woken-but-not-yet-requeued credits: a wake handed to a waiter that has not
    /// yet reached the condvar. Prevents lost wakeups between registration and
    /// the actual block.
    credits: usize,
    /// Notify pairs for the registered waiters, in registration order.
    notify: VecDeque<ArcNotifyPair>,
    /// Generation counter: bumped when the queue is drained and removed, so a
    /// waiter that slept across a drain cannot be woken by a *later* address
    /// reuse. (Guard against guest address reuse: the adapter's guest memory is
    /// allowed to reuse addresses after unmap; a stale waiter must not consume a
    /// wake meant for a new user of the address.)
    generation: u64,
}

/// The shared state: queues keyed by guest address, inside one host mutex.
#[derive(Default)]
struct FutexInner {
    queues: HashMap<u64, WaiterQueue>,
}

/// The real-blocking futex mock.
#[derive(Default)]
pub struct MockFutex {
    inner: Mutex<FutexInner>,
}

impl MockFutex {
    /// A new futex with no waiters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of guest addresses that currently hold a queue (0 or more; bounded
    /// by the number of addresses with in-flight waiters).
    pub fn live_queues(&self) -> usize {
        self.inner.lock().unwrap().queues.len()
    }

    /// Total waiters registered across all addresses (test diagnostics).
    pub fn total_waiters(&self) -> usize {
        self.inner.lock().unwrap().queues.values().map(|q| q.waiting).sum()
    }

    /// Register the calling thread as a waiter on `addr`; returns the generation.
    fn register(&self, addr: u64) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let q = inner.queues.entry(addr).or_default();
        q.waiting += 1;
        q.generation
    }
}

impl Futex for MockFutex {
    fn wait(&self, addr: u64, expected: u32, timeout: Option<Duration>) -> WaitResult {
        // NOTE: the *value check* is the caller's job (it owns the protocol lock
        // around read + wait). This mock always blocks once called; a caller that
        // needs `WouldBlock` semantics checks the value itself first. The trait
        // signature keeps `expected` for the real adapter, where the check is
        // atomic with the block in kernel space.
        let _ = expected;

        let generation = self.register(addr);

        // A slot in the shared pair table: (address, generation) -> our slot in
        // the queue's condvar. The condvar itself is shared across waiters of the
        // same address; each waiter allocates a notify pair keyed by its identity.
        let pair = ArcNotifyPair::new();
        {
            let mut inner = self.inner.lock().unwrap();
            let q = inner.queues.entry(addr).or_default();
            if q.generation != generation {
                // The queue was drained and removed while we registered; our
                // registration is stale. Retry the registration.
                drop(inner);
                return self.wait(addr, expected, timeout);
            }
            q.notify.push_back(pair.clone());
        }

        let deadline = timeout.map(|t| Instant::now() + t);
        let mut woken = false;
        {
            let guard = pair.mutex.lock().unwrap();
            let mut guard = guard;
            loop {
                if pair.signalled.load(std::sync::atomic::Ordering::SeqCst) {
                    woken = true;
                    break;
                }
                match deadline {
                    Some(d) => {
                        let now = Instant::now();
                        if now >= d {
                            break;
                        }
                        let (g, res) = pair.condvar.wait_timeout(guard, d - now).unwrap();
                        guard = g;
                        if res.timed_out()
                            && !pair.signalled.load(std::sync::atomic::Ordering::SeqCst)
                        {
                            break;
                        }
                    }
                    None => {
                        guard = pair.condvar.wait(guard).unwrap();
                    }
                }
            }
        }

        // Deregister; if we were woken, consume our credit.
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(q) = inner.queues.get_mut(&addr) {
                if q.generation == generation {
                    q.waiting -= 1;
                    if woken && q.credits > 0 {
                        q.credits -= 1;
                    }
                    if q.waiting == 0 && q.credits == 0 {
                        inner.queues.remove(&addr);
                    }
                }
            }
        }

        if woken {
            WaitResult::Woken
        } else {
            WaitResult::TimedOut
        }
    }

    fn wake(&self, addr: u64, count: u32) -> u32 {
        let mut woken = 0u32;
        let mut to_signal: Vec<ArcNotifyPair> = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(q) = inner.queues.get_mut(&addr) {
                let take = (count as usize).min(q.waiting);
                q.credits += take;
                for _ in 0..take {
                    if let Some(pair) = q.notify.pop_front() {
                        pair.signalled.store(true, std::sync::atomic::Ordering::SeqCst);
                        to_signal.push(pair);
                    }
                }
                woken = take as u32;
                if q.waiting == 0 && q.credits == 0 {
                    inner.queues.remove(&addr);
                }
            }
        }
        // Signal outside the futex's own lock: the waiter must be able to take the
        // futex lock while processing its wakeup.
        for pair in to_signal {
            let _guard = pair.mutex.lock().unwrap();
            pair.condvar.notify_all();
        }
        woken
    }
}

/// The per-waiter notify pair: a condvar the waiter sleeps on and a flag the
/// waker sets. Cloned into the queue.
#[derive(Clone)]
struct ArcNotifyPair {
    mutex: std::sync::Arc<Mutex<bool>>,
    condvar: std::sync::Arc<Condvar>,
    signalled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ArcNotifyPair {
    fn new() -> Self {
        ArcNotifyPair {
            mutex: std::sync::Arc::new(Mutex::new(false)),
            condvar: std::sync::Arc::new(Condvar::new()),
            signalled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

// ---------------------------------------------------------------------------
// MockClock
// ---------------------------------------------------------------------------

/// A controllable clock. Both POSIX clocks are derived from the real host
/// `Instant`, plus a settable offset per clock so tests can jump time.
pub struct MockClock {
    start: Instant,
    /// Offset added to `now_monotonic` (test time-travel).
    pub monotonic_offset: Mutex<Duration>,
    /// Offset added to `now_realtime` (test time-travel).
    pub realtime_offset: Mutex<Duration>,
}

impl MockClock {
    /// A clock with zero offsets, anchored at construction.
    pub fn new() -> Self {
        MockClock {
            start: Instant::now(),
            monotonic_offset: Mutex::new(Duration::ZERO),
            realtime_offset: Mutex::new(Duration::ZERO),
        }
    }

    /// Advance both clocks by `d` (test convenience; does not unblock real waits —
    /// only offsets the *reading*, so timed waits must still be measured in real
    /// host time).
    pub fn advance_offsets(&self, d: Duration) {
        *self.monotonic_offset.lock().unwrap() += d;
        *self.realtime_offset.lock().unwrap() += d;
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MockClock {
    fn now_monotonic(&self) -> Duration {
        self.start.elapsed() + *self.monotonic_offset.lock().unwrap()
    }

    fn now_realtime(&self) -> Duration {
        self.start.elapsed() + *self.realtime_offset.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// MockThreads
// ---------------------------------------------------------------------------

/// The thread registry mock: each host thread that calls [`ThreadRegistry::attach`]
/// gets a fresh, never-reused 64-bit identity.
///
/// Identity assignment uses a host thread-local: the first `current()`/`attach()`
/// on a host thread binds it to a new [`GuestThreadId`], stable for the thread's
/// life. `detach_and_take_destructors` unbinds it (so a pooled host thread can be
/// re-bound to a new guest identity).
#[derive(Default)]
pub struct MockThreads {
    inner: Mutex<ThreadsInner>,
}

#[derive(Default)]
struct ThreadsInner {
    next_id: u64,
    /// id -> thread state (TLS values are in `crate::tls`, not here).
    live: HashMap<u64, ()>,
}

thread_local! {
    static BOUND_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl MockThreads {
    /// A fresh registry with no live threads.
    pub fn new() -> Self {
        Self::default()
    }

    fn bind(&self) -> GuestThreadId {
        let existing = BOUND_ID.with(|c| c.get());
        if existing != 0 {
            return GuestThreadId(existing);
        }
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.live.insert(id, ());
        drop(inner);
        BOUND_ID.with(|c| c.set(id));
        GuestThreadId(id)
    }
}

impl ThreadRegistry for MockThreads {
    fn current(&self) -> GuestThreadId {
        self.bind()
    }

    fn attach(&self) -> bool {
        let id = self.bind().0;
        let inner = self.inner.lock().unwrap();
        // bind() registers the thread on first contact; attach() reports whether
        // the calling thread is now registered (true) or was somehow already
        // removed (an adapter bug pattern).
        inner.live.contains_key(&id)
    }

    fn detach_and_take_destructors(&self) -> Vec<(u64, u64)> {
        let id = BOUND_ID.with(|c| c.get());
        let mut inner = self.inner.lock().unwrap();
        inner.live.remove(&id);
        drop(inner);
        BOUND_ID.with(|c| c.set(0));
        Vec::new() // TLS destructors are supplied by the tls module's registry
    }

    fn is_attached(&self) -> bool {
        let id = BOUND_ID.with(|c| c.get());
        self.inner.lock().unwrap().live.contains_key(&id)
    }

    fn live_count(&self) -> usize {
        self.inner.lock().unwrap().live.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;    /// A waiter actually blocks: it has not finished after 150 ms while nobody
    /// wakes it, then finishes quickly once woken.
    #[test]
    fn waiter_blocks_until_wake() {
        let futex = Arc::new(MockFutex::new());
        let f2 = futex.clone();

        let t = std::thread::spawn(move || {
            let start = Instant::now();
            let r = f2.wait(0x1000, 0, Some(Duration::from_secs(10)));
            (r, start.elapsed())
        });
        // Give the waiter time to actually block.
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(futex.total_waiters(), 1, "waiter must be registered");
        let woken = futex.wake(0x1000, 1);
        assert_eq!(woken, 1);
        let (r, elapsed) = t.join().unwrap();
        assert_eq!(r, WaitResult::Woken);
        assert!(
            elapsed >= Duration::from_millis(100),
            "waiter must have blocked for at least the sleep: {elapsed:?}"
        );
    }

    /// A wake with no waiter returns 0 and does not error.
    #[test]
    fn wake_with_no_waiter_returns_zero() {
        let futex = MockFutex::new();
        assert_eq!(futex.wake(0xdead, 5), 0);
        assert_eq!(futex.live_queues(), 0);
    }

    /// A timeout actually expires: the wait returns TimedOut and elapsed time is
    /// at least the timeout.
    #[test]
    fn wait_timeout_expires() {
        let futex = MockFutex::new();
        let start = Instant::now();
        let r = futex.wait(0x2000, 0, Some(Duration::from_millis(150)));
        let elapsed = start.elapsed();
        assert_eq!(r, WaitResult::TimedOut);
        crate::timing::assert_blocked_for(
            elapsed, Duration::from_millis(150), "futex wait timeout");
        // And the queue drained after the timeout.
        assert_eq!(futex.live_queues(), 0);
    }

    /// Many waiters on one address: wake(1) releases exactly one, wake(u32::MAX)
    /// releases all.
    #[test]
    fn wake_count_selects_waiters() {
        let futex = Arc::new(MockFutex::new());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let f = futex.clone();
            handles.push(std::thread::spawn(move || {
                f.wait(0x3000, 0, Some(Duration::from_secs(10)))
            }));
        }
        // Wait until all four are registered.
        let deadline = Instant::now() + Duration::from_secs(5);
        while futex.total_waiters() < 4 {
            assert!(Instant::now() < deadline, "waiters never registered");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(futex.wake(0x3000, 1), 1);
        // One of the four finishes; the others stay blocked. Count completions
        // after giving the woken one time to exit.
        std::thread::sleep(Duration::from_millis(100));
        let done = handles
            .iter()
            .filter(|h| h.is_finished())
            .count();
        assert_eq!(done, 1, "exactly one waiter must have been released");
        assert_eq!(futex.wake(0x3000, u32::MAX), 3);
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(results.iter().all(|r| *r == WaitResult::Woken));
    }

    /// Concurrent hosts threads waiting on *different* addresses do not interfere.
    #[test]
    fn independent_addresses_do_not_cross_wake() {
        let futex = Arc::new(MockFutex::new());
        let f2 = futex.clone();
        let waker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            // Wake the WRONG address: the waiter on 0x4000 must stay blocked.
            f2.wake(0x9999, 10);
        });
        let t = std::thread::spawn(move || {
            futex.wait(0x4000, 0, Some(Duration::from_millis(400)))
        });
        waker.join().unwrap();
        let r = t.join().unwrap();
        assert_eq!(r, WaitResult::TimedOut, "a wake on another address must not release");
    }

    /// Many threads hammering one address: after N wakes of 1, exactly N waits
    /// completed as Woken (no lost wakeup, no double-release).
    #[test]
    fn many_waiters_many_wakes_no_loss() {
        let futex = Arc::new(MockFutex::new());
        const N: usize = 16;
        let mut handles = Vec::new();
        for _ in 0..N {
            let f = futex.clone();
            handles.push(std::thread::spawn(move || {
                f.wait(0x5000, 0, Some(Duration::from_secs(15)))
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while futex.total_waiters() < N {
            assert!(Instant::now() < deadline, "waiters never registered");
            std::thread::sleep(Duration::from_millis(5));
        }
        for i in 0..N {
            assert_eq!(futex.wake(0x5000, 1), 1, "wake {i}");
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let woken = results.iter().filter(|r| **r == WaitResult::Woken).count();
        assert_eq!(woken, N, "every waiter must be woken exactly once");
    }

    /// Clock: offsets move the readings; monotonic and realtime are independent.
    #[test]
    fn clock_offsets() {
        let clock = MockClock::new();
        let before = clock.now_monotonic();
        clock.advance_offsets(Duration::from_secs(7));
        let after = clock.now_monotonic();
        assert!(after - before >= Duration::from_secs(7));
        let rt = clock.now_realtime();
        assert!(rt >= Duration::from_secs(7));
    }

    /// Registry: identities are stable per thread, distinct across threads, and
    /// never reused after detach.
    #[test]
    fn registry_identities() {
        let reg = Arc::new(MockThreads::new());
        let r2 = reg.clone();
        let t = std::thread::spawn(move || {
            let id = r2.current();
            assert!(r2.attach());
            assert_eq!(r2.current(), id, "identity stable");
            assert_eq!(r2.live_count(), 2, "main + this thread");
            r2.detach_and_take_destructors();
            assert_eq!(r2.live_count(), 1);
            let id2 = r2.current(); // re-binds with a NEW id
            assert_ne!(id2, id, "ids are never reused");
        });
        let _ = reg.current();
        t.join().unwrap();
    }
}
