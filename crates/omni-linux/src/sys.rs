//! The small syscalls: ids, clocks, `uname`, randomness, limits, `prctl`, signal state (stored;
//! delivery is A5) and a minimal futex (A4 completes it).
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Condvar, Mutex};

use crate::errno::*;
use crate::process::{Exit, Process, Task};
use crate::syscall::{nr, Table};

const ETIMEDOUT: Errno = Errno(110);
const SIGKILL: u64 = 9;
const SIGSTOP: u64 = 19;

pub struct SysState {
    pub pid: i32,
    pub uid: u32,
    start: Instant,
    actions: Mutex<[[u8; 32]; 65]>,
    futex: Mutex<HashMap<u64, u64>>, // address -> wake generation
    futex_cv: Condvar,
    /// `PR_SET_TAGGED_ADDR_CTRL`'s value. Tagged pointers are accepted either way (see
    /// `guest::untag`); this is what `PR_GET_TAGGED_ADDR_CTRL` reports back.
    tagged_addr_ctrl: std::sync::atomic::AtomicU64,
    /// The file-creation mask (`umask`), 022 to start with as a shell's is.
    umask: std::sync::atomic::AtomicU32,
}

impl SysState {
    #[must_use]
    pub fn new(pid: i32, uid: u32) -> Self {
        Self { pid, uid, start: Instant::now(), actions: Mutex::new([[0; 32]; 65]), futex: Mutex::default(), futex_cv: Condvar::new(), tagged_addr_ctrl: std::sync::atomic::AtomicU64::new(0), umask: std::sync::atomic::AtomicU32::new(0o022) }
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

    /// Wake every waiter on `addr` (A1 has one thread; A4 counts and limits properly).
    pub fn futex_wake(&self, addr: u64) {
        *self.futex.lock().entry(addr).or_insert(0) += 1;
        self.futex_cv.notify_all();
    }
}

fn timespec(secs: u64, nanos: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&secs.to_le_bytes());
    b[8..].copy_from_slice(&u64::from(nanos).to_le_bytes());
    b
}

fn read_timespec(p: &Process, at: u64) -> Result<Duration, Errno> {
    let sec = p.mem.read_u64(at)?;
    let nsec = p.mem.read_u64(at + 8)?;
    if nsec >= 1_000_000_000 {
        return Err(EINVAL);
    }
    Ok(Duration::new(sec, nsec as u32))
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

fn sys_nanosleep(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    std::thread::sleep(read_timespec(p, a[0])?);
    Ok(0)
}

fn sys_clock_nanosleep(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let d = read_timespec(p, a[2])?;
    if a[1] & 1 != 0 {
        // TIMER_ABSTIME
        let n = now(p, a[0])?;
        std::thread::sleep(d.saturating_sub(n));
    } else {
        std::thread::sleep(d);
    }
    Ok(0)
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
    let op = a[1] & !(128 | 256); // FUTEX_PRIVATE_FLAG, FUTEX_CLOCK_REALTIME
    match op {
        0 => {
            let timeout = if a[3] == 0 { None } else { Some(read_timespec(p, a[3])?) };
            let mut table = p.sys.futex.lock();
            let current = u32::from_le_bytes(p.mem.read(a[0], 4)?.try_into().expect("4 bytes"));
            if current != a[2] as u32 {
                return Err(EAGAIN);
            }
            let seen = *table.entry(a[0]).or_insert(0);
            let deadline = timeout.map(|d| Instant::now() + d);
            while table.get(&a[0]).copied() == Some(seen) {
                match deadline {
                    Some(d) => {
                        if p.sys.futex_cv.wait_until(&mut table, d).timed_out() {
                            return Err(ETIMEDOUT);
                        }
                    }
                    None => p.sys.futex_cv.wait(&mut table),
                }
            }
            Ok(0)
        }
        1 => {
            p.sys.futex_wake(a[0]);
            Ok(0)
        }
        other => {
            p.refusals.record(format!("futex op {other}"), t.pc, t.lr);
            Err(ENOSYS)
        }
    }
}

/// Send `sig` to this task. A1 has one thread and no delivery (A5): the default action is carried
/// out -- terminate, or nothing for the signals ignored by default -- and a handler the guest
/// installed is recorded as a refusal, not run.
fn send_signal(p: &Process, t: &mut Task, target: i64, sig: u64) -> SysResult {
    if target != i64::from(t.tid) && target != i64::from(p.sys.pid) && target != 0 && target != -1 {
        return Err(ESRCH);
    }
    if sig == 0 {
        return Ok(0);
    }
    if !(1..=64).contains(&sig) {
        return Err(EINVAL);
    }
    let handler = u64::from_le_bytes(p.sys.actions.lock()[sig as usize][..8].try_into().expect("8 bytes"));
    match handler {
        1 => Ok(0), // SIG_IGN
        0 => {
            match sig {
                17 | 18 | 23 | 28 => {}                          // SIGCHLD, SIGCONT, SIGURG, SIGWINCH: ignored
                19..=22 => p.refusals.record(format!("stop by signal {sig}"), t.pc, t.lr),
                _ => t.exit = Some(Exit::Signal(sig as i32)),     // terminate
            }
            Ok(0)
        }
        _ => {
            p.refusals.record(format!("signal delivery to a handler (A5): signal {sig}"), t.pc, t.lr);
            Ok(0)
        }
    }
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

pub fn install(table: &mut Table) {
    table.set(nr::KILL, sys_kill);
    table.set(nr::TKILL, sys_tkill);
    table.set(nr::TGKILL, sys_tgkill);
    table.set(nr::RT_TGSIGQUEUEINFO, sys_tgkill);
    table.set(nr::GETPID, sys_getpid);
    table.set(nr::GETPPID, sys_getppid);
    table.set(nr::GETTID, sys_gettid);
    for n in [nr::GETUID, nr::GETEUID, nr::GETGID, nr::GETEGID] {
        table.set(n, sys_getuid);
    }
    table.set(nr::SET_TID_ADDRESS, sys_set_tid_address);
    table.set(nr::SET_ROBUST_LIST, sys_set_robust_list);
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
