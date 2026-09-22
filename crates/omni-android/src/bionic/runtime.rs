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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// The guest address of the most recent park. See [`AddressFutex::parked_on`].
    last_wait: AtomicU64,
    /// Every address currently parked on, and how many threads are on each.
    ///
    /// **Kept so that a shutdown can reach them.** `parking_lot_core` has no "unpark everything"
    /// across all keys -- it is a hash table and there is no key list -- so the only way to wake
    /// every waiter is to know which addresses have one. Maintained around the park itself, so
    /// an entry exists for exactly as long as a thread is on that queue.
    parked: Mutex<HashMap<u64, usize>>,
    /// Set when the instance is shutting down. See [`AddressFutex::stop`].
    stopping: AtomicBool,
    /// How many parks were entered with **no deadline**. See [`AddressFutex::indefinite_parks`].
    indefinite: AtomicU64,
    /// Wakes that landed within eight bytes of a waiter without landing on it. See
    /// [`AddressFutex::near_misses`].
    near_misses: Mutex<Vec<(u64, u64)>>,
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

    /// Park on `addr` **only if `still_expected()` still holds**, checked atomically with the
    /// decision to block.
    ///
    /// # This is the comparison the trait's `wait` deliberately does not make
    ///
    /// The module documentation explains why [`Futex::wait`] ignores its `expected`: some of
    /// `omni-bionic`'s own callers pass a placeholder, and honouring it there would turn blocking
    /// contention into a busy spin. That argument is about *those callers*. It says nothing about
    /// a caller that has a real word and a real value to compare it against — and a **raw
    /// `futex(FUTEX_WAIT)` from guest code is exactly that**: Linux's contract is that the kernel
    /// compares `*uaddr` with `val` and returns `EAGAIN` without sleeping if they differ, and a
    /// waiter that skipped the comparison would sleep through a wake that had already happened.
    ///
    /// So the capability is added here rather than by changing what `wait` means for everyone.
    ///
    /// # Why this closes the window, and where the closure runs
    ///
    /// `still_expected` is `parking_lot_core::park`'s **`validate`** callback, which runs with the
    /// queue's bucket lock held. A concurrent [`Futex::wake`] on the same address must take that
    /// lock to find the queue, so it cannot land between the comparison and the park. That is the
    /// same property the kernel gets from its hash-bucket spinlock, and it is the whole reason
    /// this is a callback rather than a value compared before the call.
    ///
    /// **The closure must not panic and must not itself park**, which is `parking_lot_core`'s
    /// requirement. A reader of guest memory satisfies both: it returns a `Result` and takes only
    /// the address space's own lock. That lock is safe to take here because **nothing in this
    /// runtime parks while holding it** — the pager's own invariant is that the thread running
    /// guest code must not hold the space lock, so the inversion that would deadlock (hold the
    /// space lock, then wait on a futex) has no path.
    ///
    /// Returns [`WaitResult::WouldBlock`] when the comparison failed, which is the caller's
    /// `EAGAIN`.
    pub fn wait_compared(
        &self,
        addr: u64,
        still_expected: impl Fn() -> bool,
        timeout: Option<Duration>,
    ) -> WaitResult {
        if self.stopped() {
            return WaitResult::WouldBlock;
        }
        let _parked = self.enter_park(addr);
        self.waits.fetch_add(1, Ordering::Relaxed);
        self.last_wait.store(addr, Ordering::Relaxed);
        if timeout.is_none() {
            self.indefinite.fetch_add(1, Ordering::Relaxed);
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        // SAFETY: as `Futex::wait`, and with one addition. `park` requires that the key is not
        // concurrently used by another parking implementation with incompatible invariants — the
        // key is a guest address, which no other parker in this process uses — and that
        // `validate`, `before_sleep` and `timed_out` neither panic nor park. `before_sleep` and
        // `timed_out` are empty. `validate` is the caller's `still_expected`, whose contract is
        // stated above and is discharged by its only caller, which reads four bytes of guest
        // memory through the checked path.
        let result = unsafe {
            parking_lot_core::park(
                addr as usize,
                still_expected,
                || {},
                |_, _| {},
                parking_lot_core::DEFAULT_PARK_TOKEN,
                deadline,
            )
        };
        match result {
            parking_lot_core::ParkResult::Unparked(_) => WaitResult::Woken,
            parking_lot_core::ParkResult::TimedOut => WaitResult::TimedOut,
            // `validate` said the word had already changed. **This is the answer, not a
            // degenerate case**: it is `FUTEX_WAIT`'s `EAGAIN`, and it is the whole value of
            // performing the comparison.
            parking_lot_core::ParkResult::Invalid => WaitResult::WouldBlock,
        }
    }

    /// **Stop accepting waits, and wake everything already parked.**
    ///
    /// # Why a stop switch on the *futex* and not only on the thread runner
    ///
    /// `Bionic::stop_guest_threads` asks every created guest thread to stop, and `drive` reads
    /// that between run windows — which reaches a thread that is *executing* and not one that is
    /// *parked*. A parked thread executes no guest instructions, so no window ever ends for it.
    ///
    /// MEASURED, and it arrived the moment the raw `futex` syscall was implemented: before it,
    /// the engine's worker threads died on the refusal and teardown was quiet; after it, they
    /// lived, parked indefinitely, and **two of them were still running when the gate asked them
    /// to stop**. Implementing a blocking primitive correctly is what made its shutdown path
    /// reachable.
    ///
    /// So this does both halves: it wakes every address a thread is parked on, and it makes every
    /// later wait return [`WaitResult::WouldBlock`] without parking. The second half is what stops
    /// a woken thread simply parking again — every caller of a futex re-checks its predicate and
    /// loops, so waking without refusing is a wake the guest immediately undoes.
    ///
    /// **A `WouldBlock` loop is a spin, and that is bounded rather than ignored.** A guest that
    /// spins consumes its run window, and `drive` reads the thread-runner's stop switch at the end
    /// of it — so the spin lasts at most one window (`GUEST_THREAD_STEP_WINDOW`, a million guest
    /// instructions) and then the thread stops. That is D16's mechanism doing exactly what it was
    /// built for: the window boundary is a decision point that exists whatever the guest is doing.
    ///
    /// Idempotent, and it cannot be taken back: a futex that has been stopped belongs to an
    /// instance that is shutting down.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let addresses: Vec<u64> = self.parked.lock().keys().copied().collect();
        for address in addresses {
            // Every waiter on every address, not one each: a queue with three threads on it needs
            // three wakes, and `unpark_all` is the operation that does not have to know how many.
            omni_bionic::threads::Futex::wake(self, address, u32::MAX);
        }
    }

    /// Whether [`stop`](AddressFutex::stop) has been called.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// How many threads are parked, and on how many distinct addresses.
    ///
    /// **A detector rather than a watch**, unlike [`activity`](AddressFutex::activity): it is zero
    /// exactly when nothing is parked, so a shutdown that left a thread behind is visible in it.
    #[must_use]
    pub fn parked_now(&self) -> (usize, usize) {
        let parked = self.parked.lock();
        (parked.values().sum(), parked.len())
    }

    /// Wakes that landed **within eight bytes of a parked waiter, without landing on it**.
    ///
    /// # The one failure this futex cannot report any other way
    ///
    /// A waiter is keyed by an address and a wake is keyed by an address, so a wake that targets
    /// the wrong one of two adjacent words in the same object does nothing and reports nothing:
    /// the waiter stays parked, the wake reports zero unparked, and both are exactly what a
    /// correct call on an uncontended address also looks like. Nothing in the counters
    /// distinguishes them.
    ///
    /// It is a real hazard here rather than a theoretical one, because the guest's own primitives
    /// and this layer's are keyed independently: `libroblox.so` parks with a raw `futex` syscall
    /// on `obj + 4` — the **high half** of a 64-bit atomic whose upper word is a sequence counter
    /// — while `omni-bionic`'s mutexes, rwlocks and semaphores wake on the address of the object
    /// itself. A primitive whose guest half and host half disagreed by four bytes would look
    /// precisely like the stall M6 is stopped on.
    ///
    /// Each entry is `(the address woken, the address parked on)`. Empty means the hazard did not
    /// occur, which is a **detector** and not a watch: it is zero exactly when no wake came near a
    /// waiter it missed.
    ///
    /// The check runs only when something is actually parked, which is rare — measured at two
    /// waiters across a whole startup — so the common path is one uncontended mutex acquisition.
    #[must_use]
    pub fn near_misses(&self) -> Vec<(u64, u64)> {
        self.near_misses.lock().clone()
    }

    /// How many parks were entered with no deadline at all.
    ///
    /// **The number that separates a slow wait from a stranded one.** Every wait inside
    /// `omni-bionic` passes a bounded slice and re-checks its predicate, so a lost wake there
    /// heals; only a caller that passes `None` can leave a thread where nothing but a wake or a
    /// shutdown will reach it. If threads are parked and this is zero, they are in a bounded slice
    /// and the counters will move.
    #[must_use]
    pub fn indefinite_parks(&self) -> u64 {
        self.indefinite.load(Ordering::Relaxed)
    }

    /// Every address currently parked on, with how many threads are on each.
    ///
    /// **What [`parked_now`](AddressFutex::parked_now) cannot say.** That pair reports *how many*,
    /// which answers "did shutdown leave anyone behind"; this reports *where*, which is what a run
    /// stopped on a lock somebody else holds needs. A guest address is a host address under
    /// identity mapping (D4), so the caller can read the object's bytes and subtract the load base
    /// to see whether it is in the image at all — an address outside it is a heap or stack object,
    /// which already narrows what kind of wait it is.
    ///
    /// Takes the table's lock, so it is a diagnostic call and not a hot path.
    #[must_use]
    pub fn parked_addresses(&self) -> Vec<(u64, usize)> {
        let mut out: Vec<(u64, usize)> =
            self.parked.lock().iter().map(|(addr, count)| (*addr, *count)).collect();
        out.sort_unstable();
        out
    }

    /// Record this thread as parked on `addr` for as long as the guard lives.
    fn enter_park(&self, addr: u64) -> ParkedOn<'_> {
        *self.parked.lock().entry(addr).or_insert(0) += 1;
        ParkedOn { futex: self, addr }
    }

    /// The guest address of the most recent park, or `0` if nothing has parked.
    ///
    /// **What a stuck guest looks like from another thread.** A guest blocked here executes no
    /// guest instructions, so nothing on its own thread reports again; this says *which object*
    /// it is blocked on, and the caller can then read that object's bytes out of guest memory.
    /// One relaxed store on a path that is about to sleep.
    #[must_use]
    pub fn parked_on(&self) -> u64 {
        self.last_wait.load(Ordering::Relaxed)
    }
}

/// Keeps an address in [`AddressFutex::parked`] for as long as a thread is on its queue.
///
/// A guard rather than a matched pair, for the reason every guard in this project is one: `park`
/// has three exits — woken, timed out, and the validate callback refusing — and a pair of calls
/// around them would eventually miss one. An address left in the table after its thread has gone
/// makes `stop` wake a queue nobody is on, which is harmless, and makes `parked_now` report a
/// thread that does not exist, which is not: it would say a shutdown had failed when it had not.
struct ParkedOn<'a> {
    futex: &'a AddressFutex,
    addr: u64,
}

impl Drop for ParkedOn<'_> {
    fn drop(&mut self) {
        let mut parked = self.futex.parked.lock();
        if let Some(count) = parked.get_mut(&self.addr) {
            *count -= 1;
            if *count == 0 {
                parked.remove(&self.addr);
            }
        }
    }
}

impl Futex for AddressFutex {
    fn wait(&self, addr: u64, expected: u32, timeout: Option<Duration>) -> WaitResult {
        // See the module docs: the comparison is the caller's, because not every caller in
        // `omni-bionic` passes a meaningful `expected`.
        let _ = expected;
        if self.stopped() {
            // The instance is shutting down. Every caller re-checks its predicate after
            // `WouldBlock` and loops, which is the spin `stop` documents as bounded by one run
            // window.
            return WaitResult::WouldBlock;
        }
        let _parked = self.enter_park(addr);
        self.waits.fetch_add(1, Ordering::Relaxed);
        self.last_wait.store(addr, Ordering::Relaxed);
        if timeout.is_none() {
            self.indefinite.fetch_add(1, Ordering::Relaxed);
        }
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
        // See `near_misses`. Guarded on the table being non-empty so that the overwhelmingly
        // common case -- a wake with nothing parked anywhere -- is one lock and one `is_empty`.
        {
            let parked = self.parked.lock();
            if !parked.is_empty() && !parked.contains_key(&addr) {
                let near: Vec<u64> = parked
                    .keys()
                    .copied()
                    .filter(|at| at.abs_diff(addr) <= 8)
                    .collect();
                if !near.is_empty() {
                    drop(parked);
                    let mut misses = self.near_misses.lock();
                    for at in near {
                        if !misses.contains(&(addr, at)) {
                            misses.push((addr, at));
                        }
                    }
                }
            }
        }
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
    /// The `bool` is **true when the block is newly handed out**, which the caller needs
    /// because blocks are recycled: a thread that exits returns its block to the free list, and
    /// the next thread to take it would otherwise inherit whatever the previous one left in
    /// `errno` and in the `locale_t` cell. That is a plausible wrong answer rather than a
    /// crash, which is the shape Global Constraint 1 is about, so the caller zeroes the block's
    /// header when this says the block is fresh.
    pub fn attach_current(
        &self,
        capacity: usize,
        block_at: impl Fn(usize) -> GuestAddr,
    ) -> Option<(ThreadSlot, bool)> {
        let key = std::thread::current().id();
        let mut inner = self.inner.lock();
        if let Some(slot) = inner.slots.get(&key) {
            return Some((*slot, false));
        }
        let index = inner.take_block(capacity)?;
        let slot = ThreadSlot { id: self.next_id(), block: block_at(index), index };
        inner.slots.insert(key, slot);
        Some((slot, true))
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

    /// Whether any live host thread holds this identity.
    ///
    /// A linear scan of at most [`MAX_GUEST_THREADS`](crate::bionic::MAX_GUEST_THREADS) entries,
    /// which is what `pthread_getschedparam` needs to tell "a thread of this instance" from "a
    /// `pthread_t` nobody handed out". A second map keyed the other way would be a second thing
    /// to keep in step with this one for a question asked once per call rather than once per
    /// crossing.
    #[must_use]
    pub fn knows(&self, id: GuestThreadId) -> bool {
        self.inner.lock().slots.values().any(|slot| slot.id == id)
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
