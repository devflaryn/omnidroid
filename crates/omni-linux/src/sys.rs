//! The small syscalls: ids, clocks, `uname`, randomness, limits, `prctl`, signal state (stored;
//! delivery is A5) and a minimal futex (A4 completes it).
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::errno::*;
use crate::process::{Exit, Process, Task};
use crate::syscall::{nr, Table};

const SIGKILL: u64 = 9;
const SIGSTOP: u64 = 19;

pub struct SysState {
    pub pid: i32,
    pub uid: u32,
    start: Instant,
    actions: Mutex<[[u8; 32]; 65]>,
    /// `PR_SET_TAGGED_ADDR_CTRL`'s value. Tagged pointers are accepted either way (see
    /// `guest::untag`); this is what `PR_GET_TAGGED_ADDR_CTRL` reports back.
    tagged_addr_ctrl: std::sync::atomic::AtomicU64,
    /// The file-creation mask (`umask`), 022 to start with as a shell's is.
    umask: std::sync::atomic::AtomicU32,
}

impl SysState {
    #[must_use]
    pub fn new(pid: i32, uid: u32) -> Self {
        Self { pid, uid, start: Instant::now(), actions: Mutex::new([[0; 32]; 65]), tagged_addr_ctrl: std::sync::atomic::AtomicU64::new(0), umask: std::sync::atomic::AtomicU32::new(0o022) }
    }

    /// Time since the process started.
    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.start.elapsed()
    }

    /// The file-creation mask.
    #[must_use]
    pub fn umask(&self) -> u32 {
        self.umask.load(std::sync::atomic::Ordering::Relaxed)
    }

}

fn timespec(secs: u64, nanos: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&secs.to_le_bytes());
    b[8..].copy_from_slice(&u64::from(nanos).to_le_bytes());
    b
}

fn read_timespec(p: &Process, at: u64) -> Result<Duration, Errno> {
    let sec = p.mem.read_u64(at)? as i64;
    let nsec = p.mem.read_u64(at + 8)?;
    if sec < 0 || nsec >= 1_000_000_000 {
        return Err(EINVAL);
    }
    Ok(Duration::new(sec as u64, nsec as u32))
}

/// `now + d`, or no deadline at all when that is beyond what the host's clock can hold.
fn deadline_after(d: Duration) -> Option<Instant> {
    Instant::now().checked_add(d)
}

fn signals(t: &Task) -> crate::futex::Signals<'_> {
    crate::futex::Signals { pending: &t.pending, mask: t.sigmask }
}

fn now(p: &Process, clock: u64) -> Result<Duration, Errno> {
    match clock {
        0 | 5 | 8 | 11 => SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| EINVAL), // REALTIME(_COARSE/_ALARM), TAI
        1 | 2 | 3 | 4 | 6 | 7 | 9 => Ok(p.sys.start.elapsed() + Duration::from_secs(1000)), // monotonic family, cputime approximated
        _ => Err(EINVAL),
    }
}

fn sys_getpid(p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(p.sys.pid as u64) }
fn sys_getppid(_p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(1) }
fn sys_gettid(_p: &Process, t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(t.tid as u64) }
fn sys_getuid(p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(u64::from(p.sys.uid)) }

fn sys_set_tid_address(_p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    t.clear_child_tid = a[0];
    Ok(t.tid as u64)
}

fn sys_set_robust_list(_p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(0) }

/// `clone`: only the thread shape (`CLONE_VM | CLONE_SIGHAND | CLONE_THREAD`, as bionic's
/// `pthread_create` asks); a fork is refused by name.
fn sys_clone(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    const CLONE_VM: u64 = 0x100;
    const CLONE_SIGHAND: u64 = 0x800;
    const CLONE_THREAD: u64 = 0x1_0000;
    const CLONE_SETTLS: u64 = 0x8_0000;
    const CLONE_PARENT_SETTID: u64 = 0x10_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
    const CLONE_CHILD_SETTID: u64 = 0x100_0000;
    let flags = a[0];
    let thread = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    if flags & thread != thread {
        p.refusals.record(format!("clone: flags {flags:#x} (fork)"), t.pc, t.lr);
        return Err(ENOSYS);
    }
    let (stack, parent_tid, tls, child_tid) = (a[1], a[2], a[3], a[4]);
    let tid = p.allocate_tid();
    if flags & CLONE_PARENT_SETTID != 0 {
        p.mem.write_u32(parent_tid, tid as u32)?;
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        p.mem.write_u32(child_tid, tid as u32)?;
    }
    let process = std::sync::Arc::clone(&t.process);
    let clear = if flags & CLONE_CHILD_CLEARTID != 0 { child_tid } else { 0 };
    process.spawn_thread(t, tid, stack, (flags & CLONE_SETTLS != 0).then_some(tls), clear)?;
    t.clone_regs = None;
    Ok(tid as u64)
}

fn sys_exit(_p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    t.exit = Some(Exit::Thread(a[0] as i32));
    Ok(0)
}

fn sys_exit_group(_p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    t.exit = Some(Exit::Group(a[0] as i32));
    Ok(0)
}

fn sys_rt_sigaction(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let sig = a[0];
    if !(1..=64).contains(&sig) || a[3] != 8 {
        return Err(EINVAL);
    }
    let mut actions = p.sys.actions.lock();
    if a[2] != 0 {
        p.mem.write(a[2], &actions[sig as usize])?;
    }
    if a[1] != 0 {
        if sig == SIGKILL || sig == SIGSTOP {
            return Err(EINVAL);
        }
        actions[sig as usize] = p.mem.read(a[1], 32)?.try_into().expect("32 bytes");
    }
    Ok(0)
}

fn sys_rt_sigprocmask(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[3] != 8 {
        return Err(EINVAL);
    }
    if a[2] != 0 {
        p.mem.write_u64(a[2], t.sigmask)?;
    }
    if a[1] != 0 {
        let set = p.mem.read_u64(a[1])? & !((1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1)));
        t.sigmask = match a[0] {
            0 => t.sigmask | set,  // SIG_BLOCK
            1 => t.sigmask & !set, // SIG_UNBLOCK
            2 => set,              // SIG_SETMASK
            _ => return Err(EINVAL),
        };
    }
    Ok(0)
}

fn sys_sigaltstack(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[1] != 0 {
        p.mem.write(a[1], &t.altstack)?;
    }
    if a[0] != 0 {
        t.altstack = p.mem.read(a[0], 24)?.try_into().expect("24 bytes");
    }
    Ok(0)
}

fn sys_prctl(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    match a[0] {
        15 => {
            // PR_SET_NAME; the main thread's name is also the process's `comm`.
            t.name = p.mem.read_cstr(a[1], 4096)?.into_iter().take(15).collect();
            if t.tid == p.sys.pid {
                *p.comm.lock() = t.name.clone();
            }
            Ok(0)
        }
        16 => { let mut n = t.name.clone(); n.resize(16, 0); p.mem.write(a[1], &n)?; Ok(0) } // PR_GET_NAME
        55 => { p.sys.tagged_addr_ctrl.store(a[1], std::sync::atomic::Ordering::Relaxed); Ok(0) } // PR_SET_TAGGED_ADDR_CTRL
        56 => Ok(p.sys.tagged_addr_ctrl.load(std::sync::atomic::Ordering::Relaxed)), // PR_GET_TAGGED_ADDR_CTRL
        3 => Ok(1),                              // PR_GET_DUMPABLE
        4 | 38 | 0x59616d61 => Ok(0),            // PR_SET_DUMPABLE, PR_SET_NO_NEW_PRIVS, PR_SET_PTRACER
        39 => Ok(0),                             // PR_GET_NO_NEW_PRIVS
        0x5356_4d41 => Ok(0),                    // PR_SET_VMA (names anonymous memory): accepted, ignored
        other => {
            p.refusals.record(format!("prctl option {other:#x}"), t.pc, t.lr);
            Err(EINVAL)
        }
    }
}

fn sys_clock_gettime(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let d = now(p, a[0])?;
    p.mem.write(a[1], &timespec(d.as_secs(), d.subsec_nanos()))?;
    Ok(0)
}

fn sys_clock_getres(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    now(p, a[0])?;
    if a[1] != 0 {
        p.mem.write(a[1], &timespec(0, 1))?;
    }
    Ok(0)
}

fn sys_gettimeofday(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[0] != 0 {
        let d = now(p, 0)?;
        p.mem.write(a[0], &timespec(d.as_secs(), d.subsec_micros()))?;
    }
    Ok(0)
}

/// Sleep for `d`, interruptibly; on `EINTR` write the time left to `remaining` (if not null).
fn sleep_for(p: &Process, t: &Task, d: Duration, remaining: u64) -> SysResult {
    let deadline = deadline_after(d);
    match p.futexes.sleep_until(deadline, signals(t)) {
        Ok(()) => Ok(0),
        Err(e) => {
            if remaining != 0 {
                let left = deadline.map_or(d, |dl| dl.saturating_duration_since(Instant::now()));
                p.mem.write(remaining, &timespec(left.as_secs(), left.subsec_nanos()))?;
            }
            Err(e)
        }
    }
}

fn sys_nanosleep(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let d = read_timespec(p, a[0])?;
    sleep_for(p, t, d, a[1])
}

fn sys_clock_nanosleep(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let d = read_timespec(p, a[2])?;
    if a[1] & 1 != 0 {
        // TIMER_ABSTIME: no remaining time is reported.
        let n = now(p, a[0])?;
        sleep_for(p, t, d.saturating_sub(n), 0)
    } else {
        sleep_for(p, t, d, a[3])
    }
}

fn sys_getrandom(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let mut buf = vec![0u8; (a[1] as usize).min(1 << 20)];
    omni_platform::process::random_bytes(&mut buf).map_err(|_| EIO)?;
    p.mem.write(a[0], &buf)?;
    Ok(buf.len() as u64)
}

fn sys_uname(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let mut u = [0u8; 6 * 65];
    for (i, s) in ["Linux", "localhost", "6.1.0-omnidroid", "#1 SMP PREEMPT", "aarch64", "localdomain"].iter().enumerate() {
        u[i * 65..i * 65 + s.len()].copy_from_slice(s.as_bytes());
    }
    p.mem.write(a[0], &u)?;
    Ok(0)
}

/// A resource limit's (soft, hard) pair, as `prlimit64` and `/proc/<pid>/limits` report it.
#[must_use]
pub fn limit(resource: u64) -> (u64, u64) {
    const INF: u64 = u64::MAX;
    match resource {
        3 => (8 << 20, INF),     // RLIMIT_STACK
        7 => (32768, 32768),     // RLIMIT_NOFILE
        _ => (INF, INF),
    }
}

fn sys_prlimit64(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[3] != 0 {
        let (soft, hard) = limit(a[1]);
        p.mem.write_u64(a[3], soft)?;
        p.mem.write_u64(a[3] + 8, hard)?;
    }
    Ok(0)
}

fn sys_getrlimit(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    sys_prlimit64(p, t, [0, a[0], 0, a[1], 0, 0])
}

fn sys_sched_getaffinity(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let cpus = std::thread::available_parallelism().map_or(1, usize::from).min(8);
    let mask: u64 = (1u64 << cpus) - 1;
    if a[1] < 8 {
        return Err(EINVAL);
    }
    p.mem.write_u64(a[2], mask)?;
    Ok(8)
}

fn sys_sched_yield(_p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult {
    std::thread::yield_now();
    Ok(0)
}

/// Every thread is `SCHED_OTHER` at priority 0; a change of policy or priority is accepted and
/// has no effect (the host schedules the thread).
fn sys_sched_zero(_p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult {
    Ok(0)
}

fn sys_sched_getparam(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    p.mem.write_u32(a[1], 0)?;
    Ok(0)
}

fn sys_umask(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    Ok(u64::from(p.sys.umask.swap(a[0] as u32 & 0o777, std::sync::atomic::Ordering::Relaxed)))
}

fn sys_sysinfo(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let mut b = [0u8; 112];
    b[..8].copy_from_slice(&p.sys.start.elapsed().as_secs().to_le_bytes()); // uptime
    b[32..40].copy_from_slice(&(8u64 << 30).to_le_bytes()); // totalram (D36 caps the device at 8 GiB)
    b[40..48].copy_from_slice(&(4u64 << 30).to_le_bytes()); // freeram
    b[80..82].copy_from_slice(&1u16.to_le_bytes()); // procs
    b[104..108].copy_from_slice(&1u32.to_le_bytes()); // mem_unit
    p.mem.write(a[0], &b)?;
    Ok(0)
}

fn sys_getrusage(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    p.mem.write(a[1], &[0u8; 144])?;
    Ok(0)
}

fn sys_futex(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    const PRIVATE: u64 = 128;
    const CLOCK_REALTIME: u64 = 256;
    let all = u32::MAX;
    match a[1] & !(PRIVATE | CLOCK_REALTIME) {
        // FUTEX_WAIT: a relative timeout.
        0 => {
            let deadline = if a[3] == 0 { None } else { deadline_after(read_timespec(p, a[3])?) };
            p.futexes.wait(&p.mem, a[0], a[2] as u32, all, deadline, Some(signals(t)))
        }
        // FUTEX_WAIT_BITSET: an absolute timeout on CLOCK_MONOTONIC (or REALTIME with the flag).
        9 => {
            let deadline = if a[3] == 0 {
                None
            } else {
                let clock = if a[1] & CLOCK_REALTIME != 0 { 0 } else { 1 };
                let at = read_timespec(p, a[3])?;
                deadline_after(at.saturating_sub(now(p, clock)?))
            };
            p.futexes.wait(&p.mem, a[0], a[2] as u32, a[5] as u32, deadline, Some(signals(t)))
        }
        1 => p.futexes.wake(a[0], a[2], all),
        10 => p.futexes.wake(a[0], a[2], a[5] as u32),
        3 => p.futexes.requeue(&p.mem, a[0], a[2], a[4], a[3], None),
        4 => p.futexes.requeue(&p.mem, a[0], a[2], a[4], a[3], Some(a[5] as u32)),
        5 => p.futexes.wake_op(&p.mem, a[0], a[2], a[4], a[3], a[5] as u32),
        other => {
            p.refusals.record(format!("futex op {other}"), t.pc, t.lr);
            Err(ENOSYS)
        }
    }
}

/// A signal's disposition: (handler, flags, restorer, mask) from `rt_sigaction`'s store.
impl SysState {
    #[must_use]
    pub fn action(&self, sig: i32) -> (u64, u64, u64, u64) {
        let a = self.actions.lock()[sig as usize];
        let word = |i: usize| u64::from_le_bytes(a[i * 8..i * 8 + 8].try_into().expect("8"));
        (word(0), word(1), word(2), word(3))
    }

    /// `SA_RESETHAND`: back to the default action once delivered.
    pub fn reset_action(&self, sig: i32) {
        self.actions.lock()[sig as usize] = [0; 32];
    }
}

/// Signals ignored when their action is the default.
fn ignored_by_default(sig: u64) -> bool {
    matches!(sig, 17 | 18 | 23 | 28)
}

/// Send `sig` to task `target` (a tid; the process id or 0/-1 mean this task). A handler makes it
/// pending on the target, which the target's run loop delivers (A5); the default action is carried
/// out here -- terminate, or nothing for the signals ignored by default.
fn send_signal(p: &Process, t: &mut Task, target: i64, sig: u64) -> SysResult {
    let own = target == i64::from(t.tid) || target == i64::from(p.sys.pid) || target == 0 || target == -1;
    let tid = if own { t.tid } else { i32::try_from(target).map_err(|_| ESRCH)? };
    if !own && !p.tids().contains(&tid) {
        return Err(ESRCH);
    }
    if sig == 0 {
        return Ok(0);
    }
    if !(1..=64).contains(&sig) {
        return Err(EINVAL);
    }
    let (handler, ..) = p.sys.action(sig as i32);
    match handler {
        1 => Ok(0), // SIG_IGN
        0 => {
            match sig {
                _ if ignored_by_default(sig) => {}
                19..=22 => p.refusals.record(format!("stop by signal {sig}"), t.pc, t.lr),
                _ => t.exit = Some(Exit::Signal(sig as i32)), // terminate
            }
            Ok(0)
        }
        _ if tid == t.tid => {
            t.pending.fetch_or(1 << (sig - 1), std::sync::atomic::Ordering::SeqCst);
            Ok(0)
        }
        _ => {
            p.post_signal(tid, sig as i32);
            Ok(0)
        }
    }
}

/// `rt_sigreturn`: the run loop restores the frame at `sp` (the syscall entry defers to it).
fn sys_rt_sigreturn(_p: &Process, t: &mut Task, _a: [u64; 6]) -> SysResult {
    t.sigreturn = true;
    Ok(0)
}

/// `rt_sigtimedwait` (`sigwait`, ART's signal catcher): take the lowest pending signal in the set
/// and answer its number (and siginfo); wait for one otherwise -- until the timeout (`EAGAIN`) or
/// a signal outside the set that the task does not block (`EINTR`).
fn sys_rt_sigtimedwait(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[3] != 8 {
        return Err(EINVAL);
    }
    let set = p.mem.read_u64(a[0])? & !((1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1)));
    let deadline = if a[2] == 0 { None } else { deadline_after(read_timespec(p, a[2])?) };
    let take = |t: &Task| -> Option<u64> {
        let ready = t.pending.load(std::sync::atomic::Ordering::SeqCst) & set;
        (ready != 0).then(|| {
            let sig = u64::from(ready.trailing_zeros()) + 1;
            t.pending.fetch_and(!(1 << (sig - 1)), std::sync::atomic::Ordering::SeqCst);
            sig
        })
    };
    let sig = loop {
        if let Some(sig) = take(t) {
            break sig;
        }
        if t.pending.load(std::sync::atomic::Ordering::SeqCst) & !t.sigmask != 0 {
            if p.trace {
                eprintln!("[sigwait] set {set:#x} pending {:#x} mask {:#x}: EINTR", t.pending.load(std::sync::atomic::Ordering::SeqCst), t.sigmask);
            }
            return Err(EINTR); // a signal outside the set that a handler takes
        }
        // Wake for a signal in the set, or for one the task would take as a handler.
        let wake = crate::futex::Signals { pending: &t.pending, mask: !set & t.sigmask };
        let slept = p.futexes.sleep_until(deadline, wake);
        if let Some(sig) = take(t) {
            break sig;
        }
        match slept {
            Ok(()) if a[2] != 0 => return Err(EAGAIN),
            Ok(()) => {}
            Err(e) => return Err(e),
        }
    };
    if a[1] != 0 {
        let mut info = [0u8; 128];
        info[0..4].copy_from_slice(&(sig as i32).to_le_bytes());
        info[8..12].copy_from_slice(&crate::signal::SI_TKILL.to_le_bytes());
        info[16..20].copy_from_slice(&p.sys.pid.to_le_bytes());
        info[20..24].copy_from_slice(&p.sys.uid.to_le_bytes());
        p.mem.write(a[1], &info)?;
    }
    Ok(sig)
}

/// `rt_sigsuspend`: wait under a temporary mask until a signal is deliverable, then `EINTR`. The
/// temporary mask stands while that signal's handler is entered; the frame records the old one
/// (`saved_sigmask`), so the handler's return restores it.
fn sys_rt_sigsuspend(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[1] != 8 {
        return Err(EINVAL);
    }
    let temporary = p.mem.read_u64(a[0])? & !((1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1)));
    t.saved_sigmask = Some(t.sigmask);
    t.sigmask = temporary;
    loop {
        match p.futexes.sleep_until(None, signals(t)) {
            Err(e) => return Err(e),
            Ok(()) => continue,
        }
    }
}

fn sys_rt_sigpending(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let pending = t.pending.load(std::sync::atomic::Ordering::SeqCst) & t.sigmask;
    p.mem.write_u64(a[0], pending)?;
    Ok(0)
}

fn sys_kill(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    send_signal(p, t, a[0] as i64, a[1])
}

fn sys_tkill(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    send_signal(p, t, a[0] as i64, a[1])
}

/// `tgkill(tgid, tid, sig)` and `rt_tgsigqueueinfo(tgid, tid, sig, info)`: the target is `tid`.
fn sys_tgkill(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[0] as i64 != i64::from(p.sys.pid) {
        return Err(ESRCH);
    }
    send_signal(p, t, a[1] as i64, a[2])
}

/// Nice values are remembered for no one: every task runs at nice 0, which `getpriority` reports
/// as the kernel does (`20 - nice`), and `setpriority` is accepted.
fn sys_getpriority(_p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult {
    Ok(20)
}

fn sys_setpriority(_p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult {
    Ok(0)
}

/// `process_vm_readv` on this process (what libunwindstack reads memory with, so a bad pointer is
/// an error and not a fault): each remote range copied into the local ones in order, stopping at
/// the first that cannot be read. Another pid is `ESRCH`: there is no other process.
fn sys_process_vm_readv(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[0] as i64 as i32 != p.sys.pid {
        return Err(crate::errno::ESRCH);
    }
    if a[2] > 1024 || a[4] > 1024 || a[5] != 0 {
        return Err(EINVAL);
    }
    let iov = |at: u64, n: u64| -> Result<Vec<(u64, usize)>, crate::errno::Errno> {
        (0..n).map(|i| Ok((p.mem.read_u64(at + i * 16)?, p.mem.read_u64(at + i * 16 + 8)? as usize))).collect()
    };
    let (local, remote) = (iov(a[1], a[2])?, iov(a[3], a[4])?);
    let mut data = Vec::new();
    for (base, len) in remote {
        match p.mem.read(base, len.min((1 << 24) - data.len().min(1 << 24))) {
            Ok(bytes) => data.extend_from_slice(&bytes),
            Err(e) if data.is_empty() => return Err(e),
            Err(_) => break,
        }
    }
    let mut done = 0;
    for (base, len) in local {
        let n = len.min(data.len() - done);
        p.mem.write(base, &data[done..done + n])?;
        done += n;
        if done == data.len() {
            break;
        }
    }
    Ok(done as u64)
}

/// `membarrier`: every command a JIT uses is offered. Each is a full host fence -- the expedited
/// "all threads of this process" forms included, since a guest thread's instructions are host
/// instructions and the host fence orders them.
fn sys_membarrier(_p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const QUERY: u64 = 0;
    // GLOBAL | PRIVATE_EXPEDITED | REGISTER_PRIVATE_EXPEDITED | PRIVATE_EXPEDITED_SYNC_CORE |
    // REGISTER_PRIVATE_EXPEDITED_SYNC_CORE
    const SUPPORTED: u64 = 1 | 8 | 16 | 32 | 64;
    match a[0] {
        QUERY => Ok(SUPPORTED),
        cmd if cmd & SUPPORTED == cmd && cmd.count_ones() == 1 => {
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

pub fn install(table: &mut Table) {
    table.set(nr::RT_SIGTIMEDWAIT, sys_rt_sigtimedwait);
    table.set(nr::RT_SIGSUSPEND, sys_rt_sigsuspend);
    table.set(nr::MEMBARRIER, sys_membarrier);
    table.set(nr::GETPRIORITY, sys_getpriority);
    table.set(nr::SETPRIORITY, sys_setpriority);
    table.set(nr::PROCESS_VM_READV, sys_process_vm_readv);
    table.set(nr::KILL, sys_kill);
    table.set(nr::TKILL, sys_tkill);
    table.set(nr::TGKILL, sys_tgkill);
    table.set(nr::RT_TGSIGQUEUEINFO, sys_tgkill);
    table.set(nr::RT_SIGRETURN, sys_rt_sigreturn);
    table.set(nr::RT_SIGPENDING, sys_rt_sigpending);
    table.set(nr::GETPID, sys_getpid);
    table.set(nr::GETPPID, sys_getppid);
    table.set(nr::GETTID, sys_gettid);
    for n in [nr::GETUID, nr::GETEUID, nr::GETGID, nr::GETEGID] {
        table.set(n, sys_getuid);
    }
    table.set(nr::SET_TID_ADDRESS, sys_set_tid_address);
    table.set(nr::SET_ROBUST_LIST, sys_set_robust_list);
    table.set(nr::CLONE, sys_clone);
    table.set(nr::EXIT, sys_exit);
    table.set(nr::EXIT_GROUP, sys_exit_group);
    table.set(nr::RT_SIGACTION, sys_rt_sigaction);
    table.set(nr::RT_SIGPROCMASK, sys_rt_sigprocmask);
    table.set(nr::SIGALTSTACK, sys_sigaltstack);
    table.set(nr::PRCTL, sys_prctl);
    table.set(nr::CLOCK_GETTIME, sys_clock_gettime);
    table.set(nr::CLOCK_GETRES, sys_clock_getres);
    table.set(nr::GETTIMEOFDAY, sys_gettimeofday);
    table.set(nr::NANOSLEEP, sys_nanosleep);
    table.set(nr::CLOCK_NANOSLEEP, sys_clock_nanosleep);
    table.set(nr::GETRANDOM, sys_getrandom);
    table.set(nr::UNAME, sys_uname);
    table.set(nr::PRLIMIT64, sys_prlimit64);
    table.set(nr::GETRLIMIT, sys_getrlimit);
    table.set(nr::SCHED_GETAFFINITY, sys_sched_getaffinity);
    table.set(nr::SCHED_YIELD, sys_sched_yield);
    for n in [nr::SCHED_GETSCHEDULER, nr::SCHED_SETSCHEDULER, nr::SCHED_SETPARAM, nr::SCHED_GET_PRIORITY_MAX, nr::SCHED_GET_PRIORITY_MIN] {
        table.set(n, sys_sched_zero);
    }
    table.set(nr::SCHED_GETPARAM, sys_sched_getparam);
    table.set(nr::SYSINFO, sys_sysinfo);
    table.set(nr::UMASK, sys_umask);
    table.set(nr::GETRUSAGE, sys_getrusage);
    table.set(nr::FUTEX, sys_futex);
}
