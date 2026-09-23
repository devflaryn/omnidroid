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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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

    /// Park on the guest word at `addr` **only if it still holds `expected`**, checked atomically
    /// with the decision to block.
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
    /// # Why this closes the window, and where the comparison runs
    ///
    /// The comparison is `parking_lot_core::park`'s **`validate`** callback, which runs with the
    /// queue's bucket lock held. A concurrent [`Futex::wake`] on the same address must take that
    /// lock to find the queue, so it cannot land between the comparison and the park. That is the
    /// same property the kernel gets from its hash-bucket spinlock, and it is the whole reason
    /// the comparison is a callback rather than a value compared before the call.
    ///
    /// # Two steps, and the one lock rule that separates them
    ///
    /// `validate` **must not call into `parking_lot` at all**. That is `parking_lot_core`'s
    /// contract (`parking_lot.rs:584`), and it is why the word is handled in two steps:
    ///
    /// 1. **`admit` runs first, before the park, holding nothing.** This is where the caller
    ///    resolves the word, checks it and commits it, through the address space. That path takes
    ///    the space's map lock, which is a `parking_lot::Mutex`, and here it may. An error is
    ///    returned unchanged, without parking. It is the caller's `EFAULT`.
    /// 2. **The comparison inside `validate` is one atomic load of the admitted word**, and
    ///    nothing else. D4's identity mapping makes the guest address the host address, so there
    ///    is nothing to resolve.
    ///
    /// MEASURED, as a 180-second freeze with every thread blocked and the CPU flat: the first
    /// version compared by reading the word *through the space* inside `validate`. Its reasoning
    /// was that nothing parks while holding the space lock, which is true and was not the
    /// question. **The space lock parks when it is contended**: `RawMutex::lock_slow` spins ten
    /// times and then parks on the mutex's own address, which needs that address's bucket while
    /// `validate` already holds the futex's. If the two share a bucket, the waiter blocks on a
    /// lock it holds itself, and the space lock's owner blocks when it unlocks and tries to wake
    /// it. If they do not share one, a table growth (which locks every bucket in order) does the
    /// same. The unit test
    /// `checking_the_word_under_a_contended_lock_in_the_same_bucket_does_not_deadlock` builds
    /// that collision on purpose.
    ///
    /// # What a concurrent `munmap` does
    ///
    /// * **To a waiter already asleep: nothing.** A parked waiter never touches the word again.
    ///   It is keyed by the address alone and stays parked until a wake on that address, its
    ///   timeout, or [`stop`](AddressFutex::stop). Linux's private futex behaves the same way.
    /// * **Between `admit` and the load:** this is the same check-then-access window that every
    ///   [`GuestMem`](crate::mem::GuestMem) read has, and it is a guest use-after-free: a thread
    ///   unmaps a word another thread is entering `FUTEX_WAIT` on. The load raises an access
    ///   violation inside `validate`. The demand pager examines it, which takes the space lock
    ///   under the bucket lock (the hazard above). It declines a free address, and the violation
    ///   goes on to the next host fault handler, which is the same outcome as a racing
    ///   `read_bytes`. An `madvise(MADV_DONTNEED)` in the same window is the one legal case: the
    ///   pager commits the page again and the load reads zero, as Linux would. Both now need
    ///   that narrow race; the defect this replaced needed only a contended space lock.
    ///
    /// Returns [`WaitResult::WouldBlock`] when the comparison failed, which is the caller's
    /// `EAGAIN`.
    ///
    /// # Errors
    ///
    /// Whatever `admit` returned. Nothing has parked or been counted when it does.
    ///
    /// # Safety
    ///
    /// When `admit` returns `Ok`, `addr` must be four-byte aligned, and its four bytes must be
    /// mapped, readable and committed memory of this process. D4 makes a checked guest address
    /// exactly that. The only thing that can falsify it later is the guest unmapping the word,
    /// described above.
    pub unsafe fn wait_compared<E>(
        &self,
        addr: u64,
        expected: u32,
        admit: impl FnOnce() -> Result<(), E>,
        timeout: Option<Duration>,
    ) -> Result<WaitResult, E> {
        // Step 1, outside every `parking_lot` lock. **First**, before the stop check: an
        // unreadable word is `EFAULT` whether or not the instance is shutting down, which is the
        // order the caller's own check used to give.
        admit()?;
        // Step 2. SAFETY: `admit` returned `Ok`, which is this function's precondition for
        // `word_holds`.
        let still_expected = || unsafe { word_holds(addr, expected) };
        if self.stopped() {
            return Ok(WaitResult::WouldBlock);
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
        // `validate` and `timed_out` neither panic nor call into `parking_lot`. `before_sleep` and
        // `timed_out` are empty. `validate` is `still_expected`: one atomic load and a
        // comparison, which takes no lock of any kind.
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
        Ok(match result {
            parking_lot_core::ParkResult::Unparked(_) => WaitResult::Woken,
            parking_lot_core::ParkResult::TimedOut => WaitResult::TimedOut,
            // `validate` said the word had already changed. **This is the answer, not a
            // degenerate case**: it is `FUTEX_WAIT`'s `EAGAIN`, and it is the whole value of
            // performing the comparison.
            parking_lot_core::ParkResult::Invalid => WaitResult::WouldBlock,
        })
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

/// Whether the admitted word at `addr` holds `expected`: **one atomic load, and nothing else.**
///
/// This is everything [`AddressFutex::wait_compared`] runs under the bucket lock, and it is a
/// function of its own so that the rule is visible: no lock, no address-space call, no path into
/// `parking_lot`. A plain `SeqCst` load of four bytes is one the standard library also permits
/// on read-only memory, and `FUTEX_WAIT` only reads its word.
///
/// # Safety
///
/// `addr` is four-byte aligned and its four bytes are mapped, readable, committed memory of this
/// process, which is what [`AddressFutex::wait_compared`]'s `admit` established.
unsafe fn word_holds(addr: u64, expected: u32) -> bool {
    // SAFETY: the caller's contract, above. Identity mapping (D4) makes the guest address the
    // host address, and the reference lives only for this one load.
    let word = unsafe { AtomicU32::from_ptr(addr as usize as *mut u32) };
    word.load(Ordering::SeqCst) == expected
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
    /// Interrupted exactly when [`stop`](AddressFutex::stop) has been called: from then on
    /// `wait` refuses at once, and a primitive looping in the host must return to the guest.
    fn interrupted(&self) -> bool {
        self.stopped()
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;

    /// Set in the child copy of this test binary that [`in_a_child`] runs.
    const CHILD: &str = "OMNI_ANDROID_FUTEX_BUCKET_CHILD";

    /// The bucket `parking_lot_core` 0.9.12 files `key` under, in a table of 2^16 buckets.
    ///
    /// Its hash is `key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)` (`parking_lot.rs:351`).
    /// A smaller table takes a **prefix** of these sixteen bits, so two keys that agree here share a
    /// bucket at every table size up to 2^16. That matters because the table only grows, and grows
    /// whenever a thread is created. Up to 21,845 live threads, a collision found now is still one
    /// when the park happens.
    fn bucket(key: usize) -> usize {
        key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48
    }

    /// A value from `candidates` whose address shares `target`'s bucket.
    ///
    /// Sixteen bits of hash make a match about one candidate in 65,536. The pools below hold 2^20,
    /// so a search that finds nothing has probability about e^-16, and it fails by name rather
    /// than quietly testing two keys in different buckets.
    fn sharing_a_bucket_with<T>(
        target: usize,
        candidates: &'static [T],
        key_of: impl Fn(&T) -> usize,
    ) -> &'static T {
        candidates.iter().find(|candidate| bucket(key_of(candidate)) == bucket(target)).unwrap_or_else(
            || {
                panic!(
                    "none of {} candidates shares parking_lot_core's bucket with {target:#x}",
                    candidates.len()
                )
            },
        )
    }

    /// Run the test named `name` in a child copy of this test binary. Fail if the child fails,
    /// runs no test, or has not finished within `limit`.
    ///
    /// # Why a child
    ///
    /// The defect these tests exist for is a deadlock **inside `parking_lot_core`'s own table**,
    /// where the losing thread keeps a bucket lock forever. In this process that would not stay one
    /// test's problem. Every later park that hashes to that bucket would stop with it, and so would
    /// any growth of the table, because growing locks every bucket and a new thread can trigger it.
    /// The rest of the suite would then hang instead of failing. A child keeps the damage inside a
    /// process that is thrown away. The child's own `recv_timeout` turns the deadlock into a failed
    /// assertion, and `limit` is only the backstop that kills it if even that does not happen.
    fn in_a_child(name: &str, limit: Duration) {
        let module = module_path!().split_once("::").map_or(module_path!(), |(_, rest)| rest);
        let path = format!("{module}::{name}");
        let mut child = Command::new(std::env::current_exe().expect("the test binary"))
            .args([path.as_str(), "--exact", "--nocapture", "--test-threads", "1"])
            .env(CHILD, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the child");
        // Drained on their own threads so that a chatty child cannot fill a pipe and look hung.
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let out = thread::spawn(move || {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut stdout, &mut text).ok();
            text
        });
        let err = thread::spawn(move || {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut stderr, &mut text).ok();
            text
        });
        let deadline = Instant::now() + limit;
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll the child") {
                break Some(status);
            }
            if Instant::now() >= deadline {
                child.kill().ok();
                child.wait().ok();
                break None;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let output = format!(
            "{}{}",
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default()
        );
        let Some(status) = status else {
            panic!("`{path}` had not finished after {limit:?} and was killed. Its output:\n{output}");
        };
        assert!(status.success(), "`{path}` failed in the child ({status}). Its output:\n{output}");
        // A filter that matched nothing also exits 0. That would be a pass that tested nothing.
        assert!(
            output.contains("test result: ok. 1 passed"),
            "`{path}` did not run exactly one test in the child. Its output:\n{output}"
        );
    }

    /// **Checking the futex word may take a lock that shares the futex's own bucket without
    /// deadlocking.**
    ///
    /// # The deadlock this is the detector for
    ///
    /// A gate run froze for 180 s: every thread blocked, CPU flat. `FUTEX_WAIT` compared the word
    /// inside `parking_lot_core::park`'s `validate` by reading it through the guest address space.
    /// That read takes the space's map lock, which is a `parking_lot::Mutex`. `validate` runs
    /// holding the futex's bucket lock, and its contract is that it must not call into
    /// `parking_lot` at all. An uncontended mutex never does. A contended one parks on its own
    /// address, and parking needs that address's bucket. When that is the bucket already held (or
    /// the table grows meanwhile), the waiter blocks on a lock it holds itself, and the mutex's
    /// owner blocks too when it unlocks and tries to wake the waiter.
    ///
    /// Constructed rather than waited for. The futex word and a `parking_lot` mutex are chosen so
    /// that they share a bucket. One thread holds the mutex, and the waiter's `admit` (the step
    /// that, in production, goes through the address space) has to take it. MEASURED before the
    /// fix, when that step ran inside `validate`: `DEADLOCK: after 5 s only [] had finished`.
    /// Neither the waiter nor the mutex's holder came back. Row `futexlock-A1` puts it back.
    #[test]
    fn checking_the_word_under_a_contended_lock_in_the_same_bucket_does_not_deadlock() {
        if std::env::var_os(CHILD).is_some() {
            return checking_the_word_contends_a_lock_in_the_futex_bucket();
        }
        in_a_child(
            "checking_the_word_under_a_contended_lock_in_the_same_bucket_does_not_deadlock",
            Duration::from_secs(60),
        );
    }

    /// The body of the test above, which runs only in the child.
    fn checking_the_word_contends_a_lock_in_the_futex_bucket() {
        let word: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
        let addr = word.as_ptr() as usize;
        let pool: &'static [Mutex<()>] = Box::leak((0..1 << 20).map(|_| Mutex::new(())).collect());
        // SAFETY: `raw` is used only for its address, which is the key `parking_lot`'s
        // `RawMutex::lock_slow` parks on (`raw_mutex.rs:248`). Nothing locks or unlocks through it.
        let lock = sharing_a_bucket_with(addr, pool, |m| unsafe { m.raw() } as *const _ as usize);
        let futex: &'static AddressFutex = Box::leak(Box::new(AddressFutex::new()));

        let (held_tx, held_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let holder_done = done_tx.clone();
        thread::spawn(move || {
            let guard = lock.lock();
            held_tx.send(()).ok();
            // Far longer than `lock_slow`'s ten spins, so the waiter really parks on the mutex.
            thread::sleep(Duration::from_millis(200));
            drop(guard);
            holder_done.send(("holder", None)).ok();
        });
        held_rx.recv_timeout(Duration::from_secs(5)).expect("the holder took the mutex");

        thread::spawn(move || {
            // SAFETY: `word` is a live, aligned `AtomicU32` this test leaked, so it is readable
            // whatever `admit` does.
            let result = unsafe {
                futex.wait_compared(
                    addr as u64,
                    0,
                    || {
                        drop(lock.lock());
                        Ok::<(), ()>(())
                    },
                    Some(Duration::from_millis(50)),
                )
            };
            done_tx.send(("waiter", Some(result))).ok();
        });

        let mut finished = Vec::new();
        while finished.len() < 2 {
            match done_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(done) => finished.push(done),
                Err(_) => panic!(
                    "DEADLOCK: after 5 s only {finished:?} had finished. The futex word {addr:#x} \
                     and the mutex {:#x} share parking_lot_core's bucket {:#06x}, and checking the \
                     word took that mutex while the futex held the bucket",
                    // SAFETY: as above, the address only.
                    unsafe { lock.raw() } as *const _ as usize,
                    bucket(addr),
                ),
            }
        }
        assert!(
            finished.contains(&("waiter", Some(Ok(WaitResult::TimedOut)))),
            "the word held what was expected and nobody woke it, so the wait parks and times \
             out: {finished:?}"
        );
    }

    /// **The word is compared under the bucket lock, not before it.**
    ///
    /// The over-correction the fix above invites: "the comparison may not take locks, so compare
    /// first, then park." That reopens the lost-wake window `FUTEX_WAIT` exists to close. A
    /// waiter that compares, and then has the word change and the wake land before it reaches
    /// the queue, sleeps through a wake that has already happened.
    ///
    /// Constructed rather than raced. A second thread parks on a key of this test's own that
    /// shares the word's bucket, and it holds that bucket from inside its `validate`, which
    /// calls nothing in `parking_lot`. The waiter can then get as far as the queue and no
    /// further. The word changes while it waits there. Compared under the bucket lock, it sees
    /// the change and refuses to park. Compared before it, it parks on a stale value and times
    /// out. Row `futexlock-B1` is that version.
    #[test]
    fn the_word_is_compared_under_the_bucket_lock_and_not_before_it() {
        let word: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
        let addr = word.as_ptr() as usize;
        let pool: &'static [u8] = Box::leak(vec![0u8; 1 << 20].into_boxed_slice());
        let holder_key = std::ptr::from_ref(sharing_a_bucket_with(addr, pool, |byte| {
            std::ptr::from_ref(byte) as usize
        })) as usize;
        let futex: &'static AddressFutex = Box::leak(Box::new(AddressFutex::new()));

        let (holding_tx, holding_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder = thread::spawn(move || {
            // SAFETY: `holder_key` is the address of a byte in a buffer this test owns and nothing
            // else parks on. `validate` uses only `std` channels, never `parking_lot`, and
            // discards their results rather than panicking. The other two callbacks are empty.
            unsafe {
                parking_lot_core::park(
                    holder_key,
                    || {
                        holding_tx.send(()).ok();
                        release_rx.recv_timeout(Duration::from_secs(10)).ok();
                        false
                    },
                    || {},
                    |_, _| {},
                    parking_lot_core::DEFAULT_PARK_TOKEN,
                    None,
                )
            }
        });
        holding_rx.recv_timeout(Duration::from_secs(5)).expect("the holder is holding the bucket");

        let waits_before = futex.activity().0;
        let (result_tx, result_rx) = mpsc::channel();
        thread::spawn(move || {
            // SAFETY: `word` is a live, aligned `AtomicU32` this test leaked.
            let result = unsafe {
                futex.wait_compared(addr as u64, 0, || Ok::<(), ()>(()), Some(Duration::from_secs(2)))
            };
            result_tx.send(result).ok();
        });
        // **Wait for the witness, not for a duration.** `waits` is counted after `admit` and
        // before the park, so once it moves, anything compared before the park has been compared
        // against the word as it is now. It is an atomic, not a lock, so polling it cannot
        // contend for the bucket the holder has.
        let deadline = Instant::now() + Duration::from_secs(5);
        while futex.activity().0 == waits_before {
            assert!(Instant::now() < deadline, "the waiter never reached the park");
            thread::sleep(Duration::from_millis(1));
        }
        word.store(1, Ordering::SeqCst);
        release_tx.send(()).expect("the holder is still waiting to be released");

        let result = result_rx.recv_timeout(Duration::from_secs(10)).expect("the waiter returned");
        assert_eq!(
            result,
            Ok(WaitResult::WouldBlock),
            "the word changed before the waiter could reach the queue, so the comparison under \
             the bucket lock must see it and refuse to park. TimedOut means it compared before \
             the park and slept on a stale value: the lost-wake window"
        );
        assert_eq!(
            holder.join().expect("the holder"),
            parking_lot_core::ParkResult::Invalid,
            "the holder's own park refuses, which is how it gives the bucket back"
        );
    }
}
