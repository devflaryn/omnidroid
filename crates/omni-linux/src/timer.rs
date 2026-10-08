//! **POSIX timers**: `timer_create`, `timer_settime`, `timer_gettime`, `timer_getoverrun`,
//! `timer_delete`, as the kernel keeps them -- per process, on a clock, firing a signal.
//!
//! A timer notifies as its `sigevent` asks: `SIGEV_SIGNAL` (its signal to the process: here the
//! main thread takes it), `SIGEV_THREAD_ID` (to one thread) or `SIGEV_NONE` (nothing; it only
//! runs). `SIGEV_THREAD` is not the kernel's: bionic makes it of `SIGEV_THREAD_ID` to a thread of
//! its own blocked in `rt_sigtimedwait` for signal 32, which calls the callback when the signal's
//! `si_code` is `SI_TIMER` -- so the signal carries a timer's `siginfo` (`si_timerid`,
//! `si_overrun`, `si_value`), delivered with it or handed to `rt_sigtimedwait`
//! ([`crate::process::Process::post_signal_info`]).
//!
//! As Linux: a timer whose signal is still queued when it expires again does not queue another --
//! the expiry is counted as an overrun, reported in `si_overrun` when the signal is taken and by
//! `timer_getoverrun` after; a periodic timer that fell behind counts every period it missed.
//!
//! Why here: `mediaextractor`'s watchdog (libmediautils' `Watchdog`, `SIGEV_THREAD_ID` + `SIGABRT`)
//! took `ENOSYS` as fatal, and MediaProvider's boot scan restarted it ~25 times a session
//! (HANDOFF "LIGHTER AND FASTER").
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::errno::{Errno, SysResult, EINVAL};
use crate::process::{Process, Task};
use crate::signal::{SigInfo, SI_TIMER};
use crate::syscall::{nr, Table};

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
const SIGEV_THREAD_ID: i32 = 4;
const SIGALRM: i32 = 14;
const TIMER_ABSTIME: u64 = 1;

/// One timer.
#[derive(Debug)]
struct Timer {
    clock: u64,
    notify: i32,
    signo: i32,
    value: u64,
    tid: i32,
    interval: Duration,
    next: Option<Instant>,
    /// Its signal is queued and not yet taken.
    queued: bool,
    /// Expiries while its signal waited.
    overrun: i32,
    /// The overrun the signal last taken carried (`timer_getoverrun`).
    last_overrun: i32,
    /// Bumped at each arming: an expiry scheduled for an earlier arming is not this one's.
    armed: u64,
}

/// A process's timers.
#[derive(Default)]
pub struct Timers {
    inner: Mutex<(HashMap<i32, Timer>, i32)>,
}

impl Timers {
    /// A timer's signal was taken (delivered, or returned by `rt_sigtimedwait`): its `siginfo`
    /// with the overrun counted meanwhile, and the timer free to queue the next.
    pub(crate) fn dequeued(&self, mut info: SigInfo) -> SigInfo {
        let mut inner = self.inner.lock();
        if let Some(t) = inner.0.get_mut(&info.timer) {
            info.overrun = t.overrun;
            t.last_overrun = t.overrun;
            t.overrun = 0;
            t.queued = false;
        }
        info
    }
}

/// An expiry to come.
struct Due {
    at: Instant,
    process: Weak<Process>,
    id: i32,
    armed: u64,
}

/// Every expiry to come in this host process, and the thread that fires them.
static SCHEDULE: LazyLock<(Mutex<Vec<Due>>, Condvar)> = LazyLock::new(|| {
    let _ = std::thread::Builder::new().name("omni-posix-timers".into()).spawn(fire_loop);
    (Mutex::new(Vec::new()), Condvar::new())
});

fn schedule(due: Due) {
    let (list, cv) = &*SCHEDULE;
    list.lock().push(due);
    cv.notify_one();
}

fn fire_loop() {
    let (list, cv) = &*SCHEDULE;
    let mut dues = list.lock();
    loop {
        let now = Instant::now();
        let ready: Vec<Due> = {
            let mut ready = Vec::new();
            let mut i = 0;
            while i < dues.len() {
                if dues[i].at <= now {
                    ready.push(dues.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            ready
        };
        if !ready.is_empty() {
            drop(dues);
            for due in ready {
                if let Some(p) = due.process.upgrade() {
                    fire(&p, due.id, due.armed);
                }
            }
            dues = list.lock();
            continue;
        }
        match dues.iter().map(|d| d.at).min() {
            Some(at) => {
                cv.wait_until(&mut dues, at);
            }
            None => cv.wait(&mut dues),
        }
    }
}

/// Timer `id` of `p` expires (if it is still the arming that was scheduled).
fn fire(p: &Arc<Process>, id: i32, armed: u64) {
    let now = Instant::now();
    let mut inner = p.timers.inner.lock();
    let Some(t) = inner.0.get_mut(&id) else { return };
    if t.armed != armed {
        return;
    }
    let Some(next) = t.next else { return };
    if next > now {
        schedule(Due { at: next, process: Arc::downgrade(p), id, armed });
        return;
    }
    // Every period that has passed is an expiry.
    let mut expiries = 1i32;
    if t.interval.is_zero() {
        t.next = None;
    } else {
        let mut n = next + t.interval;
        while n <= now {
            n += t.interval;
            expiries = expiries.saturating_add(1);
        }
        t.next = Some(n);
        schedule(Due { at: n, process: Arc::downgrade(p), id, armed });
    }
    if t.notify == SIGEV_NONE {
        return;
    }
    if t.queued {
        t.overrun = t.overrun.saturating_add(expiries);
        return;
    }
    t.queued = true;
    t.overrun = expiries - 1;
    let info = SigInfo { signo: t.signo, code: SI_TIMER, timer: id, value: t.value, ..SigInfo::default() };
    let tid = if t.notify == SIGEV_THREAD_ID { t.tid } else { p.sys.pid };
    drop(inner);
    p.post_signal_info(tid, info);
}

fn read_timespec(p: &Process, at: u64) -> Result<Duration, Errno> {
    let b: [u8; 16] = p.mem.read_array(at)?;
    let (s, ns) = (i64::from_le_bytes(b[0..8].try_into().expect("8")), i64::from_le_bytes(b[8..16].try_into().expect("8")));
    if s < 0 || !(0..1_000_000_000).contains(&ns) {
        return Err(EINVAL);
    }
    Ok(Duration::new(s as u64, ns as u32))
}

fn write_itimerspec(p: &Process, at: u64, interval: Duration, value: Duration) -> Result<(), Errno> {
    let mut b = Vec::with_capacity(32);
    for d in [interval, value] {
        b.extend_from_slice(&(d.as_secs() as i64).to_le_bytes());
        b.extend_from_slice(&i64::from(d.subsec_nanos()).to_le_bytes());
    }
    p.mem.write(at, &b)
}

/// `timer_create(clockid, sevp, timerid)`.
fn sys_timer_create(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let clock = a[0];
    // The clocks a timer runs on: REALTIME, MONOTONIC, the CPU-time clocks, BOOTTIME and the alarms.
    if !matches!(clock, 0..=3 | 7..=9) {
        return Err(EINVAL);
    }
    let (value, signo, notify, tid) = if a[1] == 0 {
        (None, SIGALRM, SIGEV_SIGNAL, 0)
    } else {
        let b: [u8; 20] = p.mem.read_array(a[1])?;
        let word = |at: usize| i32::from_le_bytes(b[at..at + 4].try_into().expect("4"));
        (Some(u64::from_le_bytes(b[0..8].try_into().expect("8"))), word(8), word(12), word(16))
    };
    match notify {
        SIGEV_NONE => {}
        SIGEV_SIGNAL | SIGEV_THREAD_ID if (1..=64).contains(&signo) => {}
        // SIGEV_THREAD is the C library's (bionic's is SIGEV_THREAD_ID underneath), not the kernel's.
        SIGEV_THREAD | _ => return Err(EINVAL),
    }
    if notify == SIGEV_THREAD_ID && !p.tids().contains(&tid) {
        return Err(EINVAL);
    }
    let mut inner = p.timers.inner.lock();
    let id = inner.1;
    inner.1 += 1;
    let timer = Timer { clock, notify, signo, value: value.unwrap_or(id as u64), tid, interval: Duration::ZERO, next: None, queued: false, overrun: 0, last_overrun: 0, armed: 0 };
    inner.0.insert(id, timer);
    drop(inner);
    if let Err(e) = p.mem.write(a[2], &id.to_le_bytes()) {
        p.timers.inner.lock().0.remove(&id);
        return Err(e);
    }
    Ok(0)
}

/// `timer_settime(timerid, flags, new, old)`.
fn sys_timer_settime(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let id = a[0] as i64 as i32;
    let (interval, value) = (read_timespec(p, a[2])?, read_timespec(p, a[2] + 16)?);
    let now = Instant::now();
    let clock = p.timers.inner.lock().0.get(&id).map(|t| t.clock).ok_or(EINVAL)?;
    let next = if value.is_zero() {
        None
    } else if a[1] & TIMER_ABSTIME != 0 {
        // An absolute time on the timer's clock: as far from now as it is from the clock's now.
        let clock_now = crate::sys::clock_now(p, clock)?;
        Some(now + value.saturating_sub(clock_now))
    } else {
        Some(now + value)
    };
    let mut inner = p.timers.inner.lock();
    let t = inner.0.get_mut(&id).ok_or(EINVAL)?;
    let old = (t.interval, t.next.map_or(Duration::ZERO, |n| n.saturating_duration_since(now)));
    t.interval = interval;
    t.next = next;
    t.armed += 1;
    let armed = t.armed;
    drop(inner);
    if a[3] != 0 {
        write_itimerspec(p, a[3], old.0, old.1)?;
    }
    if let (Some(at), Some(me)) = (next, p.me.get()) {
        schedule(Due { at, process: me.clone(), id, armed });
    }
    Ok(0)
}

/// `timer_gettime(timerid, curr)`: the interval and the time to the next expiry.
fn sys_timer_gettime(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let id = a[0] as i64 as i32;
    let (interval, left) = {
        let inner = p.timers.inner.lock();
        let t = inner.0.get(&id).ok_or(EINVAL)?;
        (t.interval, t.next.map_or(Duration::ZERO, |n| n.saturating_duration_since(Instant::now())))
    };
    write_itimerspec(p, a[1], interval, left)?;
    Ok(0)
}

/// `timer_getoverrun(timerid)`: the overrun of the signal last taken.
fn sys_timer_getoverrun(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let inner = p.timers.inner.lock();
    let t = inner.0.get(&(a[0] as i64 as i32)).ok_or(EINVAL)?;
    Ok(t.last_overrun as u64)
}

/// `timer_delete(timerid)`: disarmed and gone (a signal already queued stays queued).
fn sys_timer_delete(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    p.timers.inner.lock().0.remove(&(a[0] as i64 as i32)).map(|_| 0).ok_or(EINVAL)
}

pub fn install(table: &mut Table) {
    table.set(nr::TIMER_CREATE, sys_timer_create);
    table.set(nr::TIMER_SETTIME, sys_timer_settime);
    table.set(nr::TIMER_GETTIME, sys_timer_gettime);
    table.set(nr::TIMER_GETOVERRUN, sys_timer_getoverrun);
    table.set(nr::TIMER_DELETE, sys_timer_delete);
}
