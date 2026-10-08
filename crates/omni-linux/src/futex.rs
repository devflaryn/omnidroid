//! The futex wait queue (milestone A4): Linux's `futex(2)` semantics for one process.
//!
//! Queues are keyed by the untagged guest address (a tagged and an untagged pointer to one word are
//! one futex). The value check in `wait` and the enqueue happen under the lock `wake` takes, so a
//! wake that follows the guest's store cannot fall between them. A woken waiter is marked in
//! `woken` by id, so a requeued waiter is woken wherever it has been moved to.
//!
//! **Each waiting task parks its own host thread, and a wake unparks only the tasks it woke.**
//! This was one condition variable per process, and every wake was a `notify_all`: in a game
//! world each of Roblox's ~100k futex wakes a second woke *every* task waiting in the process --
//! every other futex waiter, every `nanosleep`, every PI-lock waiter -- to take the one lock, find
//! it was not its turn, and sleep again. A wake is now one `unpark` per woken task; sleepers are
//! only woken by their own deadline, a signal (`interrupt`) or the process ending. Parking is the
//! host thread's own token (`std::thread::park`), so a wake between dropping the lock and parking
//! is not lost; a spurious return re-checks under the lock and parks again.
use std::collections::{HashMap, HashSet, VecDeque};
use std::thread::Thread;
use std::time::Instant;

use parking_lot::{Mutex, MutexGuard};

use crate::errno::{Errno, SysResult, EAGAIN, EINTR, EINVAL};
use crate::guest::{untag, GuestMem};

const ETIMEDOUT: Errno = Errno(110);
const EDEADLK: Errno = Errno(35);
const EPERM: Errno = Errno(1);

/// A priority-inheritance futex word: the owner's tid, and whether any wait for it.
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

struct Waiter {
    id: u64,
    bitset: u32,
}

#[derive(Default)]
struct State {
    queues: HashMap<u64, VecDeque<Waiter>>,
    woken: HashSet<u64>,
    /// Every task waiting here now -- on a futex, a PI lock, or a sleep -- by wait id: the host
    /// thread to unpark when it is woken or interrupted.
    parked: HashMap<u64, Thread>,
    next_id: u64,
    /// Set by `interrupt_all` (the process is ending): every wait answers `EINTR`.
    interrupted: bool,
}

/// A waiting task's signals: its pending set and its mask. A wait ends with `EINTR` as soon as a
/// pending signal is not blocked -- checked under the queue lock before sleeping and on every
/// wake, so a signal posted just before the wait began is never lost (A2-A5 review, Critical 1).
#[derive(Clone, Copy)]
pub struct Signals<'a> {
    pub pending: &'a std::sync::atomic::AtomicU64,
    pub mask: u64,
}

impl Signals<'_> {
    fn deliverable(self) -> bool {
        self.pending.load(std::sync::atomic::Ordering::SeqCst) & !self.mask != 0
    }
}

/// Host threads to unpark once the queue lock is dropped. A wake that wakes one task (the common
/// case) does not allocate.
#[derive(Default)]
struct Unpark {
    one: Option<Thread>,
    more: Vec<Thread>,
}

impl Unpark {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, t: Thread) {
        if self.one.is_none() {
            self.one = Some(t);
        } else {
            self.more.push(t);
        }
    }
}

impl State {
    fn remove(&mut self, id: u64) {
        self.queues.retain(|_, q| {
            q.retain(|w| w.id != id);
            !q.is_empty()
        });
    }

    /// A new wait id, its task's host thread registered to be unparked.
    fn register(&mut self) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.parked.insert(id, std::thread::current());
        id
    }

    /// Wake up to `n` waiters on `key` whose bitset meets `bitset`, oldest first; their threads go
    /// to `unpark`.
    fn wake(&mut self, key: u64, n: u64, bitset: u32, unpark: &mut Unpark) -> u64 {
        let Self { queues, woken: marked, parked, .. } = self;
        let Some(queue) = queues.get_mut(&key) else { return 0 };
        let mut woken = 0u64;
        queue.retain(|w| {
            if woken < n && w.bitset & bitset != 0 {
                marked.insert(w.id);
                if let Some(t) = parked.get(&w.id) {
                    unpark.push(t.clone());
                }
                woken += 1;
                false
            } else {
                true
            }
        });
        if queue.is_empty() {
            queues.remove(&key);
        }
        woken
    }

    /// Every waiting task's thread (a signal, the process ending).
    fn everyone(&self) -> Unpark {
        Unpark { one: None, more: self.parked.values().cloned().collect() }
    }
}

/// `futex_herd=1` (lever, measurement only): every wake unparks every waiting task, as the one
/// condition variable did -- the old behaviour, to A/B against in one session.
pub static HERD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn unpark_all(threads: Unpark) {
    for t in threads.one.into_iter().chain(threads.more) {
        t.unpark();
    }
}

fn key(addr: u64) -> Result<u64, Errno> {
    let key = untag(addr);
    if key % 4 != 0 {
        return Err(EINVAL);
    }
    Ok(key)
}

fn read_u32(mem: &GuestMem, addr: u64) -> Result<u32, Errno> {
    Ok(u32::from_le_bytes(mem.read(addr, 4)?.try_into().expect("four bytes")))
}

#[derive(Default)]
pub struct Futexes {
    state: Mutex<State>,
}

impl Futexes {
    /// Drop the lock and park until unparked, `deadline`, or spuriously; then take the lock again.
    /// The caller re-checks everything. True when the deadline has passed.
    fn park<'a>(&'a self, st: MutexGuard<'a, State>, deadline: Option<Instant>) -> (MutexGuard<'a, State>, bool) {
        drop(st);
        let late = match deadline {
            Some(d) => {
                let now = Instant::now();
                if now < d {
                    std::thread::park_timeout(d - now);
                }
                Instant::now() >= d
            }
            None => {
                std::thread::park();
                false
            }
        };
        (self.state.lock(), late)
    }

    /// `FUTEX_WAIT`/`FUTEX_WAIT_BITSET`: sleep while `*addr == expected`, until woken, the
    /// deadline, or an interrupt.
    pub fn wait(&self, mem: &GuestMem, addr: u64, expected: u32, bitset: u32, deadline: Option<Instant>, signals: Option<Signals<'_>>) -> SysResult {
        if bitset == 0 {
            return Err(EINVAL);
        }
        let key = key(addr)?;
        let mut st = self.state.lock();
        if st.interrupted || signals.is_some_and(Signals::deliverable) {
            return Err(EINTR);
        }
        if read_u32(mem, addr)? != expected {
            return Err(EAGAIN);
        }
        let id = st.register();
        st.queues.entry(key).or_default().push_back(Waiter { id, bitset });
        let result = loop {
            if st.woken.remove(&id) {
                break Ok(0);
            }
            if st.interrupted || signals.is_some_and(Signals::deliverable) {
                st.remove(id);
                break Err(EINTR);
            }
            let late;
            (st, late) = self.park(st, deadline);
            if late {
                if st.woken.remove(&id) {
                    break Ok(0);
                }
                st.remove(id);
                break Err(ETIMEDOUT);
            }
        };
        st.parked.remove(&id);
        result
    }

    /// `FUTEX_WAKE`/`FUTEX_WAKE_BITSET`: how many were woken.
    pub fn wake(&self, addr: u64, n: u64, bitset: u32) -> SysResult {
        if bitset == 0 {
            return Err(EINVAL);
        }
        let key = key(addr)?;
        let mut unpark = Unpark::new();
        let woken = {
            let mut st = self.state.lock();
            let woken = st.wake(key, n, bitset, &mut unpark);
            if woken > 0 && HERD.load(std::sync::atomic::Ordering::Relaxed) {
                unpark = st.everyone();
            }
            woken
        };
        unpark_all(unpark);
        Ok(woken)
    }

    /// `FUTEX_REQUEUE`/`FUTEX_CMP_REQUEUE`: wake `n_wake` on `addr`, move up to `n_requeue` of the
    /// rest to `addr2`; answers woken + moved.
    pub fn requeue(&self, mem: &GuestMem, addr: u64, n_wake: u64, addr2: u64, n_requeue: u64, expected: Option<u32>) -> SysResult {
        let (from, to) = (key(addr)?, key(addr2)?);
        let mut st = self.state.lock();
        if let Some(e) = expected {
            if read_u32(mem, addr)? != e {
                return Err(EAGAIN);
            }
        }
        let mut unpark = Unpark::new();
        let woken = st.wake(from, n_wake, u32::MAX, &mut unpark);
        let mut moved = 0u64;
        if from != to {
            let mut take = VecDeque::new();
            if let Some(q) = st.queues.get_mut(&from) {
                while moved < n_requeue {
                    let Some(w) = q.pop_front() else { break };
                    take.push_back(w);
                    moved += 1;
                }
                if q.is_empty() {
                    st.queues.remove(&from);
                }
            }
            st.queues.entry(to).or_default().extend(take);
        }
        drop(st);
        unpark_all(unpark);
        Ok(woken + moved)
    }

    /// `FUTEX_WAKE_OP`: `*addr2 = op(*addr2)`, wake `n1` on `addr`, and `n2` on `addr2` if the old
    /// value met the comparison.
    pub fn wake_op(&self, mem: &GuestMem, addr: u64, n1: u64, addr2: u64, n2: u64, op: u32) -> SysResult {
        let (k1, k2) = (key(addr)?, key(addr2)?);
        let sext12 = |v: u32| ((v << 20) as i32 >> 20) as u32;
        let mut oparg = sext12((op >> 12) & 0xfff);
        if op & (8 << 28) != 0 {
            oparg = 1u32.checked_shl(oparg).unwrap_or(0); // FUTEX_OP_OPARG_SHIFT
        }
        let cmparg = sext12(op & 0xfff) as i32;
        let word = mem.atomic_u32(addr2)?;
        let mut st = self.state.lock();
        let update = |old: u32| -> Option<u32> {
            Some(match (op >> 28) & 7 {
                0 => oparg,
                1 => old.wrapping_add(oparg),
                2 => old | oparg,
                3 => old & !oparg,
                4 => old ^ oparg,
                _ => return None,
            })
        };
        let cmp_ok = matches!((op >> 24) & 15, 0..=5);
        if update(0).is_none() || !cmp_ok {
            return Err(crate::errno::ENOSYS);
        }
        let old = word
            .fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, update)
            .expect("update is total for a valid op");
        let old_signed = old as i32;
        let met = match (op >> 24) & 15 {
            0 => old_signed == cmparg,
            1 => old_signed != cmparg,
            2 => old_signed < cmparg,
            3 => old_signed <= cmparg,
            4 => old_signed > cmparg,
            _ => old_signed >= cmparg,
        };
        let mut unpark = Unpark::new();
        let mut woken = st.wake(k1, n1, u32::MAX, &mut unpark);
        if met {
            woken += st.wake(k2, n2, u32::MAX, &mut unpark);
        }
        drop(st);
        unpark_all(unpark);
        Ok(woken)
    }

    /// `FUTEX_LOCK_PI`: take the lock at `addr` for `tid` -- at once when no one owns it, otherwise
    /// marking it contended (`FUTEX_WAITERS`, so its owner's unlock comes here) and waiting until
    /// it is released, the deadline, or the process ending. A lock taken while others still wait
    /// keeps the mark. (Priorities are not boosted: every task runs at one priority here.)
    pub fn lock_pi(&self, mem: &GuestMem, addr: u64, tid: u32, deadline: Option<Instant>) -> SysResult {
        let key = key(addr)?;
        let word = mem.atomic_u32(addr)?;
        let mut st = self.state.lock();
        // The wait id, registered (to be unparked) for as long as this call may park.
        let id = st.register();
        let mut queued = false;
        let result = loop {
            if queued && st.woken.remove(&id) {
                queued = false;
            }
            if st.interrupted {
                if queued {
                    st.remove(id);
                }
                break Err(EINTR);
            }
            let v = word.load(std::sync::atomic::Ordering::SeqCst);
            let owner = v & FUTEX_TID_MASK;
            if owner == 0 {
                if queued {
                    st.remove(id);
                    queued = false;
                }
                let others = st.queues.get(&key).map_or(0, VecDeque::len);
                let mine = tid | (v & FUTEX_OWNER_DIED) | if others > 0 { FUTEX_WAITERS } else { 0 };
                if word.compare_exchange(v, mine, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok() {
                    break Ok(0);
                }
                continue;
            }
            if owner == tid {
                if queued {
                    st.remove(id);
                }
                break Err(EDEADLK);
            }
            if v & FUTEX_WAITERS == 0 && word.compare_exchange(v, v | FUTEX_WAITERS, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_err() {
                continue;
            }
            if !queued {
                st.queues.entry(key).or_default().push_back(Waiter { id, bitset: u32::MAX });
                queued = true;
            }
            let late;
            (st, late) = self.park(st, deadline);
            if late && !st.woken.remove(&id) {
                st.remove(id);
                break Err(ETIMEDOUT);
            }
            if late {
                queued = false;
            }
        };
        st.woken.remove(&id);
        st.parked.remove(&id);
        result
    }

    /// `FUTEX_TRYLOCK_PI`: take the lock only if no one owns it.
    pub fn trylock_pi(&self, mem: &GuestMem, addr: u64, tid: u32) -> SysResult {
        key(addr)?;
        let word = mem.atomic_u32(addr)?;
        let _st = self.state.lock();
        let v = word.load(std::sync::atomic::Ordering::SeqCst);
        match v & FUTEX_TID_MASK {
            0 => {
                let mine = tid | (v & (FUTEX_OWNER_DIED | FUTEX_WAITERS));
                word.compare_exchange(v, mine, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).map(|_| 0).map_err(|_| EAGAIN)
            }
            owner if owner == tid => Err(EDEADLK),
            _ => Err(EAGAIN),
        }
    }

    /// `FUTEX_UNLOCK_PI`: release the lock `tid` owns, and wake those waiting to take it.
    pub fn unlock_pi(&self, mem: &GuestMem, addr: u64, tid: u32) -> SysResult {
        let key = key(addr)?;
        let word = mem.atomic_u32(addr)?;
        let mut st = self.state.lock();
        if word.load(std::sync::atomic::Ordering::SeqCst) & FUTEX_TID_MASK != tid {
            return Err(EPERM);
        }
        word.store(0, std::sync::atomic::Ordering::SeqCst);
        let mut unpark = Unpark::new();
        st.wake(key, u64::MAX, u32::MAX, &mut unpark);
        drop(st);
        unpark_all(unpark);
        Ok(0)
    }

    /// How many wait on `addr` now (tests and `/proc` diagnostics).
    #[must_use]
    pub fn waiters(&self, addr: u64) -> usize {
        self.state.lock().queues.get(&untag(addr)).map_or(0, VecDeque::len)
    }

    /// A signal was posted to `tid` (its pending bit is already set): wake every waiter so the one
    /// that belongs to it sees the bit. Taking the lock first is what orders this after a waiter's
    /// check, so the wake cannot fall between its check and its sleep.
    pub fn interrupt(&self, _tid: i32) {
        let everyone = self.state.lock().everyone();
        unpark_all(everyone);
    }

    /// Sleep until `deadline` (`None`: forever), a deliverable signal, or the process ending --
    /// `nanosleep` and `clock_nanosleep`. `Ok` when the time ran out, `EINTR` otherwise.
    pub fn sleep_until(&self, deadline: Option<Instant>, signals: Signals<'_>) -> Result<(), Errno> {
        let mut st = self.state.lock();
        if st.interrupted || signals.deliverable() {
            return Err(EINTR);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(());
        }
        let id = st.register();
        let result = loop {
            let late;
            (st, late) = self.park(st, deadline);
            if st.interrupted || signals.deliverable() {
                break Err(EINTR);
            }
            if late {
                break Ok(());
            }
        };
        st.parked.remove(&id);
        result
    }

    /// Whether the process is ending (`interrupt_all`): every wait answers `EINTR` from now on.
    #[must_use]
    pub fn interrupted(&self) -> bool {
        self.state.lock().interrupted
    }

    /// End every wait, now and later, with `EINTR`: the process is exiting.
    pub fn interrupt_all(&self) {
        let everyone = {
            let mut st = self.state.lock();
            st.interrupted = true;
            st.everyone()
        };
        unpark_all(everyone);
    }
}
