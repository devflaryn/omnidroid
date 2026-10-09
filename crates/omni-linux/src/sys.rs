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
    /// The user and group ids (real = effective = saved here), which `setuid`/`setgid` change.
    uid: std::sync::atomic::AtomicU32,
    gid: std::sync::atomic::AtomicU32,
    groups: Mutex<Vec<u32>>,
    /// The capabilities held (effective = permitted here): every one for root, none for another
    /// user, unless granted (init's `capabilities`, the zygote's for system_server).
    caps: std::sync::atomic::AtomicU64,
    /// `PR_SET_KEEPCAPS`: a root process keeps its capabilities across `setuid`.
    keepcaps: std::sync::atomic::AtomicBool,
    start: Instant,
    actions: Mutex<[[u8; 32]; 65]>,
    /// The file-creation mask (`umask`), 022 to start with as a shell's is.
    umask: std::sync::atomic::AtomicU32,
    /// The system-call filters and `no_new_privs` (`crate::seccomp`).
    pub seccomp: crate::seccomp::Seccomp,
}

impl SysState {
    #[must_use]
    pub fn new(pid: i32, uid: u32) -> Self {
        Self { pid, uid: uid.into(), gid: uid.into(), groups: Mutex::default(), caps: (if uid == 0 { ALL_CAPS } else { 0 }).into(), keepcaps: false.into(), start: Instant::now(), actions: Mutex::new([[0; 32]; 65]), umask: std::sync::atomic::AtomicU32::new(0o022), seccomp: crate::seccomp::Seccomp::default() }
    }

    /// Become `uid` (and its group), as a zygote child becomes its app: no capabilities kept.
    pub(crate) fn become_user(&self, uid: u32) {
        self.uid.store(uid, std::sync::atomic::Ordering::Relaxed);
        self.gid.store(uid, std::sync::atomic::Ordering::Relaxed);
        self.set_caps(0);
    }

    /// Take these credentials as a whole (engine-granted elevation): ids, groups and capabilities together.
    pub fn assume(&self, uid: u32, gid: u32, groups: Vec<u32>, caps: u64) {
        self.uid.store(uid, std::sync::atomic::Ordering::Relaxed);
        self.gid.store(gid, std::sync::atomic::Ordering::Relaxed);
        *self.groups.lock() = groups;
        self.set_caps(caps);
    }

    #[must_use]
    pub fn uid(&self) -> u32 {
        self.uid.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[must_use]
    pub fn gid(&self) -> u32 {
        self.gid.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A fork child's or an executed program's ids and capabilities: the process's.
    pub fn inherit_ids(&self, from: &SysState) {
        self.uid.store(from.uid(), std::sync::atomic::Ordering::Relaxed);
        self.gid.store(from.gid(), std::sync::atomic::Ordering::Relaxed);
        *self.groups.lock() = from.groups.lock().clone();
        self.caps.store(from.caps(), std::sync::atomic::Ordering::Relaxed);
        // Filters and no_new_privs are kept across fork and execve.
        self.seccomp.inherit(&from.seccomp);
    }

    /// Whether `gid` is this process's group or one of its supplementary groups.
    #[must_use]
    pub fn in_group(&self, gid: u32) -> bool {
        self.gid() == gid || self.groups.lock().contains(&gid)
    }

    /// The capabilities held, bit `n` for `CAP_*` number `n`.
    #[must_use]
    pub fn caps(&self) -> u64 {
        self.caps.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Grant exactly these capabilities (init for a service; the launcher for system_server, as
    /// the zygote grants them).
    pub fn set_caps(&self, caps: u64) {
        self.caps.store(caps & ALL_CAPS, std::sync::atomic::Ordering::Relaxed);
    }

    /// A fork child's: its parent's signal actions and umask.
    pub fn inherit(&self, parent: &SysState) {
        *self.actions.lock() = *parent.actions.lock();
        self.umask.store(parent.umask.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
    }

    /// An executed program's: ignored signals stay ignored, handled ones return to their default
    /// (the handlers are gone with the old image); the umask stays.
    pub fn inherit_ignored(&self, old: &SysState) {
        let old_actions = *old.actions.lock();
        let mut actions = self.actions.lock();
        for (sig, action) in old_actions.iter().enumerate() {
            if u64::from_le_bytes(action[..8].try_into().expect("8 bytes")) == 1 {
                actions[sig] = *action;
            }
        }
        self.umask.store(old.umask.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
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
    let b: [u8; 16] = p.mem.read_array(at)?;
    let sec = i64::from_le_bytes(b[..8].try_into().expect("8"));
    let nsec = u64::from_le_bytes(b[8..].try_into().expect("8"));
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

pub(crate) fn clock_now(p: &Process, clock: u64) -> Result<Duration, Errno> {
    now(p, clock)
}

/// `CLOCK_MONOTONIC` (and `BOOTTIME`): one clock for every guest process of this host process, as
/// the kernel's is for every process -- timestamps are compared across processes (a composer's
/// vsync, a fence's signal time, a frame's deadline). It starts at 1000 s, a device's uptime.
///
/// And one for every host process of an instance: an app's host process (`crate::zygote`) is
/// given its system's origin -- the wall-clock time at which the clock read zero,
/// `OMNI_MONOTONIC_ORIGIN` in nanoseconds -- and reads the same numbers from then on.
///
/// **Read from the guest's counter** (`CNTVCT_EL0`, [`omni_cpu::cntpct`]) as the vDSO reads it
/// (`crate::vdso`): [`counter_ns`] of it plus [`counter_offset`]. The system call and the vDSO are
/// then one function of time, and a clock read one way and then the other never goes back.
#[must_use]
pub fn monotonic() -> Duration {
    let ns = i128::from(counter_ns(omni_cpu::cntpct())) + counter_offset();
    Duration::from_nanos(ns.max(0) as u64)
}

/// Nanoseconds of `ticks` of the guest's counter, as the vDSO computes them: whole seconds, then
/// the remainder, so nothing overflows.
#[must_use]
pub fn counter_ns(ticks: u64) -> u64 {
    let f = u64::from(omni_cpu::CNTFRQ_HZ);
    (ticks / f) * 1_000_000_000 + (ticks % f) * 1_000_000_000 / f
}

/// `CLOCK_REALTIME` less `CLOCK_MONOTONIC`, in ns: the instance's origin -- the wall-clock time at
/// which its `CLOCK_MONOTONIC` read zero (`OMNI_MONOTONIC_ORIGIN`). The guest's wall clock is its
/// monotonic clock from there, in every host process of the instance and through the vDSO alike;
/// it does not follow the host's clock being set while it runs.
#[must_use]
pub fn realtime_offset() -> i128 {
    clock_anchor().1
}

/// `CLOCK_MONOTONIC` when the guest's counter read 0, in ns: the counter's epoch on the platform's
/// monotonic clock, plus this instance's shift of it.
#[must_use]
pub fn counter_offset() -> i128 {
    static OFFSET: std::sync::OnceLock<i128> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        let (counter, platform) = (omni_cpu::cntpct_epoch(), omni_platform::clock::monotonic_epoch());
        let since = if counter >= platform { counter.duration_since(platform).as_nanos() as i128 } else { -(platform.duration_since(counter).as_nanos() as i128) };
        since + clock_anchor().0
    })
}

/// `OMNI_MONOTONIC_ORIGIN` for a host process started for this instance.
#[must_use]
pub fn monotonic_origin() -> String {
    clock_anchor().1.to_string()
}

/// (what is added to the platform's monotonic reading, the origin in wall-clock nanoseconds).
fn clock_anchor() -> &'static (i128, i128) {
    static ANCHOR: std::sync::OnceLock<(i128, i128)> = std::sync::OnceLock::new();
    ANCHOR.get_or_init(|| {
        let platform = omni_platform::clock::monotonic_now().as_nanos() as i128;
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i128);
        let origin = std::env::var("OMNI_MONOTONIC_ORIGIN")
            .ok()
            .and_then(|v| v.parse::<i128>().ok())
            .unwrap_or(wall - platform - Duration::from_secs(1000).as_nanos() as i128);
        ((wall - origin) - platform, origin)
    })
}

fn now(_p: &Process, clock: u64) -> Result<Duration, Errno> {
    match clock {
        // REALTIME(_COARSE/_ALARM), TAI: the monotonic clock from the instance's origin, as the vDSO.
        0 | 5 | 8 | 11 => Ok(Duration::from_nanos((monotonic().as_nanos() as i128 + realtime_offset()).max(0) as u64)),
        1 | 2 | 3 | 4 | 6 | 7 | 9 => Ok(monotonic()), // monotonic family, cputime approximated
        _ => Err(EINVAL),
    }
}

fn sys_getpid(p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(p.sys.pid as u64) }
fn sys_getppid(p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(p.family.ppid() as u64) }
fn sys_gettid(_p: &Process, t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(t.tid as u64) }
fn sys_getuid(p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(u64::from(p.sys.uid())) }
fn sys_getgid(p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(u64::from(p.sys.gid())) }

/// Change an id to `wanted` (`-1`: unchanged): root may take any, another user only its own.
fn set_id(id: &std::sync::atomic::AtomicU32, root: bool, wanted: u64) -> Result<(), Errno> {
    let wanted = wanted as u32;
    if wanted == u32::MAX {
        return Ok(());
    }
    if !root && wanted != id.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(EPERM);
    }
    id.store(wanted, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// `setuid`, `setreuid`, `setresuid` (and `setfsuid`): real, effective and saved are one id here;
/// the last of the ids given that is not `-1` is the one taken. A group change is checked against
/// the user before it (the order a daemon drops privileges in: groups, then user).
/// A root process that becomes another user loses its capabilities, unless it asked to keep
/// them (`PR_SET_KEEPCAPS`).
fn left_root(p: &Process, was_root: bool) {
    if was_root && p.sys.uid() != 0 && !p.sys.keepcaps.load(std::sync::atomic::Ordering::Relaxed) {
        p.sys.set_caps(0);
    }
}

fn sys_setuid(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let root = p.sys.uid() == 0;
    set_id(&p.sys.uid, root, a[0])?;
    left_root(p, root);
    Ok(0)
}
fn sys_setresuid(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let root = p.sys.uid() == 0;
    for id in [a[0], a[1], a[2]] {
        set_id(&p.sys.uid, root, id)?;
    }
    left_root(p, root);
    Ok(0)
}
fn sys_setreuid(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let root = p.sys.uid() == 0;
    for id in [a[0], a[1]] {
        set_id(&p.sys.uid, root, id)?;
    }
    left_root(p, root);
    Ok(0)
}
fn sys_setgid(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    set_id(&p.sys.gid, p.sys.uid() == 0, a[0]).map(|()| 0)
}
fn sys_setresgid(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let root = p.sys.uid() == 0;
    for id in [a[0], a[1], a[2]] {
        set_id(&p.sys.gid, root, id)?;
    }
    Ok(0)
}
fn sys_setregid(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let root = p.sys.uid() == 0;
    for id in [a[0], a[1]] {
        set_id(&p.sys.gid, root, id)?;
    }
    Ok(0)
}
/// `setfsuid`/`setfsgid`: the previous id, always (as the kernel answers).
fn sys_setfsuid(p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(u64::from(p.sys.uid())) }
fn sys_setfsgid(p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(u64::from(p.sys.gid())) }
fn sys_getresuid(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    for at in [a[0], a[1], a[2]] {
        p.mem.write_u32(at, p.sys.uid())?;
    }
    Ok(0)
}
fn sys_getresgid(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    for at in [a[0], a[1], a[2]] {
        p.mem.write_u32(at, p.sys.gid())?;
    }
    Ok(0)
}
/// `setgroups(n, list)`: root only.
fn sys_setgroups(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    if p.sys.uid() != 0 {
        return Err(EPERM);
    }
    let n = usize::try_from(a[0]).map_err(|_| EINVAL)?;
    if n > 65536 {
        return Err(EINVAL);
    }
    let bytes = p.mem.read(a[1], n * 4)?;
    *p.sys.groups.lock() = bytes.chunks(4).map(|c| u32::from_le_bytes(c.try_into().expect("4"))).collect();
    Ok(0)
}
/// `getgroups(size, list)`: the count with size 0; `EINVAL` when they do not fit.
fn sys_getgroups(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let groups = p.sys.groups.lock().clone();
    if a[0] == 0 {
        return Ok(groups.len() as u64);
    }
    if (a[0] as usize) < groups.len() {
        return Err(EINVAL);
    }
    let bytes: Vec<u8> = groups.iter().flat_map(|g| g.to_le_bytes()).collect();
    p.mem.write(a[1], &bytes)?;
    Ok(groups.len() as u64)
}

fn sys_set_tid_address(_p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    t.clear_child_tid = a[0];
    Ok(t.tid as u64)
}

/// Every capability this kernel knows (`CAP_CHOWN` 0 .. `CAP_CHECKPOINT_RESTORE` 40).
pub const ALL_CAPS: u64 = (1 << 41) - 1;

/// The `CAP_*` number of a capability's name (`BLOCK_SUSPEND`, `CAP_NET_ADMIN`, ...).
#[must_use]
pub fn cap_number(name: &str) -> Option<u32> {
    const NAMES: [&str; 41] = [
        "CHOWN", "DAC_OVERRIDE", "DAC_READ_SEARCH", "FOWNER", "FSETID", "KILL", "SETGID", "SETUID", "SETPCAP",
        "LINUX_IMMUTABLE", "NET_BIND_SERVICE", "NET_BROADCAST", "NET_ADMIN", "NET_RAW", "IPC_LOCK", "IPC_OWNER",
        "SYS_MODULE", "SYS_RAWIO", "SYS_CHROOT", "SYS_PTRACE", "SYS_PACCT", "SYS_ADMIN", "SYS_BOOT", "SYS_NICE",
        "SYS_RESOURCE", "SYS_TIME", "SYS_TTY_CONFIG", "MKNOD", "LEASE", "AUDIT_WRITE", "AUDIT_CONTROL", "SETFCAP",
        "MAC_OVERRIDE", "MAC_ADMIN", "SYSLOG", "WAKE_ALARM", "BLOCK_SUSPEND", "AUDIT_READ", "PERFMON", "BPF",
        "CHECKPOINT_RESTORE",
    ];
    let name = name.trim().to_ascii_uppercase();
    let name = name.strip_prefix("CAP_").unwrap_or(&name);
    NAMES.iter().position(|n| *n == name).map(|n| n as u32)
}

/// `capget(header, data)`: the process's capabilities, effective and permitted (none inheritable).
/// Version 3's two data words; an unknown version is answered with version 3 and `EINVAL`, as the
/// kernel asks a caller to retry.
fn sys_capget(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const V3: u32 = 0x2008_0522;
    let version = p.mem.read_u32(a[0])?;
    if version != V3 && version != 0x2007_1026 && version != 0x1998_0330 {
        p.mem.write_u32(a[0], V3)?;
        return Err(EINVAL);
    }
    if a[1] != 0 {
        let caps = p.sys.caps();
        let words = if version == 0x1998_0330 { 1 } else { 2 };
        let mut data = Vec::new();
        for word in 0..words {
            let bits = (caps >> (32 * word)) as u32;
            for field in [bits, bits, 0] {
                data.extend_from_slice(&field.to_le_bytes());
            }
        }
        p.mem.write(a[1], &data)?;
    }
    Ok(0)
}

/// `capset`: the effective set becomes what is asked, within what is held (a process drops
/// capabilities; it cannot take new ones).
fn sys_capset(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let version = p.mem.read_u32(a[0])?;
    let words = if version == 0x1998_0330 { 1 } else { 2 };
    let data = p.mem.read(a[1], 12 * words)?;
    let mut effective = 0u64;
    for word in 0..words {
        let at = 12 * word;
        effective |= u64::from(u32::from_le_bytes(data[at..at + 4].try_into().expect("4"))) << (32 * word);
    }
    if effective & !p.sys.caps() != 0 {
        return Err(EPERM);
    }
    p.sys.set_caps(effective);
    Ok(0)
}

fn sys_set_robust_list(_p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult { Ok(0) }

/// `clone`: a thread (`CLONE_VM | CLONE_SIGHAND | CLONE_THREAD`, as bionic's `pthread_create`
/// asks), or a process -- `fork`, `vfork` -- as `crate::fork` makes one. Another shape is refused
/// by name.
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
    if flags & CLONE_THREAD == 0 {
        return crate::fork::fork(p, t, flags, a);
    }
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

fn sys_exit_group(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let code = a[0] as i32;
    // A process that ends itself with a failure says so, and from where: an app's own exit is
    // otherwise silent (Roblox, 2026-09-29: exit 11 in a world, nothing in its log).
    if code != 0 {
        let at = |x: u64| p.mm.describe(x).map_or_else(String::new, |d| format!(" ({d})"));
        eprintln!("[exit] pid {} tid {} exit_group({code}) pc {:#x}{} lr {:#x}{}", p.sys.pid, t.tid, t.pc, at(t.pc), t.lr, at(t.lr));
    }
    // The heavier detail (saved registers, their memory, the backtrace) is `OMNI_EXIT_REGS=1` only:
    // on a full boot many subprocesses exit non-zero, and dumping each one's stack slows the boot.
    // A spoofed process is the app under test (the `--denylist`ed package, e.g. a game whose
    // anti-tamper exits deliberately): always dump its detail -- there are few of them, and its
    // non-zero exit is exactly what we are chasing -- so the boot stays fast without the env flag.
    if code != 0 && (p.view.spoofed || std::env::var("OMNI_EXIT_REGS").as_deref() == Ok("1")) {
        let at = |x: u64| p.mm.describe(x).map_or_else(String::new, |d| format!(" ({d})"));
        eprintln!("[exit]   x19..x23 = {:#x} {:#x} {:#x} {:#x} {:#x}", t.saved_regs[0], t.saved_regs[1], t.saved_regs[2], t.saved_regs[3], t.saved_regs[4]);
        // Dump 64 bytes at each saved register that looks like a guest pointer: a deliberately-
        // exiting app's report detail is in stack/heap buffers it passed, and the bytes may name
        // what it caught.
        for (n, &r) in t.saved_regs.iter().enumerate() {
            if r > 0x1000 && r < 0x8000_0000_0000 {
                if let Ok(b) = p.mem.read(r, 64) {
                    let ascii: String = b.iter().map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect();
                    eprintln!("[exit]   @x{} {:#x}: {} | {}", 19 + n, r, b.iter().map(|c| format!("{c:02x}")).collect::<String>(), ascii);
                }
            }
        }
        // A frame-pointer backtrace of the exiting thread (the caller that decided to exit, named
        // by lib+offset): what wrote the verdict and from where, for an app that dies deliberately
        // through a shared exit stub whose own pc/lr say nothing about the reason.
        let mut fp = t.fp;
        for depth in 0..24 {
            if fp == 0 || fp & 7 != 0 {
                break;
            }
            let (Ok(ret), Ok(next)) = (p.mem.read_u64(fp + 8), p.mem.read_u64(fp)) else { break };
            if ret == 0 {
                break;
            }
            eprintln!("[exit]   #{depth} {:#x}{}", ret, at(ret));
            if next <= fp {
                break;
            }
            fp = next;
        }
    }
    t.exit = Some(Exit::Group(code));
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

/// Install the filter `struct sock_fprog { u16 len; struct sock_filter *filter; }` at `fprog`:
/// allowed with `no_new_privs` or CAP_SYS_ADMIN.
fn install_filter(p: &Process, fprog: u64) -> Result<(), crate::errno::Errno> {
    const CAP_SYS_ADMIN: u64 = 21;
    if !p.sys.seccomp.no_new_privs.load(std::sync::atomic::Ordering::SeqCst) && p.sys.caps() & (1 << CAP_SYS_ADMIN) == 0 {
        return Err(crate::errno::EACCES);
    }
    let len = u16::from_le_bytes(p.mem.read(fprog, 2)?.try_into().expect("2")) as usize;
    let at = p.mem.read_u64(fprog + 8)?;
    if len == 0 || len > 4096 {
        return Err(EINVAL);
    }
    let bytes = p.mem.read(at, len * 8)?;
    p.sys.seccomp.install(&bytes)
}

/// `seccomp(op, flags, args)`.
fn sys_seccomp(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const TSYNC: u64 = 1;
    const LOG: u64 = 2;
    const SPEC_ALLOW: u64 = 4;
    const TSYNC_ESRCH: u64 = 16;
    match a[0] {
        // SECCOMP_SET_MODE_STRICT
        0 if a[1] == 0 && a[2] == 0 => p.sys.seccomp.set_strict().map(|()| 0),
        // SECCOMP_SET_MODE_FILTER: every thread shares the filters here, so TSYNC holds already;
        // a user-notification listener is not offered.
        1 if a[1] & !(TSYNC | LOG | SPEC_ALLOW | TSYNC_ESRCH) == 0 => install_filter(p, a[2]).map(|()| 0),
        // SECCOMP_GET_ACTION_AVAIL
        2 if a[1] == 0 => {
            let action = p.mem.read_u32(a[2])?;
            if crate::seccomp::action_available(action) { Ok(0) } else { Err(crate::errno::EOPNOTSUPP) }
        }
        _ => Err(EINVAL),
    }
}

fn sys_prctl(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    match a[0] {
        15 => {
            // PR_SET_NAME; the main thread's name is also the process's `comm`.
            t.name = p.mem.read_cstr(a[1], 4096)?.into_iter().take(15).collect();
            crate::cpuprof::renamed(t.tid, &t.name);
            if t.tid == p.sys.pid {
                *p.comm.lock() = t.name.clone();
            }
            Ok(0)
        }
        16 => { let mut n = t.name.clone(); n.resize(16, 0); p.mem.write(a[1], &n)?; Ok(0) } // PR_GET_NAME
        // PR_SET_TAGGED_ADDR_CTRL, PR_GET_TAGGED_ADDR_CTRL: a kernel without the tagged address ABI,
        // so bionic keeps its heap untagged (no `0xb4` pointer tag). Scudo's own `0x02` is another
        // thing: the address it reads and writes every chunk header through (`addHeaderTag`), fixed
        // at compile time for arm64 (`archSupportsMemoryTagging`), whatever this answers -- the
        // CPU's patch 0041 learns those instructions. Guest pointers inside structs reach the host's own
        // code (the GPU driver, D3a), which cannot untag them; TBI in the CPU and `guest::untag` at
        // the syscall boundary still make a tagged pointer work where one appears.
        55 | 56 => Err(EINVAL),
        3 => Ok(1),                              // PR_GET_DUMPABLE
        7 => Ok(u64::from(p.sys.keepcaps.load(std::sync::atomic::Ordering::Relaxed))), // PR_GET_KEEPCAPS
        8 => {
            // PR_SET_KEEPCAPS
            p.sys.keepcaps.store(a[1] != 0, std::sync::atomic::Ordering::Relaxed);
            Ok(0)
        }
        // PR_CAPBSET_READ: every capability is in the bounding set; PR_CAPBSET_DROP: accepted.
        23 => if a[1] <= 40 { Ok(1) } else { Err(EINVAL) },
        24 => if a[1] <= 40 { Ok(0) } else { Err(EINVAL) },
        // PR_SET_TIMERSLACK: there is no timer slack to set.
        29 => Ok(0),
        // PR_CAP_AMBIENT: IS_SET answers whether it is held; RAISE takes one already permitted;
        // LOWER and CLEAR_ALL are accepted.
        47 => match a[1] {
            1 => if a[2] <= 40 { Ok(u64::from(p.sys.caps() & (1 << a[2]) != 0)) } else { Err(EINVAL) },
            2 => if a[2] <= 40 && p.sys.caps() & (1 << a[2]) != 0 { Ok(0) } else { Err(EPERM) },
            3 | 4 => Ok(0),
            _ => Err(EINVAL),
        },
        4 | 0x59616d61 => Ok(0),                 // PR_SET_DUMPABLE, PR_SET_PTRACER
        // PR_SET_NO_NEW_PRIVS: once set, never cleared.
        38 => {
            if a[1] != 1 || a[2] != 0 || a[3] != 0 || a[4] != 0 {
                return Err(EINVAL);
            }
            p.sys.seccomp.no_new_privs.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(0)
        }
        39 => Ok(u64::from(p.sys.seccomp.no_new_privs.load(std::sync::atomic::Ordering::SeqCst))), // PR_GET_NO_NEW_PRIVS
        21 => Ok(u64::from(p.sys.seccomp.mode())), // PR_GET_SECCOMP
        // PR_SET_SECCOMP: strict, or a filter (`struct sock_fprog *` in the third argument).
        22 => match a[1] {
            1 => p.sys.seccomp.set_strict().map(|()| 0),
            2 => install_filter(p, a[2]).map(|()| 0),
            _ => Err(EINVAL),
        },
        // PR_SET_VMA, PR_SET_VMA_ANON_NAME: anonymous memory named `[anon:<name>]`, as
        // `/proc/<pid>/maps` shows it (ART names its spaces, bionic its allocator's). A range that
        // is a file's keeps its file's name, as Linux names only anonymous memory.
        0x5356_4d41 => {
            if a[1] == 0 && a[4] != 0 && a[3] != 0 {
                let name = p.mem.read_cstr(a[4], 80)?;
                let (start, len) = (crate::guest::untag(a[2]), a[3]);
                crate::mmap_log::named(p, t, start, len, &name);
                if p.mm.name_at(start).is_none() {
                    let mut label = b"[anon:".to_vec();
                    label.extend_from_slice(&name);
                    label.push(b']');
                    p.mm.label(start, len, &label);
                }
            }
            Ok(0)
        }
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
    // A spoofed process is shown the kernel `/proc/version` names it, not one called after this
    // runtime: the two are readings of one kernel, and a release with a vendor's name in it is a
    // tell on its own (`crate::root::spoof`).
    let (release, version) = if p.view.spoofed {
        crate::root::spoof::spoofed_uname()
    } else {
        ("6.1.99-omnidroid", "#1 SMP PREEMPT")
    };
    let mut u = [0u8; 6 * 65];
    for (i, s) in ["Linux", "localhost", release, version, "aarch64", "localdomain"].iter().enumerate() {
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

/// `sched_get_priority_max`/`_min`: Linux's ranges -- 1..99 for the real-time policies (FIFO, RR),
/// 0 for the others -- so a caller computing a real-time priority gets a valid one.
fn sys_sched_get_priority_max(_p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    match a[0] & !0x4000_0000 {
        1 | 2 => Ok(99),
        0 | 3 | 5 | 6 => Ok(0),
        _ => Err(EINVAL),
    }
}

fn sys_sched_get_priority_min(_p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    match a[0] & !0x4000_0000 {
        1 | 2 => Ok(1),
        0 | 3 | 5 | 6 => Ok(0),
        _ => Err(EINVAL),
    }
}

fn sys_sched_getparam(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    p.mem.write_u32(a[1], 0)?;
    Ok(0)
}

fn sys_umask(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    Ok(u64::from(p.sys.umask.swap(a[0] as u32 & 0o777, std::sync::atomic::Ordering::Relaxed)))
}

/// The device's RAM, in bytes: what `/proc/meminfo`'s `MemTotal` and `sysinfo`'s `totalram` say,
/// which Android (`ActivityManager.MemoryInfo.totalMem`) and apps size themselves by -- Roblox's
/// engine its caches and graphics tier (D36). 8 GiB, D36's cap, unless `OMNI_DEVICE_RAM_MB` says
/// otherwise (1024..=16384), as an emulator's RAM setting does. Only a figure: nothing is reserved.
#[must_use]
pub fn device_ram() -> u64 {
    static RAM: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *RAM.get_or_init(|| {
        let mb = std::env::var("OMNI_DEVICE_RAM_MB").ok().and_then(|v| v.parse::<u64>().ok()).filter(|mb| (1024..=16384).contains(mb));
        mb.map_or(8 << 30, |mb| mb << 20)
    })
}

fn sys_sysinfo(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let mut b = [0u8; 112];
    b[..8].copy_from_slice(&monotonic().as_secs().to_le_bytes()); // uptime
    let total = device_ram();
    b[32..40].copy_from_slice(&total.to_le_bytes()); // totalram
    b[40..48].copy_from_slice(&(total / 2).to_le_bytes()); // freeram
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
        // FUTEX_LOCK_PI (an absolute CLOCK_REALTIME timeout) and FUTEX_LOCK_PI2 (CLOCK_MONOTONIC,
        // or REALTIME with the flag).
        6 | 13 => {
            let deadline = if a[3] == 0 {
                None
            } else {
                let clock = if a[1] & !(PRIVATE | CLOCK_REALTIME) == 6 || a[1] & CLOCK_REALTIME != 0 { 0 } else { 1 };
                let at = read_timespec(p, a[3])?;
                deadline_after(at.saturating_sub(now(p, clock)?))
            };
            p.futexes.lock_pi(&p.mem, a[0], t.tid as u32, deadline)
        }
        7 => p.futexes.unlock_pi(&p.mem, a[0], t.tid as u32),
        8 => p.futexes.trylock_pi(&p.mem, a[0], t.tid as u32),
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
        // An app launched in a host process of its own (`crate::zygote`), by its pid or its
        // process group's (`kill(-pid)`): ActivityManager ending it.
        if crate::zygote::signal(tid.abs(), sig as i32).is_some() {
            return if (0..=64).contains(&sig) { Ok(0) } else { Err(EINVAL) };
        }
        // A thread of a process this one traces: a debugger probes each with signal 0 and may stop
        // or kill it (`crate::ptrace`; the self-debugging watchdog checks its tracee this way).
        if let Some(tracee) = crate::ptrace::tracee_of(p.sys.pid, tid) {
            if sig == 0 {
                return Ok(0);
            }
            if !(1..=64).contains(&sig) {
                return Err(EINVAL);
            }
            crate::fork::signal_process(&tracee, sig as i32);
            return Ok(0);
        }
        // A child of this process (vold's, installd's): the signal is its to act on.
        let child = p.family.child(tid).ok_or(ESRCH)?;
        if sig != 0 {
            if !(1..=64).contains(&sig) {
                return Err(EINVAL);
            }
            crate::fork::signal_process(&child, sig as i32);
        }
        return Ok(0);
    }
    if sig == 0 {
        return Ok(0);
    }
    if !(1..=64).contains(&sig) {
        return Err(EINVAL);
    }
    let (handler, ..) = p.sys.action(sig as i32);
    let bit = 1u64 << (sig - 1);
    // A default action is taken when the signal is *delivered*, by its target: a blocked one stays
    // pending (ART's signal catcher takes SIGQUIT with sigwait), and a terminating one kills the
    // process from the target's run loop -- never the sender for being the sender.
    match handler {
        1 => Ok(0), // SIG_IGN
        0 if ignored_by_default(sig) && !(tid == t.tid && t.sigmask & bit != 0) => Ok(0),
        0 if (19..=22).contains(&sig) => {
            p.refusals.record(format!("stop by signal {sig}"), t.pc, t.lr);
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
            Err(e) => {
                if p.trace {
                    eprintln!("[sigwait] set {set:#x} pending {:#x} mask {:#x}: woken {e:?}", t.pending.load(std::sync::atomic::Ordering::SeqCst), t.sigmask);
                }
                return Err(e);
            }
        }
    };
    // Its own `siginfo` when it came with one (a timer's `SI_TIMER`: bionic's SIGEV_THREAD thread
    // calls its callback on exactly that), else a kill's.
    let info = p.take_info(t.tid, sig as i32).unwrap_or(crate::signal::SigInfo { signo: sig as i32, code: crate::signal::SI_TKILL, pid: p.sys.pid, uid: p.sys.uid(), ..crate::signal::SigInfo::default() });
    if a[1] != 0 {
        p.mem.write(a[1], &crate::signal::encode(&info))?;
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
    table.set(nr::SCHED_GET_PRIORITY_MAX, sys_sched_get_priority_max);
    table.set(nr::SCHED_GET_PRIORITY_MIN, sys_sched_get_priority_min);
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
    table.set(nr::CAPGET, sys_capget);
    table.set(nr::CAPSET, sys_capset);
    table.set(nr::GETTID, sys_gettid);
    table.set(nr::GETUID, sys_getuid);
    table.set(nr::GETEUID, sys_getuid);
    table.set(nr::GETGID, sys_getgid);
    table.set(nr::GETEGID, sys_getgid);
    table.set(nr::SETUID, sys_setuid);
    table.set(nr::SETREUID, sys_setreuid);
    table.set(nr::SETRESUID, sys_setresuid);
    table.set(nr::SETGID, sys_setgid);
    table.set(nr::SETREGID, sys_setregid);
    table.set(nr::SETRESGID, sys_setresgid);
    table.set(nr::SETFSUID, sys_setfsuid);
    table.set(nr::SETFSGID, sys_setfsgid);
    table.set(nr::GETRESUID, sys_getresuid);
    table.set(nr::GETRESGID, sys_getresgid);
    table.set(nr::SETGROUPS, sys_setgroups);
    table.set(nr::GETGROUPS, sys_getgroups);
    table.set(nr::SET_TID_ADDRESS, sys_set_tid_address);
    table.set(nr::SET_ROBUST_LIST, sys_set_robust_list);
    table.set(nr::CLONE, sys_clone);
    table.set(nr::EXIT, sys_exit);
    table.set(nr::EXIT_GROUP, sys_exit_group);
    table.set(nr::RT_SIGACTION, sys_rt_sigaction);
    table.set(nr::RT_SIGPROCMASK, sys_rt_sigprocmask);
    table.set(nr::SIGALTSTACK, sys_sigaltstack);
    table.set(nr::PRCTL, sys_prctl);
    table.set(nr::SECCOMP, sys_seccomp);
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
    for n in [nr::SCHED_GETSCHEDULER, nr::SCHED_SETSCHEDULER, nr::SCHED_SETPARAM] {
        table.set(n, sys_sched_zero);
    }
    table.set(nr::SCHED_GETPARAM, sys_sched_getparam);
    table.set(nr::SYSINFO, sys_sysinfo);
    table.set(nr::UMASK, sys_umask);
    table.set(nr::GETRUSAGE, sys_getrusage);
    table.set(nr::FUTEX, sys_futex);
}

#[cfg(test)]
mod assume_tests {
    use super::*;

    #[test]
    fn assume_sets_ids_and_caps() {
        let s = SysState::new(1, 2000);
        s.assume(0, 0, vec![0], ALL_CAPS);
        assert_eq!(s.uid(), 0);
        assert_eq!(s.caps(), ALL_CAPS);
    }
}
