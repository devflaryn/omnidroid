//! Readiness: `eventfd`, `timerfd`, `epoll` and `ppoll` -- what every `Looper` waits in.
//!
//! One host-wide change counter stands for the kernel's wait queues: whatever can make a
//! descriptor ready (a pipe written or closed, an eventfd written, a binder transaction queued)
//! calls [`notify`], and a waiter re-checks everything it watches. A timer is ready by the clock
//! alone, so a waiter also wakes at the earliest expiry among what it watches.
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
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

struct Changed {
    generation: Mutex<u64>,
    cv: Condvar,
}

static CHANGED: Changed = Changed { generation: Mutex::new(0), cv: Condvar::new() };

/// Something a descriptor's readiness depends on changed: wake every waiter to look again.
pub fn notify() {
    *CHANGED.generation.lock() += 1;
    CHANGED.cv.notify_all();
}

/// Wait for a change, until `deadline` at the latest; `EINTR` if the task has a deliverable
/// signal. A waiter re-checks what it waits for afterwards: this can wake early.
fn wait_for_change(seen: u64, deadline: Option<Instant>, task: &Task) -> Result<(), Errno> {
    let mut generation = CHANGED.generation.lock();
    if *generation != seen {
        return Ok(());
    }
    if task.pending.load(Ordering::SeqCst) & !task.sigmask != 0 {
        return Err(EINTR);
    }
    // In slices, so a posted signal is seen without a notification of its own.
    let slice = Instant::now() + Duration::from_millis(50);
    let until = deadline.map_or(slice, |d| d.min(slice));
    CHANGED.cv.wait_until(&mut generation, until);
    Ok(())
}

fn generation() -> u64 {
    *CHANGED.generation.lock()
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
    loop {
        let seen = generation();
        {
            let mut count = efd.count.lock();
            if *count > 0 {
                let value = if efd.semaphore { 1 } else { *count };
                *count -= value;
                buf[..8].copy_from_slice(&value.to_le_bytes());
                drop(count);
                notify();
                return Ok(8);
            }
        }
        if nonblocking {
            return Err(EAGAIN);
        }
        wait_for_change(seen, None, task)?;
    }
}

fn eventfd_write(efd: &EventFd, bytes: &[u8], nonblocking: bool, task: &Task) -> Result<usize, Errno> {
    let value = u64::from_le_bytes(bytes.get(..8).ok_or(EINVAL)?.try_into().expect("8"));
    if value == u64::MAX {
        return Err(EINVAL);
    }
    loop {
        let seen = generation();
        {
            let mut count = efd.count.lock();
            if u64::MAX - 1 - *count >= value {
                *count += value;
                drop(count);
                notify();
                return Ok(8);
            }
        }
        if nonblocking {
            return Err(EAGAIN);
        }
        wait_for_change(seen, None, task)?;
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
    notify();
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
        let seen = generation();
        let n = timer.take(Instant::now());
        if n > 0 {
            buf[..8].copy_from_slice(&n.to_le_bytes());
            return Ok(8);
        }
        if nonblocking {
            return Err(EAGAIN);
        }
        let Some(next) = timer.next() else {
            wait_for_change(seen, None, task)?;
            continue;
        };
        wait_for_change(seen, Some(next), task)?;
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
        FileKind::Socket(_) => (OUT, None),
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
        let seen = generation();
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
        if let Err(e) = wait_for_change(seen, wake, t) {
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
        let seen = generation();
        let now = Instant::now();
        let mut raw = p.mem.read(a[0], n * 8)?;
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
    table.set(nr::TIMERFD_CREATE, sys_timerfd_create);
    table.set(nr::TIMERFD_SETTIME, sys_timerfd_settime);
    table.set(nr::TIMERFD_GETTIME, sys_timerfd_gettime);
}
