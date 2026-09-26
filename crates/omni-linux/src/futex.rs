//! The futex wait queue (milestone A4): Linux's `futex(2)` semantics for one process.
//!
//! Queues are keyed by the untagged guest address (a tagged and an untagged pointer to one word are
//! one futex). The value check in `wait` and the enqueue happen under the lock `wake` takes, so a
//! wake that follows the guest's store cannot fall between them. A woken waiter is marked in
//! `woken` by id, so a requeued waiter is woken wherever it has been moved to.
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use parking_lot::{Condvar, Mutex};

use crate::errno::{Errno, SysResult, EAGAIN, EINTR, EINVAL};
use crate::guest::{untag, GuestMem};

const ETIMEDOUT: Errno = Errno(110);

struct Waiter {
    id: u64,
    bitset: u32,
}

#[derive(Default)]
struct State {
    queues: HashMap<u64, VecDeque<Waiter>>,
    woken: HashSet<u64>,
    next_id: u64,
    /// Set by `interrupt_all` (the process is ending): every wait answers `EINTR`.
    interrupted: bool,
}

impl State {
    fn remove(&mut self, id: u64) {
        self.queues.retain(|_, q| {
            q.retain(|w| w.id != id);
            !q.is_empty()
        });
    }

    /// Wake up to `n` waiters on `key` whose bitset meets `bitset`, oldest first.
    fn wake(&mut self, key: u64, n: u64, bitset: u32) -> u64 {
        let Some(queue) = self.queues.get_mut(&key) else { return 0 };
        let mut woken = 0u64;
        let mut kept = VecDeque::with_capacity(queue.len());
        while let Some(w) = queue.pop_front() {
            if woken < n && w.bitset & bitset != 0 {
                self.woken.insert(w.id);
                woken += 1;
            } else {
                kept.push_back(w);
            }
        }
        if kept.is_empty() {
            self.queues.remove(&key);
        } else {
            *self.queues.get_mut(&key).expect("present") = kept;
        }
        woken
    }
}

#[derive(Default)]
pub struct Futexes {
    state: Mutex<State>,
    cv: Condvar,
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

impl Futexes {
    /// `FUTEX_WAIT`/`FUTEX_WAIT_BITSET`: sleep while `*addr == expected`, until woken, the
    /// deadline, or an interrupt.
    pub fn wait(&self, mem: &GuestMem, addr: u64, expected: u32, bitset: u32, deadline: Option<Instant>) -> SysResult {
        if bitset == 0 {
            return Err(EINVAL);
        }
        let key = key(addr)?;
        let mut st = self.state.lock();
        if st.interrupted {
            return Err(EINTR);
        }
        if read_u32(mem, addr)? != expected {
            return Err(EAGAIN);
        }
        st.next_id += 1;
        let id = st.next_id;
        st.queues.entry(key).or_default().push_back(Waiter { id, bitset });
        loop {
            if st.woken.remove(&id) {
                return Ok(0);
            }
            if st.interrupted {
                st.remove(id);
                return Err(EINTR);
            }
            match deadline {
                Some(d) => {
                    if self.cv.wait_until(&mut st, d).timed_out() {
                        if st.woken.remove(&id) {
                            return Ok(0);
                        }
                        st.remove(id);
                        return Err(ETIMEDOUT);
                    }
                }
                None => self.cv.wait(&mut st),
            }
        }
    }

    /// `FUTEX_WAKE`/`FUTEX_WAKE_BITSET`: how many were woken.
    pub fn wake(&self, addr: u64, n: u64, bitset: u32) -> SysResult {
        if bitset == 0 {
            return Err(EINVAL);
        }
        let key = key(addr)?;
        let woken = self.state.lock().wake(key, n, bitset);
        if woken > 0 {
            self.cv.notify_all();
        }
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
        let woken = st.wake(from, n_wake, u32::MAX);
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
        if woken > 0 {
            self.cv.notify_all();
        }
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
        let mut woken = st.wake(k1, n1, u32::MAX);
        if met {
            woken += st.wake(k2, n2, u32::MAX);
        }
        drop(st);
        if woken > 0 {
            self.cv.notify_all();
        }
        Ok(woken)
    }

    /// How many wait on `addr` now (tests and `/proc` diagnostics).
    #[must_use]
    pub fn waiters(&self, addr: u64) -> usize {
        self.state.lock().queues.get(&untag(addr)).map_or(0, VecDeque::len)
    }

    /// End every wait, now and later, with `EINTR`: the process is exiting.
    pub fn interrupt_all(&self) {
        self.state.lock().interrupted = true;
        self.cv.notify_all();
    }
}
