//! Readiness: `eventfd`, `timerfd`, `epoll` and `ppoll` -- what every `Looper` waits in.
//!
//! # Wait queues
//!
//! The kernel's wait queues, by what is waited on. Whatever can make a descriptor ready -- a pipe or
//! a socket pair written or closed, an eventfd written, binder work queued for a process, an input
//! event sent -- is known by a [`Key`] (the address of the thing that changed), and its change is
//! told with [`notify_key`]: that wakes the threads waiting on that key, and no others. A waiter
//! says what it waits on with [`watch`] -- the keys of every descriptor in its `epoll` set or `poll`
//! list ([`key_of`]) -- *before* it looks, so a change between the look and the sleep still wakes it.
//! A timer is ready by the clock alone, so a waiter also wakes at the earliest expiry among what it
//! watches.
//!
//! Anything without a key (a nested `epoll`, a kind of file not keyed yet) is waited on as
//! "anything": such a waiter wakes on every change, as every waiter used to, and [`notify`] (a
//! change with no key: a descriptor closed, an `epoll` set edited) wakes everyone. The two agree by
//! construction: a keyed change wakes its key's waiters and the "anything" ones.
//!
//! Why: with one wake-up for everyone, the system's host process (init's services and
//! system_server, ~200-500 waiting threads) made ~110,000-170,000 thread wake-ups a second while
//! booting, from ~700-1,300 changes (`OMNI_POLL_STATS`, run 2026-09-28) -- nearly all of them
//! threads that looked and found nothing.
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::errno::{Errno, SysResult, EAGAIN, EEXIST, EINTR, EINVAL, ENOENT};
use crate::fd::{FileKind, OpenFile};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

pub const IN: u32 = 0x001;
pub const OUT: u32 = 0x004;
pub const ERR: u32 = 0x008;
pub const HUP: u32 = 0x010;
const NVAL: u32 = 0x020;
const ONESHOT: u32 = 1 << 30;
/// `EPOLLET`: report a descriptor once per change, not for as long as it is ready.
const EDGE: u32 = 1 << 31;

/// `epoll_et=0|1` (lever; **1 by default**: it is Linux's meaning): `EPOLLET` entries are
/// edge-triggered. 0 treats them as level-triggered, as before, for an A/B.
pub static EDGE_TRIGGERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// **Changes told, by key, for edge-triggered epoll** (`EPOLLET`): each [`notify_key`] bumps its
/// key's slot, each [`notify`] the common word; an edge-triggered entry is armed again when either
/// moved since it last looked (and on a rising readiness bit), and reports once. A hashed table:
/// two keys sharing a slot only re-arm each other (a spurious event, which `EPOLLET` users take).
static EDGES: [std::sync::atomic::AtomicU32; 1 << 14] = [const { std::sync::atomic::AtomicU32::new(0) }; 1 << 14];
static EDGE_ALL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn edge_slot(key: Key) -> &'static std::sync::atomic::AtomicU32 {
    &EDGES[((key >> 4) ^ (key >> 18) ^ (key >> 32)) & ((1 << 14) - 1)]
}

/// Count a change of `key` for edge-triggered entries (what [`notify_key`] does, without waking
/// anyone): a timerfd's expiry count taken, which no waiter needs to hear of.
pub(crate) fn edge(key: Key) {
    edge_slot(key).fetch_add(1, Ordering::AcqRel);
}

/// The changes counted for `key` (or none: the common word only), as one number to compare.
fn edges_of(key: Option<Key>) -> u64 {
    let own = key.map_or(0, |k| edge_slot(k).load(Ordering::Acquire));
    (u64::from(own) << 32) | u64::from(EDGE_ALL.load(Ordering::Acquire))
}
const O_NONBLOCK: u32 = 0o4000;
const O_CLOEXEC: u64 = 0o2000000;

/// What a waiter waits on: the address of the thing whose change makes it ready.
pub type Key = usize;

/// A key for what never changes by itself (always ready, or never): nobody notifies it.
pub const INERT: Key = 1;

/// One thread's place in the queues: whether a change has woken it since it registered, and what
/// its sleep waits on -- its host thread's park token or a condition variable (`waiter_park`).
struct Waiter {
    woken: std::sync::atomic::AtomicBool,
    /// This wait sleeps parked ([`PARK`] when it registered); a waker wakes it the same way.
    parks: std::sync::atomic::AtomicBool,
    thread: std::thread::Thread,
    lock: Mutex<()>,
    cv: Condvar,
    /// Times [`signal`](Self::signal)led, for the tests.
    #[cfg(test)]
    signals: std::sync::atomic::AtomicU32,
}

impl Waiter {
    fn new() -> Self {
        Self {
            woken: std::sync::atomic::AtomicBool::new(false),
            parks: std::sync::atomic::AtomicBool::new(false),
            thread: std::thread::current(),
            lock: Mutex::new(()),
            cv: Condvar::new(),
            #[cfg(test)]
            signals: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Mark it woken: true when it was not, and the caller then [`signal`](Self::signal)s it.
    fn mark(&self) -> bool {
        !self.woken.swap(true, Ordering::AcqRel)
    }

    /// End its sleep, `woken` being set. On the condition variable, the lock taken between keeps
    /// the signal from falling between the sleeper's look at `woken` and its wait.
    fn signal(&self) {
        #[cfg(test)]
        self.signals.fetch_add(1, Ordering::Relaxed);
        if self.parks.load(Ordering::Relaxed) {
            self.thread.unpark();
        } else {
            drop(self.lock.lock());
            self.cv.notify_one();
        }
    }

    /// Mark and signal it, woken already or not: once for each of its keys a change names
    /// (`wake_dedupe=0`).
    fn wake(&self) {
        self.woken.store(true, Ordering::Release);
        self.signal();
    }
}

thread_local! {
    static ME: Arc<Waiter> = Arc::new(Waiter::new());
}

/// **`wake_dedupe`** (lever; on by default): a change marks each of its waiters woken under the
/// queue locks -- once, however many of the waiter's keys it names -- and signals them after
/// dropping the locks, as `crate::futex` unparks; and it finds no waiter on anything without taking
/// that list's lock (`ANYTHING_COUNT`; the list is empty nearly always since `poll_keyed`).
/// `wake_dedupe=0` (`OMNI_WAKE_DEDUPE=0`) wakes each key's waiters under its lock, as before. Herds
/// ([`wake_everyone`], [`notify`]) are where it shows: a looper waits on every key of its epoll set
/// and was signalled once a key, each under a shard lock its woken threads then queued on to
/// unregister -- and, registered again before the walk reached their other keys, woken again (the
/// system host at launch: 3.7% of busy samples in `__psynch_cvsignal`, most of it from
/// `wake_everyone`). Measured, a herd of 256 waiters on 6 keys each (`tests/wait_cost.rs`, M4 under
/// load, 2026-10-20): 400-1,210 waits ended and 7-44 ms of CPU before, 256 and 5-6.5 ms with this,
/// 256 and 2.7-3.7 ms with `waiter_park` too. Both levers, a pool device at Roblox's Home (two
/// ABBA pairs, M4, 2026-10-20): the system host 18.0-18.9% of a core with both off, 10.2-13.4% with
/// both on; boot and launch the same.
pub static DEDUPE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// **`waiter_park`** (lever; on by default): a wait sleeps on its host thread's park token
/// (`std::thread::park_timeout`, a relative timeout) and a waker `unpark`s it -- not on a
/// `parking_lot` condition variable, whose timed wait takes an absolute deadline (on macOS a
/// `gettimeofday` each sleep) and a pthread mutex and condition behind parking_lot's bucket lock.
/// `waiter_park=0` (`OMNI_WAITER_PARK=0`) is the condition variable. The token is the thread's own,
/// shared with `crate::futex`'s park: a token one leaves ends the other's park early, and both look
/// again (`woken` and the clock here, the futex queue under its lock) before parking again.
/// Measured, a `ppoll` or `epoll_pwait` that sleeps out a 1 ms timeout (an HttpClient's, a
/// looper's): -17% of its CPU (the median of 14 pairs, each taken in one run), mostly 17-19 us on
/// the condition variable against 13-16 us parked under load (`tests/wait_cost.rs`, M4, 2026-10-20).
pub static PARK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// One test at a time switches [`DEDUPE`] and [`PARK`] (they are the host process's).
#[cfg(test)]
pub(crate) static LEVERS_IN_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn dedupe() -> bool {
    DEDUPE.load(Ordering::Relaxed)
}

/// Waiters marked woken under a queue's lock, to signal once it is dropped (`wake_dedupe`), as
/// `crate::futex`'s `Unpark`. The usual one without allocating.
#[derive(Default)]
struct Wakes {
    one: Option<Arc<Waiter>>,
    more: Vec<Arc<Waiter>>,
}

impl Wakes {
    /// Mark `w` woken, kept to signal unless it was woken already; whether it was kept.
    fn mark(&mut self, w: &Arc<Waiter>) -> bool {
        if !w.mark() {
            return false;
        }
        if self.one.is_none() {
            self.one = Some(Arc::clone(w));
        } else {
            self.more.push(Arc::clone(w));
        }
        true
    }

    /// Signal them, the locks dropped; how many.
    fn signal(self) -> usize {
        let n = usize::from(self.one.is_some()) + self.more.len();
        for w in self.one.into_iter().chain(self.more) {
            w.signal();
        }
        n
    }
}

/// The keys a watch is in: up to four in place -- a binder read's two, an eventfd's one -- more on
/// the heap. A watch given an array allocates nothing for them.
pub(crate) enum Keys {
    Few(u8, [Key; 4]),
    Many(Vec<Key>),
}

impl std::ops::Deref for Keys {
    type Target = [Key];

    fn deref(&self) -> &[Key] {
        match self {
            Self::Few(n, keys) => &keys[..usize::from(*n)],
            Self::Many(keys) => keys,
        }
    }
}

impl From<Vec<Key>> for Keys {
    fn from(keys: Vec<Key>) -> Self {
        Self::Many(keys)
    }
}

impl<const N: usize> From<[Key; N]> for Keys {
    fn from(keys: [Key; N]) -> Self {
        if N > 4 {
            return Self::Many(keys.to_vec());
        }
        let mut few = [0; 4];
        few[..N].copy_from_slice(&keys);
        Self::Few(N as u8, few)
    }
}

/// Keys are addresses: hashed by one multiplication rather than SipHash, rotated so a shard's map
/// does not take its buckets from the bits [`shard`] chose the shard by.
#[derive(Default)]
struct KeyHasher(u64);

impl std::hash::Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(26)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(b);
        }
    }

    fn write_usize(&mut self, n: usize) {
        self.0 = n as u64;
    }
}

type KeyMap<V> = HashMap<Key, V, std::hash::BuildHasherDefault<KeyHasher>>;

/// **The queues**, split so that waiters on different things do not wait for one another's lock:
/// the change counter (for the callers that still compare it), the waiters on anything, and the
/// waiters by key in [`SHARDS`] lists chosen by the key. One lock for all of them was the system
/// host's busiest lock: each binder read registers and unregisters its thread, hundreds of threads
/// at once (M4, 2026-10-16: `parking_lot` spinning in `Watch::drop`). A change takes the lock of
/// the list its waiters are in after it is made, and a waiter is in its lists before it looks, so
/// none is missed (as with one lock).
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ANYTHING: crate::oslock::OsMutex<Vec<Arc<Waiter>>> = crate::oslock::OsMutex::new(Vec::new());
/// [`ANYTHING`]'s length, stored under its lock: a change that reads 0 skips that lock
/// (`wake_dedupe`). A waiter on anything stores it, fences, then looks; a change is made, fenced,
/// then the count read -- of two `SeqCst` fences one is first, so either the change sees the
/// waiter or the waiter's look sees the change.
static ANYTHING_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const SHARDS: usize = 64;
static KEYED_QUEUES: LazyLock<Vec<crate::oslock::OsMutex<KeyMap<Vec<Arc<Waiter>>>>>> = LazyLock::new(|| (0..SHARDS).map(|_| crate::oslock::OsMutex::default()).collect());

/// The list of waiters `key` is in.
fn shard(key: Key) -> &'static crate::oslock::OsMutex<KeyMap<Vec<Arc<Waiter>>>> {
    &KEYED_QUEUES[((key as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize % SHARDS]
}

/// Wake every waiter on anything, under the lock; how many (`wake_dedupe=0`).
fn wake_anything() -> usize {
    let anything = ANYTHING.lock();
    for w in anything.iter() {
        w.wake();
    }
    anything.len()
}

/// Wake `key`'s waiters under the lock (`wake_dedupe=0`).
fn wake_key(key: Key) {
    if let Some(list) = shard(key).lock().get(&key) {
        for w in list {
            w.wake();
        }
    }
}

/// Mark the waiters on anything into `wakes`; how many were marked.
fn mark_anything(wakes: &mut Wakes) -> usize {
    std::sync::atomic::fence(Ordering::SeqCst);
    if ANYTHING_COUNT.load(Ordering::Relaxed) == 0 {
        return 0;
    }
    ANYTHING.lock().iter().filter(|w| wakes.mark(w)).count()
}

/// Mark `key`'s waiters into `wakes`.
fn mark_key(key: Key, wakes: &mut Wakes) {
    if let Some(list) = shard(key).lock().get(&key) {
        for w in list {
            wakes.mark(w);
        }
    }
}

/// Wake every waiter of this host process, on anything and on every key; how many wake-ups were
/// sent.
fn wake_all() -> usize {
    if dedupe() {
        let mut wakes = Wakes::default();
        mark_anything(&mut wakes);
        for shard in KEYED_QUEUES.iter() {
            for w in shard.lock().values().flatten() {
                wakes.mark(w);
            }
        }
        wakes.signal()
    } else {
        let mut n = wake_anything();
        for shard in KEYED_QUEUES.iter() {
            for w in shard.lock().values().flatten() {
                w.wake();
                n += 1;
            }
        }
        n
    }
}

/// Something changed that no key names (a descriptor closed, an `epoll` set edited): wake every
/// waiter to look again.
pub fn notify() {
    EDGE_ALL.fetch_add(1, Ordering::AcqRel);
    GENERATION.fetch_add(1, Ordering::SeqCst);
    let woken = wake_all();
    STATS.notifies.fetch_add(1, Ordering::Relaxed);
    STATS.unkeyed.fetch_add(1, Ordering::Relaxed);
    STATS.herd_woken.fetch_add(woken as u64, Ordering::Relaxed);
}

/// What `key` names changed, for **one** of its waiters to act on: wake the first of them not woken
/// already, and those waiting on anything. For work any one waiter takes whole (a binder
/// process's queue: `crate::binder`'s), which woke every idle looper of the process to find it
/// gone. The caller makes sure work left behind wakes another.
pub fn notify_key_one(key: Key) {
    edge(key);
    GENERATION.fetch_add(1, Ordering::SeqCst);
    let mut wakes = dedupe().then(Wakes::default);
    let anything = match &mut wakes {
        Some(wakes) => mark_anything(wakes),
        None => wake_anything(),
    };
    if let Some(list) = shard(key).lock().get(&key) {
        // The most recent waiter first, as the kernel's binder picks the thread that went idle
        // last (`binder_select_thread_ilocked`, a list it adds to at the head): the work stays on
        // a few threads whose translations are warm, instead of going round every idle looper --
        // on arm64 each thread translates what it runs into a cache of its own.
        for w in list.iter().rev() {
            let taken = match &mut wakes {
                Some(wakes) => wakes.mark(w),
                None => {
                    let woken = w.mark();
                    if woken {
                        w.signal();
                    }
                    woken
                }
            };
            if taken {
                break;
            }
        }
    }
    if let Some(wakes) = wakes {
        wakes.signal();
    }
    STATS.notifies.fetch_add(1, Ordering::Relaxed);
    STATS.anything_woken.fetch_add(anything as u64, Ordering::Relaxed);
}

/// What `key` names changed: wake its waiters, and those waiting on anything.
pub fn notify_key(key: Key) {
    notify_keys(&[key]);
}

/// `poll_slice_ms=<ms>` (lever; **1000 by default** since the in-world A/B of 2026-10-09, PS99
/// session s4: 50 -> 1000 ms gave the system host 40 -> 31 ms of CPU a frame and fps +1.4..+5.1;
/// it was 50): the longest a poll-family wait sleeps before it looks again by itself. It was the only way a signal posted to a waiting task was seen, so every
/// waiting thread woke 20 times a second -- the system's host process has ~700 of them, ~14,000
/// wake-ups a second for nothing. A posted signal now wakes the waiters itself
/// ([`wake_everyone`], from `Process::post_signal` and a process's end), so the slice can be long.
pub static SLICE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);

/// Wake every waiter of this host process to look again: a signal was posted (the waiter whose
/// task it is sees it and answers `EINTR`), or a process is ending. Not rare: each exec'd helper's
/// end is one, and the `SIGCHLD` its parent may take another (`OMNI_POLL_STATS` counts them).
pub fn wake_everyone() {
    let woken = wake_all();
    STATS.everyone.fetch_add(1, Ordering::Relaxed);
    STATS.herd_woken.fetch_add(woken as u64, Ordering::Relaxed);
}

/// What each of `keys` names changed: their waiters woken, and those waiting on anything -- each
/// once (`wake_dedupe`).
pub fn notify_keys(keys: &[Key]) {
    for key in keys {
        edge(*key);
    }
    GENERATION.fetch_add(1, Ordering::SeqCst);
    let anything = if dedupe() {
        let mut wakes = Wakes::default();
        let anything = mark_anything(&mut wakes);
        for key in keys {
            mark_key(*key, &mut wakes);
        }
        wakes.signal();
        anything
    } else {
        let anything = wake_anything();
        for key in keys {
            wake_key(*key);
        }
        anything
    };
    STATS.notifies.fetch_add(1, Ordering::Relaxed);
    STATS.anything_woken.fetch_add(anything as u64, Ordering::Relaxed);
}

/// A thread's registration in the queues, for one wait: made before it looks, ended when dropped.
/// The thread that made it sleeps on it (a parked wait is woken by unparking that thread).
pub(crate) struct Watch {
    me: Arc<Waiter>,
    keys: Option<Keys>,
}

/// Wait on `keys` (`None`: on anything) -- registered now, before the caller looks.
pub(crate) fn watch<K: Into<Keys>>(keys: Option<K>) -> Watch {
    // A wait on no key at all (`ppoll` of no descriptor) is still a waiter: in a queue that a
    // posted signal, a process's end and an unkeyed change reach ([`wake_everyone`], [`notify`]).
    // In none, it slept through them until its slice ran out (1 s).
    let keys = keys.map(|k| {
        let k = k.into();
        if k.is_empty() { Keys::from([INERT]) } else { k }
    });
    let me = ME.with(Arc::clone);
    me.parks.store(PARK.load(Ordering::Relaxed), Ordering::Relaxed);
    me.woken.store(false, Ordering::Release);
    match &keys {
        None => {
            let mut anything = ANYTHING.lock();
            anything.push(Arc::clone(&me));
            ANYTHING_COUNT.store(anything.len(), Ordering::Relaxed);
            drop(anything);
            // Before the caller looks (`ANYTHING_COUNT`).
            std::sync::atomic::fence(Ordering::SeqCst);
        }
        Some(keys) => {
            for k in keys.iter() {
                shard(*k).lock().entry(*k).or_default().push(Arc::clone(&me));
            }
        }
    }
    Watch { me, keys }
}

/// [`watch`] on anything.
fn watch_anything() -> Watch {
    watch(None::<Keys>)
}

/// What kind of file has no key, for `OMNI_POLL_STATS`: each kind said once per host process.
fn note_unkeyed(file: &OpenFile) {
    static SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("OMNI_POLL_STATS").is_some()) {
        return;
    }
    let what = match &*file.kind.lock() {
        FileKind::Socket(s) => {
            let peer = match &s.peer {
                None => "none",
                Some(crate::socket::Peer::Inet(_)) => "inet",
                Some(crate::socket::Peer::Dns(_)) => "dns",
                Some(crate::socket::Peer::Host(_)) => "host",
                Some(_) => "other",
            };
            format!("socket domain {} type {} peer {peer}", s.domain, s.ty)
        }
        k => format!("{:?}", std::mem::discriminant(k)),
    };
    let mut said = SAID.lock();
    if !said.contains(&what) {
        eprintln!("[poll] pid {}: a wait on anything, for a {what}", std::process::id());
        said.push(what);
    }
}

/// The keys of these files, or `None` when one of them has none (then the wait is on anything).
pub(crate) fn keys_of<'a>(files: impl IntoIterator<Item = &'a OpenFile>) -> Option<Vec<Key>> {
    let mut keys = Vec::new();
    for f in files {
        let Some(k) = key_of(f) else {
            note_unkeyed(f);
            return None;
        };
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    Some(keys)
}

impl Watch {
    /// Sleep until a change it watches, `deadline`, or a slice ([`SLICE_MS`]); `EINTR` if the
    /// task has a deliverable signal. The caller looks again afterwards.
    pub(crate) fn wait(&self, deadline: Option<Instant>, task: &Task) -> Result<(), Errno> {
        if task.pending.load(Ordering::SeqCst) & !task.sigmask != 0 || task.process.futexes.interrupted() {
            return Err(EINTR);
        }
        let now = Instant::now();
        let slice = now + Duration::from_millis(SLICE_MS.load(Ordering::Relaxed).max(1));
        self.sleep_from(Some(now), deadline.map_or(slice, |d| d.min(slice)));
        Ok(())
    }

    /// Sleep until a change it watches or `until`: a host thread's wait.
    pub(crate) fn sleep(&self, until: Instant) {
        self.sleep_from(None, until);
    }

    /// [`sleep`](Self::sleep), given the time now when the caller has it. Parked, the clock is read
    /// once a park and not after a wake.
    fn sleep_from(&self, now: Option<Instant>, until: Instant) {
        let _counted = Waiting::new();
        let me = &*self.me;
        debug_assert_eq!(me.thread.id(), std::thread::current().id(), "a watch slept on by a thread that did not make it");
        if me.parks.load(Ordering::Relaxed) {
            let mut now = now.unwrap_or_else(Instant::now);
            while !me.woken.load(Ordering::Acquire) && now < until {
                std::thread::park_timeout(until - now);
                if me.woken.load(Ordering::Acquire) {
                    break;
                }
                now = Instant::now();
            }
        } else {
            let mut guard = me.lock.lock();
            while !me.woken.load(Ordering::Acquire) {
                if me.cv.wait_until(&mut guard, until).timed_out() {
                    break;
                }
            }
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let me = &self.me;
        // In order: the newest waiter is the last (`notify_key_one`).
        let gone = |list: &mut Vec<Arc<Waiter>>| {
            if let Some(i) = list.iter().rposition(|w| Arc::ptr_eq(w, me)) {
                list.remove(i);
            }
        };
        match &self.keys {
            None => {
                let mut anything = ANYTHING.lock();
                gone(&mut anything);
                ANYTHING_COUNT.store(anything.len(), Ordering::Relaxed);
            }
            Some(keys) => {
                for k in keys.iter() {
                    let mut shard = shard(*k).lock();
                    if let Some(list) = shard.get_mut(k) {
                        gone(list);
                        if list.is_empty() {
                            shard.remove(k);
                        }
                    }
                }
            }
        }
    }
}

/// **`poll_keyed`** (lever; ON by default since the night of 2026-10-09: in PS99 the system host
/// fell from 54 to 47 ms of CPU a frame and fps rose 1.2-2.4 in 5/5 pairs; `poll_keyed=0` or
/// `OMNI_POLL_KEYED=0` is the way back): fewer waits on "anything".
///
/// A thread whose `epoll`/`poll` set holds one descriptor without a key waits on anything, and is
/// woken by **every** change told in its host process -- each binder transaction queued, each
/// eventfd written (every `Looper` wake), each socket-pair message -- to look over its whole set and
/// sleep again. In the system's host process (~60 guest processes, hundreds of waiting threads)
/// that is thousands of changes a second, times each such waiter. The common descriptors without a
/// key are ones whose readiness **never changes** (a regular or `/proc` file, a device node, a
/// shared-memory region: always "ready", `readiness` below), which no change can make ready, and
/// sockets whose readiness only their **own** sends change (netlink -- netd's, vold's, healthd's
/// and ueventd's listeners --, init's property service, a datagram socket connected to a bound
/// one, `logd`'s). With this on, the first are inert and the second keyed by the socket itself,
/// told when a send on it queues an answer. And `epoll_ctl` (an add or a change) wakes the waiters
/// of that epoll set only -- each now also waits on its set's own key -- instead of every waiter
/// in the process. `OMNI_POLL_STATS` counts what is left (waiters on anything, unkeyed changes).
pub static KEYED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn keyed() -> bool {
    KEYED.load(Ordering::Relaxed)
}

/// What a change to `file` is told by: its key, or `None` for a kind of file whose changes are
/// told to everyone (then a wait on it is a wait on anything).
pub(crate) fn key_of(file: &OpenFile) -> Option<Key> {
    match &*file.kind.lock() {
        FileKind::EventFd(e) => Some(Arc::as_ptr(e) as Key),
        FileKind::TimerFd(t) => Some(Arc::as_ptr(t) as Key),
        FileKind::Pipe(end) => Some(crate::pipe::key(end)),
        FileKind::Socket(s) => crate::socket::key(s).or_else(|| if keyed() { crate::socket::own_key(s) } else { None }),
        FileKind::Binder(b) => Some(b.key()),
        FileKind::Evdev(c) => Some(Arc::as_ptr(c.device()) as Key),
        // Always ready, or never: nothing to be woken for.
        FileKind::SyncFile(_) | FileKind::Inotify(_) | FileKind::Bpf(_) | FileKind::RemoteBinder(_) => Some(INERT),
        // Always ready (`readiness`' last arm), so no change makes them so (`poll_keyed`).
        FileKind::Host { .. }
        | FileKind::Dir { .. }
        | FileKind::Dev(_)
        | FileKind::Stdin
        | FileKind::Stdout(_)
        | FileKind::Stderr(_)
        | FileKind::Synth { .. }
        | FileKind::Gpu(_)
        | FileKind::Shared(_)
        | FileKind::Fuse(_)
            if keyed() =>
        {
            Some(INERT)
        }
        _ => None,
    }
}

/// How many changes have been told with no key, and how many waiter wake-ups were sent (any
/// cause, timeouts included): for a measurement.
#[must_use]
pub fn counts() -> (u64, u64) {
    (STATS.unkeyed.load(Ordering::Relaxed), STATS.wakes.load(Ordering::Relaxed))
}

/// What `OMNI_POLL_STATS=<s>` reports: how often something changes, and how many are woken.
struct Stats {
    notifies: std::sync::atomic::AtomicU64,
    waiting: std::sync::atomic::AtomicI64,
    wakes: std::sync::atomic::AtomicU64,
    /// Changes told with no key ([`notify`]): every waiter woken.
    unkeyed: std::sync::atomic::AtomicU64,
    /// Wake-ups sent to waiters on anything by keyed changes ([`notify_key`]).
    anything_woken: std::sync::atomic::AtomicU64,
    /// Calls of [`wake_everyone`]: posted signals and processes' ends.
    everyone: std::sync::atomic::AtomicU64,
    /// Wake-ups sent by herds ([`wake_everyone`], [`notify`]): one a waiter (`wake_dedupe`), or
    /// one for each key of each waiter.
    herd_woken: std::sync::atomic::AtomicU64,
}

static STATS: Stats = Stats {
    notifies: std::sync::atomic::AtomicU64::new(0),
    waiting: std::sync::atomic::AtomicI64::new(0),
    wakes: std::sync::atomic::AtomicU64::new(0),
    unkeyed: std::sync::atomic::AtomicU64::new(0),
    anything_woken: std::sync::atomic::AtomicU64::new(0),
    everyone: std::sync::atomic::AtomicU64::new(0),
    herd_woken: std::sync::atomic::AtomicU64::new(0),
};

/// `OMNI_POLL_STATS=<seconds>`: a `[poll]` line that often -- changes told a second, threads
/// waiting, and wake-ups a second; then of those, the changes told with no key (each wakes every
/// waiter), the threads waiting on anything now, and the wake-ups a second keyed changes sent
/// them (what `poll_keyed` is for); then the herds -- `wake_everyone` calls a second -- and the
/// wake-ups a second herds of either kind sent (what `wake_dedupe` cuts).
pub fn start_stats() {
    let Some(every) = std::env::var("OMNI_POLL_STATS").ok().and_then(|v| v.parse::<u64>().ok()).filter(|&s| s > 0) else { return };
    let _ = std::thread::Builder::new().name("omni-poll-stats".into()).spawn(move || {
        let (mut unkeyed_was, mut anything_was) = (0, 0);
        loop {
            std::thread::sleep(Duration::from_secs(every));
            let n = STATS.notifies.swap(0, Ordering::Relaxed);
            let w = STATS.wakes.swap(0, Ordering::Relaxed);
            let (everyone, herd_woken) = (STATS.everyone.swap(0, Ordering::Relaxed), STATS.herd_woken.swap(0, Ordering::Relaxed));
            let (unkeyed, anything) = (STATS.unkeyed.load(Ordering::Relaxed), STATS.anything_woken.load(Ordering::Relaxed));
            let on_anything = ANYTHING_COUNT.load(Ordering::Relaxed);
            eprintln!(
                "[poll] pid {}: {} notifications/s, {} threads waiting, {} wake-ups/s; {} unkeyed/s, {on_anything} waiting on anything, {} wake-ups/s to them (poll_keyed={}); {} wake_everyone/s, {} herd wake-ups/s (wake_dedupe={})",
                std::process::id(),
                n / every,
                STATS.waiting.load(Ordering::Relaxed),
                w / every,
                (unkeyed - unkeyed_was) / every,
                (anything - anything_was) / every,
                u8::from(keyed()),
                everyone / every,
                herd_woken / every,
                u8::from(dedupe()),
            );
            (unkeyed_was, anything_was) = (unkeyed, anything);
        }
    });
}

/// Counts a waiter while it waits.
struct Waiting;

impl Waiting {
    fn new() -> Self {
        STATS.waiting.fetch_add(1, Ordering::Relaxed);
        Waiting
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        STATS.waiting.fetch_sub(1, Ordering::Relaxed);
        STATS.wakes.fetch_add(1, Ordering::Relaxed);
    }
}

/// Wait for any change after the one `seen` counted ([`generation`]), until `deadline` at the
/// latest; `EINTR` if the task has a deliverable signal. A waiter re-checks what it waits for
/// afterwards: this can wake early. A wait on anything; [`watch`] waits on what it names.
pub(crate) fn wait_for_change(seen: u64, deadline: Option<Instant>, task: &Task) -> Result<(), Errno> {
    let w = watch_anything();
    if GENERATION.load(Ordering::SeqCst) != seen {
        return Ok(());
    }
    w.wait(deadline, task)
}

/// [`wait_for_change`] for a host thread, which has no signals to see: until `deadline` at the
/// latest.
pub(crate) fn wait_for_change_host(seen: u64, deadline: Instant) {
    let w = watch_anything();
    if GENERATION.load(Ordering::SeqCst) == seen {
        w.sleep(deadline);
    }
}

pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------------- eventfd

pub struct EventFd {
    count: Mutex<u64>,
    semaphore: bool,
}

fn sys_eventfd2(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const EFD_SEMAPHORE: u64 = 1;
    if a[1] & !(EFD_SEMAPHORE | u64::from(O_NONBLOCK) | O_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let efd = Arc::new(EventFd { count: Mutex::new(a[0] & 0xffff_ffff), semaphore: a[1] & EFD_SEMAPHORE != 0 });
    let file = OpenFile { kind: Mutex::new(FileKind::EventFd(efd)), flags: Mutex::new(2 | (a[1] as u32 & O_NONBLOCK)) };
    Ok(p.fds.insert(Arc::new(file), a[1] & O_CLOEXEC != 0, 0)? as u64)
}

fn eventfd_read(efd: &EventFd, buf: &mut [u8], nonblocking: bool, task: &Task) -> Result<usize, Errno> {
    if buf.len() < 8 {
        return Err(EINVAL);
    }
    let key = std::ptr::from_ref(efd) as Key;
    // Looked at first, registered only to wait (then looked at again): a read that finds a count,
    // a looper's every wake-up, takes no queue lock to register and allocates nothing.
    let mut watching: Option<Watch> = None;
    loop {
        {
            let mut count = efd.count.lock();
            if *count > 0 {
                let value = if efd.semaphore { 1 } else { *count };
                *count -= value;
                buf[..8].copy_from_slice(&value.to_le_bytes());
                drop(count);
                notify_key(key);
                return Ok(8);
            }
        }
        if nonblocking {
            return Err(EAGAIN);
        }
        match watching.take() {
            None => watching = Some(watch(Some([key]))),
            Some(w) => w.wait(None, task)?,
        }
    }
}

fn eventfd_write(efd: &EventFd, bytes: &[u8], nonblocking: bool, task: &Task) -> Result<usize, Errno> {
    let value = u64::from_le_bytes(bytes.get(..8).ok_or(EINVAL)?.try_into().expect("8"));
    if value == u64::MAX {
        return Err(EINVAL);
    }
    let key = std::ptr::from_ref(efd) as Key;
    // As `eventfd_read`: registered only to wait.
    let mut watching: Option<Watch> = None;
    loop {
        {
            let mut count = efd.count.lock();
            if u64::MAX - 1 - *count >= value {
                *count += value;
                drop(count);
                notify_key(key);
                return Ok(8);
            }
        }
        if nonblocking {
            return Err(EAGAIN);
        }
        match watching.take() {
            None => watching = Some(watch(Some([key]))),
            Some(w) => w.wait(None, task)?,
        }
    }
}

// ---------------------------------------------------------------------------------- timerfd

pub struct TimerFd {
    clock: u64,
    state: Mutex<TimerState>,
}

#[derive(Default)]
struct TimerState {
    next: Option<Instant>,
    interval: Duration,
}

impl TimerFd {
    /// Expirations since the last read, taking them; the timer moves on to its next expiry.
    fn take(&self, now: Instant) -> u64 {
        let mut st = self.state.lock();
        let Some(next) = st.next else { return 0 };
        if now < next {
            return 0;
        }
        if st.interval.is_zero() {
            st.next = None;
            return 1;
        }
        let late = now - next;
        let n = 1 + (late.as_nanos() / st.interval.as_nanos()) as u64;
        st.next = next.checked_add(st.interval * u32::try_from(n).unwrap_or(u32::MAX));
        n
    }

    fn ready(&self, now: Instant) -> bool {
        self.state.lock().next.is_some_and(|n| now >= n)
    }

    fn next(&self) -> Option<Instant> {
        self.state.lock().next
    }
}

fn read_timespec(p: &Process, at: u64) -> Result<Duration, Errno> {
    let b: [u8; 16] = p.mem.read_array(at)?;
    let sec = i64::from_le_bytes(b[..8].try_into().expect("8"));
    let nsec = u64::from_le_bytes(b[8..].try_into().expect("8"));
    if sec < 0 || nsec >= 1_000_000_000 {
        return Err(EINVAL);
    }
    Ok(Duration::new(sec as u64, nsec as u32))
}

fn sys_timerfd_create(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    if !matches!(a[0], 0 | 1 | 7 | 8 | 9) {
        return Err(EINVAL); // REALTIME, MONOTONIC, BOOTTIME, REALTIME_ALARM, BOOTTIME_ALARM
    }
    let tfd = Arc::new(TimerFd { clock: a[0], state: Mutex::default() });
    let file = OpenFile { kind: Mutex::new(FileKind::TimerFd(tfd)), flags: Mutex::new(a[1] as u32 & O_NONBLOCK) };
    Ok(p.fds.insert(Arc::new(file), a[1] & O_CLOEXEC != 0, 0)? as u64)
}

fn timer_of(p: &Process, fd: u64) -> Result<Arc<TimerFd>, Errno> {
    match &*p.fds.get(fd as i64 as i32)?.kind.lock() {
        FileKind::TimerFd(t) => Ok(Arc::clone(t)),
        _ => Err(EINVAL),
    }
}

fn write_itimerspec(p: &Process, at: u64, interval: Duration, value: Duration) -> Result<(), Errno> {
    let mut b = [0u8; 32];
    b[0..8].copy_from_slice(&interval.as_secs().to_le_bytes());
    b[8..16].copy_from_slice(&u64::from(interval.subsec_nanos()).to_le_bytes());
    b[16..24].copy_from_slice(&value.as_secs().to_le_bytes());
    b[24..32].copy_from_slice(&u64::from(value.subsec_nanos()).to_le_bytes());
    p.mem.write(at, &b)
}

fn sys_timerfd_settime(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const TFD_TIMER_ABSTIME: u64 = 1;
    let timer = timer_of(p, a[0])?;
    let interval = read_timespec(p, a[2])?;
    let value = read_timespec(p, a[2] + 16)?;
    let now = Instant::now();
    let mut st = timer.state.lock();
    if a[3] != 0 {
        let left = st.next.map_or(Duration::ZERO, |n| n.saturating_duration_since(now));
        write_itimerspec(p, a[3], st.interval, left)?;
    }
    let key = Arc::as_ptr(&timer) as Key;
    st.interval = interval;
    st.next = if value.is_zero() {
        None
    } else if a[1] & TFD_TIMER_ABSTIME != 0 {
        let clock_now = crate::sys::clock_now(p, timer.clock)?;
        now.checked_add(value.saturating_sub(clock_now))
    } else {
        now.checked_add(value)
    };
    drop(st);
    notify_key(key);
    Ok(0)
}

fn sys_timerfd_gettime(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let timer = timer_of(p, a[0])?;
    let st = timer.state.lock();
    let left = st.next.map_or(Duration::ZERO, |n| n.saturating_duration_since(Instant::now()));
    write_itimerspec(p, a[1], st.interval, left)?;
    Ok(0)
}

fn timerfd_read(timer: &TimerFd, buf: &mut [u8], nonblocking: bool, task: &Task) -> Result<usize, Errno> {
    if buf.len() < 8 {
        return Err(EINVAL);
    }
    loop {
        let watch = watch(Some([std::ptr::from_ref(timer) as Key]));
        let n = timer.take(Instant::now());
        if n > 0 {
            // Its expiries taken: the next one is a new edge (`EPOLLET`).
            edge(std::ptr::from_ref(timer) as Key);
            buf[..8].copy_from_slice(&n.to_le_bytes());
            return Ok(8);
        }
        if nonblocking {
            return Err(EAGAIN);
        }
        watch.wait(timer.next(), task)?;
    }
}

// ---------------------------------------------------------------------------------- readiness

/// What a descriptor is ready for, as `EPOLL*`/`POLL*` bits (the same values for these).
fn readiness(file: &OpenFile, now: Instant) -> (u32, Option<Instant>) {
    match &*file.kind.lock() {
        FileKind::EventFd(e) => {
            let c = *e.count.lock();
            ((if c > 0 { IN } else { 0 }) | (if c < u64::MAX - 1 { OUT } else { 0 }), None)
        }
        // The expiry ahead is when to look again; one passed (the timer ready, not read yet) is
        // not -- until a read or a new setting nothing about it changes with time, and a waiter
        // that does not report it (`EPOLLET` already reported, a mask without `EPOLLIN`) slept
        // until that past instant, at once, and looked again: a busy loop in system_server's
        // ConnectivityService, ~6% of a core in one `epoll_pwait` a second (M4, 2026-10-16).
        FileKind::TimerFd(t) => (if t.ready(now) { IN } else { 0 }, t.next().filter(|n| *n > now)),
        FileKind::Pipe(end) => (crate::pipe::readiness(end), None),
        FileKind::Epoll(ep) => (if ep.any_ready(now) { IN } else { 0 }, None),
        FileKind::Socket(s) => (crate::socket::readiness(s), None),
        // Every fence here is signalled when made.
        FileKind::SyncFile(_) => (IN, None),
        FileKind::Binder(b) => (b.readiness(), None),
        FileKind::Inotify(_) => (0, None),
        FileKind::Evdev(c) => (c.readiness(), None),
        FileKind::Bpf(_) => (0, None),
        // A remote binder is waited on by its ioctl, not polled: always writable.
        FileKind::RemoteBinder(_) => (crate::poll::OUT, None),
        _ => (IN | OUT, None),
    }
}

// ---------------------------------------------------------------------------------- epoll

pub struct Epoll {
    /// fd -> (the file when added, the events asked for, the caller's data, its edge state).
    interest: Mutex<BTreeMap<i32, (std::sync::Weak<OpenFile>, u32, u64, Edge)>>,
}

/// An `EPOLLET` entry's state: whether a change since its last report arms it, the changes counted
/// for it when it last looked ([`edges_of`]), and the readiness it saw then.
#[derive(Clone, Copy)]
struct Edge {
    armed: bool,
    seen: u64,
    last: u32,
}

impl Edge {
    /// Added or changed (`EPOLL_CTL_ADD`/`MOD`): reported once if ready, as Linux does.
    fn new() -> Self {
        Self { armed: true, seen: 0, last: 0 }
    }

    /// Whether to report `ready` now; looked at every scan. Armed by a change told for the file
    /// or a readiness bit that was not there at the last look; disarmed by a report.
    fn report(&mut self, edges: u64, ready: u32, wanted: u32) -> bool {
        if edges != self.seen {
            self.seen = edges;
            self.armed = true;
        }
        if ready & !self.last != 0 {
            self.armed = true;
        }
        self.last = ready;
        self.armed && ready & wanted != 0
    }
}

impl Epoll {
    fn any_ready(&self, now: Instant) -> bool {
        self.interest.lock().values().any(|(file, events, _, _)| {
            file.upgrade().is_some_and(|f| readiness(&f, now).0 & (*events | ERR | HUP) != 0)
        })
    }
}

fn sys_epoll_create1(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[0] & !O_CLOEXEC != 0 {
        return Err(EINVAL);
    }
    let ep = Arc::new(Epoll { interest: Mutex::default() });
    let file = OpenFile { kind: Mutex::new(FileKind::Epoll(ep)), flags: Mutex::new(0) };
    Ok(p.fds.insert(Arc::new(file), a[0] & O_CLOEXEC != 0, 0)? as u64)
}

fn epoll_of(p: &Process, fd: u64) -> Result<Arc<Epoll>, Errno> {
    match &*p.fds.get(fd as i64 as i32)?.kind.lock() {
        FileKind::Epoll(e) => Ok(Arc::clone(e)),
        _ => Err(EINVAL),
    }
}

fn sys_epoll_ctl(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const ADD: u64 = 1;
    const DEL: u64 = 2;
    const MOD: u64 = 3;
    let ep = epoll_of(p, a[0])?;
    let fd = a[2] as i64 as i32;
    let file = p.fds.get(fd)?;
    if a[2] == a[0] {
        return Err(EINVAL);
    }
    let mut interest = ep.interest.lock();
    match a[1] {
        DEL => interest.remove(&fd).map(|_| 0).ok_or(ENOENT),
        ADD | MOD => {
            let ev = p.mem.read(a[3], 16)?;
            let events = u32::from_le_bytes(ev[0..4].try_into().expect("4"));
            let data = u64::from_le_bytes(ev[8..16].try_into().expect("8"));
            let present = interest.get(&fd).is_some_and(|(f, _, _, _)| f.upgrade().is_some_and(|f| Arc::ptr_eq(&f, &file)));
            match (a[1], present) {
                (ADD, true) => return Err(EEXIST),
                (MOD, false) => return Err(ENOENT),
                _ => {}
            }
            interest.insert(fd, (Arc::downgrade(&file), events, data, Edge::new()));
            drop(interest);
            // The set's own waiters look again (`poll_keyed`), or everyone does.
            if keyed() {
                notify_key(Arc::as_ptr(&ep) as Key);
            } else {
                notify();
            }
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

/// Set a temporary signal mask for a wait (`epoll_pwait`, `ppoll`), answering the one to restore.
fn temporary_mask(p: &Process, t: &mut Task, at: u64, size: u64) -> Result<Option<u64>, Errno> {
    if at == 0 {
        return Ok(None);
    }
    if size != 8 {
        return Err(EINVAL);
    }
    let old = t.sigmask;
    t.sigmask = p.mem.read_u64(at)? & !((1 << 8) | (1 << 18));
    Ok(Some(old))
}

fn sys_epoll_pwait(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let ep = epoll_of(p, a[0])?;
    let max = a[2] as i64 as i32;
    if max <= 0 || max > 1 << 16 {
        return Err(EINVAL);
    }
    let timeout = a[3] as i64 as i32;
    let deadline = (timeout >= 0).then(|| Instant::now() + Duration::from_millis(timeout as u64));
    let restore = temporary_mask(p, t, a[4], a[5])?;
    // Registered only once a look has found nothing (then looked again before sleeping, so a
    // change between still wakes it): a wait that finds a descriptor ready -- most of a busy
    // looper's -- takes no queue lock and allocates nothing.
    let mut watching: Option<Watch> = None;
    let result = loop {
        let now = Instant::now();
        // The events found: the first `SMALL` on the stack.
        const SMALL: usize = 32;
        let mut small = [0u8; SMALL * 16];
        let mut big: Vec<u8> = Vec::new();
        let mut count = 0usize;
        let mut earliest: Option<Instant> = None;
        {
            let mut interest = ep.interest.lock();
            interest.retain(|_, (f, _, _, _)| f.strong_count() > 0);
            for (fd, (file, events, data, edge)) in interest.iter_mut() {
                // A descriptor closed since it was added (its number reused) is not reported.
                let Some(file) = file.upgrade() else { continue };
                if !p.fds.get(*fd).is_ok_and(|f| Arc::ptr_eq(&f, &file)) {
                    continue;
                }
                let (ready, next) = readiness(&file, now);
                if let Some(n) = next {
                    earliest = Some(earliest.map_or(n, |e: Instant| e.min(n)));
                }
                let got = ready & (*events | ERR | HUP);
                // `EPOLLET`: once per change, not for as long as it is ready. Treated as level,
                // a socket that stays writable (UDP: always) or an eventfd nobody reads (mio's
                // waker: written to wake, never read) answers every wait at once -- tokio's
                // driver then never sleeps (DnsResolver's `doh-handler` at 99% of a core).
                if *events & EDGE != 0 && EDGE_TRIGGERED.load(Ordering::Relaxed) && !edge.report(edges_of(key_of(&file)), ready, *events | ERR | HUP) {
                    continue;
                }
                if got != 0 && count < max as usize {
                    if *events & EDGE != 0 {
                        edge.armed = false;
                    }
                    let mut event = [0u8; 16];
                    event[..4].copy_from_slice(&got.to_le_bytes());
                    event[8..].copy_from_slice(&data.to_le_bytes());
                    if count < SMALL {
                        small[count * 16..count * 16 + 16].copy_from_slice(&event);
                    } else {
                        if count == SMALL {
                            big.extend_from_slice(&small);
                        }
                        big.extend_from_slice(&event);
                    }
                    count += 1;
                    if *events & ONESHOT != 0 {
                        *events = 0;
                    }
                }
            }
        }
        if count > 0 {
            let bytes = if count <= SMALL { &small[..count * 16] } else { &big[..] };
            break p.mem.write(a[1], bytes).map(|()| count as u64);
        }
        if deadline.is_some_and(|d| now >= d) {
            break Ok(0);
        }
        let Some(watch_now) = &watching else {
            // What the set waits on: its descriptors' keys. Then look again.
            watching = Some({
                let interest = ep.interest.lock();
                let files: Vec<Arc<OpenFile>> = interest.values().filter_map(|(f, _, _, _)| f.upgrade()).collect();
                let mut keys = keys_of(files.iter().map(|f| &**f));
                if let (Some(keys), true) = (keys.as_mut(), keyed()) {
                    // `poll_keyed`: an edit of the set (`epoll_ctl`) is told by the set's key.
                    keys.push(Arc::as_ptr(&ep) as Key);
                }
                watch(keys)
            });
            continue;
        };
        let wake = match (deadline, earliest) {
            (Some(d), Some(e)) => Some(d.min(e)),
            (d, e) => d.or(e),
        };
        if let Err(e) = watch_now.wait(wake, t) {
            break Err(e);
        }
        // Woken: look first again, registering anew only if that finds nothing.
        watching = None;
    };
    if let Some(old) = restore {
        // A signal the temporary mask let through is delivered under it, as the kernel does
        // (`saved_sigmask`); otherwise the old mask is back now.
        if result == Err(EINTR) {
            t.saved_sigmask = Some(old);
        } else {
            t.sigmask = old;
        }
    }
    result
}

fn sys_ppoll(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let n = a[1] as usize;
    if n > 1 << 16 {
        return Err(EINVAL);
    }
    let deadline = if a[2] == 0 { None } else { Instant::now().checked_add(read_timespec(p, a[2])?) };
    let timed = a[2] != 0;
    let restore = temporary_mask(p, t, a[3], a[4])?;
    // As `epoll_pwait`: registered only once a look has found nothing; the list on the stack when
    // it is short (a looper polls one or two descriptors).
    let mut watching: Option<Watch> = None;
    let mut small = [0u8; 32 * 8];
    let mut big: Vec<u8> = Vec::new();
    let result = loop {
        let raw: &mut [u8] = if n <= 32 {
            &mut small[..n * 8]
        } else {
            big.resize(n * 8, 0);
            &mut big
        };
        p.mem.read_into(a[0], raw)?;
        let now = Instant::now();
        let mut count = 0;
        let mut earliest: Option<Instant> = None;
        for i in 0..n {
            let e = &mut raw[i * 8..i * 8 + 8];
            let fd = i32::from_le_bytes(e[0..4].try_into().expect("4"));
            let events = u32::from(u16::from_le_bytes(e[4..6].try_into().expect("2")));
            let revents = if fd < 0 {
                0
            } else {
                match p.fds.get(fd) {
                    Ok(file) => {
                        let (ready, next) = readiness(&file, now);
                        if let Some(n) = next {
                            earliest = Some(earliest.map_or(n, |x: Instant| x.min(n)));
                        }
                        ready & (events | ERR | HUP)
                    }
                    Err(_) => NVAL,
                }
            };
            e[6..8].copy_from_slice(&(revents as u16).to_le_bytes());
            if revents != 0 {
                count += 1;
            }
        }
        if count > 0 || (timed && deadline.is_none_or(|d| now >= d)) {
            break p.mem.write(a[0], raw).map(|()| count as u64);
        }
        let Some(watch_now) = &watching else {
            let files: Vec<Arc<OpenFile>> = (0..n).filter_map(|i| p.fds.get(i32::from_le_bytes(raw[i * 8..i * 8 + 4].try_into().expect("4"))).ok()).collect();
            watching = Some(watch(keys_of(files.iter().map(|f| &**f))));
            continue;
        };
        let wake = match (deadline, earliest) {
            (Some(d), Some(e)) => Some(d.min(e)),
            (d, e) => d.or(e),
        };
        if let Err(e) = watch_now.wait(wake, t) {
            break Err(e);
        }
        watching = None;
    };
    if let Some(old) = restore {
        if result == Err(EINTR) {
            t.saved_sigmask = Some(old);
        } else {
            t.sigmask = old;
        }
    }
    result
}

/// `pselect6(nfds, readfds, writefds, exceptfds, timeout, {sigmask, size})`: `select` as bionic
/// makes it. Readable is `POLLIN | POLLHUP | POLLERR`, writable `POLLOUT | POLLERR`, as Linux's
/// `select` reads them; nothing here has exceptional (`POLLPRI`) data. The time left is written
/// back, as Linux does.
fn sys_pselect6(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let n = a[0] as i64;
    if !(0..=1 << 16).contains(&n) {
        return Err(EINVAL);
    }
    let n = n as usize;
    let bytes = n.div_ceil(64) * 8;
    let read_set = |at: u64| -> Result<Option<Vec<u8>>, Errno> { if at == 0 { Ok(None) } else { p.mem.read(at, bytes).map(Some) } };
    let sets = [read_set(a[1])?, read_set(a[2])?, read_set(a[3])?];
    let deadline = if a[4] == 0 { None } else { Instant::now().checked_add(read_timespec(p, a[4])?) };
    let timed = a[4] != 0;
    let restore = if a[5] == 0 { None } else { temporary_mask(p, t, p.mem.read_u64(a[5])?, p.mem.read_u64(a[5] + 8)?)? };
    let bit = |set: &Option<Vec<u8>>, fd: usize| set.as_ref().is_some_and(|s| s[fd / 8] & (1 << (fd % 8)) != 0);
    let result = loop {
        let seen = generation();
        let now = Instant::now();
        let mut out = [vec![0u8; bytes], vec![0u8; bytes], vec![0u8; bytes]];
        let mut count = 0u64;
        let mut earliest: Option<Instant> = None;
        let mut bad = false;
        for fd in 0..n {
            let (r, w) = (bit(&sets[0], fd), bit(&sets[1], fd));
            if !r && !w && !bit(&sets[2], fd) {
                continue;
            }
            let Ok(file) = p.fds.get(fd as i32) else {
                bad = true;
                break;
            };
            let (ready, next) = readiness(&file, now);
            if let Some(x) = next {
                earliest = Some(earliest.map_or(x, |e: Instant| e.min(x)));
            }
            if r && ready & (IN | HUP | ERR) != 0 {
                out[0][fd / 8] |= 1 << (fd % 8);
                count += 1;
            }
            if w && ready & (OUT | ERR) != 0 {
                out[1][fd / 8] |= 1 << (fd % 8);
                count += 1;
            }
        }
        if bad {
            break Err(crate::errno::EBADF);
        }
        if count > 0 || (timed && deadline.is_none_or(|d| now >= d)) {
            let written = (|| -> Result<(), Errno> {
                for (i, at) in [a[1], a[2], a[3]].into_iter().enumerate() {
                    if at != 0 {
                        p.mem.write(at, &out[i])?;
                    }
                }
                if let Some(d) = deadline {
                    let left = d.saturating_duration_since(Instant::now());
                    p.mem.write_u64(a[4], left.as_secs())?;
                    p.mem.write_u64(a[4] + 8, u64::from(left.subsec_nanos()))?;
                }
                Ok(())
            })();
            break written.map(|()| count);
        }
        let wake = match (deadline, earliest) {
            (Some(d), Some(e)) => Some(d.min(e)),
            (d, e) => d.or(e),
        };
        if let Err(e) = wait_for_change(seen, wake, t) {
            break Err(e);
        }
    };
    if let Some(old) = restore {
        if result == Err(EINTR) {
            t.saved_sigmask = Some(old);
        } else {
            t.sigmask = old;
        }
    }
    result
}

// ---------------------------------------------------------------------------------- I/O

/// `read` on a descriptor this module owns; `None` for any other.
pub fn read(file: &OpenFile, buf: &mut [u8], task: &Task) -> Option<Result<usize, Errno>> {
    let nonblocking = *file.flags.lock() & O_NONBLOCK != 0;
    let kind = file.kind.lock();
    match &*kind {
        FileKind::EventFd(e) => {
            let e = Arc::clone(e);
            drop(kind);
            Some(eventfd_read(&e, buf, nonblocking, task))
        }
        FileKind::TimerFd(tm) => {
            let tm = Arc::clone(tm);
            drop(kind);
            Some(timerfd_read(&tm, buf, nonblocking, task))
        }
        FileKind::Epoll(_) => Some(Err(EINVAL)),
        _ => None,
    }
}

/// `write` on a descriptor this module owns; `None` for any other.
pub fn write(file: &OpenFile, bytes: &[u8], task: &Task) -> Option<Result<usize, Errno>> {
    let nonblocking = *file.flags.lock() & O_NONBLOCK != 0;
    let kind = file.kind.lock();
    match &*kind {
        FileKind::EventFd(e) => {
            let e = Arc::clone(e);
            drop(kind);
            Some(eventfd_write(&e, bytes, nonblocking, task))
        }
        FileKind::TimerFd(_) | FileKind::Epoll(_) => Some(Err(EINVAL)),
        _ => None,
    }
}

pub fn install(table: &mut Table) {
    table.set(nr::EVENTFD2, sys_eventfd2);
    table.set(nr::EPOLL_CREATE1, sys_epoll_create1);
    table.set(nr::EPOLL_CTL, sys_epoll_ctl);
    table.set(nr::EPOLL_PWAIT, sys_epoll_pwait);
    table.set(nr::PPOLL, sys_ppoll);
    table.set(nr::PSELECT6, sys_pselect6);
    table.set(nr::TIMERFD_CREATE, sys_timerfd_create);
    table.set(nr::TIMERFD_SETTIME, sys_timerfd_settime);
    table.set(nr::TIMERFD_GETTIME, sys_timerfd_gettime);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A wait on no descriptor at all** -- `ppoll(NULL, 0, timeout, mask)`, or a list of only
    /// negative fds: its watch has no keys. A posted signal ([`wake_everyone`]) and a process's end
    /// must still reach it at once; it was in no queue, so only the next slice (1 s) did.
    #[test]
    fn a_wait_on_no_descriptors_is_woken_by_a_posted_signal() {
        for wake in [wake_everyone as fn(), notify] {
            let w = watch(Some(Vec::new()));
            let started = Instant::now();
            let waker = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                wake();
            });
            w.sleep(Instant::now() + Duration::from_secs(3));
            waker.join().expect("waker");
            assert!(started.elapsed() < Duration::from_millis(1000), "woken after {:?}, not by the wake", started.elapsed());
        }
    }

    fn levers(dedupe: bool, park: bool) {
        DEDUPE.store(dedupe, Ordering::Relaxed);
        PARK.store(park, Ordering::Relaxed);
    }

    /// **A waiter on many keys is signalled once a change** (`wake_dedupe`): marked for each key,
    /// kept once, and a second change that finds it woken keeps nothing to signal.
    #[test]
    fn a_waiter_on_many_keys_is_signalled_once() {
        let w = Arc::new(Waiter::new());
        let mut wakes = Wakes::default();
        let kept = (0..6).filter(|_| wakes.mark(&w)).count();
        assert_eq!(kept, 1);
        assert_eq!(wakes.signal(), 1);
        let mut again = Wakes::default();
        assert!(!again.mark(&w), "woken already");
        assert_eq!(again.signal(), 0);
    }

    /// **A waiter on many keys is signalled once** by each real way of waking it with
    /// `wake_dedupe` -- a change of all its keys, a herd, an unkeyed change -- and once a key
    /// without it. A herd from another test meanwhile finds it woken already and signals nothing
    /// more (the lever is held).
    #[test]
    fn a_change_of_all_its_keys_signals_a_waiter_once() {
        let _one = LEVERS_IN_TEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let words = [0u64; 6];
        let keys: Vec<Key> = words.iter().map(|w| std::ptr::from_ref(w) as Key).collect();
        // Counted from before it is in a queue, so no herd falls outside the count.
        let fresh = || {
            ME.with(|me| me.signals.store(0, Ordering::SeqCst));
            watch(Some(keys.clone()))
        };
        for park in [false, true] {
            levers(true, park);
            for wake in [(|keys: &[Key]| notify_keys(keys)) as fn(&[Key]), |_| wake_everyone(), |_| notify()] {
                let w = fresh();
                wake(&keys);
                assert_eq!(w.me.signals.load(Ordering::SeqCst), 1, "waiter_park={park}");
            }
        }
        levers(false, true);
        let w = fresh();
        notify_keys(&keys);
        assert!(w.me.signals.load(Ordering::SeqCst) >= keys.len() as u32, "wake_dedupe=0: once a key");
        drop(w);
        levers(true, true);
    }

    /// **No wake is lost**, whichever way waiters are woken and sleep: threads register, look and
    /// sleep (5 s at most), while a change is made and then told, one at a time, each to be seen
    /// at once -- told by key, by several keys at once, by a herd; and to waiters on anything, which
    /// every keyed change reaches (past `ANYTHING_COUNT` with `wake_dedupe`).
    #[test]
    fn no_wake_is_lost() {
        const WAITERS: usize = 6;
        const ROUNDS: u64 = 1000;
        let _one = LEVERS_IN_TEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (dedupe, park) in [(false, false), (true, false), (false, true), (true, true)] {
            levers(dedupe, park);
            // Each waiter's word, changed by the test, and what it last saw of it.
            let words: Arc<Vec<(std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64)>> = Arc::new((0..WAITERS).map(|_| Default::default()).collect());
            let keys: Vec<Key> = words.iter().map(|w| std::ptr::from_ref(&w.0) as Key).collect();
            let threads: Vec<_> = (0..WAITERS)
                .map(|i| {
                    let (words, key) = (Arc::clone(&words), keys[i]);
                    std::thread::spawn(move || {
                        let (word, seen) = &words[i];
                        let mut last = 0;
                        while last < ROUNDS {
                            let w = if i % 3 == 2 { watch_anything() } else { watch(Some([key, INERT])) };
                            let now = word.load(Ordering::SeqCst);
                            if now != last {
                                last = now;
                                seen.store(now, Ordering::SeqCst);
                                continue;
                            }
                            w.sleep(Instant::now() + Duration::from_secs(5));
                        }
                    })
                })
                .collect();
            for round in 1..=ROUNDS {
                for (word, _) in words.iter() {
                    word.store(round, Ordering::SeqCst);
                }
                match round % 3 {
                    0 => wake_everyone(),
                    1 => keys.iter().for_each(|k| notify_key(*k)),
                    _ => notify_keys(&keys),
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                for (i, (_, seen)) in words.iter().enumerate() {
                    while seen.load(Ordering::SeqCst) != round {
                        assert!(Instant::now() < deadline, "round {round}: waiter {i} slept through its change (wake_dedupe={dedupe}, waiter_park={park})");
                        std::thread::yield_now();
                    }
                }
            }
            for t in threads {
                t.join().expect("waiter");
            }
        }
        levers(true, true);
    }

    /// **A wait sleeps the way it began**: `waiter_park` switched while it sleeps, its waker still
    /// wakes it on the token or the condition variable it waits on.
    #[test]
    fn a_wait_switched_midway_is_still_woken() {
        let _one = LEVERS_IN_TEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (dedupe, park) in [(false, false), (true, false), (false, true), (true, true)] {
            levers(dedupe, park);
            let word = 0u64;
            let key = std::ptr::from_ref(&word) as Key;
            let (tx, rx) = std::sync::mpsc::channel();
            let sleeper = std::thread::spawn(move || {
                let w = watch(Some([key]));
                tx.send(()).expect("told");
                let started = Instant::now();
                w.sleep(started + Duration::from_secs(5));
                started.elapsed()
            });
            rx.recv().expect("registered");
            std::thread::sleep(Duration::from_millis(30));
            PARK.store(!park, Ordering::Relaxed);
            notify_key(key);
            let slept = sleeper.join().expect("sleeper");
            assert!(slept < Duration::from_secs(1), "woken after {slept:?} (wake_dedupe={dedupe}, waiter_park={park} then {})", !park);
        }
        levers(true, true);
    }

    /// `notify_key_one` wakes one waiter, the newest, in both ways of waking. (A herd from another
    /// test meanwhile wakes both: that try is taken again.)
    #[test]
    fn one_waiter_is_woken_for_work_one_takes() {
        let _one = LEVERS_IN_TEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let herds = || STATS.everyone.load(Ordering::SeqCst) + STATS.unkeyed.load(Ordering::SeqCst);
        for dedupe in [false, true] {
            levers(dedupe, true);
            for _try in 0..5 {
                let before = herds();
                let word = 0u64;
                let key = std::ptr::from_ref(&word) as Key;
                let (tx, rx) = std::sync::mpsc::channel();
                let sleepers: Vec<_> = (0..2)
                    .map(|_| {
                        let tx = tx.clone();
                        let sleeper = std::thread::spawn(move || {
                            let w = watch(Some([key]));
                            tx.send(()).expect("told");
                            w.sleep(Instant::now() + Duration::from_millis(300));
                            w.me.woken.load(Ordering::SeqCst)
                        });
                        rx.recv().expect("registered");
                        sleeper
                    })
                    .collect();
                notify_key_one(key);
                let woken: Vec<bool> = sleepers.into_iter().map(|s| s.join().expect("sleeper")).collect();
                if herds() != before {
                    continue;
                }
                assert_eq!(woken, [false, true], "the newest alone (wake_dedupe={dedupe})");
                break;
            }
        }
        levers(true, true);
    }
}
