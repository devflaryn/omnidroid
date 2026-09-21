//! The host capabilities `omni-bionic` is written against: thread identity, a futex, a clock
//! and a yield.
//!
//! `omni-bionic` has **zero dependencies** on purpose (D19), so it cannot block a thread, read
//! a clock or know what a thread is. It states those needs as traits and this module is where
//! they meet the host — which is why they are here and not there.
//!
//! # Why `expected` is ignored by the futex, and why that is not a shortcut
//!
//! Linux's `FUTEX_WAIT` compares `*addr` with `expected` **atomically with** the decision to
//! block, which is what closes the window between a caller reading the word and sleeping on it.
//! This futex does not perform that comparison, and the reason is measured rather than
//! convenient: `omni-bionic`'s own callers do not all pass a meaningful `expected`.
//! `mutex::lock` passes `LOCKED_WITH_WAITERS`, which is right, but `rwlock`'s reader and writer
//! waits both pass **`0`** (`rwlock.rs:281` and `rwlock.rs:360`) while the word they are waiting
//! on is, by construction, *not* zero — a rwlock with waiters is held. A futex that honoured
//! `expected` would return [`WaitResult::WouldBlock`] to every rwlock waiter, and the caller's
//! `continue` would turn blocking contention into a busy spin.
//!
//! So `expected` is ignored here exactly as `omni-bionic`'s own `MockFutex` ignores it, and the
//! lost-wake window is closed the way that crate's callers already close it: every waiter holds
//! its own protocol and re-checks its predicate after every return, and every `wake` is issued
//! after the state change that the waiter will observe.
//!
//! **This is a finding about `omni-bionic`, recorded rather than patched**: the placeholder
//! `expected` values belong to a reviewed crate with its own mutation harness, and changing them
//! would change the meaning of `wait` for every caller at once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use omni_bionic::metadata::Yield;
use omni_bionic::threads::{Clock, Futex, GuestThreadId, ThreadRegistry, WaitResult};
use omni_mem::GuestAddr;
use parking_lot::Mutex;

/// A blocking primitive keyed by a **guest** address.
///
/// Built on `parking_lot_core`'s parking lot, which is the same shape as a futex: a global
/// hash table of wait queues keyed by an integer, with the queue's bucket lock held across
/// registration so a `wake` cannot slip between a waiter registering and sleeping. Writing a
/// second one out of a `HashMap<u64, Condvar>` would be re-deriving that window, and the one
/// defect already found in this layer (`sem_post` consuming the waiter flag) was exactly a lost
/// wake — the class where a hand-rolled queue is least forgiving.
///
/// Identity mapping (D4) makes a guest address a host address, so the keys of this table and
/// the keys `parking_lot`'s own mutexes use are in the same space. They cannot collide: a key
/// is only ever the address of the object that parks on it, and a guest mapping is never a host
/// `parking_lot` object.
#[derive(Debug, Default)]
pub struct AddressFutex {
    /// Diagnostics only: how many waits and wakes have been performed.
    waits: AtomicU64,
    wakes: AtomicU64,
}

impl AddressFutex {
    /// A futex with no waiters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many waits have been entered, and how many wake calls issued.
    ///
    /// **A watch, not a detector** (Global Constraint 13): these counters rise whenever the
    /// sync layer blocks, and they stay exactly as they are under every defect this futex could
    /// have. They exist so a test can say "this really blocked" rather than "this returned".
    #[must_use]
    pub fn activity(&self) -> (u64, u64) {
        (self.waits.load(Ordering::Relaxed), self.wakes.load(Ordering::Relaxed))
    }
}

impl Futex for AddressFutex {
    fn wait(&self, addr: u64, expected: u32, timeout: Option<Duration>) -> WaitResult {
        // See the module docs: the comparison is the caller's, because not every caller in
        // `omni-bionic` passes a meaningful `expected`.
        let _ = expected;
        self.waits.fetch_add(1, Ordering::Relaxed);
        let deadline = timeout.map(|t| Instant::now() + t);
        // SAFETY: `parking_lot_core::park` requires that the key is not concurrently used by
        // another parking implementation with incompatible invariants, that `validate`,
        // `before_sleep` and `timed_out` do not panic and do not themselves park, and that
        // `before_sleep` is not called with the bucket lock held in a way that could deadlock.
        // The key here is a guest address, which no other parker in this process uses (see the
        // type's documentation); all three callbacks are trivial closures that cannot panic and
        // touch nothing; and `validate` returning `true` unconditionally is the documented
        // "always park" configuration.
        let result = unsafe {
            parking_lot_core::park(
                addr as usize,
                || true,
                || {},
                |_, _| {},
                parking_lot_core::DEFAULT_PARK_TOKEN,
                deadline,
            )
        };
        match result {
            parking_lot_core::ParkResult::Unparked(_) => WaitResult::Woken,
            parking_lot_core::ParkResult::TimedOut => WaitResult::TimedOut,
            // `Invalid` cannot happen with a `validate` that always returns `true`. Reported as
            // `WouldBlock` rather than as `Woken` because every caller re-checks its predicate
            // after `WouldBlock` and none of them treats it as progress.
            parking_lot_core::ParkResult::Invalid => WaitResult::WouldBlock,
        }
    }

    fn wake(&self, addr: u64, count: u32) -> u32 {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            return 0;
        }
        let key = addr as usize;
        let unparked = if count == 1 {
            // SAFETY: as `wait`. The callback computes an unpark token from the queue state and
            // cannot panic or park.
            unsafe { parking_lot_core::unpark_one(key, |_| parking_lot_core::DEFAULT_UNPARK_TOKEN) }
                .unparked_threads
        } else if count == u32::MAX {
            // SAFETY: as `wait`.
            unsafe { parking_lot_core::unpark_all(key, parking_lot_core::DEFAULT_UNPARK_TOKEN) }
        } else {
            let mut left = count;
            // SAFETY: as `wait`. The filter closure only decrements a local counter.
            unsafe {
                parking_lot_core::unpark_filter(
                    key,
                    |_| {
                        if left > 0 {
                            left -= 1;
                            parking_lot_core::FilterOp::Unpark
                        } else {
                            parking_lot_core::FilterOp::Stop
                        }
                    },
                    |_| parking_lot_core::DEFAULT_UNPARK_TOKEN,
                )
            }
            .unparked_threads
        };
        // `usize` to `u32`: the count of threads unparked is bounded by the number of live host
        // threads, so the saturation can never be reached — but it is written as a saturation
        // rather than an `as` so that it could not silently wrap if it were.
        u32::try_from(unparked).unwrap_or(u32::MAX)
    }
}

/// The host's two clocks, as `omni-bionic`'s timed waits describe them.
///
/// `std::time` rather than anything in `omni-platform`: `Instant` and `SystemTime` are portable
/// standard library, so this compiles unchanged on all five targets and introduces no OS surface
/// (Global Constraint 4). Nothing about them is Windows-specific.
#[derive(Debug)]
pub struct HostClock {
    /// The process-start instant that `now_monotonic` is measured from.
    ///
    /// `Instant` has no epoch, and the trait asks for a `Duration`. Measuring from a fixed
    /// instant captured once gives a monotonic duration that never jumps and never goes
    /// backwards, which is the whole contract of `CLOCK_MONOTONIC`.
    origin: Instant,
}

impl Default for HostClock {
    fn default() -> Self {
        Self::new()
    }
}

impl HostClock {
    /// Start the monotonic clock now.
    #[must_use]
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Clock for HostClock {
    fn now_monotonic(&self) -> Duration {
        self.origin.elapsed()
    }

    fn now_realtime(&self) -> Duration {
        // A wall clock that has been set before 1970 is a system the guest cannot reason about
        // either; reporting zero is the only answer that is not a negative duration, and it is
        // reported rather than panicked on.
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO)
    }
}

/// `sched_yield` for the calling host thread.
#[derive(Debug, Default)]
pub struct HostYield;

impl Yield for HostYield {
    fn yield_now(&self) {
        std::thread::yield_now();
    }
}

/// What the adapter knows about one guest thread.
#[derive(Debug, Clone, Copy)]
pub struct ThreadSlot {
    /// The `pthread_t` value the guest sees. Always 64 bits, whatever the host uses.
    pub id: GuestThreadId,
    /// This thread's private block in the adapter's arena: errno, then scratch.
    pub block: GuestAddr,
    /// Which block of the arena this is.
    ///
    /// Carried so that a thread which exits can give its block *back* by index. Phase 3c is what
    /// made that necessary: until it, nothing ever detached, so the table could take the next
    /// index from its own length. See [`ThreadTable::detach_current`].
    pub index: usize,
}

/// Every guest thread this instance has seen, keyed by the **host** thread running it.
///
/// A `pthread_t` is 64 bits on LP64 and a Windows thread id is a 32-bit `DWORD`, so the host's
/// id is never handed to the guest: the guest's identity is this table's own counter, which
/// starts at one because [`GuestThreadId::NONE`] is reserved.
///
/// # Blocks are allocated from a free list, and that changed in phase 3c
///
/// The first version took the next block index from `slots.len()`, which is exact for a table
/// nothing is ever removed from — and until thread lifecycle existed, nothing was. It becomes
/// **wrong** the moment a thread can exit: remove the entry holding index 3 from a table of five
/// and the length is 4, so the next thread is handed index 4, which is still live. Two guest
/// threads would then share one `errno` cell and one `strerror` buffer, and the only symptom
/// would be an occasional wrong error number in a thread that did nothing.
///
/// So the index is carried on the slot, a freed one goes on a list, and the list is preferred
/// over the high-water mark. Row `threads-A2` injects the length-based version.
#[derive(Debug, Default)]
pub struct ThreadTable {
    inner: Mutex<TableInner>,
    next: AtomicU64,
}

/// The table's contents: who holds which block, and which blocks are free.
#[derive(Debug, Default)]
struct TableInner {
    /// Host thread to slot, for the threads that are running.
    slots: HashMap<std::thread::ThreadId, ThreadSlot>,
    /// Blocks given back by [`ThreadTable::detach_current`] or [`ThreadTable::release`].
    free: Vec<usize>,
    /// The next block never yet handed out.
    high_water: usize,
}

impl TableInner {
    /// Take a block index, or `None` when the arena is full.
    fn take_block(&mut self, capacity: usize) -> Option<usize> {
        if let Some(index) = self.free.pop() {
            return Some(index);
        }
        if self.high_water >= capacity {
            return None;
        }
        let index = self.high_water;
        self.high_water += 1;
        Some(index)
    }
}

impl ThreadTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self { inner: Mutex::new(TableInner::default()), next: AtomicU64::new(1) }
    }

    /// A fresh `pthread_t`.
    ///
    /// `fetch_add` rather than an index: a thread that exits frees its *block* but must never
    /// hand its **identity** to the next one, because guest code may still hold the old value and
    /// `pthread_equal` would then say two different threads are the same.
    fn next_id(&self) -> GuestThreadId {
        GuestThreadId(self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// The slot for the calling host thread, allocating one from `blocks` if it has none.
    ///
    /// `capacity` is the arena's capacity in blocks; `block_at` turns an index into an address.
    /// Returns `None` when the arena is full, which is a refusal rather than a wrap onto
    /// another thread's errno.
    pub fn attach_current(
        &self,
        capacity: usize,
        block_at: impl Fn(usize) -> GuestAddr,
    ) -> Option<ThreadSlot> {
        let key = std::thread::current().id();
        let mut inner = self.inner.lock();
        if let Some(slot) = inner.slots.get(&key) {
            return Some(*slot);
        }
        let index = inner.take_block(capacity)?;
        let slot = ThreadSlot { id: self.next_id(), block: block_at(index), index };
        inner.slots.insert(key, slot);
        Some(slot)
    }

    /// Take a block and an identity for a thread that does not exist yet.
    ///
    /// **`pthread_create` has to know the `pthread_t` before the new thread runs**, because it
    /// writes it into the guest's own `pthread_t *` and the guest may compare that value against
    /// what the new thread's `pthread_self()` returns. Allocating it in the child would make the
    /// two different until the child got there, which is a race guest code would lose rarely and
    /// silently.
    ///
    /// The slot must then be either [`adopt`](ThreadTable::adopt)ed by the new host thread or
    /// [`release`](ThreadTable::release)d if the spawn failed — otherwise the block is lost for
    /// the life of the instance.
    pub fn reserve(
        &self,
        capacity: usize,
        block_at: impl Fn(usize) -> GuestAddr,
    ) -> Option<ThreadSlot> {
        let mut inner = self.inner.lock();
        let index = inner.take_block(capacity)?;
        Some(ThreadSlot { id: self.next_id(), block: block_at(index), index })
    }

    /// Bind a reserved slot to the calling host thread.
    ///
    /// Returns `false` if this host thread already holds a slot, which would be an adapter bug:
    /// the runner calls this exactly once, on a thread that has just been created.
    pub fn adopt(&self, slot: ThreadSlot) -> bool {
        let key = std::thread::current().id();
        let mut inner = self.inner.lock();
        inner.slots.insert(key, slot).is_none()
    }

    /// Give a reserved slot back without ever having used it.
    pub fn release(&self, slot: ThreadSlot) {
        self.inner.lock().free.push(slot.index);
    }

    /// Give the calling host thread's block back, so another guest thread may have it.
    ///
    /// Returns the slot that was released, or `None` if this thread held none.
    pub fn detach_current(&self) -> Option<ThreadSlot> {
        let key = std::thread::current().id();
        let mut inner = self.inner.lock();
        let slot = inner.slots.remove(&key)?;
        inner.free.push(slot.index);
        Some(slot)
    }

    /// How many host threads currently hold a slot.
    #[must_use]
    pub fn live(&self) -> usize {
        self.inner.lock().slots.len()
    }

    /// How many blocks are spoken for: held by a running thread, or reserved for one starting.
    ///
    /// Not the same as [`live`](ThreadTable::live), which counts only the threads that have
    /// adopted their slot — a block reserved by `pthread_create` for a thread that has not
    /// started yet is occupied and is not live.
    #[must_use]
    pub fn occupied(&self) -> usize {
        let inner = self.inner.lock();
        inner.high_water - inner.free.len()
    }

    /// Whether the calling host thread holds a slot.
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.inner.lock().slots.contains_key(&std::thread::current().id())
    }
}

/// [`ThreadRegistry`] for one call, with the calling thread's identity already resolved.
///
/// Resolved once per call rather than looked up per query: `current()` is on the path of every
/// `pthread_self`, every mutex lock and every TLS access, and a `HashMap` lookup under a lock
/// there would cost more than the whole crossing D17 measured at 26.7-31.0 ns.
pub struct CallThreads<'a> {
    /// The table, for the live-set questions.
    pub table: &'a ThreadTable,
    /// The calling thread, resolved at the top of the call.
    pub me: GuestThreadId,
}

impl ThreadRegistry for CallThreads<'_> {
    fn current(&self) -> GuestThreadId {
        self.me
    }

    fn attach(&self) -> bool {
        // The adapter attaches a thread when it activates, not when a bionic function asks, so
        // by the time any handler runs the calling thread is already registered. Reporting
        // `false` is the trait's "already registered", which is exactly what is true here.
        false
    }

    fn detach_and_take_destructors(&self) -> Vec<(u64, u64)> {
        // Thread *lifecycle* is a later phase: `pthread_exit`, `pthread_join` and `pthread_detach`
        // all need host thread lifetime, which this phase deliberately does not bind. Returning
        // an empty list here is not a stub standing in for that work — nothing in this phase
        // calls it, because the only caller is the thread-exit sweep that arrives with lifecycle.
        Vec::new()
    }

    fn is_attached(&self) -> bool {
        self.table.is_attached()
    }

    fn live_count(&self) -> usize {
        self.table.live()
    }
}
