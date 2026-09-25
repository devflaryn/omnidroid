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
//!   was deleted yields a DEFINED result: getspecific returns NULL and
//!   setspecific is refused with EINVAL. Its slot can be reallocated by a later
//!   key_create, as in bionic, and the new key starts NULL on every thread.
//!
//! ## Why a read takes no lock (and writes nothing another thread reads)
//!
//! MEASURED (Pet Simulator 99 in-world, `OMNI_PERF`): `pthread_getspecific` runs
//! 1.4-1.9 M times a second on one busy worker, and when every thread's values
//! lived in one `HashMap` behind one mutex, `getspecific` was ~4% of all non-JIT
//! samples and 2-12% of those threads' wall time — ~60 threads taking one lock
//! and hashing to read a word each owns alone.
//!
//! So the table is split by who writes it, the way bionic's is:
//!
//! * **Key slots** ([`TlsRegistry::generations`]): one generation per slot, 0 for
//!   a free one, as atomics. Only `key_create`/`key_delete` write them, under the
//!   registry's lock; everything else only loads them. A generation is never
//!   reused, so a key deleted and re-created in the same slot is a different key.
//! * **A guest thread's values** ([`ThreadValues`]): 128 `(generation, value)`
//!   pairs, written only by that thread. A value is visible only while its pair's
//!   generation equals the slot's — which is how a deleted key reads NULL and a
//!   re-created one does not see the old values, with nothing swept at delete.
//!
//! The values are keyed by **guest** thread id in a map that is locked only to
//! find a thread's block the first time; after that the calling host thread keeps
//! it in a small thread-local cache keyed by (registry, guest thread). A read is
//! then the slot's generation, the cache's first entry, and the pair. Keying by
//! guest id rather than by host thread keeps the old semantics exactly when a
//! host thread runs more than one guest identity, or one guest identity is
//! served from another host thread (a sweep, a host-initiated call).
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
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as HostMutex};

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
pub struct TlsRegistry {
    /// This registry's identity in every host thread's [`CACHE`], unique for the
    /// life of the process. **Not its address**: a dropped registry's address is
    /// the next one's, and a cache that matched on it would hand the new registry
    /// the old one's values.
    id: u64,
    /// Each slot's generation, 0 when the slot is free: the only part of the key
    /// table a `getspecific`/`setspecific` reads. A mirror of `inner.keys`, written
    /// under `inner`'s lock by `key_create`/`key_delete` and nowhere else, so on
    /// the hot path these lines are only ever read. Boxed and aligned so no field
    /// of whatever embeds the registry shares their cache lines.
    generations: Box<SlotGenerations>,
    /// Key table bookkeeping and `__cxa_thread_atexit` registrations: all rare.
    inner: HostMutex<TlsInner>,
    /// Every guest thread's values, by guest thread id. Locked only to find a
    /// thread's block when the calling host thread has not cached it (the first
    /// call, or after the thread's exit sweep retired it) — never per access.
    threads: HostMutex<HashMap<GuestThreadId, Arc<ThreadValues>>>,
    /// How many accesses had to take `threads`' lock. Diagnostic, and the
    /// detector for a cache that stopped caching: written only on that slow path.
    slow_lookups: AtomicU64,
}

impl Default for TlsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Where registry identities come from; 0 is never handed out.
static NEXT_REGISTRY: AtomicU64 = AtomicU64::new(1);

/// One generation per key slot. See [`TlsRegistry::generations`].
#[repr(align(128))]
struct SlotGenerations([AtomicU64; PTHREAD_KEYS_MAX]);

/// One guest thread's `pthread_setspecific` values: a pair per key slot.
///
/// Written only by the thread it belongs to (and by its own exit sweep), so its
/// cache lines are that thread's alone; aligned so no other thread's block
/// shares one. A pair's value counts only while its generation is the slot's.
#[repr(align(128))]
struct ThreadValues {
    /// Set when the exit sweep leaves this thread nothing and its block is taken
    /// out of the map: a host thread still caching it must look again rather than
    /// write where nothing will read.
    retired: AtomicBool,
    pairs: [ValuePair; PTHREAD_KEYS_MAX],
}

/// A value and the generation of the key it was stored under.
struct ValuePair {
    generation: AtomicU64,
    value: AtomicU64,
}

impl ThreadValues {
    fn new() -> Self {
        Self {
            retired: AtomicBool::new(false),
            pairs: std::array::from_fn(|_| ValuePair {
                generation: AtomicU64::new(0),
                value: AtomicU64::new(0),
            }),
        }
    }
}

impl ValuePair {
    /// The value, if it was stored under the key that has generation `generation`.
    #[inline]
    fn read(&self, generation: u64) -> u64 {
        if self.generation.load(Ordering::Relaxed) == generation {
            self.value.load(Ordering::Relaxed)
        } else {
            0
        }
    }

    #[inline]
    fn write(&self, generation: u64, value: u64) {
        self.value.store(value, Ordering::Relaxed);
        self.generation.store(generation, Ordering::Relaxed);
    }
}

/// A block this host thread has already looked up.
struct Cached {
    registry: u64,
    thread: GuestThreadId,
    values: Arc<ThreadValues>,
}

/// How many (registry, guest thread) blocks one host thread keeps. A guest thread
/// is one host thread, so one entry is the common case; the rest cover a host
/// thread that serves several identities or several registries (tests, the main
/// thread) without taking the lock on every switch.
const CACHE_ENTRIES: usize = 4;

thread_local! {
    // A `thread_local!` carries no rustdoc, so: the blocks this host thread has
    // looked up, most recent first. See `TlsRegistry::with_values`.
    static CACHE: RefCell<Vec<Cached>> = const { RefCell::new(Vec::new()) };
}

#[derive(Default)]
struct TlsInner {
    /// key slot -> (dtor, generation). `next_key` is the slot index.
    keys: Vec<KeySlot>,
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
        Self {
            id: NEXT_REGISTRY.fetch_add(1, Ordering::Relaxed),
            generations: Box::new(SlotGenerations(std::array::from_fn(|_| AtomicU64::new(0)))),
            inner: HostMutex::new(TlsInner::default()),
            threads: HostMutex::new(HashMap::new()),
            slow_lookups: AtomicU64::new(0),
        }
    }

    /// The generation of the live key in slot `idx`, or `None` for a free slot or
    /// an index past the table.
    #[inline]
    fn live_generation(&self, idx: usize) -> Option<u64> {
        let generation = self.generations.0.get(idx)?.load(Ordering::Acquire);
        (generation != 0).then_some(generation)
    }

    /// Run `f` on `thread`'s values: from this host thread's cache when its most
    /// recent entry is that block (no lock, no shared write), otherwise through
    /// [`thread_values`](Self::thread_values).
    #[inline]
    fn with_values<R>(&self, thread: GuestThreadId, f: impl FnOnce(&ThreadValues) -> R) -> R {
        let mut f = Some(f);
        let fast = CACHE
            .try_with(|cell| {
                let cache = cell.try_borrow().ok()?;
                let hit = cache.first()?;
                if hit.registry == self.id
                    && hit.thread == thread
                    && !hit.values.retired.load(Ordering::Relaxed)
                {
                    f.take().map(|f| f(&hit.values))
                } else {
                    None
                }
            })
            .ok()
            .flatten();
        match fast {
            Some(result) => result,
            None => {
                let values = self.thread_values(thread);
                (f.take().expect("the fast path returned without running it"))(&values)
            }
        }
    }

    /// `thread`'s values the slow way: promoted from further down this host
    /// thread's cache, or found (or made) in the map under its lock and cached.
    #[cold]
    fn thread_values(&self, thread: GuestThreadId) -> Arc<ThreadValues> {
        let mine = |c: &Cached| c.registry == self.id && c.thread == thread;
        let promoted = CACHE
            .try_with(|cell| {
                let mut cache = cell.try_borrow_mut().ok()?;
                let at = cache
                    .iter()
                    .position(|c| mine(c) && !c.values.retired.load(Ordering::Relaxed))?;
                let entry = cache.remove(at);
                let values = Arc::clone(&entry.values);
                cache.insert(0, entry);
                Some(values)
            })
            .ok()
            .flatten();
        if let Some(values) = promoted {
            return values;
        }
        self.slow_lookups.fetch_add(1, Ordering::Relaxed);
        let values = Arc::clone(
            self.threads
                .lock()
                .unwrap()
                .entry(thread)
                .or_insert_with(|| Arc::new(ThreadValues::new())),
        );
        let _ = CACHE.try_with(|cell| {
            if let Ok(mut cache) = cell.try_borrow_mut() {
                cache.retain(|c| !mine(c) && !c.values.retired.load(Ordering::Relaxed));
                cache.insert(
                    0,
                    Cached { registry: self.id, thread, values: Arc::clone(&values) },
                );
                cache.truncate(CACHE_ENTRIES);
            }
        });
        values
    }

    /// Take `thread`'s block out of the map once its exit sweep has left it
    /// nothing, so a process that starts and ends threads for hours does not
    /// keep a block per thread it ever ran. Marked retired first, for any host
    /// thread still caching it.
    fn retire(&self, thread: GuestThreadId) {
        if let Some(values) = self.threads.lock().unwrap().remove(&thread) {
            values.retired.store(true, Ordering::Relaxed);
        }
        let _ = CACHE.try_with(|cell| {
            if let Ok(mut cache) = cell.try_borrow_mut() {
                cache.retain(|c| !(c.registry == self.id && c.thread == thread));
            }
        });
    }

    /// How many `getspecific`/`setspecific`/sweep accesses had to take the lock
    /// on the thread map because the calling host thread had not cached the
    /// block. Diagnostic: on a warm thread this does not move.
    pub fn slow_lookups(&self) -> u64 {
        self.slow_lookups.load(Ordering::Relaxed)
    }

    /// `pthread_key_create(&key, dtor)`. Returns the pthread error code:
    /// 0, or EAGAIN when all 128 slots are taken (returned, not errno).
    pub fn key_create(&self, dtor: u64) -> Result<u32, i32> {
        let mut inner = self.inner.lock().unwrap();
        if inner.keys.len() >= PTHREAD_KEYS_MAX {
            // Look for a deleted (freed) slot first: bionic reuses slots.
            if let Some(idx) = inner.keys.iter().position(|s| s.dtor == u64::MAX && s.generation == 0) {
                inner.generation += 1;
                let gen = inner.generation;
                inner.keys[idx] = KeySlot { dtor, generation: gen };
                // Released after the slot is filled: a thread that sees this
                // generation sees the key it names. A new generation, so every
                // value stored under the slot's previous key stays invisible.
                self.generations.0[idx].store(gen, Ordering::Release);
                return Ok(idx as u32);
            }
            return Err(consts::EAGAIN);
        }
        inner.generation += 1;
        let gen = inner.generation;
        inner.keys.push(KeySlot { dtor, generation: gen });
        let idx = inner.keys.len() - 1;
        self.generations.0[idx].store(gen, Ordering::Release);
        Ok(idx as u32)
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
        // Mark freed: dtor sentinel u64::MAX, generation 0. Every thread's value
        // for it is dropped by that alone: a pair counts only while its
        // generation is the slot's, and no later key gets this generation again.
        let freed = inner.keys[idx].generation != 0;
        inner.keys[idx] = KeySlot { dtor: u64::MAX, generation: 0 };
        self.generations.0[idx].store(0, Ordering::Release);
        if freed {
            0
        } else {
            consts::EINVAL
        }
    }

    /// `pthread_setspecific(key, value)`. 0, or EINVAL for a dead/invalid key.
    /// Storing NULL clears the entry (POSIX: a NULL set is a valid clear).
    ///
    /// No lock and no write outside `thread`'s own block (see the module docs).
    pub fn setspecific(&self, thread: GuestThreadId, key: u32, value: u64) -> i32 {
        let idx = key as usize;
        let Some(generation) = self.live_generation(idx) else {
            return consts::EINVAL;
        };
        self.with_values(thread, |values| values.pairs[idx].write(generation, value));
        0
    }

    /// `pthread_getspecific(key)`. The current value or NULL for a dead key
    /// (defined behaviour, matches bionic's reuse model).
    ///
    /// No lock and no shared write: the slot's generation, then `thread`'s pair.
    pub fn getspecific(&self, thread: GuestThreadId, key: u32) -> u64 {
        let idx = key as usize;
        let Some(generation) = self.live_generation(idx) else {
            return 0;
        };
        self.with_values(thread, |values| values.pairs[idx].read(generation))
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

        // Phase 2: pthread_key destructors -- bionic's `pthread_key_clean_all`, step for step.
        //
        // Up to PTHREAD_DESTRUCTOR_ITERATIONS rounds of an ascending walk over the key table.
        // At each key that has a destructor and a non-NULL value, the value is cleared and the
        // destructor is called **before the walk moves on to the next key**; a key with no
        // destructor is left alone ("just in case another destructor function is responsible
        // for manually releasing the corresponding data", bionic's own comment). A round that
        // called nothing ends the sweep, and a destructor that sets a key again is met by the
        // next round.
        //
        // **It used to clear every value in a round before calling any destructor**, and clear
        // the destructor-less ones too. Then a destructor saw NULL for every other key -- one
        // that asked `getspecific` for a later key's object, or for a key nothing destroys, got
        // nothing where a device hands it the value. With the engine's mimalloc, whose default
        // heap is a `pthread_getspecific` value, a lower key's destructor that allocated found
        // no heap, made a fresh one and set it, and mimalloc's own destructor then pointed the
        // key at the empty heap -- a heap orphaned under this thread's TPIDR_EL0, which the next
        // thread given the same TLS block inherits as its thread id.
        for _round in 0..PTHREAD_DESTRUCTOR_ITERATIONS {
            let mut called = 0usize;
            let mut idx = 0usize;
            loop {
                // The key table's lock, so the destructor is the one of the generation the value
                // was stored under -- and released before the call, which may create a key.
                let next = {
                    let inner = self.inner.lock().unwrap();
                    let Some(slot) = inner.keys.get(idx).copied() else {
                        break;
                    };
                    let dtor = if slot.dtor == u64::MAX { 0 } else { slot.dtor };
                    if slot.generation == 0 || dtor == 0 {
                        None
                    } else {
                        self.with_values(thread, |values| {
                            let v = values.pairs[idx].read(slot.generation);
                            (v != 0).then(|| {
                                values.pairs[idx].write(slot.generation, 0);
                                (dtor, v)
                            })
                        })
                    }
                };
                if let Some((dtor, value)) = next {
                    run_recorded(dtor, value);
                    work.push((dtor, value));
                    called += 1;
                }
                idx += 1;
            }
            if called == 0 {
                break;
            }
        }

        // A thread the sweep left with nothing to destroy gives its block back, and the values
        // of keys with no destructor go with it, as bionic frees a thread's TLS at its exit. One
        // whose destructors kept re-setting past the cap keeps it, value and all, as before:
        // that value is still what `getspecific` answers for it.
        let leftover = {
            let inner = self.inner.lock().unwrap();
            self.with_values(thread, |values| {
                inner.keys.iter().enumerate().any(|(idx, slot)| {
                    slot.generation != 0
                        && slot.dtor != 0
                        && slot.dtor != u64::MAX
                        && values.pairs[idx].read(slot.generation) != 0
                })
            })
        };
        if !leftover {
            self.retire(thread);
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

    /// Exit sweep, as bionic's `pthread_key_clean_all`: destructors run for non-NULL keys in
    /// ascending key order, each key's value cleared just before ITS destructor is called --
    /// so a destructor still sees every later key's value -- NULL-valued keys skipped, and a
    /// key with no destructor neither called nor cleared.
    #[test]
    fn exit_sweep_order_and_clearing() {
        let reg = Arc::new(TlsRegistry::new());
        let k0 = reg.key_create(0x1000).unwrap(); // dtor 0x1000
        let k1 = reg.key_create(0x2000).unwrap(); // dtor 0x2000
        let k2 = reg.key_create(0).unwrap();      // no dtor
        let me = GuestThreadId(42);
        assert_eq!(reg.setspecific(me, k0, 0xAA0), 0);
        assert_eq!(reg.setspecific(me, k1, 0xAA1), 0);
        assert_eq!(reg.setspecific(me, k2, 0xAA2), 0); // not destroyed: no dtor

        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut run = |dtor: u64, value: u64| {
            calls.lock().unwrap().push((dtor, value));
            // While "inside" a destructor, its own value is ALREADY cleared...
            if dtor == 0x1000 {
                assert_eq!(reg.getspecific(me, k0), 0, "k0 cleared before its dtor runs");
                // ...and a later key's is NOT yet: bionic clears one key at a time.
                assert_eq!(reg.getspecific(me, k1), 0xAA1, "k1 still set while k0's dtor runs");
            } else {
                assert_eq!(reg.getspecific(me, k1), 0, "k1 cleared before its dtor runs");
            }
            // A key without a destructor is left for the thread's TLS to be freed with.
            assert_eq!(reg.getspecific(me, k2), 0xAA2, "a no-dtor key is not cleared");
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

    /// Claim every slot, so the next `key_create` after a delete must reuse one.
    fn full_table(reg: &TlsRegistry, dtor: u64) {
        for i in 0..PTHREAD_KEYS_MAX {
            assert_eq!(reg.key_create(dtor), Ok(i as u32), "slot {i}");
        }
    }

    /// A key deleted and re-created **in the same slot** is a new key: every
    /// thread reads NULL for it, including one that cached its block before the
    /// delete and one on another host thread; the neighbouring key is untouched.
    #[test]
    fn delete_then_recreate_in_the_same_slot_hides_old_values() {
        let reg = Arc::new(TlsRegistry::new());
        full_table(&reg, 0);
        let (a, b) = (GuestThreadId(21), GuestThreadId(22));
        assert_eq!(reg.setspecific(a, 5, 0x55), 0);
        assert_eq!(reg.setspecific(a, 6, 0x66), 0);
        let r = reg.clone();
        std::thread::spawn(move || assert_eq!(r.setspecific(b, 5, 0x77), 0)).join().unwrap();

        assert_eq!(reg.key_delete(5), 0);
        assert_eq!(reg.getspecific(a, 5), 0, "a deleted key reads NULL");
        assert_eq!(reg.key_create(0), Ok(5), "the freed slot is the one reused");
        assert_eq!(reg.getspecific(a, 5), 0, "the new key does not see the old key's value");
        assert_eq!(reg.getspecific(b, 5), 0, "nor another thread's old value");
        let r = reg.clone();
        std::thread::spawn(move || assert_eq!(r.getspecific(b, 5), 0, "nor from its own host thread"))
            .join()
            .unwrap();
        assert_eq!(reg.getspecific(a, 6), 0x66, "the neighbouring key is untouched");
        assert_eq!(reg.setspecific(a, 5, 0x99), 0);
        assert_eq!(reg.getspecific(a, 5), 0x99, "the new key stores normally");
        assert_eq!(reg.getspecific(b, 5), 0);
        // Past the table: defined, not a panic.
        assert_eq!(reg.getspecific(a, PTHREAD_KEYS_MAX as u32), 0);
        assert_eq!(reg.setspecific(a, PTHREAD_KEYS_MAX as u32, 1), consts::EINVAL);
        assert_eq!(reg.getspecific(a, u32::MAX), 0);
    }

    /// The exit sweep hands a re-created key's destructor none of the value the
    /// slot's previous key held: that value died with its key.
    #[test]
    fn a_recreated_slot_does_not_hand_an_old_value_to_the_new_destructor() {
        let reg = TlsRegistry::new();
        full_table(&reg, 0xD0);
        let me = GuestThreadId(31);
        assert_eq!(reg.setspecific(me, 3, 0xAB), 0);
        assert_eq!(reg.setspecific(me, 4, 0xCD), 0);
        assert_eq!(reg.key_delete(3), 0);
        assert_eq!(reg.key_create(0xD1), Ok(3));
        let work = reg.take_exit_work(me, &mut |_, _| {});
        assert_eq!(work, vec![(0xD0, 0xCD)], "only the live key's value, to its own destructor");
    }

    /// Eight host threads, each its own guest thread, hammer the same keys with
    /// values only they write, while a ninth creates and deletes other keys: every
    /// read answers the reader's own last write.
    #[test]
    fn concurrent_threads_each_see_their_own_values() {
        const THREADS: u64 = 8;
        const ITERS: u64 = 100_000;
        let reg = Arc::new(TlsRegistry::new());
        let keys: Vec<u32> = (0..4).map(|_| reg.key_create(0).unwrap()).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let churn = {
            let (reg, stop) = (reg.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut cycles = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let k = reg.key_create(0).unwrap();
                    assert_eq!(reg.getspecific(GuestThreadId(999), k), 0, "a fresh key starts NULL");
                    reg.setspecific(GuestThreadId(999), k, 1);
                    assert_eq!(reg.key_delete(k), 0);
                    cycles += 1;
                }
                cycles
            })
        };
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let (reg, keys) = (reg.clone(), keys.clone());
                std::thread::spawn(move || {
                    let me = GuestThreadId(100 + t);
                    for n in 1..=ITERS {
                        for (j, &k) in keys.iter().enumerate() {
                            let v = (t << 48) | ((j as u64) << 40) | n;
                            assert_eq!(reg.setspecific(me, k, v), 0);
                            assert_eq!(reg.getspecific(me, k), v, "thread {t} key {k} at {n}");
                        }
                    }
                    for (j, &k) in keys.iter().enumerate() {
                        assert_eq!(reg.getspecific(me, k), (t << 48) | ((j as u64) << 40) | ITERS);
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        assert!(churn.join().unwrap() > 0, "the churn ran alongside");
    }

    /// One host thread serving more guest identities than it caches keeps them
    /// all apart, in any order.
    #[test]
    fn one_host_thread_serving_several_guest_ids_keeps_them_apart() {
        let reg = TlsRegistry::new();
        let key = reg.key_create(0).unwrap();
        let ids: Vec<GuestThreadId> = (1..=(CACHE_ENTRIES as u64 + 3)).map(GuestThreadId).collect();
        for id in &ids {
            assert_eq!(reg.getspecific(*id, key), 0, "{id} starts NULL");
            assert_eq!(reg.setspecific(*id, key, id.0 * 10), 0);
        }
        for round in 0..3 {
            for id in ids.iter().rev().chain(ids.iter()).step_by(round + 1) {
                assert_eq!(reg.getspecific(*id, key), id.0 * 10, "{id} in round {round}");
            }
        }
    }

    /// Values belong to the **guest** thread: one set on one host thread is the
    /// value another host thread reads for that guest thread, and back.
    #[test]
    fn values_belong_to_the_guest_thread_not_the_host_thread() {
        let reg = Arc::new(TlsRegistry::new());
        let key = reg.key_create(0).unwrap();
        let g = GuestThreadId(40);
        assert_eq!(reg.setspecific(g, key, 1), 0);
        let r = reg.clone();
        std::thread::spawn(move || {
            assert_eq!(r.getspecific(g, key), 1);
            assert_eq!(r.setspecific(g, key, 2), 0);
        })
        .join()
        .unwrap();
        assert_eq!(reg.getspecific(g, key), 2);
    }

    /// A thread its exit sweep left with nothing gives its block back; a value
    /// set for it afterwards still lands where every host thread reads it.
    #[test]
    fn a_swept_thread_gives_its_block_back_and_later_values_still_land() {
        let reg = Arc::new(TlsRegistry::new());
        let key = reg.key_create(0).unwrap();
        let g = GuestThreadId(50);
        assert_eq!(reg.setspecific(g, key, 5), 0);
        assert!(reg.threads.lock().unwrap().contains_key(&g));
        assert_eq!(reg.take_exit_work(g, &mut |_, _| {}), vec![]);
        assert!(!reg.threads.lock().unwrap().contains_key(&g), "the block was given back");
        assert_eq!(reg.getspecific(g, key), 0, "swept");
        assert_eq!(reg.setspecific(g, key, 6), 0);
        let r = reg.clone();
        std::thread::spawn(move || assert_eq!(r.getspecific(g, key), 6, "seen from another host thread"))
            .join()
            .unwrap();
    }

    /// A block given back by a sweep on one host thread while **another** host
    /// thread still caches it: that thread's next write must land in the block
    /// every host thread reads, not in the one given back.
    #[test]
    fn a_block_given_back_elsewhere_is_not_written_through_a_stale_cache() {
        let reg = Arc::new(TlsRegistry::new());
        let key = reg.key_create(0).unwrap();
        let g = GuestThreadId(60);
        let (to_worker, from_main) = std::sync::mpsc::channel::<()>();
        let (to_main, from_worker) = std::sync::mpsc::channel::<()>();
        let r = reg.clone();
        let worker = std::thread::spawn(move || {
            assert_eq!(r.setspecific(g, key, 1), 0, "the worker caches g's block");
            to_main.send(()).unwrap();
            from_main.recv().unwrap();
            assert_eq!(r.getspecific(g, key), 0, "the sweep cleared it");
            assert_eq!(r.setspecific(g, key, 7), 0);
        });
        from_worker.recv().unwrap();
        assert_eq!(reg.take_exit_work(g, &mut |_, _| {}), vec![]);
        assert!(!reg.threads.lock().unwrap().contains_key(&g), "given back");
        to_worker.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(reg.getspecific(g, key), 7, "the worker's write after the give-back");
    }

    /// Two registries on one host thread, with the same guest id and the same key
    /// number, do not share values -- nor does one made after another was dropped.
    #[test]
    fn two_registries_on_one_host_thread_do_not_share_values() {
        let me = GuestThreadId(1);
        let one = TlsRegistry::new();
        let two = TlsRegistry::new();
        let (k1, k2) = (one.key_create(0).unwrap(), two.key_create(0).unwrap());
        assert_eq!(k1, k2, "the same key number in both");
        assert_eq!(one.setspecific(me, k1, 0xA), 0);
        assert_eq!(two.getspecific(me, k2), 0);
        assert_eq!(two.setspecific(me, k2, 0xB), 0);
        assert_eq!(one.getspecific(me, k1), 0xA);
        drop(one);
        let three = TlsRegistry::new();
        let k3 = three.key_create(0).unwrap();
        assert_eq!(three.getspecific(me, k3), 0, "a new registry starts NULL");
    }

    /// **The hot path takes no lock.** Once a host thread has looked its blocks
    /// up, reads and writes -- even alternating between two guest identities --
    /// never go to the shared map again.
    #[test]
    fn a_warm_thread_takes_no_lock() {
        let reg = TlsRegistry::new();
        let key = reg.key_create(0).unwrap();
        let (a, b) = (GuestThreadId(1), GuestThreadId(2));
        assert_eq!(reg.setspecific(a, key, 1), 0);
        assert_eq!(reg.setspecific(b, key, 2), 0);
        let before = reg.slow_lookups();
        for n in 0..1000u64 {
            assert_eq!(reg.setspecific(a, key, n + 10), 0);
            assert_eq!(reg.getspecific(b, key), 2);
            assert_eq!(reg.getspecific(a, key), n + 10);
        }
        assert_eq!(reg.slow_lookups(), before, "a warm thread went to the locked map");
        assert!(before >= 2, "the cold lookups are counted: {before}");
    }

    /// `getspecific` alone, without the import crossing around it (which
    /// `omni-android`'s `tests/perf.rs` measures), on one thread and on eight.
    ///
    /// `cargo test -p omni-bionic --release --lib -- --ignored --nocapture getspecific_alone`
    #[test]
    #[ignore = "measurement, not a test"]
    fn the_cost_of_getspecific_alone() {
        const EACH: u64 = 10_000_000;
        for threads in [1u64, 8] {
            let reg = Arc::new(TlsRegistry::new());
            let key = reg.key_create(0).unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(threads as usize + 1));
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let (reg, barrier) = (reg.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        let me = GuestThreadId(t + 1);
                        reg.setspecific(me, key, t + 1);
                        barrier.wait();
                        let mut sum = 0u64;
                        for _ in 0..EACH {
                            sum = sum.wrapping_add(reg.getspecific(std::hint::black_box(me), key));
                        }
                        assert_eq!(sum, (t + 1).wrapping_mul(EACH));
                    })
                })
                .collect();
            barrier.wait();
            let start = std::time::Instant::now();
            for h in handles {
                h.join().unwrap();
            }
            println!(
                "getspecific alone, {threads} thread(s): {:.1} ns per call per thread",
                start.elapsed().as_secs_f64() * 1e9 / EACH as f64
            );
        }
    }
}
