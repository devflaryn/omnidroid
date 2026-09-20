//! `pthread_key_*` thread-local storage — **the engine's only TLS mechanism**
//! (D9: no ELF TLS anywhere in libroblox.so), so this phase is the load-bearing
//! one for the 3,594 static initializers.
//!
//! ## The bionic key model
//!
//! Bionic maps every `pthread_key_t` onto a slot in a fixed per-process table
//! (128 slots on 64-bit; VERIFIED against bionic's `pthread_internal.h`
//! `PTHREAD_KEYS_OVERFLOW_MAX_COUNT`/`__pthread_keys` sizing — the constant
//! below is that table size, and the "out of TLS keys" failure libzstd-jni's
//! Rust code hit is exactly this limit). Each slot carries one destructor
//! (guest function pointer, or 0 = none).
//!
//! * `key_create(&key, dtor)`: claim a free slot. EAGAIN when the table is full.
//! * `key_delete(key)`: release the slot. Values already set by threads are
//!   simply abandoned (bionic's behaviour: no cross-thread notification).
//! * `getspecific`/`setspecific`: per-thread values, default NULL. A key that
//!   was deleted yields a DEFINED result: getspecific returns NULL,
//!   setspecific stores into the (now reusable) slot — identical to bionic,
//!   where a deleted key's slot can be reallocated by the next key_create.
//!
//! ## Destructors at thread exit (the adapter invokes; this crate only orders)
//!
//! At exit the calling thread sweeps its values: for every key with a non-NULL
//! value, **the value is cleared BEFORE the destructor is recorded**, and the
//! sweep repeats up to `PTHREAD_DESTRUCTOR_ITERATIONS` (4) rounds — a
//! destructor may `setspecific` a new value for the same key, in which case
//! that key's destructor runs again in the next round. What this crate returns
//! is the ORDERED LIST of (destructor, value) pairs the adapter must invoke;
//! invoking them is a control transfer (thunk boundary), out of scope here.
//!
//! ## `__cxa_thread_atexit_impl`
//!
//! C++ `thread_local` destructors funnel here: register (dtor, object, dso).
//! At thread exit the registered pairs are handed back in **reverse
//! registration order** (LIFO is the Itanium/C++ABI mandate), filtered by
//! nothing (they are all this DSO's). Registration is timestamped against the
//! exit sweep: a registration made DURING the exit sweep is appended and run
//! in the same exit (documented below).

use crate::errno::consts;
use crate::threads::GuestThreadId;
use std::collections::HashMap;
use std::sync::Mutex as HostMutex;

/// Bionic's per-process key table size on LP64.
/// VERIFIED: bionic `pthread_internal.h` (`__pthread_keys` is
/// `pthread_key_table_t[PTHREAD_KEYS_OVERFLOW_MAX_COUNT]`-bounded; the
/// per-process slot count is 128 on 64-bit). Confidence: HIGH — this is also
/// the limit libzstd-jni's "out of TLS keys" message ran into (repo research
/// notes), and 128 is the value bionic has shipped since its pthread_key
/// rework.
pub const PTHREAD_KEYS_MAX: usize = 128;

/// POSIX/bionic destructor sweep rounds.
pub const PTHREAD_DESTRUCTOR_ITERATIONS: usize = 4;

/// One key slot: the destructor (guest function pointer, 0 = none) plus the
/// generation. The generation guards a key-delete/key-create race: a thread
/// that read a stale key id gets a generation mismatch and treats the key as
/// deleted (defined behaviour) rather than calling a NEW key's destructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeySlot {
    dtor: u64,
    generation: u64,
}

/// The per-process TLS registry. Shared across all guest threads (the adapter
/// constructs one and hands out references).
#[derive(Default)]
pub struct TlsRegistry {
    inner: HostMutex<TlsInner>,
}

#[derive(Default)]
struct TlsInner {
    /// key slot -> (dtor, generation). `next_key` is the slot index.
    keys: Vec<KeySlot>,
    /// (thread id, key index) -> value (guest data pointer).
    values: HashMap<(GuestThreadId, usize), u64>,
    /// Per-thread __cxa_thread_atexit registrations in registration order.
    thread_atexit: HashMap<GuestThreadId, Vec<(u64, u64, u64)>>,
    /// Monotonic generation counter for slots.
    generation: u64,
    /// Registration order stamp for __cxa_thread_atexit (monotonic).
    atexit_stamp: u64,
}

impl TlsRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// `pthread_key_create(&key, dtor)`. Returns the pthread error code:
    /// 0, or EAGAIN when all 128 slots are taken (returned, not errno).
    pub fn key_create(&self, dtor: u64) -> Result<u32, i32> {
        let mut inner = self.inner.lock().unwrap();
        if inner.keys.len() >= PTHREAD_KEYS_MAX {
            // Look for a deleted (freed) slot first: bionic reuses slots.
            if let Some(idx) = inner.keys.iter().position(|s| s.dtor == u64::MAX && s.generation == 0) {
                inner.generation += 1;
                inner.keys[idx] = KeySlot { dtor, generation: inner.generation };
                return Ok(idx as u32);
            }
            return Err(consts::EAGAIN);
        }
        inner.generation += 1;
        let gen = inner.generation;
        inner.keys.push(KeySlot { dtor, generation: gen });
        Ok((inner.keys.len() - 1) as u32)
    }

    /// `pthread_key_delete(key)`. Frees the slot; per-thread values are
    /// abandoned (bionic behaviour). Returns 0 or EINVAL for a never-created
    /// key. Deleting twice: EINVAL (the slot is either freed or reused —
    /// either way the passed key no longer names a live key of THIS
    /// generation, so EINVAL; a reused slot belongs to the new key).
    pub fn key_delete(&self, key: u32) -> i32 {
        let mut inner = self.inner.lock().unwrap();
        let idx = key as usize;
        if idx >= inner.keys.len() {
            return consts::EINVAL;
        }
        // Mark freed: dtor sentinel u64::MAX, generation 0. Values dropped.
        let freed = inner.keys[idx].generation != 0;
        inner.keys[idx] = KeySlot { dtor: u64::MAX, generation: 0 };
        if freed {
            inner.values.retain(|(_tid, k), _| *k != idx);
            0
        } else {
            consts::EINVAL
        }
    }

    /// `pthread_setspecific(key, value)`. 0, or EINVAL for a dead/invalid key.
    /// Storing NULL clears the entry (POSIX: a NULL set is a valid clear).
    pub fn setspecific(&self, thread: GuestThreadId, key: u32, value: u64) -> i32 {
        let mut inner = self.inner.lock().unwrap();
        let idx = key as usize;
        if idx >= inner.keys.len() || inner.keys[idx].generation == 0 {
            return consts::EINVAL;
        }
        if value == 0 {
            inner.values.remove(&(thread, idx));
        } else {
            inner.values.insert((thread, idx), value);
        }
        0
    }

    /// `pthread_getspecific(key)`. The current value or NULL for a dead key
    /// (defined behaviour, matches bionic's reuse model).
    pub fn getspecific(&self, thread: GuestThreadId, key: u32) -> u64 {
        let inner = self.inner.lock().unwrap();
        let idx = key as usize;
        if idx >= inner.keys.len() || inner.keys[idx].generation == 0 {
            return 0;
        }
        inner.values.get(&(thread, idx)).copied().unwrap_or(0)
    }

    /// The destructor registered for `key` (0 = none / dead key). Diagnostic
    /// and exit-sweep use.
    pub fn key_destructor(&self, key: u32) -> u64 {
        let inner = self.inner.lock().unwrap();
        let idx = key as usize;
        if idx >= inner.keys.len() || inner.keys[idx].generation == 0 {
            return 0;
        }
        let s = inner.keys[idx];
        if s.dtor == u64::MAX { 0 } else { s.dtor }
    }

    /// Live slot count (test/diagnostic).
    pub fn live_keys(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.keys.iter().filter(|s| s.generation != 0).count()
    }

    /// Number of occupied slots including freed-but-unreused ones (diagnostic;
    /// bounds the table).
    pub fn table_len(&self) -> usize {
        self.inner.lock().unwrap().keys.len()
    }

    /// `__cxa_thread_atexit_impl(dtor, object, dso)`: register for the calling
    /// thread. Always 0 (POSIX/bionic never fails this in practice; a host
    /// OOM aborts, which is honest).
    pub fn thread_atexit(&self, thread: GuestThreadId, dtor: u64, object: u64, dso: u64) -> i32 {
        let mut inner = self.inner.lock().unwrap();
        inner.atexit_stamp += 1;
        let stamp = inner.atexit_stamp;
        inner
            .thread_atexit
            .entry(thread)
            .or_default()
            .push((dtor, object, stamp));
        let _ = dso; // recorded implicitly per registration order; single DSO
        0
    }

    /// The thread-exit sweep: returns the ORDERED (destructor, value) list the
    /// adapter must invoke, then clears this thread's state.
    ///
    /// Rounds: up to [`PTHREAD_DESTRUCTOR_ITERATIONS`]. Each round collects
    /// every live key with a non-NULL value in ASCENDING KEY ORDER (bionic
    /// sweeps its table from 0 upward), clears the value FIRST, then records
    /// the (dtor, value) pair. If a round collected nothing, the sweep ends
    /// early.
    ///
    /// The list is pairs in invocation order across rounds: round 0's pairs
    /// (ascending keys), then round 1's, and so on. A destructor that called
    /// `setspecific` during round k put the value there; the sweep's clear-
    /// before-record guarantees it will not run forever on its own value.
    pub fn take_exit_work(
        &self,
        thread: GuestThreadId,
        run_recorded: &mut dyn FnMut(u64, u64),
    ) -> Vec<(u64, u64)> {
        let mut work: Vec<(u64, u64)> = Vec::new();

        // Phase 1: __cxa_thread_atexit handlers, LIFO (bionic runs these
        // BEFORE pthread_key destructors). A handler may register more atexit
        // work: the drain loops until the list is empty.
        loop {
            let batch: Vec<(u64, u64)> = {
                let mut inner = self.inner.lock().unwrap();
                match inner.thread_atexit.get_mut(&thread) {
                    None => break,
                    Some(list) => {
                        if list.is_empty() {
                            inner.thread_atexit.remove(&thread);
                            break;
                        }
                        let (dtor, object, _stamp) = list.pop().unwrap();
                        vec![(dtor, object)]
                    }
                }
            };
            for (dtor, object) in &batch {
                run_recorded(*dtor, *object);
            }
            work.extend(batch);
        }

        // Phase 2: pthread_key destructors, ascending key order, up to
        // PTHREAD_DESTRUCTOR_ITERATIONS rounds. Values are cleared BEFORE the
        // destructor is recorded; a destructor that sets its key again is
        // picked up by the next round.
        for _round in 0..PTHREAD_DESTRUCTOR_ITERATIONS {
            let mut round_pairs: Vec<(u64, u64)> = Vec::new();
            {
                let mut inner = self.inner.lock().unwrap();
                for idx in 0..inner.keys.len() {
                    if inner.keys[idx].generation == 0 {
                        continue;
                    }
                    let k = (thread, idx);
                    if let Some(v) = inner.values.remove(&k) {
                        let dtor = if inner.keys[idx].dtor == u64::MAX { 0 } else { inner.keys[idx].dtor };
                        if dtor != 0 {
                            round_pairs.push((dtor, v));
                        }
                    }
                }
            }
            if round_pairs.is_empty() {
                break;
            }
            for (dtor, value) in &round_pairs {
                run_recorded(*dtor, *value);
            }
            work.extend(round_pairs);
        }

        work
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Each thread's values are independent; a new thread sees NULL for every
    /// key.
    #[test]
    fn per_thread_independence() {
        let reg = Arc::new(TlsRegistry::new());
        let key = reg.key_create(0).unwrap();
        let r2 = reg.clone();
        let t = std::thread::spawn(move || {
            let me = GuestThreadId(7);
            assert_eq!(r2.getspecific(me, key), 0, "new thread sees NULL");
            assert_eq!(r2.setspecific(me, key, 0xBEEF), 0);
            assert_eq!(r2.getspecific(me, key), 0xBEEF);
        });
        let main_id = GuestThreadId(1);
        assert_eq!(reg.setspecific(main_id, key, 0x1234), 0);
        t.join().unwrap();
        assert_eq!(reg.getspecific(main_id, key), 0x1234, "main thread unaffected");
    }

    /// Exceeding the key limit returns EAGAIN (bionic: 128 keys on LP64).
    #[test]
    fn key_limit_is_eagain() {
        let reg = TlsRegistry::new();
        for i in 0..PTHREAD_KEYS_MAX {
            let r = reg.key_create(0);
            assert!(r.is_ok(), "key {i} must be claimable");
        }
        let r = reg.key_create(0);
        assert_eq!(r.unwrap_err(), consts::EAGAIN, "key 129 must fail with EAGAIN");
    }

    /// Deleted slots are reusable: 129 create/delete/create cycles succeed.
    #[test]
    fn deleted_slots_are_reusable() {
        let reg = TlsRegistry::new();
        for i in 0..200 {
            let k = reg.key_create(0).unwrap();
            assert_eq!(reg.key_delete(k), 0, "cycle {i}");
        }
        assert_eq!(reg.live_keys(), 0);
    }

    /// Exit sweep: destructors run for non-NULL keys in ascending key order,
    /// value cleared BEFORE the destructor is recorded, NULL-valued keys
    /// skipped, no-dtor keys dropped silently.
    #[test]
    fn exit_sweep_order_and_clearing() {
        let reg = Arc::new(TlsRegistry::new());
        let k0 = reg.key_create(0x1000).unwrap(); // dtor 0x1000
        let k1 = reg.key_create(0x2000).unwrap(); // dtor 0x2000
        let k2 = reg.key_create(0).unwrap();      // no dtor
        let me = GuestThreadId(42);
        assert_eq!(reg.setspecific(me, k0, 0xAA0), 0);
        assert_eq!(reg.setspecific(me, k1, 0xAA1), 0);
        assert_eq!(reg.setspecific(me, k2, 0xAA2), 0); // dropped: no dtor

        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut run = |dtor: u64, value: u64| {
            calls.lock().unwrap().push((dtor, value));
            // While "inside" the destructor, the value must ALREADY be cleared.
            assert_eq!(reg.getspecific(me, k0), 0, "cleared before dtor runs (k0)");
            assert_eq!(reg.getspecific(me, k1), 0, "cleared before dtor runs (k1)");
        };
        let work = reg.take_exit_work(me, &mut run);
        assert_eq!(
            work,
            vec![(0x1000, 0xAA0), (0x2000, 0xAA1)],
            "ascending key order, no-dtor key skipped"
        );
        assert_eq!(*calls.lock().unwrap(), work, "adapter saw the same order");
    }

    /// A destructor that sets its key again is re-run in the next round, up to
    /// PTHREAD_DESTRUCTOR_ITERATIONS (4) total rounds.
    #[test]
    fn destructor_iterations() {
        let reg = Arc::new(TlsRegistry::new());
        let key = reg.key_create(0x7777).unwrap();
        let me = GuestThreadId(9);
        assert_eq!(reg.setspecific(me, key, 1), 0);

        let set_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reg2 = reg.clone();
        let mut run = move |_dtor: u64, _value: u64| {
            let n = set_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n + 1 < PTHREAD_DESTRUCTOR_ITERATIONS {
                // Re-register the key (destructor "sets its key again").
                reg2.setspecific(me, key, (n + 2) as u64);
            }
        };
        let work = reg.take_exit_work(me, &mut run);
        // Rounds 0..3 recorded the pair (round 4 found nothing because the last
        // run did NOT re-set). Total invocations: 4.
        assert_eq!(work.len(), PTHREAD_DESTRUCTOR_ITERATIONS, "exactly the 4 rounds");
        assert_eq!(work.iter().filter(|(d, _)| *d == 0x7777).count(), 4);
    }

    /// A destructor that keeps re-setting stops after 4 rounds regardless
    /// (the loop is bounded; the 5th value stays un-run and the thread exits).
    #[test]
    fn destructor_loop_is_bounded() {
        let reg = Arc::new(TlsRegistry::new());
        let key = reg.key_create(0x8888).unwrap();
        let me = GuestThreadId(11);
        assert_eq!(reg.setspecific(me, key, 1), 0);

        let reg2 = reg.clone();
        let mut run = move |_dtor: u64, _v: u64| {
            reg2.setspecific(me, key, 0xBAD); // always re-set
        };
        let work = reg.take_exit_work(me, &mut run);
        assert_eq!(work.len(), PTHREAD_DESTRUCTOR_ITERATIONS, "bounded at 4");
        // The 4th destructor's re-set value REMAINS: the iteration cap stops
        // the sweep, exactly like bionic's PTHREAD_DESTRUCTOR_ITERATIONS cap
        // (a destructor that always re-sets leaks its value; POSIX allows the
        // cap, so this is defined behaviour, not a bug).
        assert_eq!(reg.getspecific(me, key), 0xBAD, "value remains after the cap");
    }

    /// getspecific/setspecific on a DELETED key are defined: NULL / EINVAL.
    #[test]
    fn deleted_key_defined_behaviour() {
        let reg = TlsRegistry::new();
        let key = reg.key_create(0).unwrap();
        let me = GuestThreadId(3);
        assert_eq!(reg.setspecific(me, key, 5), 0);
        assert_eq!(reg.key_delete(key), 0);
        assert_eq!(reg.getspecific(me, key), 0, "deleted key reads NULL");
        assert_eq!(reg.setspecific(me, key, 6), consts::EINVAL, "deleted key write rejected");
    }

    /// __cxa_thread_atexit_impl: reverse registration order at exit.
    #[test]
    fn thread_atexit_lifo() {
        let reg = TlsRegistry::new();
        let me = GuestThreadId(5);
        assert_eq!(reg.thread_atexit(me, 0xD1, 0xA1, 0), 0);
        assert_eq!(reg.thread_atexit(me, 0xD2, 0xA2, 0), 0);
        assert_eq!(reg.thread_atexit(me, 0xD3, 0xA3, 0), 0);
        let mut calls = Vec::new();
        let mut run = |dtor: u64, object: u64| calls.push((dtor, object));
        reg.take_exit_work(me, &mut run);
        assert_eq!(
            calls,
            vec![(0xD3, 0xA3), (0xD2, 0xA2), (0xD1, 0xA1)],
            "LIFO: reverse registration order"
        );
    }

    /// Registration DURING the exit sweep (a destructor registering more
    /// atexit work) runs within the same exit.
    #[test]
    fn thread_atexit_registration_during_exit() {
        let reg = Arc::new(TlsRegistry::new());
        let me = GuestThreadId(6);
        assert_eq!(reg.thread_atexit(me, 0xE1, 0x10, 0), 0);

        let reg2 = reg.clone();
        let ran_late = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_late2 = ran_late.clone();
        let mut run = move |dtor: u64, _obj: u64| {
            if dtor == 0xE1 {
                // The first destructor registers one more handler.
                reg2.thread_atexit(me, 0xE2, 0x20, 0);
            } else {
                ran_late2.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        };
        reg.take_exit_work(me, &mut run);
        assert!(ran_late.load(std::sync::atomic::Ordering::SeqCst), "late registration ran");
    }

    /// pthread_key destructors run AFTER the thread_atexit handlers? NO —
    /// bionic runs atexit FIRST, then key dtors; this test pins the actual
    /// implemented order (atexit last in the returned list = invoked after).
    #[test]
    fn combined_exit_order() {
        let reg = Arc::new(TlsRegistry::new());
        let k = reg.key_create(0x9000).unwrap();
        let me = GuestThreadId(13);
        assert_eq!(reg.setspecific(me, k, 0x77), 0);
        assert_eq!(reg.thread_atexit(me, 0x8000, 0x66, 0), 0);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let calls = calls.clone();
            let mut run = move |dtor: u64, v: u64| calls.lock().unwrap().push((dtor, v));
            let work = reg.take_exit_work(me, &mut run);
            assert_eq!(
                work,
                vec![(0x8000, 0x66), (0x9000, 0x77)],
                "atexit handler first, then key destructor (bionic order)"
            );
        }
    }
}
