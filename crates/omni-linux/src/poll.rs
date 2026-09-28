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
const O_NONBLOCK: u32 = 0o4000;
const O_CLOEXEC: u64 = 0o2000000;

/// What a waiter waits on: the address of the thing whose change makes it ready.
pub type Key = usize;

/// A key for what never changes by itself (always ready, or never): nobody notifies it.
pub const INERT: Key = 1;

/// One thread's place in the queues.
#[derive(Default)]
struct Waiter {
    woken: Mutex<bool>,
    cv: Condvar,
}

impl Waiter {
    fn wake(&self) {
        *self.woken.lock() = true;
        self.cv.notify_one();
    }
}

thread_local! {
    static ME: Arc<Waiter> = Arc::new(Waiter::default());
}

/// The queues: the change counter (for the callers that still compare it), the waiters on
/// anything, and the waiters by key.
#[derive(Default)]
struct Queues {
    generation: u64,
    anything: Vec<Arc<Waiter>>,
    keyed: HashMap<Key, Vec<Arc<Waiter>>>,
}

static QUEUES: LazyLock<Mutex<Queues>> = LazyLock::new(Mutex::default);

/// Something changed that no key names (a descriptor closed, an `epoll` set edited): wake every
/// waiter to look again.
pub fn notify() {
    let mut q = QUEUES.lock();
    q.generation += 1;
    for w in q.anything.iter().chain(q.keyed.values().flatten()) {
        w.wake();
    }
    STATS.notifies.fetch_add(1, Ordering::Relaxed);
}

/// What `key` names changed: wake its waiters, and those waiting on anything.
pub fn notify_key(key: Key) {
    let mut q = QUEUES.lock();
    q.generation += 1;
    for w in q.anything.iter().chain(q.keyed.get(&key).into_iter().flatten()) {
        w.wake();
    }
    STATS.notifies.fetch_add(1, Ordering::Relaxed);
}

/// A thread's registration in the queues, for one wait: made before it looks, ended when dropped.
pub(crate) struct Watch {
    me: Arc<Waiter>,
    keys: Option<Vec<Key>>,
}

/// Wait on `keys` (`None`: on anything) -- registered now, before the caller looks.
pub(crate) fn watch(keys: Option<Vec<Key>>) -> Watch {
    let me = ME.with(Arc::clone);
    *me.woken.lock() = false;
    let mut q = QUEUES.lock();
    match &keys {
        None => q.anything.push(Arc::clone(&me)),
        Some(keys) => {
            for k in keys {
                q.keyed.entry(*k).or_default().push(Arc::clone(&me));
            }
        }
    }
    Watch { me, keys }
}

/// The keys of these files, or `None` when one of them has none (then the wait is on anything).
pub(crate) fn keys_of<'a>(files: impl IntoIterator<Item = &'a OpenFile>) -> Option<Vec<Key>> {
    let mut keys = Vec::new();
    for f in files {
        let k = key_of(f)?;
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    Some(keys)
}

impl Watch {
    /// Sleep until a change it watches, `deadline`, or a slice of 50 ms (so a posted signal is
    /// seen); `EINTR` if the task has a deliverable signal. The caller looks again afterwards.
    pub(crate) fn wait(&self, deadline: Option<Instant>, task: &Task) -> Result<(), Errno> {
        if task.pending.load(Ordering::SeqCst) & !task.sigmask != 0 || task.process.futexes.interrupted() {
            return Err(EINTR);
        }
        let slice = Instant::now() + Duration::from_millis(50);
        self.sleep(deadline.map_or(slice, |d| d.min(slice)));
        Ok(())
    }

    /// Sleep until a change it watches or `until`: a host thread's wait.
    pub(crate) fn sleep(&self, until: Instant) {
        let _counted = Waiting::new();
        let mut woken = self.me.woken.lock();
        while !*woken {
            if self.me.cv.wait_until(&mut woken, until).timed_out() {
                break;
            }
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let mut q = QUEUES.lock();
        let me = &self.me;
        let gone = |list: &mut Vec<Arc<Waiter>>| {
            if let Some(i) = list.iter().position(|w| Arc::ptr_eq(w, me)) {
                list.swap_remove(i);
            }
        };
        match &self.keys {
            None => gone(&mut q.anything),
            Some(keys) => {
                for k in keys {
                    if let Some(list) = q.keyed.get_mut(k) {
                        gone(list);
                        if list.is_empty() {
                            q.keyed.remove(k);
                        }
                    }
                }
            }
        }
    }
}

/// What a change to `file` is told by: its key, or `None` for a kind of file whose changes are
/// told to everyone (then a wait on it is a wait on anything).
pub(crate) fn key_of(file: &OpenFile) -> Option<Key> {
    match &*file.kind.lock() {
        FileKind::EventFd(e) => Some(Arc::as_ptr(e) as Key),
        FileKind::TimerFd(t) => Some(Arc::as_ptr(t) as Key),
        FileKind::Pipe(end) => Some(crate::pipe::key(end)),
        FileKind::Socket(s) => crate::socket::key(s),
        FileKind::Binder(b) => Some(b.key()),
        FileKind::Evdev(c) => Some(Arc::as_ptr(c.device()) as Key),
        // Always ready, or never: nothing to be woken for.
        FileKind::SyncFile(_) | FileKind::Inotify(_) | FileKind::Bpf(_) | FileKind::RemoteBinder(_) => Some(INERT),
        _ => None,
    }
}

/// What `OMNI_POLL_STATS=<s>` reports: how often something changes, and how many are woken.
struct Stats {
    notifies: std::sync::atomic::AtomicU64,
    waiting: std::sync::atomic::AtomicI64,
    wakes: std::sync::atomic::AtomicU64,
}

static STATS: Stats = Stats { notifies: std::sync::atomic::AtomicU64::new(0), waiting: std::sync::atomic::AtomicI64::new(0), wakes: std::sync::atomic::AtomicU64::new(0) };

/// `OMNI_POLL_STATS=<seconds>`: a `[poll]` line that often -- changes told a second, threads
/// waiting, and wake-ups a second.
pub fn start_stats() {
    let Some(every) = std::env::var("OMNI_POLL_STATS").ok().and_then(|v| v.parse::<u64>().ok()).filter(|&s| s > 0) else { return };
    let _ = std::thread::Builder::new().name("omni-poll-stats".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_secs(every));
        let n = STATS.notifies.swap(0, Ordering::Relaxed);
        let w = STATS.wakes.swap(0, Ordering::Relaxed);
        eprintln!("[poll] pid {}: {} notifications/s, {} threads waiting, {} wake-ups/s", std::process::id(), n / every, STATS.waiting.load(Ordering::Relaxed), w / every);
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
    let w = watch(None);
    if QUEUES.lock().generation != seen {
        return Ok(());
    }
    w.wait(deadline, task)
}

/// [`wait_for_change`] for a host thread, which has no signals to see: until `deadline` at the
/// latest.
pub(crate) fn wait_for_change_host(seen: u64, deadline: Instant) {
    let w = watch(None);
    if QUEUES.lock().generation == seen {
        w.sleep(deadline);
    }
}

pub(crate) fn generation() -> u64 {
    QUEUES.lock().generation
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
    loop {
        let watch = watch(Some(vec![key]));
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
        watch.wait(None, task)?;
    }
}

fn eventfd_write(efd: &EventFd, bytes: &[u8], nonblocking: bool, task: &Task) -> Result<usize, Errno> {
    let value = u64::from_le_bytes(bytes.get(..8).ok_or(EINVAL)?.try_into().expect("8"));
    if value == u64::MAX {
        return Err(EINVAL);
    }
    let key = std::ptr::from_ref(efd) as Key;
    loop {
        let watch = watch(Some(vec![key]));
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
        watch.wait(None, task)?;
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
    let sec = p.mem.read_u64(at)? as i64;
    let nsec = p.mem.read_u64(at + 8)?;
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
        let watch = watch(Some(vec![std::ptr::from_ref(timer) as Key]));
        let n = timer.take(Instant::now());
        if n > 0 {
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
        FileKind::TimerFd(t) => (if t.ready(now) { IN } else { 0 }, t.next()),
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
    /// fd -> (the file when added, the events asked for, the caller's data).
    interest: Mutex<BTreeMap<i32, (std::sync::Weak<OpenFile>, u32, u64)>>,
}

impl Epoll {
    fn any_ready(&self, now: Instant) -> bool {
        self.interest.lock().values().any(|(file, events, _)| {
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
            let present = interest.get(&fd).is_some_and(|(f, _, _)| f.upgrade().is_some_and(|f| Arc::ptr_eq(&f, &file)));
            match (a[1], present) {
                (ADD, true) => return Err(EEXIST),
                (MOD, false) => return Err(ENOENT),
                _ => {}
            }
            interest.insert(fd, (Arc::downgrade(&file), events, data));
            drop(interest);
            notify();
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
    let result = loop {
        // What the set waits on, before it is looked at: its descriptors' keys.
        let watch = {
            let interest = ep.interest.lock();
            let files: Vec<Arc<OpenFile>> = interest.values().filter_map(|(f, _, _)| f.upgrade()).collect();
            watch(keys_of(files.iter().map(|f| &**f)))
        };
        let now = Instant::now();
        let mut out = Vec::new();
        let mut earliest: Option<Instant> = None;
        {
            let mut interest = ep.interest.lock();
            interest.retain(|_, (f, _, _)| f.strong_count() > 0);
            for (fd, (file, events, data)) in interest.iter_mut() {
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
                if got != 0 && out.len() < max as usize {
                    out.push((got, *data));
                    if *events & ONESHOT != 0 {
                        *events = 0;
                    }
                }
            }
        }
        if !out.is_empty() {
            let mut bytes = Vec::with_capacity(out.len() * 16);
            for (events, data) in &out {
                bytes.extend_from_slice(&events.to_le_bytes());
                bytes.extend_from_slice(&[0; 4]);
                bytes.extend_from_slice(&data.to_le_bytes());
            }
            break p.mem.write(a[1], &bytes).map(|()| out.len() as u64);
        }
        if deadline.is_some_and(|d| now >= d) {
            break Ok(0);
        }
        let wake = match (deadline, earliest) {
            (Some(d), Some(e)) => Some(d.min(e)),
            (d, e) => d.or(e),
        };
        if let Err(e) = watch.wait(wake, t) {
            break Err(e);
        }
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
    let result = loop {
        let mut raw = p.mem.read(a[0], n * 8)?;
        let files: Vec<Arc<OpenFile>> = (0..n).filter_map(|i| p.fds.get(i32::from_le_bytes(raw[i * 8..i * 8 + 4].try_into().expect("4"))).ok()).collect();
        let watch = watch(keys_of(files.iter().map(|f| &**f)));
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
            break p.mem.write(a[0], &raw).map(|()| count as u64);
        }
        let wake = match (deadline, earliest) {
            (Some(d), Some(e)) => Some(d.min(e)),
            (d, e) => d.or(e),
        };
        if let Err(e) = watch.wait(wake, t) {
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
