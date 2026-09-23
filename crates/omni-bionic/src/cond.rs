//! `pthread_cond`: condition variables over a 48-byte guest `pthread_cond_t`.
//!
//! ## Why the waiter list lives here, not in the futex
//!
//! POSIX requires that a signal delivered after a waiter has *registered* may
//! never be lost — and registration happens while the waiter still holds or is
//! releasing the mutex, possibly long before it actually sleeps. The Linux futex
//! model has no such pre-registration: `FUTEX_WAKE` on an address with no
//! sleeping waiter is simply lost. The cond therefore keeps its own host-side
//! waiter registry ([`CondWaiters`]): `wait_begin` inserts the calling thread's
//! notify entry BEFORE releasing the mutex; `signal`/`broadcast` mark entries
//! and wake them; `wait_end` sleeps on the thread's own entry until marked (or
//! timed out), removes it, and reacquires the mutex. The [`crate::threads::Futex`] trait keeps
//! exactly the pinned shape (`wait`/`wake` on an address) and is used to sleep.
//!
//! ## Semantics pinned by tests
//!
//! * `wait` atomically releases the mutex and blocks (registration precedes the
//!   release, so a signal racing the release cannot be lost).
//! * `wait` reacquires the mutex before returning — on EVERY exit path,
//!   including `timedwait`'s ETIMEDOUT.
//! * `signal` wakes at least one registered waiter; `broadcast` wakes all.
//! * `timedwait` measures its deadline against the cond's OWN clock
//!   (`pthread_condattr_setclock`: realtime default, monotonic supported).
//! * Spurious wakeups are legal; callers use predicate loops.

use crate::atomics::GuestAtomic;
use crate::errno::consts;
use crate::layouts::sizes;
use crate::memory::GuestMemory;
use crate::threads::GuestThreadId;
use core::time::Duration;
use std::collections::HashMap;
use std::sync::{Condvar, Mutex as HostMutex};

/// Clock selectors stored in word 1 of the guest struct.
mod clock_sel {
    /// `CLOCK_REALTIME` (bionic default; matches `PTHREAD_COND_INITIALIZER`).
    pub const REALTIME: u32 = 0;
    /// `CLOCK_MONOTONIC`.
    pub const MONOTONIC: u32 = 1;
}

/// POSIX clock numbers for the condattr API.
pub mod clock_id {
    /// `CLOCK_REALTIME` (= 0 on Linux).
    pub const CLOCK_REALTIME: i32 = 0;
    /// `CLOCK_MONOTONIC` (= 1 on Linux).
    pub const CLOCK_MONOTONIC: i32 = 1;
}

// ---------------------------------------------------------------------------
// Host-side waiter registry
// ---------------------------------------------------------------------------

/// One registered waiter: a notify flag + condvar the thread sleeps on.
#[derive(Clone)]
struct WaiterEntry {
    mutex: std::sync::Arc<HostMutex<bool>>,
    condvar: std::sync::Arc<Condvar>,
}

impl WaiterEntry {
    fn new() -> Self {
        WaiterEntry {
            mutex: std::sync::Arc::new(HostMutex::new(false)),
            condvar: std::sync::Arc::new(Condvar::new()),
        }
    }
}

/// The cond's host-side waiter registry, keyed by the cond's guest address.
///
/// Entries are queued FIFO per cond address. `signal` marks + wakes the oldest
/// entry (POSIX: waking more than one is legal, waking none when empty is fine);
/// `broadcast` marks + wakes every entry. A waiter marked before it sleeps
/// observes the flag immediately and does not block — closing the lost-wakeup
/// window.
#[derive(Default)]
pub struct CondWaiters {
    queues: HostMutex<HashMap<u64, Vec<WaiterEntry>>>,
    /// Set once the instance is shutting down. See [`CondWaiters::stop`].
    stopping: core::sync::atomic::AtomicBool,
}

impl CondWaiters {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the calling thread on `cond_addr`; returns its entry.
    fn register(&self, cond_addr: u64) -> WaiterEntry {
        let entry = WaiterEntry::new();
        self.queues
            .lock()
            .unwrap()
            .entry(cond_addr)
            .or_default()
            .push(entry.clone());
        entry
    }

    /// Deregister the calling thread's entry (wait finished or timed out).
    /// Returns whether it had been marked (woken) before removal.
    fn deregister(&self, cond_addr: u64, entry: &WaiterEntry) -> bool {
        let mut queues = self.queues.lock().unwrap();
        let mut was_marked = false;
        if let Some(v) = queues.get_mut(&cond_addr) {
            if let Some(pos) = v.iter().position(|e| std::sync::Arc::ptr_eq(&e.mutex, &entry.mutex)) {
                let removed = v.remove(pos);
                was_marked = *removed.mutex.lock().unwrap();
            }
            if v.is_empty() {
                queues.remove(&cond_addr);
            }
        }
        was_marked
    }

    /// Wake up to `count` registered waiters on `cond_addr`; returns how many
    /// were marked. Marking happens under the registry lock, so a waiter
    /// registered before this call is guaranteed to observe it.
    fn wake(&self, cond_addr: u64, count: u32) -> usize {
        let mut to_notify: Vec<WaiterEntry> = Vec::new();
        let woken = {
            let mut queues = self.queues.lock().unwrap();
            let mut woken = 0usize;
            if let Some(v) = queues.get_mut(&cond_addr) {
                let take = (count as usize).min(v.len());
                for entry in v.drain(..take) {
                    {
                        let mut flag = entry.mutex.lock().unwrap();
                        *flag = true;
                    }
                    to_notify.push(entry);
                }
                woken = take;
                if v.is_empty() {
                    queues.remove(&cond_addr);
                }
            }
            woken
        };
        for entry in to_notify {
            let _guard = entry.mutex.lock().unwrap();
            entry.condvar.notify_all();
        }
        woken
    }

    /// Shut the registry down: **every current waiter is woken, and every later one returns at
    /// once.**
    ///
    /// # One wake is not enough, and that is the whole reason this is a flag
    ///
    /// [`wake_all`](CondWaiters::wake_all) alone was tried and does not work. A correct
    /// `pthread_cond_wait` caller is a **predicate loop**: it wakes, re-checks its condition,
    /// finds it still false and waits again. So a one-shot wake releases the thread for as long
    /// as it takes to re-read one word. MEASURED: with `wake_all` on the stop path and no flag,
    /// `join_guest_threads` still timed out after 60 seconds with the same thread in the same
    /// wait.
    ///
    /// The flag makes every subsequent wait return immediately, which gives the thread a run
    /// window in which to read [`Bionic::stop_guest_threads`](crate)'s own switch and stop. It is
    /// the shape `AddressFutex::stop` already has, for the same reason, one primitive along.
    ///
    /// Idempotent and **cannot be taken back**: a registry that has been stopped belongs to an
    /// instance that is shutting down.
    pub fn stop(&self) {
        self.stopping.store(true, core::sync::atomic::Ordering::Release);
        self.wake_all();
    }

    /// Whether [`stop`](CondWaiters::stop) has been called.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stopping.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Wake **every** registered waiter, on every condition variable, and report how many.
    ///
    /// # Why shutdown needs this and `wake` cannot serve it
    ///
    /// [`wake`](CondWaiters::wake) is keyed by address, and a shutdown has no address list: the
    /// registry is a `HashMap` and the caller does not know which conds a guest has waited on.
    /// So a thread in `pthread_cond_wait` is, without this, **unreachable by anything except a
    /// signal the guest itself must send** -- and a guest that is being torn down is precisely
    /// the guest that will never send it.
    ///
    /// That is not hypothetical. `Bionic::stop_guest_threads` already learned this lesson once
    /// for the futex (`AddressFutex::stop`, added when implementing the raw `futex` syscall
    /// turned the engine's workers from dying into parking), and the cond registry is the other
    /// half of the same gap: **MEASURED in M6**, once the engine got past its client-settings
    /// phase it left a worker in `pthread_cond_wait`, `join_guest_threads` timed out after 60
    /// seconds, and the gate could not tear the address space down.
    ///
    /// A woken waiter is **marked**, exactly as a real signal marks it, so it returns `0` from
    /// `wait_end` rather than `ETIMEDOUT`. That is correct on both counts: POSIX permits a
    /// spurious wakeup at any time and every caller re-checks its predicate, and reporting a
    /// timeout that did not happen is the believable wrong answer this project refuses
    /// everywhere else. The caller then reads the stop switch at its next run-window boundary
    /// and stops, which is what the wake is for.
    pub fn wake_all(&self) -> usize {
        let mut to_notify: Vec<WaiterEntry> = Vec::new();
        {
            let mut queues = self.queues.lock().unwrap();
            for (_, waiters) in queues.iter_mut() {
                for entry in waiters.drain(..) {
                    {
                        let mut flag = entry.mutex.lock().unwrap();
                        *flag = true;
                    }
                    to_notify.push(entry);
                }
            }
            queues.clear();
        }
        let woken = to_notify.len();
        for entry in to_notify {
            let _guard = entry.mutex.lock().unwrap();
            entry.condvar.notify_all();
        }
        woken
    }

    /// Number of waiters currently registered on `cond_addr` (test/diagnostics).
    pub fn registered(&self, cond_addr: u64) -> usize {
        self.queues
            .lock()
            .unwrap()
            .get(&cond_addr)
            .map(|v| v.len())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// condattr
// ---------------------------------------------------------------------------

/// `pthread_condattr_init`: 8 zero bytes (clock = realtime by default).
pub fn attr_init(mem: &mut impl GuestMemory, attr_addr: u64) -> Result<i32, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_CONDATTR_T)?;
    mem.write(attr_addr, &[0u8; 8])?;
    Ok(0)
}

/// `pthread_condattr_destroy`: validates only.
pub fn attr_destroy(
    _mem: &mut impl GuestMemory,
    attr_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_CONDATTR_T)?;
    Ok(0)
}

/// `pthread_condattr_setclock`. Bionic accepts only CLOCK_REALTIME and
/// CLOCK_MONOTONIC; anything else is EINVAL (returned, not errno).
pub fn attr_setclock(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
    clock: i32,
) -> Result<i32, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_CONDATTR_T)?;
    match clock {
        clock_id::CLOCK_REALTIME => mem.write(attr_addr + 4, &clock_sel::REALTIME.to_le_bytes())?,
        clock_id::CLOCK_MONOTONIC => mem.write(attr_addr + 4, &clock_sel::MONOTONIC.to_le_bytes())?,
        _ => return Ok(consts::EINVAL),
    }
    Ok(0)
}

/// `pthread_condattr_getclock`: returns `Ok(Ok(clock))`.
pub fn attr_getclock(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
) -> Result<Result<i32, i32>, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_CONDATTR_T)?;
    let mut b = [0u8; 4];
    mem.read(attr_addr + 4, &mut b)?;
    Ok(Ok(i32::from_le_bytes(b)))
}

// ---------------------------------------------------------------------------
// init / destroy
// ---------------------------------------------------------------------------

/// `pthread_cond_init(cond, attr)`. `attr_addr == 0` means NULL attr = default
/// (realtime clock). Writes the full 48-byte struct; all-zero is the valid
/// `PTHREAD_COND_INITIALIZER` state (realtime, no waiters).
pub fn init(
    mem: &mut impl GuestMemory,
    cond_addr: u64,
    attr_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(cond_addr, sizes::PTHREAD_COND_T)?;
    let clock = if attr_addr == 0 {
        clock_sel::REALTIME
    } else {
        check_range(attr_addr, 8)?;
        let mut b = [0u8; 4];
        mem.read(attr_addr + 4, &mut b)?;
        let v = u32::from_le_bytes(b);
        if v > clock_sel::MONOTONIC {
            return Ok(consts::EINVAL);
        }
        v
    };
    let mut bytes = [0u8; 48];
    bytes[4..8].copy_from_slice(&clock.to_le_bytes());
    mem.write(cond_addr, &bytes)?;
    Ok(0)
}

/// Which clock `pthread_cond_timedwait`'s absolute deadline is measured against, as
/// [`clock_id`] numbers it.
///
/// **This reads back what [`init`] wrote**, and it exists because the alternative is worse. A
/// `timedwait` is given an *absolute* time; turning that into a relative sleep requires knowing
/// which clock it is absolute in, and bionic keeps the answer in a bit of its own
/// `pthread_cond_t` state word. This crate does not model bionic's bit layout -- there is no
/// bionic source on the development host to check it against, and a guessed bit is the kind of
/// evidence `docs/VERIFICATION.md` entry 10 is about. It keeps the selector in a field it
/// defined, at `cond + 4`, so reading it back is reading this crate's own convention rather than
/// reconstructing another implementation's.
///
/// An all-zero struct is `PTHREAD_COND_INITIALIZER`, which is `CLOCK_REALTIME` — so a statically
/// initialised cond answers correctly without ever having been through [`init`].
///
/// # Errors
///
/// [`crate::memory::Fault`] for a `cond_addr` the guest cannot own, and `Err` of the inner
/// `Result` carrying `EINVAL` for a selector this crate never writes.
pub fn clock_of(
    mem: &mut impl GuestMemory,
    cond_addr: u64,
) -> Result<Result<i32, i32>, crate::memory::Fault> {
    check_range(cond_addr, sizes::PTHREAD_COND_T)?;
    let mut b = [0u8; 4];
    mem.read(cond_addr + 4, &mut b)?;
    Ok(match u32::from_le_bytes(b) {
        clock_sel::REALTIME => Ok(clock_id::CLOCK_REALTIME),
        clock_sel::MONOTONIC => Ok(clock_id::CLOCK_MONOTONIC),
        _ => Err(consts::EINVAL),
    })
}

/// `pthread_cond_destroy`: validates, zeroes the struct (the canonical dead
/// state) and wakes any registered stragglers so a buggy waiter fails fast
/// instead of hanging. POSIX leaves destroy-with-waiters undefined.
pub fn destroy(
    mem: &mut impl GuestMemory,
    waiters: &CondWaiters,
    cond_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(cond_addr, sizes::PTHREAD_COND_T)?;
    mem.write(cond_addr, &[0u8; 48])?;
    waiters.wake(cond_addr, u32::MAX as usize as u32);
    Ok(0)
}

// ---------------------------------------------------------------------------
// wait (two-phase) / signal / broadcast
// ---------------------------------------------------------------------------

/// `pthread_cond_wait`, phase 1: register the calling thread on the cond's
/// waiter list, THEN release the mutex. The order is the atomicity: any signal
/// delivered after registration is already addressed to this thread.
///
/// `futex` parameter is accepted for interface symmetry with the other
/// primitives and future adapter use; the release path needs no futex (the
/// mutex's own unlock performs its wake).
#[allow(clippy::too_many_arguments)]
pub fn wait_begin(
    mem: &mut (impl GuestMemory + GuestAtomic),
    owners: &crate::mutex::OwnerTable,
    threads: &impl crate::threads::ThreadRegistry,
    waiters: &CondWaiters,
    cond_addr: u64,
    mutex_addr: u64,
) -> Result<(), crate::memory::Fault> {
    check_range(cond_addr, sizes::PTHREAD_COND_T)?;
    let entry = waiters.register(cond_addr);
    REGISTERED.with(|c| c.borrow_mut().insert(cond_addr, entry));
    // Now release the mutex: from here on, other threads may lock it and
    // signal; our entry is already queued.
    crate::mutex::unlock(mem, &NopFutex, owners, threads, mutex_addr)?;
    Ok(())
}

thread_local! {
    /// The calling thread's registered cond entries (cond_addr -> entry).
    /// Invariant: at most one live registration per (thread, cond) — a thread
    /// waits on one cond at a time.
    static REGISTERED: std::cell::RefCell<HashMap<u64, WaiterEntry>> =
        std::cell::RefCell::new(HashMap::new());
}

/// `pthread_cond_wait`, phase 2: sleep until signalled (or timeout expires),
/// deregister, then REACQUIRE the mutex before returning. The returned code is
/// 0 (signalled or spuriously woken) or ETIMEDOUT — with the mutex held in
/// every case.
///
/// `timeout == None` blocks until signalled, unboundedly, as POSIX requires — and
/// it is safe to do so because there is no lost-wake window to heal. The mark and
/// the notify both happen under the waiter's OWN entry mutex (see `CondWaiters::wake`)
/// and the sleeper re-checks the flag under that same
/// mutex before every sleep, so a wake that lands before the sleep is observed
/// rather than missed. A marked waiter returns immediately.
#[allow(clippy::too_many_arguments)]
pub fn wait_end(
    threads: &impl crate::threads::ThreadRegistry,
    waiters: &CondWaiters,
    cond_addr: u64,
    mutex_addr: u64,
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl crate::threads::Futex,
    timeout: Option<Duration>,
) -> Result<i32, crate::memory::Fault> {
    let me = threads.current();
    let _ = me; // identity is implicit in the thread-local entry
    let entry = REGISTERED
        .with(|c| c.borrow_mut().remove(&cond_addr))
        .ok_or(crate::memory::Fault(cond_addr))?; // no live registration

    // Sleep until marked or timed out, on the thread's OWN entry.
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    let mut signalled = false;
    {
        let guard = entry.mutex.lock().unwrap();
        let mut guard = guard;
        loop {
            if *guard {
                signalled = true;
                break;
            }
            // **The shutdown switch, read before every sleep.** See `CondWaiters::stop`: a
            // one-shot wake cannot release a predicate loop, because the loop waits again. This
            // reports the wait as *signalled* rather than timed out, which is correct on both
            // counts -- POSIX permits a spurious wakeup at any time and every caller re-checks
            // its predicate, and an ETIMEDOUT would report a deadline that did not pass.
            if waiters.stopped() {
                signalled = true;
                break;
            }
            match deadline {
                Some(d) => {
                    let now = std::time::Instant::now();
                    if now >= d {
                        break;
                    }
                    let (g, _) = entry.condvar.wait_timeout(guard, d - now).unwrap();
                    guard = g;
                }
                None => {
                    guard = entry.condvar.wait(guard).unwrap();
                }
            }
        }
    }
    // Ensure we are off the list (wake already removed marked entries; a
    // timed-out waiter removes itself here).
    let _was_marked = waiters.deregister(cond_addr, &entry);

    // Reacquire the mutex BEFORE returning — every exit path. The owners table
    // captured by `with_owners` and the ambient id drive the identity checks.
    let owners = NopOwners::ambient();
    // A relock the futex's shutdown interrupted did **not** take the mutex, so it must not be
    // reported as a wait that returned holding it: its `EINTR` is passed on, for the
    // embedding's handler to refuse (see `mutex::lock`).
    if crate::mutex::lock(mem, futex, &owners, &NopThreads, mutex_addr)? == consts::EINTR {
        return Ok(consts::EINTR);
    }
    Ok(if signalled { 0 } else { consts::ETIMEDOUT })
}

/// `pthread_cond_signal`: wake at least one registered waiter. Returns 0.
pub fn signal(waiters: &CondWaiters, cond_addr: u64) -> Result<i32, crate::memory::Fault> {
    check_range(cond_addr, sizes::PTHREAD_COND_T)?;
    waiters.wake(cond_addr, 1);
    Ok(0)
}

/// `pthread_cond_broadcast`: wake all registered waiters. Returns 0.
pub fn broadcast(waiters: &CondWaiters, cond_addr: u64) -> Result<i32, crate::memory::Fault> {
    check_range(cond_addr, sizes::PTHREAD_COND_T)?;
    waiters.wake(cond_addr, u32::MAX);
    Ok(0)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn check_range(addr: u64, len: u64) -> Result<(), crate::memory::Fault> {
    if addr == 0 {
        return Err(crate::memory::Fault(0));
    }
    match addr.checked_add(len - 1) {
        Some(_) => Ok(()),
        None => Err(crate::memory::Fault(addr)),
    }
}

/// A futex that never blocks — the mutex release in `wait_begin` must not
/// itself block (it never does; its wake is best-effort).
struct NopFutex;

impl crate::threads::Futex for NopFutex {
    fn wait(&self, _addr: u64, _expected: u32, _timeout: Option<Duration>) -> crate::threads::WaitResult {
        crate::threads::WaitResult::Woken
    }
    fn wake(&self, _addr: u64, _count: u32) -> u32 {
        0
    }
}


/// Owner-table stand-in for reacquisition: resolves the ambient table set by
/// [`with_owners`].
struct NopOwners;

impl NopOwners {
    fn ambient() -> std::sync::Arc<crate::mutex::OwnerTable> {
        AMBIENT_OWNERS
            .with(|c| c.borrow().clone())
            .expect("no ambient OwnerTable: call with_owners first")
    }
}

thread_local! {
    static AMBIENT_OWNERS: std::cell::RefCell<Option<std::sync::Arc<crate::mutex::OwnerTable>>> =
        const { std::cell::RefCell::new(None) };
}

/// Set the ambient owners table for the duration of `f` (an `Arc` clone; no raw
/// pointers, `#![forbid(unsafe_code)]` holds).
pub fn with_owners<R>(owners: std::sync::Arc<crate::mutex::OwnerTable>, f: impl FnOnce() -> R) -> R {
    AMBIENT_OWNERS.with(|c| {
        *c.borrow_mut() = Some(owners);
        let r = f();
        *c.borrow_mut() = None;
        r
    })
}

thread_local! {
    static AMBIENT_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0xFFFF_FFFF_0000_0001) };
}

impl crate::threads::ThreadRegistry for NopThreads {
    fn current(&self) -> GuestThreadId {
        AMBIENT_ID.with(|c| GuestThreadId(c.get()))
    }
    fn attach(&self) -> bool {
        true
    }
    fn detach_and_take_destructors(&self) -> Vec<(u64, u64)> {
        Vec::new()
    }
    fn is_attached(&self) -> bool {
        true
    }
    fn live_count(&self) -> usize {
        1
    }
}

/// Set the ambient thread-registry id for the duration of `f`.
pub fn with_registry<R>(threads: &impl crate::threads::ThreadRegistry, f: impl FnOnce() -> R) -> R {
    AMBIENT_ID.with(|c| c.set(threads.current().0));
    f()
}

struct NopThreads;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMemory;
    use crate::mock_threads::{MockClock, MockFutex, MockThreads};
    use crate::mutex::OwnerTable;
    use crate::shared_mem::SharedMockMemory;

    fn base_mem() -> MockMemory {
        let mut m = MockMemory::new();
        m.map(0x1000, &[0u8; 48]); // cond
        m.map(0x2000, &[0u8; 40]); // mutex
        m.map(0x3000, &[0u8; 4]); // predicate flag
        m
    }

    type Fixture = (
        SharedMockMemory,
        std::sync::Arc<MockFutex>,
        std::sync::Arc<OwnerTable>,
        std::sync::Arc<MockThreads>,
        std::sync::Arc<CondWaiters>,
        std::sync::Arc<MockClock>,
    );

    fn fixture() -> Fixture {
        let mem = SharedMockMemory::new(base_mem());
        mem.with_exclusive(|g| {
            init(g, 0x1000, 0).unwrap();
        });
        (
            mem,
            std::sync::Arc::new(MockFutex::new()),
            std::sync::Arc::new(OwnerTable::new()),
            std::sync::Arc::new(MockThreads::new()),
            std::sync::Arc::new(CondWaiters::new()),
            std::sync::Arc::new(MockClock::new()),
        )
    }

    /// signal/broadcast with no waiter are fine and return 0.
    #[test]
    fn signal_with_no_waiter_is_fine() {
        let (_, _f, _o, _t, waiters, _c) = fixture();
        assert_eq!(signal(&waiters, 0x1000).unwrap(), 0);
        assert_eq!(broadcast(&waiters, 0x1000).unwrap(), 0);
    }

    /// init with a condattr sets the clock word; NULL attr = realtime; bad
    /// clock: EINVAL (returned).
    #[test]
    fn init_sets_clock() {
        let mut mem = base_mem();
        mem.map(0x4000, &[0u8; 8]);
        attr_init(&mut mem, 0x4000).unwrap();
        assert_eq!(attr_setclock(&mut mem, 0x4000, clock_id::CLOCK_MONOTONIC).unwrap(), 0);
        assert_eq!(init(&mut mem, 0x1000, 0x4000).unwrap(), 0);
        let mut b = [0u8; 4];
        mem.read(0x1004, &mut b).unwrap();
        assert_eq!(u32::from_le_bytes(b), clock_sel::MONOTONIC);
        init(&mut mem, 0x1000, 0).unwrap();
        mem.read(0x1004, &mut b).unwrap();
        assert_eq!(u32::from_le_bytes(b), clock_sel::REALTIME);
        assert_eq!(attr_setclock(&mut mem, 0x4000, 7).unwrap(), consts::EINVAL);
    }

    /// Full wait/signal cycle across real host threads: waiter blocks until the
    /// signal, reacquires the mutex, and the predicate flag is only observed
    /// under the mutex.
    #[test]
    fn wait_blocks_until_signal_and_relocks() {
        let (mem, futex, owners, threads, waiters, _clock) = fixture();
        let w = {
            let (mem, futex, owners, threads, waiters) =
                (mem.clone(), futex.clone(), owners.clone(), threads.clone(), waiters.clone());
            std::thread::spawn(move || {
                let mut m = mem.clone();
                with_owners(owners.clone(), || with_registry(&*threads, || {
                    assert_eq!(crate::mutex::lock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                    let mut pred = false;
                    let mut rounds = 0;
                    while !pred {
                        wait_begin(&mut m, &owners, &*threads, &waiters, 0x1000, 0x2000).unwrap();
                        let r = wait_end(&*threads, &waiters, 0x1000, 0x2000, &mut m, &*futex, None).unwrap();
                        assert_eq!(r, 0);
                        rounds += 1;
                        assert!(rounds < 100, "predicate never satisfied");
                        let mut b = [0u8; 4];
                        m.read(0x3000, &mut b).unwrap();
                        pred = u32::from_le_bytes(b) == 1;
                    }
                    assert_eq!(crate::mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                }));
            })
        };
        let s = {
            let (mem, futex, owners, threads, waiters) =
                (mem.clone(), futex.clone(), owners.clone(), threads.clone(), waiters.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let mut m = mem.clone();
                with_owners(owners.clone(), || with_registry(&*threads, || {
                    assert_eq!(crate::mutex::lock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                    m.write(0x3000, &1u32.to_le_bytes()).unwrap();
                    assert_eq!(signal(&waiters, 0x1000).unwrap(), 0);
                    assert_eq!(crate::mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                }));
            })
        };
        s.join().unwrap();
        w.join().unwrap();
    }

    /// `clock_of` reads back exactly what `init` wrote, for all three ways a cond is born.
    ///
    /// The third is the one worth having: an **all-zero** struct is `PTHREAD_COND_INITIALIZER`,
    /// a statically initialised cond that never went through `init`, and it must answer
    /// `CLOCK_REALTIME`. A `timedwait` on one is otherwise turned into an absolute deadline on
    /// the wrong clock, which on this host is a difference of decades.
    #[test]
    fn clock_of_reads_back_what_init_wrote_including_the_static_initializer() {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &[0u8; 48]);
        mem.map(0x2000, &[0u8; 8]);

        // A cond that never went through `init`: all zero, so CLOCK_REALTIME.
        assert_eq!(clock_of(&mut mem, 0x1000).unwrap(), Ok(clock_id::CLOCK_REALTIME));

        // A NULL attr, which is bionic's default.
        assert_eq!(init(&mut mem, 0x1000, 0).unwrap(), 0);
        assert_eq!(clock_of(&mut mem, 0x1000).unwrap(), Ok(clock_id::CLOCK_REALTIME));

        // An attr that asked for the monotonic clock.
        assert_eq!(attr_init(&mut mem, 0x2000).unwrap(), 0);
        assert_eq!(attr_setclock(&mut mem, 0x2000, clock_id::CLOCK_MONOTONIC).unwrap(), 0);
        assert_eq!(init(&mut mem, 0x1000, 0x2000).unwrap(), 0);
        assert_eq!(clock_of(&mut mem, 0x1000).unwrap(), Ok(clock_id::CLOCK_MONOTONIC));

        // And back, so the field is read rather than remembered.
        assert_eq!(attr_setclock(&mut mem, 0x2000, clock_id::CLOCK_REALTIME).unwrap(), 0);
        assert_eq!(init(&mut mem, 0x1000, 0x2000).unwrap(), 0);
        assert_eq!(clock_of(&mut mem, 0x1000).unwrap(), Ok(clock_id::CLOCK_REALTIME));
    }

    /// A selector `init` never writes is `EINVAL` rather than a clock picked by falling through.
    #[test]
    fn clock_of_refuses_a_selector_this_crate_never_writes() {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &[0u8; 48]);
        mem.write(0x1004, &7u32.to_le_bytes()).unwrap();
        assert_eq!(clock_of(&mut mem, 0x1000).unwrap(), Err(consts::EINVAL));
    }

    /// **`stop` releases a predicate loop, which is the case a single wake cannot.**
    ///
    /// The waiter here does what a correct `pthread_cond_wait` caller does: waits, re-checks a
    /// predicate that is never satisfied, and waits again. `wake_all` alone lets it go round the
    /// loop once and park again -- MEASURED on the real engine, where `join_guest_threads` still
    /// timed out after 60 seconds with `wake_all` on the stop path and no flag.
    ///
    /// The assertion is that the loop **ends**, under a deadline the test itself enforces, and
    /// that it ends **signalled** rather than timed out: reporting ETIMEDOUT would be reporting a
    /// deadline that never passed, and this wait was given none.
    #[test]
    fn stop_releases_a_waiter_that_re_waits_on_an_unsatisfied_predicate() {
        let (mem, futex, owners, threads, waiters, _clock) = fixture();
        let rounds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = {
            let (mem, futex, owners, threads, waiters, rounds) = (
                mem.clone(),
                futex.clone(),
                owners.clone(),
                threads.clone(),
                waiters.clone(),
                rounds.clone(),
            );
            std::thread::spawn(move || {
                let mut m = mem.clone();
                with_owners(owners.clone(), || with_registry(&*threads, || {
                    assert_eq!(
                        crate::mutex::lock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(),
                        0
                    );
                    // The predicate at 0x3000 is never written, so this loop only ever ends
                    // because the registry was stopped.
                    loop {
                        wait_begin(&mut m, &owners, &*threads, &waiters, 0x1000, 0x2000).unwrap();
                        let r =
                            wait_end(&*threads, &waiters, 0x1000, 0x2000, &mut m, &*futex, None)
                                .unwrap();
                        assert_eq!(r, 0, "a stopped wait is signalled, not timed out");
                        rounds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let mut b = [0u8; 4];
                        m.read(0x3000, &mut b).unwrap();
                        if u32::from_le_bytes(b) == 1 || waiters.stopped() {
                            break;
                        }
                    }
                }));
            })
        };
        std::thread::sleep(Duration::from_millis(120));
        assert!(!w.is_finished(), "the waiter must still be in its loop before the stop");
        waiters.stop();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !w.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(w.is_finished(), "`stop` must release a waiter that re-waits");
        w.join().unwrap();
        assert!(waiters.stopped());
    }

    /// timedwait returns ETIMEDOUT with the mutex RELOCKED: proved by the
    /// waiter unlocking the mutex successfully after the timeout.
    #[test]
    fn timedwait_times_out_with_mutex_relocked() {
        let (mem, futex, owners, threads, waiters, _clock) = fixture();
        let mut m = mem.clone();
        with_owners(owners.clone(), || with_registry(&*threads, || {
            assert_eq!(crate::mutex::lock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
            wait_begin(&mut m, &owners, &*threads, &waiters, 0x1000, 0x2000).unwrap();
            let start = std::time::Instant::now();
            let r = wait_end(&*threads, &waiters, 0x1000, 0x2000, &mut m, &*futex, Some(Duration::from_millis(150))).unwrap();
            let elapsed = start.elapsed();
            assert_eq!(r, consts::ETIMEDOUT);
            crate::timing::assert_blocked_for(
                elapsed, Duration::from_millis(150), "cond timedwait");
            // The mutex is relocked (by us): a plain unlock succeeds.
            assert_eq!(crate::mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
        }));
    }

    /// No lost wakeup: a signal delivered after `wait_begin` registered the
    /// waiter (but before it sleeps) still wakes it.
    #[test]
    fn signal_after_registration_not_lost() {
        let (mem, futex, owners, threads, waiters, _clock) = fixture();
        let w = {
            let (mem, futex, owners, threads, waiters) =
                (mem.clone(), futex.clone(), owners.clone(), threads.clone(), waiters.clone());
            std::thread::spawn(move || {
                let mut m = mem.clone();
                with_owners(owners.clone(), || with_registry(&*threads, || {
                    assert_eq!(crate::mutex::lock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                    wait_begin(&mut m, &owners, &*threads, &waiters, 0x1000, 0x2000).unwrap();
                    std::thread::sleep(Duration::from_millis(200));
                    let start = std::time::Instant::now();
                    let r = wait_end(&*threads, &waiters, 0x1000, 0x2000, &mut m, &*futex, Some(Duration::from_millis(2_000))).unwrap();
                    assert!(start.elapsed() < Duration::from_millis(1_000), "must be woken by the early signal");
                    assert_eq!(r, 0);
                    assert_eq!(crate::mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                }));
            })
        };
        let s = {
            let (mem, futex, owners, threads, waiters) =
                (mem.clone(), futex.clone(), owners.clone(), threads.clone(), waiters.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let mut m = mem.clone();
                with_owners(owners.clone(), || with_registry(&*threads, || {
                    assert_eq!(crate::mutex::lock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0, "mutex must be released by wait_begin");
                    assert_eq!(signal(&waiters, 0x1000).unwrap(), 0);
                    assert_eq!(crate::mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                }));
            })
        };
        s.join().unwrap();
        w.join().unwrap();
    }

    /// broadcast wakes ALL waiters.
    #[test]
    fn broadcast_wakes_all() {
        let (mem, futex, owners, threads, waiters, _clock) = fixture();
        const N: usize = 6;
        let mut ws = Vec::new();
        for _ in 0..N {
            let (mem, futex, owners, threads, waiters) =
                (mem.clone(), futex.clone(), owners.clone(), threads.clone(), waiters.clone());
            ws.push(std::thread::spawn(move || {
                let mut m = mem.clone();
                with_owners(owners.clone(), || with_registry(&*threads, || {
                    assert_eq!(crate::mutex::lock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                    wait_begin(&mut m, &owners, &*threads, &waiters, 0x1000, 0x2000).unwrap();
                    let r = wait_end(&*threads, &waiters, 0x1000, 0x2000, &mut m, &*futex, Some(Duration::from_secs(10))).unwrap();
                    assert_eq!(r, 0, "every waiter must be woken by the broadcast");
                    assert_eq!(crate::mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                }));
            }));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while waiters.registered(0x1000) < N {
            assert!(std::time::Instant::now() < deadline, "waiters never registered");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(broadcast(&waiters, 0x1000).unwrap(), 0);
        for w in ws {
            w.join().unwrap();
        }
    }

    /// signal wakes exactly one of several registered waiters; the others keep
    /// sleeping (bounded by their 5 s timeout, asserted woken-only-one).
    #[test]
    fn signal_wakes_exactly_one() {
        let (mem, futex, owners, threads, waiters, _clock) = fixture();
        const N: usize = 4;
        let mut ws = Vec::new();
        for _ in 0..N {
            let (mem, futex, owners, threads, waiters) =
                (mem.clone(), futex.clone(), owners.clone(), threads.clone(), waiters.clone());
            ws.push(std::thread::spawn(move || {
                let mut m = mem.clone();
                with_owners(owners.clone(), || with_registry(&*threads, || {
                    assert_eq!(crate::mutex::lock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap(), 0);
                    wait_begin(&mut m, &owners, &*threads, &waiters, 0x1000, 0x2000).unwrap();
                    // A HANG GUARD, deliberately far longer than the observation window below.
                    // It was 400 ms, which raced the test: a waiter's clock starts here, but the
                    // main thread only begins observing after polling all four registrations at
                    // 10 ms a turn and then sleeping 300 ms. Once registration took ~100 ms -- and
                    // it does under the seven binaries the mutation harness runs at once -- a
                    // waiter's own timeout expired INSIDE the observation window, a second thread
                    // finished, and `finished == 1` failed. Seen three times: twice directly, and
                    // once inflating a `wcslen` mutation's catch list, which is how a flake
                    // launders itself into evidence.
                    wait_end(&*threads, &waiters, 0x1000, 0x2000, &mut m, &*futex, Some(WAITER_HANG_GUARD)).unwrap();
                    crate::mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x2000).unwrap()
                }));
            }));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while waiters.registered(0x1000) < N {
            assert!(std::time::Instant::now() < deadline, "waiters never registered");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(signal(&waiters, 0x1000).unwrap(), 0);

        // Wait for the signalled thread rather than sleeping a guessed interval, and bound the
        // wait far below WAITER_HANG_GUARD so no timeout can contaminate the count.
        let observe_until = std::time::Instant::now() + OBSERVATION_WINDOW;
        while ws.iter().filter(|w| w.is_finished()).count() < 1
            && std::time::Instant::now() < observe_until
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let finished = ws.iter().filter(|w| w.is_finished()).count();
        assert_eq!(finished, 1, "exactly one waiter may be woken by one signal");
        assert!(
            OBSERVATION_WINDOW.saturating_mul(2) < WAITER_HANG_GUARD,
            "the hang guard must not be able to fire while the count is being taken",
        );

        // Release the rest explicitly instead of waiting out their timeouts.
        assert_eq!(broadcast(&waiters, 0x1000).unwrap(), 0);
        for w in ws {
            w.join().unwrap();
        }
    }

    /// A waiter's timeout in [`signal_wakes_exactly_one`] is a hang guard, nothing more.
    const WAITER_HANG_GUARD: Duration = Duration::from_secs(5);
    /// How long the test will wait for the signalled thread to finish.
    const OBSERVATION_WINDOW: Duration = Duration::from_millis(500);

    /// Guard regions: writes stay inside the 48-byte struct.
    #[test]
    fn writes_stay_in_struct() {
        let mut mem = MockMemory::new();
        mem.map(0x0FC0, &[0xA5; 32]);
        mem.map(0x1000, &[0u8; 48]);
        mem.map(0x1030, &[0xA5; 32]);
        let waiters = CondWaiters::new();
        init(&mut mem, 0x1000, 0).unwrap();
        signal(&waiters, 0x1000).unwrap();
        broadcast(&waiters, 0x1000).unwrap();
        destroy(&mut mem, &waiters, 0x1000).unwrap();
        for (addr, len) in [(0x0FC0u64, 32usize), (0x1030, 32)] {
            let mut buf = vec![0u8; len];
            mem.read(addr, &mut buf).unwrap();
            assert!(buf.iter().all(|&b| b == 0xA5), "guard corrupted at {addr:#x}");
        }
    }

    /// Hostile: null/wrapping addresses fault; an unmapped address is caught by
    /// the waiters-free paths as a no-op (signal/broadcast with no waiters are
    /// legal regardless of mapping — POSIX has no observable difference), so the
    /// mapped-check happens in init/destroy/wait_begin/wait_end.
    #[test]
    fn hostile_inputs() {
        let mut mem = base_mem();
        let waiters = CondWaiters::new();
        assert!(signal(&waiters, 0).is_err());
        assert!(broadcast(&waiters, 0).is_err());
        assert!(destroy(&mut mem, &waiters, 0).is_err());
        assert!(init(&mut mem, 0, 0).is_err());
        assert!(init(&mut mem, 0, u64::MAX - 30).is_err());
        assert!(init(&mut mem, u64::MAX - 30, 0).is_err());
        // signal on an unmapped-but-valid address is a no-op (no waiters).
        assert_eq!(signal(&waiters, 0xdead_0000).unwrap(), 0);
        // Garbage clock selector in the attr: init rejects with EINVAL.
        mem.map(0x4000, &[0u8; 8]);
        mem.write(0x4004, &99u32.to_le_bytes()).unwrap();
        assert_eq!(init(&mut mem, 0x1000, 0x4000).unwrap(), consts::EINVAL);
    }
}
