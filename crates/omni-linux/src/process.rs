//! A guest process: its address space, descriptors, and tasks, and the loop that runs a task.
use std::path::PathBuf;
use std::sync::Arc;

use omni_cpu::dynarmic::{DynarmicBackend, DynarmicOptions};
use omni_cpu::{ExitReason, GuestAddressSpace, GuestCpu, GuestCpuBackend, GuestThreadConfig, RunLimit, ThunkCall, ThunkContext, XReg};
use omni_mem::{GuestSpace, GuestSpaceConfig};
use parking_lot::Mutex;

use crate::exec::{self, *};
use crate::fd::{FdTable, Output};
use crate::guest::GuestMem;
use crate::mm::{MapRequest, Mm};
use crate::sys::SysState;
use crate::syscall::{name_of, Refusals, Table};
use crate::vfs::{Sysroot, Vfs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    Exited(i32),
    Killed { signal: i32, pc: u64, detail: String },
}

/// How a task asked to end: `exit` ends the thread, `exit_group` the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Thread(i32),
    Group(i32),
    /// Ended by a signal whose action is to terminate (A5 delivers to handlers).
    Signal(i32),
}

pub struct SpawnConfig {
    pub sysroot: PathBuf,
    pub instance_dir: PathBuf,
    pub argv: Vec<Vec<u8>>,
    pub envp: Vec<Vec<u8>>,
    pub stdout: Output,
    pub stderr: Output,
    pub trace: bool,
}

pub struct Process {
    pub mem: GuestMem,
    pub table: Table,
    pub refusals: Refusals,
    pub vfs: Vfs,
    pub fds: FdTable,
    pub cwd: Mutex<Vec<u8>>,
    pub mm: Mm,
    pub sys: SysState,
    /// The process's futex wait queue (A4).
    pub futexes: crate::futex::Futexes,
    /// Live tasks by tid, with the handle that stops each one's run (A4).
    tasks: Mutex<std::collections::HashMap<i32, TaskHandle>>,
    /// Signalled when a task ends, for `run` waiting on the others.
    task_ended: parking_lot::Condvar,
    /// The task forking, while its process is frozen (`crate::fork`).
    freeze: Mutex<Option<i32>>,
    thawed: parking_lot::Condvar,
    next_tid: std::sync::atomic::AtomicI32,
    /// How the process ends, once something has ended it (`exit_group`, a fatal signal or fault).
    group_exit: Mutex<Option<ExitStatus>>,
    pub trace: bool,
    /// The program's arguments, as `/proc/<pid>/cmdline` reports them.
    pub argv: Vec<Vec<u8>>,
    /// The main thread's name (`/proc/<pid>/comm`): argv[0]'s basename until `PR_SET_NAME`.
    pub comm: Mutex<Vec<u8>>,
    /// The `[vdso]` page's `__kernel_rt_sigreturn`: where a handler returns when its action has no
    /// `SA_RESTORER` (bionic on arm64 sets none; the kernel uses the vDSO's trampoline). 0 before
    /// `spawn` maps it.
    pub sigtramp: std::sync::atomic::AtomicU64,
    /// `/dev/__properties__`: property_info, properties_serial and the one context's area (A3).
    pub props: crate::procfs::PropFiles,
    /// This process, for what must reach it later from outside (the property service).
    pub(crate) me: std::sync::OnceLock<std::sync::Weak<Process>>,
    /// Its parent and children (`crate::fork`).
    pub(crate) family: crate::fork::Family,
    /// Shared with the vfork children running in this process's memory.
    backend: Option<Arc<DynarmicBackend>>,
    pub(crate) start: Mutex<Option<(u64, u64)>>, // (pc, sp) of the main task
    exit: Mutex<Option<ExitStatus>>,
    /// Signalled when `exit` is set.
    exited: parking_lot::Condvar,
    scratch: u64,
    /// The `siginfo` of signals sent to a task by another (a timer's `SI_TIMER`), by tid, taken
    /// when the signal is delivered or waited for.
    queued_infos: Mutex<Vec<(i32, crate::signal::SigInfo)>>,
    /// The process's POSIX timers (`timer_create`).
    pub(crate) timers: crate::timer::Timers,
}

/// Every process of this host process, for what reports on all of them (`OMNI_MEM_TRACE`).
static ALL: Mutex<Vec<std::sync::Weak<Process>>> = Mutex::new(Vec::new());

/// The live processes of this host process.
#[must_use]
pub fn all_live() -> Vec<Arc<Process>> {
    ALL.lock().iter().filter_map(std::sync::Weak::upgrade).collect()
}

impl Process {
    /// Bytes its translation cache has committed (0 without a shared cache).
    #[must_use]
    pub fn code_cache_committed(&self) -> u64 {
        self.backend.as_ref().and_then(|b| b.code_cache_stats()).map_or(0, |s| s.committed_bytes)
    }

    /// Bytes of host code its translation cache has emitted, ever.
    #[must_use]
    pub fn code_emitted(&self) -> u64 {
        self.backend.as_ref().and_then(|b| b.code_cache_stats()).map_or(0, |s| s.code_bytes_emitted)
    }

    /// Drop the translations of `[start, start + len)` on every thread of this process.
    pub(crate) fn invalidate_code(&self, start: u64, len: u64) {
        if let (Some(b), Ok(range)) = (&self.backend, omni_cpu::GuestRange::new(start as usize, len as usize)) {
            b.invalidate_code_everywhere(range);
        }
    }

    /// Drop its translations (`crate::code_trim`).
    pub fn trim_code(&self) {
        if let Some(b) = &self.backend {
            b.clear_code_cache();
        }
    }

    /// Bytes of guest memory it has committed.
    #[must_use]
    pub fn guest_committed(&self) -> u64 {
        self.mem.space().stats().committed as u64
    }
}

/// What other tasks reach of a task: the handle that stops its run and its pending signals.
struct TaskHandle {
    halt: omni_cpu::HaltHandle,
    pending: Arc<std::sync::atomic::AtomicU64>,
    state: Arc<std::sync::atomic::AtomicU8>,
}

/// Where a task is: running guest code, in the kernel (a syscall, or between runs), or parked
/// while its process is frozen for a fork.
pub(crate) const IN_GUEST: u8 = 0;
pub(crate) const IN_KERNEL: u8 = 1;
pub(crate) const PARKED: u8 = 2;

pub struct Task {
    pub tid: i32,
    pub process: Arc<Process>,
    pub pc: u64,
    pub lr: u64,
    pub clear_child_tid: u64,
    pub sigmask: u64,
    pub altstack: [u8; 24],
    pub name: Vec<u8>,
    pub exit: Option<Exit>,
    /// `x0`..`x30` and `sp` at a `clone`, captured by the syscall entry for `clone` only.
    pub clone_regs: Option<([u64; 31], u64)>,
    /// Signals posted to this task and not yet delivered (bit `n - 1` for signal `n`).
    pub pending: Arc<std::sync::atomic::AtomicU64>,
    /// Set by `rt_sigreturn`: the run loop restores the frame at `sp`.
    pub sigreturn: bool,
    /// The mask `rt_sigsuspend` replaced: the next handler's frame records it, so the handler's
    /// return restores it (as the kernel's `saved_sigmask`).
    pub saved_sigmask: Option<u64>,
    /// `TPIDR_EL0` at the last `clone`: a thread made without `CLONE_SETTLS` inherits it.
    pub clone_tpidr: Option<u64>,
    /// A signal this task raised itself with its own `siginfo` (a seccomp trap's SIGSYS), used
    /// when that signal is delivered.
    pub queued_info: Option<crate::signal::SigInfo>,
    /// `IN_GUEST`, `IN_KERNEL` or `PARKED`.
    pub(crate) state: Arc<std::sync::atomic::AtomicU8>,
}

const GUEST_SPACE_BYTES: usize = 64 << 30;

/// `OMNI_SIGNAL_TRACE=1`: every signal delivered to a guest handler, with where it hit -- the
/// syscall trace's signal lines alone, cheap enough to leave on.
fn signal_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_SIGNAL_TRACE").as_deref() == Ok("1"))
}
/// Where the guest space is reserved: below 4 GiB, because ART keeps its heap and boot image there
/// (compressed references are 32 bits; the boot image goes near `ART_BASE_ADDRESS`, 0x70000000),
/// and a guest address is a host address (D4). The host may already hold pieces of that range --
/// Windows keeps `KUSER_SHARED_DATA` at `0x7FFE0000` in every process -- and the space steps
/// around them (`GuestSpaceConfig::around_host`). A host that maps nothing that low (macOS: a hard
/// 4 GiB `__PAGEZERO`) gets the same guest layout with its part below 4 GiB backed elsewhere
/// (`GuestSpaceConfig::low_window`, D41).
const GUEST_SPACE_LOW_BASE: usize = 0x1000_0000;

/// The least of a low space that must be free for it to be taken: the host's own pieces of that
/// range (`KUSER_SHARED_DATA`, a DLL, a thread's stack) are megabytes, never gigabytes.
const LOW_SPACE_MIN_FREE: usize = GUEST_SPACE_BYTES / 2;

/// The guest space, at [`GUEST_SPACE_LOW_BASE`] if the host has that free, else wherever it
/// chooses (a program that needs no low memory still runs; ART will not).
///
/// "Free" means most of the range. Every process of a host process asks for the same low range,
/// and the one that holds it (system_server) was reserved around what the host held there then --
/// host threads' 1 MiB stacks among them. When such a thread exits, its stack is the one free
/// piece of the range, and a space reserved around everything else "succeeded" with 1 MiB free of
/// 64 GiB: every service init started after that failed to map its first segment (D5, ~1 boot in
/// 3, right after odsign stopped). Such a space is given back and the host chooses.
/// A default-sized space with the guest's 4 KiB pages (D42), for a process that is not one of the
/// system's own (a stand-in, a handler test).
fn small_space() -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig { guest_page: Some(omni_mem::GUEST_PAGE), ..GuestSpaceConfig::default() }).expect("a guest space")
}

pub fn reserve_space() -> Result<GuestSpace, omni_mem::MemError> {
    // D41: below the host's floor the low range cannot be the host's own; it is a based window.
    let low_window = omni_platform::vm::lowest_mappable_address() > GUEST_SPACE_LOW_BASE;
    let config = |base: Option<usize>| GuestSpaceConfig {
        base,
        size: GUEST_SPACE_BYTES,
        around_host: base.is_some(),
        low_window: low_window && base.is_some(),
        // 4 KiB pages for the guest on every host (D42): a sub-page overlay where the host's page
        // is larger (`omni_mem::subpage`), nothing at all where it is 4 KiB.
        guest_page: Some(omni_mem::GUEST_PAGE),
        ..GuestSpaceConfig::default()
    };
    match GuestSpace::with_config(config(Some(GUEST_SPACE_LOW_BASE))) {
        Ok(low) if low.stats().free >= LOW_SPACE_MIN_FREE => Ok(low),
        low => {
            match low {
                Ok(low) => tracing::debug!(free = low.stats().free, "the low range is mostly another's; reserving where the host chooses"),
                Err(e) => tracing::warn!(%e, "no guest space below 4 GiB; reserving where the host chooses"),
            }
            GuestSpace::with_config(config(None))
        }
    }
}
const STACK_BYTES: u64 = 8 << 20;
/// Process ids: 1000 for the first live process of this host process, then the next free
/// thousand, so processes that run side by side (servicemanager, system_server, an app) are told
/// apart -- by binder's sender pid, by `/proc/<pid>` -- and their thread ids never meet.
static LIVE_PIDS: Mutex<std::collections::BTreeSet<i32>> = Mutex::new(std::collections::BTreeSet::new());

/// The pid the next process of this host process gets, when one was assigned (an app launched
/// for the system's ActivityManager gets the pid it was promised).
static ASSIGNED_PID: Mutex<Option<i32>> = Mutex::new(None);

/// Give the next process made here pid `pid`.
pub fn assign_next_pid(pid: i32) {
    *ASSIGNED_PID.lock() = Some(pid);
}

/// A pid for a process of another host process (an app's): out of this one's, and kept from it.
pub fn reserve_pid() -> i32 {
    allocate_pid()
}

fn allocate_pid() -> i32 {
    let mut live = LIVE_PIDS.lock();
    if let Some(pid) = ASSIGNED_PID.lock().take() {
        live.insert(pid);
        return pid;
    }
    // Onward from the last one handed out, as the kernel allocates (wrapping at pid_max): a pid
    // just freed is not given again at once -- a shell waiting for two children of one pipeline
    // tells them apart by it.
    static LAST: Mutex<i32> = Mutex::new(0);
    let mut last = LAST.lock();
    const SLOTS: i32 = 4_194_304 / 1000;
    let pid = (1..=SLOTS)
        .map(|step| ((*last / 1000 + step - 1) % SLOTS + 1) * 1000)
        .find(|p| !live.contains(p))
        .expect("a free pid");
    *last = pid;
    live.insert(pid);
    pid
}
const UID: u32 = 10000;

fn altstack_disabled() -> [u8; 24] {
    let mut s = [0u8; 24];
    s[8..12].copy_from_slice(&2i32.to_le_bytes()); // SS_DISABLE
    s
}

impl Task {
    #[must_use]
    pub fn new(tid: i32, process: Arc<Process>) -> Self {
        Self { tid, process, pc: 0, lr: 0, clear_child_tid: 0, sigmask: 0, altstack: altstack_disabled(), name: Vec::new(), exit: None, clone_regs: None, pending: Arc::default(), sigreturn: false, saved_sigmask: None, clone_tpidr: None, queued_info: None, state: Arc::new(std::sync::atomic::AtomicU8::new(IN_KERNEL)) }
    }
}

/// `OMNI_SYSCALL_STATS=<seconds>`: per system call, how many were made in this host process and how
/// long they took, summed and printed (`syscall_stats`) -- what share of a process's time is the
/// kernel's, and which call's.
fn syscall_stats_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OMNI_SYSCALL_STATS").is_some())
}

/// `OMNI_SLOW_SYSCALL_MS=<ms>`: every system call of this host process that takes at least that
/// long, logged as it returns (`[slow]`: the instance's monotonic ms, task, call, how long, where
/// from) -- where a thread that should be busy waits, without a whole trace's cost.
fn slow_syscall_ms() -> Option<u64> {
    static ON: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_SLOW_SYSCALL_MS").ok().and_then(|v| v.parse().ok()))
}

const STAT_CALLS: usize = 512;
static STAT_COUNT: [std::sync::atomic::AtomicU64; STAT_CALLS] = [const { std::sync::atomic::AtomicU64::new(0) }; STAT_CALLS];
static STAT_NANOS: [std::sync::atomic::AtomicU64; STAT_CALLS] = [const { std::sync::atomic::AtomicU64::new(0) }; STAT_CALLS];

/// The calls since the last report, most time first: (name, calls, total ms), and the counters
/// reset.
pub fn syscall_stats() -> Vec<(String, u64, u64)> {
    let mut out: Vec<(String, u64, u64)> = (0..STAT_CALLS)
        .filter_map(|n| {
            let c = STAT_COUNT[n].swap(0, std::sync::atomic::Ordering::Relaxed);
            let ns = STAT_NANOS[n].swap(0, std::sync::atomic::Ordering::Relaxed);
            let name = match n {
                500 => "ioctl:gpu".to_string(),
                501 => "ioctl:binder".to_string(),
                _ => name_of(n as u64).into_owned(),
            };
            (c > 0).then(|| (name, c, ns / 1_000_000))
        })
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out
}

/// `OMNI_THREAD_DUMP=<seconds>`: every task's system call in progress is kept (number, pc, lr,
/// since when, the task's name), for [`blocked_calls`] -- where the threads of a process that has
/// stopped making progress wait, without a full trace's cost.
fn thread_dump() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OMNI_THREAD_DUMP").is_some())
}

type InFlight = std::collections::HashMap<i32, (u64, u64, u64, std::time::Instant, Vec<u8>)>;
static IN_FLIGHT: Mutex<Option<InFlight>> = Mutex::new(None);

/// The system calls in progress for longer than `at_least` (`OMNI_THREAD_DUMP`), each as a line:
/// tid, name, call, and its return address named by `describe`.
pub fn blocked_calls(at_least: std::time::Duration, describe: impl Fn(u64) -> String) -> Vec<String> {
    let now = std::time::Instant::now();
    let map = IN_FLIGHT.lock();
    let mut out: Vec<(i32, String)> = map
        .iter()
        .flatten()
        .filter(|(_, (_, _, _, since, _))| now.duration_since(*since) >= at_least)
        .map(|(tid, (nr, pc, lr, since, name))| {
            (*tid, format!("{tid} {:?} {} for {}s, lr {}, pc {}", String::from_utf8_lossy(name), name_of(*nr), now.duration_since(*since).as_secs(), describe(*lr), describe(*pc)))
        })
        .collect();
    out.sort();
    out.into_iter().map(|(_, s)| s).collect()
}

/// The in-loop syscall entry (`GuestCpu::set_svc_handler`): `ThunkContext` is the task's address.
fn on_svc(call: &mut ThunkCall<'_>) {
    // SAFETY: the context is `&mut Task` of the task this CPU runs, set by `run_task`, which owns
    // the task for exactly as long as the CPU can call this.
    let task = unsafe { &mut *(call.context().0 as *mut Task) };
    task.state.store(IN_KERNEL, std::sync::atomic::Ordering::SeqCst);
    let number = call.x(8);
    let args = [call.x(0), call.x(1), call.x(2), call.x(3), call.x(4), call.x(5)];
    task.pc = call.address() as u64;
    task.lr = call.lr() as u64;
    if number == crate::syscall::nr::CLONE {
        let mut regs = [0u64; 31];
        for (n, r) in regs.iter_mut().enumerate() {
            *r = call.x(n as u32);
        }
        task.clone_regs = Some((regs, call.sp() as u64));
        task.clone_tpidr = call.tpidr_el0();
    }
    let process = Arc::clone(&task.process);
    if process.trace {
        // A call that may wait is shown as it starts too: a thread that never returns is then seen
        // where it waits.
        use crate::syscall::nr;
        if matches!(number, nr::FUTEX | nr::EPOLL_PWAIT | nr::PPOLL | nr::IOCTL | nr::READ | nr::NANOSLEEP | nr::CLOCK_NANOSLEEP) {
            let what = if number == nr::PPOLL || (number == nr::FUTEX && args[1] & 0x7f == 9) {
                // The first descriptor polled, and what it is.
                let fd = if number == nr::PPOLL { process.mem.read_u32(args[0]).map_or(-1, |f| f as i32) } else { -1 };
                let kind = process.fds.get(fd).map_or_else(|_| "?".to_string(), |f| String::from_utf8_lossy(&crate::fd::guest_path_of(&f)).into_owned());
                // The return addresses of the frames above (the frame-pointer chain), by library.
                let maps = process.mm.file_mappings();
                let name = |at: u64| {
                    maps.iter()
                        .find(|(start, len, _, _)| (*start..start + len).contains(&at))
                        .map_or_else(|| format!("{at:#x}"), |(start, _, guest, offset)| format!("{}+{:#x}", String::from_utf8_lossy(guest).rsplit('/').next().unwrap_or_default(), at - start + offset))
                };
                let mut frames = vec![name(task.lr)];
                let mut fp = call.x(29);
                for _ in 0..8 {
                    let (Ok(next), Ok(ret)) = (process.mem.read_u64(fp), process.mem.read_u64(fp + 8)) else { break };
                    if ret == 0 {
                        break;
                    }
                    frames.push(name(ret & 0x00ff_ffff_ffff_ffff));
                    fp = next;
                }
                format!(" fd {fd} {kind} from {}", frames.join(" < "))
            } else {
                String::new()
            };
            eprintln!("[{}] {}({:#x}, {:#x}, {:#x}, {:#x}){what} ...", task.tid, name_of(number), args[0], args[1], args[2], args[3]);
        }
    }
    if thread_dump() {
        IN_FLIGHT.lock().get_or_insert_with(Default::default).insert(task.tid, (number, task.pc, task.lr, std::time::Instant::now(), task.name.clone()));
    }
    let stats_from = (syscall_stats_on() || slow_syscall_ms().is_some()).then(std::time::Instant::now);
    let result = process.syscall(task, number, args);
    if let (Some(t0), Some(ms)) = (stats_from, slow_syscall_ms()) {
        let took = t0.elapsed();
        if took.as_millis() >= u128::from(ms) {
            let maps = process.mm.file_mappings();
            let lib = |at: u64| {
                maps.iter()
                    .find(|(start, len, _, _)| (*start..start + len).contains(&at))
                    .map_or_else(|| format!("{at:#x}"), |(start, _, guest, offset)| format!("{}+{:#x}", String::from_utf8_lossy(guest).rsplit('/').next().unwrap_or_default(), at - start + offset))
            };
            let fd = match number {
                crate::syscall::nr::PPOLL => process.mem.read_u32(args[0]).map_or(-1, |f| f as i32),
                crate::syscall::nr::FUTEX | crate::syscall::nr::NANOSLEEP | crate::syscall::nr::CLOCK_NANOSLEEP => -1,
                _ => args[0] as i32,
            };
            let kind = process.fds.get(fd).map_or_else(|_| String::new(), |f| format!(" fd {fd} {}", String::from_utf8_lossy(&crate::fd::guest_path_of(&f))));
            // The return addresses of the frames above (the frame-pointer chain).
            let mut frames = Vec::new();
            let mut fp = call.x(29);
            for _ in 0..10 {
                let (Ok(next), Ok(ret)) = (process.mem.read_u64(fp), process.mem.read_u64(fp + 8)) else { break };
                if ret == 0 {
                    break;
                }
                frames.push(lib(ret & 0x00ff_ffff_ffff_ffff));
                fp = next;
            }
            eprintln!(
                "[slow] {} {}:{} {:?} {}({:#x}, {:#x}, {:#x}, {:#x}){kind} took {} ms = {:#x}, from {} < {} < {}",
                crate::sys::monotonic().as_millis(),
                process.sys.pid,
                task.tid,
                String::from_utf8_lossy(&task.name),
                name_of(number),
                args[0],
                args[1],
                args[2],
                args[3],
                took.as_millis(),
                result,
                lib(task.pc),
                lib(task.lr),
                frames.join(" < ")
            );
        }
    }
    if let Some(t0) = stats_from {
        // ioctl split by its device: the GPU's ('G', one per Vulkan command) and binder's ('b').
        let n = match (number, (args[1] >> 8) & 0xff) {
            (crate::syscall::nr::IOCTL, 0x47) => 500,
            (crate::syscall::nr::IOCTL, 0x62) => 501,
            _ => (number as usize).min(STAT_CALLS - 1),
        };
        STAT_COUNT[n].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        STAT_NANOS[n].fetch_add(t0.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    }
    if thread_dump() {
        if let Some(m) = IN_FLIGHT.lock().as_mut() {
            m.remove(&task.tid);
        }
    }
    if process.trace {
        // Path-taking calls show their path: what a trace is read for.
        use crate::syscall::nr;
        let path_arg = match number {
            nr::OPENAT | nr::NEWFSTATAT | nr::FACCESSAT | nr::FACCESSAT2 | nr::READLINKAT | nr::MKDIRAT
            | nr::UNLINKAT | nr::FCHMODAT | nr::FCHOWNAT | nr::STATX => Some(args[1]),
            nr::STATFS | nr::CHDIR => Some(args[0]),
            _ => None,
        };
        let path = path_arg
            .and_then(|a| process.mem.read_cstr(a, 4096).ok())
            .map_or_else(String::new, |p| format!(" \"{}\"", String::from_utf8_lossy(&p)));
        eprintln!("[{}] {}({:#x}, {:#x}, {:#x}, {:#x}){path} = {:#x}", task.tid, name_of(number), args[0], args[1], args[2], args[3], result);
    }
    call.set_x(0, result);
    // Frozen for a fork: not back to guest code until the child has let go of the memory.
    process.park_if_frozen(task.tid, &task.state);
    task.state.store(IN_GUEST, std::sync::atomic::Ordering::SeqCst);
    let deliverable = task.pending.load(std::sync::atomic::Ordering::SeqCst) & !task.sigmask != 0;
    if task.exit.is_some() || task.sigreturn || deliverable {
        call.defer_to_caller();
    }
}

impl Process {
    pub fn syscall(&self, task: &mut Task, number: u64, args: [u64; 6]) -> u64 {
        // The process's seccomp filters answer first.
        if self.sys.seccomp.active() {
            use crate::seccomp::Verdict;
            match self.sys.seccomp.check(number, task.pc, &args) {
                Verdict::Allow => {}
                Verdict::Errno(e) => return crate::errno::Errno(i32::from(e)).as_return(),
                Verdict::Trap(data) => {
                    task.queued_info = Some(crate::signal::SigInfo {
                        signo: 31,
                        code: crate::signal::SYS_SECCOMP,
                        addr: task.pc,
                        errno: i32::from(data),
                        syscall: number as i32,
                        arch: crate::seccomp::AUDIT_ARCH_AARCH64,
                        ..crate::signal::SigInfo::default()
                    });
                    task.pending.fetch_or(1 << 30, std::sync::atomic::Ordering::SeqCst);
                    return crate::errno::ENOSYS.as_return();
                }
                Verdict::Kill => {
                    self.end(ExitStatus::Killed { signal: 31, pc: task.pc, detail: format!("seccomp: {} is not allowed", name_of(number)) });
                    return crate::errno::ENOSYS.as_return();
                }
                Verdict::Unavailable => return crate::errno::ENOSYS.as_return(),
            }
        }
        match self.table.get(number) {
            Some(handler) => match handler(self, task, args) {
                Ok(v) => v,
                Err(e) => e.as_return(),
            },
            None => {
                self.refusals.record(name_of(number).into_owned(), task.pc, task.lr);
                crate::errno::ENOSYS.as_return()
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(space: Arc<GuestSpace>, vfs: Vfs, argv: Vec<Vec<u8>>, stdout: Output, stderr: Output, trace: bool, backend: Option<Arc<DynarmicBackend>>, scratch: u64, uid: u32) -> Arc<Self> {
        Self::assemble_as(space, None, None, vfs, argv, FdTable::standard(stdout, stderr), trace, backend, scratch, uid)
    }

    /// A process in `space`: under `layout` (a vfork child's is its parent's) and `pid` (an
    /// executed image keeps its process's), else fresh ones.
    #[allow(clippy::too_many_arguments)]
    fn assemble_as(space: Arc<GuestSpace>, layout: Option<crate::guest::Layout>, pid: Option<i32>, vfs: Vfs, argv: Vec<Vec<u8>>, fds: FdTable, trace: bool, backend: Option<Arc<DynarmicBackend>>, scratch: u64, uid: u32) -> Arc<Self> {
        let mut table = Table::new();
        crate::install_all(&mut table);
        let (_, dropped) = crate::props::Properties::from_sysroot(vfs.sysroot());
        let props = crate::procfs::PropFiles {
            info: crate::props::property_info_bytes(),
            serial: crate::props::serial_area_bytes(),
            // The live area of the instance's property service: what every process reads.
            area: crate::props::PropertyService::global(vfs.sysroot()).area_bytes(),
            apex_info: crate::apex::apex_info_list(vfs.sysroot()),
        };
        let comm = argv.first().map_or_else(Vec::new, |a| {
            a.rsplit(|&b| b == b'/').next().unwrap_or(a).iter().copied().take(15).collect()
        });
        let pid = pid.unwrap_or_else(allocate_pid);
        let layout = layout.unwrap_or_default();
        let p = Arc::new(Self {
            mem: GuestMem::new(Arc::clone(&space), Arc::clone(&layout)),
            table,
            refusals: Refusals::default(),
            vfs,
            fds,
            cwd: Mutex::new(b"/".to_vec()),
            mm: Mm::new(space, layout),
            sys: SysState::new(pid, uid),
            futexes: crate::futex::Futexes::default(),
            tasks: Mutex::new(std::collections::HashMap::new()),
            task_ended: parking_lot::Condvar::new(),
            freeze: Mutex::new(None),
            thawed: parking_lot::Condvar::new(),
            next_tid: std::sync::atomic::AtomicI32::new(pid + 1),
            group_exit: Mutex::new(None),
            trace,
            argv,
            comm: Mutex::new(comm),
            props,
            me: std::sync::OnceLock::new(),
            family: crate::fork::Family::default(),
            sigtramp: std::sync::atomic::AtomicU64::new(0),
            backend,
            start: Mutex::new(None),
            exit: Mutex::new(None),
            exited: parking_lot::Condvar::new(),
            scratch,
            queued_infos: Mutex::new(Vec::new()),
            timers: crate::timer::Timers::default(),
        });
        let _ = p.me.set(Arc::downgrade(&p));
        {
            let mut all = ALL.lock();
            all.retain(|w| w.strong_count() > 0);
            all.push(Arc::downgrade(&p));
        }
        split_report_start();
        // `/proc` and `/sys` are generated from the process itself (`procfs`).
        let proc: Arc<dyn crate::procfs::ProcFs> = Arc::clone(&p) as Arc<dyn crate::procfs::ProcFs>;
        p.vfs.attach_proc(Arc::downgrade(&proc));
        for name in dropped {
            p.refusals.record(format!("property dropped (too long for its kind): {name}"), 0, 0);
        }
        p
    }

    /// The program as an app runs (`AID_APP_START`, 10000).
    pub fn spawn(config: SpawnConfig) -> Result<Arc<Self>, String> {
        Self::spawn_as(config, UID)
    }

    /// The program as user `uid`, as init starts a daemon (`user system` is 1000).
    pub fn spawn_as(config: SpawnConfig, uid: u32) -> Result<Arc<Self>, String> {
        let sysroot = Sysroot::open(&config.sysroot)?;
        let exe = config.argv.first().ok_or("no program: argv is empty")?.clone();
        // The instance's writable state: what a device keeps on its data partition and tmpfs, and
        // `/linkerconfig`, which `linkerconfig` writes at boot for `linker64` to read (sub-project B).
        // `/mnt` and `/storage` are the tmpfs a device's init makes them: shared storage's mount
        // points (`crate::mount`).
        let writable_dirs = ["data", "tmp", "linkerconfig", "metadata", "mnt", "storage"];
        let mut writable = Vec::new();
        for dir in writable_dirs {
            let host = config.instance_dir.join(dir);
            std::fs::create_dir_all(&host).map_err(|e| format!("{}: {e}", host.display()))?;
            writable.push((format!("/{dir}").into_bytes(), host));
        }
        // What init.rc makes before any service runs: its `mkdir`s on the writable mounts.
        crate::boot::make_init_dirs(&sysroot, &config.instance_dir);
        let vfs = Vfs::new(sysroot, writable, exe.clone()).with_binds(crate::vfs::Binds::of(&config.instance_dir))
            .with_owners(crate::owners::Owners::of(&config.instance_dir));
        let space = Arc::new(reserve_space().map_err(|e| format!("reserve the guest address space: {e}"))?);
        let backend = DynarmicBackend::new(Arc::clone(&space), Self::cpu_options()).map_err(|e| format!("the CPU backend: {e}"))?;
        let p = Self::assemble(space, vfs, config.argv.clone(), config.stdout, config.stderr, config.trace, Some(Arc::new(backend)), 0, uid);
        p.load(&exe, &config.argv, &config.envp)?;
        Ok(p)
    }

    /// The CPU options every process runs with. `OMNI_DYNARMIC_OPT=<hex mask>`: only these (safe)
    /// JIT optimizations -- 0 for none, to tell a translation fault from a kernel one.
    fn cpu_options() -> DynarmicOptions {
        // Top Byte Ignore: arm64 Linux gives user space TBI, and Android's heap depends on it.
        // 512 guest threads: ART alone starts about twenty, Roblox runs dozens, and system_server
        // well over a hundred (the value-compare monitor costs nothing per slot unused).
        // A fault the guest means (ART's implicit null checks) is served once through the callback,
        // not by moving the instruction there for good (`recompile_on_declined_fault`).
        let mut options = DynarmicOptions {
            top_byte_ignore: true,
            max_threads: 512,
            recompile_on_declined_fault: false,
            ..DynarmicOptions::default()
        };
        if let Some(mask) = std::env::var("OMNI_DYNARMIC_OPT").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
            options.optimizations_override = Some(mask);
        }
        options
    }

    /// `execve` in a vfork child: `exe` loaded into a fresh space as `spawn` loads a program, under
    /// this process's pid, uid and cwd, with its descriptors less the close-on-exec ones.
    pub(crate) fn exec_image(self: &Arc<Self>, exe: &[u8], argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<Arc<Self>, String> {
        let vfs = self.vfs.for_exec(exe.to_vec());
        let space = Arc::new(reserve_space().map_err(|e| format!("reserve the guest address space: {e}"))?);
        let backend = DynarmicBackend::new(Arc::clone(&space), Self::cpu_options()).map_err(|e| format!("the CPU backend: {e}"))?;
        let fds = self.fds.for_exec();
        let p = Self::assemble_as(space, None, Some(self.sys.pid), vfs, argv.to_vec(), fds, self.trace, Some(Arc::new(backend)), 0, self.sys.uid());
        *p.cwd.lock() = self.cwd.lock().clone();
        p.sys.inherit_ignored(&self.sys);
        p.sys.inherit_ids(&self.sys);
        p.family.inherit(&self.family);
        p.load(exe, argv, envp)?;
        Ok(p)
    }

    /// A stand-in for another host process's process (`crate::remote`): its pid and uid, its memory
    /// and descriptors reached through `mem` and `fds`; no CPU runs it.
    pub fn stand_in(sysroot: Arc<crate::vfs::Sysroot>, pid: i32, uid: u32, mem: Arc<dyn crate::guest::Remote>, fds: Arc<dyn crate::fd::RemoteFds>) -> Arc<Self> {
        let space = Arc::new(small_space());
        let vfs = Vfs::new(sysroot, Vec::new(), b"/remote".to_vec());
        let p = Self::assemble_as(space, None, Some(pid), vfs, vec![b"/remote".to_vec()], FdTable::standard(Output::Host, Output::Host), false, None, 0, uid);
        p.mem.set_remote(mem);
        p.fds.set_remote(fds);
        p.family.mark_stand_in();
        p
    }

    /// A vfork child of this process: the same memory (space, layout lock, CPU backend), a copy of
    /// the descriptors, cwd, signal actions and umask, and a pid of its own.
    pub(crate) fn fork_child(self: &Arc<Self>) -> Arc<Self> {
        let vfs = self.vfs.for_exec(self.vfs.exe().to_vec());
        let fds = self.fds.for_fork();
        let child = Self::assemble_as(Arc::clone(self.mem.space()), Some(self.mem.layout().clone()), None, vfs, self.argv.clone(), fds, self.trace, self.backend.clone(), self.scratch, self.sys.uid());
        *child.cwd.lock() = self.cwd.lock().clone();
        child.sys.inherit(&self.sys);
        child.sys.inherit_ids(&self.sys);
        *child.comm.lock() = self.comm.lock().clone();
        child.sigtramp.store(self.sigtramp.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
        child.family.set_parent(self);
        child
    }

    /// Load `exe` (and its interpreter), map the stack and the vDSO, and build the initial stack
    /// from `argv`, `envp` and the auxiliary vector: what `run` then starts.
    fn load(self: &Arc<Self>, exe: &[u8], argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<(), String> {
        let p = self;
        let mut loader = Task::new(p.sys.pid, Arc::clone(p));
        let name = |e| format!("{}: {e:?}", String::from_utf8_lossy(&exe));
        let program = exec::load_elf(p, &loader, exe).map_err(name)?;
        let (entry, base) = match &program.interp {
            Some(interp) => {
                let i = exec::load_elf(p, &loader, interp).map_err(|e| format!("{}: {e:?}", String::from_utf8_lossy(interp)))?;
                (i.entry, i.bias)
            }
            None => (program.entry, 0),
        };
        let page = p.mm.page_size();
        let stack = p.mm.map(p, &loader, MapRequest { addr: 0, len: STACK_BYTES + page, prot: 3, flags: 0x22 | 0x20000, fd: -1, offset: 0 }).map_err(|e| format!("the main stack: {e:?}"))?;
        p.mm.protect(stack, page, 0).map_err(|e| format!("the stack guard: {e:?}"))?;
        p.mm.label(stack + page, STACK_BYTES, b"[stack]");
        // The `[vdso]` (`crate::vdso`): the kernel's time functions and its signal trampoline.
        let image = crate::vdso::filled();
        let len = (image.len() as u64).div_ceil(page) * page;
        let vdso = p.mm.map(p, &loader, MapRequest { addr: 0, len, prot: 3, flags: 0x22, fd: -1, offset: 0 }).map_err(|e| format!("the vdso: {e:?}"))?;
        p.mem.write(vdso, &image).map_err(|e| format!("the vdso: {e:?}"))?;
        p.mm.protect(vdso, len, 5).map_err(|e| format!("the vdso: {e:?}"))?;
        p.mm.label(vdso, len, b"[vdso]");
        p.sigtramp.store(vdso + crate::vdso::layout().sigreturn, std::sync::atomic::Ordering::Relaxed);
        let top = stack + STACK_BYTES + page;
        let mut random = [0u8; 16];
        omni_platform::process::random_bytes(&mut random).map_err(|e| format!("AT_RANDOM: {e}"))?;
        let auxv = [
            (AT_PHDR, program.phdr), (AT_PHENT, 56), (AT_PHNUM, program.phnum), (AT_PAGESZ, page),
            (AT_BASE, base), (AT_FLAGS, 0), (AT_ENTRY, program.entry), (AT_UID, u64::from(UID)),
            (AT_EUID, u64::from(UID)), (AT_GID, u64::from(UID)), (AT_EGID, u64::from(UID)),
            (AT_HWCAP, HWCAP), (AT_HWCAP2, 0), (AT_CLKTCK, 100), (AT_SECURE, 0), (AT_SYSINFO_EHDR, vdso),
        ];
        let (bytes, sp) = exec::build_stack(top, argv, envp, &auxv, random, exe);
        p.mem.write(top - bytes.len() as u64, &bytes).map_err(|e| format!("the initial stack: {e:?}"))?;
        loader.exit = None;
        if p.trace {
            eprintln!("[exec] stack [{stack:#x}, {top:#x}) guard [{stack:#x}, {:#x}) sp {sp:#x} entry {entry:#x}", stack + page);
        }
        *p.start.lock() = Some((entry, sp));
        Ok(())
    }

    /// Run the main task to its end; `exit_group` or its last `exit` ends the process.
    /// Run the program to its end: `exit_group`, a fatal signal or fault, or its last thread's
    /// `exit`. Every other thread is stopped and joined before this returns.
    pub fn run(self: &Arc<Self>) -> ExitStatus {
        let (pc, sp) = self.start.lock().expect("spawned");
        self.run_from(pc, |cpu| cpu.set_sp(sp as usize))
    }

    /// Run the main task from `pc`, its registers set by `setup`: `run`'s loop, for a program
    /// just loaded or a vfork child continuing from its parent's `clone`.
    pub(crate) fn run_from(self: &Arc<Self>, pc: u64, setup: impl FnOnce(&mut dyn GuestCpu)) -> ExitStatus {
        let Some(mut cpu) = self.new_cpu() else {
            let status = ExitStatus::Killed { signal: 6, pc, detail: "the CPU backend made no CPU for the main task".into() };
            *self.exit.lock() = Some(status.clone());
            return status;
        };
        // The task is reached two ways: by `on_svc`, through the context pointer, from inside the
        // JIT; and by the loop below, between runs. So no reference to it may live across
        // `cpu.run` -- a `&mut Task` held there let the optimizer keep `exit` in a register and
        // miss `exit_group` (a release-only hang that recursed bionic's exit onto the guard page).
        // It is a raw pointer, and the loop reads it with a volatile load.
        let mut main = Task::new(self.sys.pid, Arc::clone(self));
        main.sigmask = self.family.start_mask();
        let task: *mut Task = Box::into_raw(Box::new(main));
        cpu.set_svc_handler(on_svc, ThunkContext(task as usize)).expect("the syscall entry");
        setup(&mut *cpu);
        // SAFETY: `task` is live (just made); nothing else reaches it yet.
        let (pending, state) = unsafe { (Arc::clone(&(*task).pending), Arc::clone(&(*task).state)) };
        self.tasks.lock().insert(self.sys.pid, TaskHandle { halt: cpu.halt_handle(), pending, state });
        let (status, asked) = self.run_task(&mut *cpu, task, pc);
        drop(cpu);
        // SAFETY: `task` came from `Box::into_raw` above, and the only other path to it, the CPU's
        // syscall handler, was dropped with the CPU on the line before.
        let task = unsafe { Box::from_raw(task) };
        let status = self.task_finished(&task, status, asked);
        drop(task);
        // Wait for the others: they were halted if the process is ending, or they end by
        // themselves if the main thread only left with `exit`.
        let mut tasks = self.tasks.lock();
        let mut ending_since: Option<std::time::Instant> = None;
        while !tasks.is_empty() {
            self.task_ended.wait_for(&mut tasks, std::time::Duration::from_millis(200));
            if self.group_exit.lock().is_some() {
                for handle in tasks.values() {
                    handle.halt.request();
                }
                // A process killed ends whatever its threads are doing: one blocked in a wait the
                // halt cannot reach (a host-level wait in a driver) is not waited for.
                let since = *ending_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > std::time::Duration::from_secs(5) {
                    let stuck: Vec<i32> = tasks.keys().copied().collect();
                    eprintln!("[process {}] ended; tasks {stuck:?} did not stop and are abandoned", self.sys.pid);
                    break;
                }
            }
        }
        drop(tasks);
        let status = self.group_exit.lock().clone().unwrap_or(status);
        // Exit closes every descriptor, whoever still holds the process (a parent that has not
        // waited for it): a pipe's reader sees end of file once its last writer has ended.
        self.fds.close_all();
        // Replaced by `execve`: the process goes on as the new image, and ends as it does.
        let status = match self.family.successor() {
            Some(next) => next.wait_exit(),
            None => status,
        };
        *self.exit.lock() = Some(status.clone());
        self.exited.notify_all();
        status
    }

    /// Wait for this image to end (following `execve` to the image that ends).
    pub fn wait_exit(&self) -> ExitStatus {
        let mut exit = self.exit.lock();
        loop {
            if let Some(status) = exit.clone() {
                return status;
            }
            self.exited.wait(&mut exit);
        }
    }

    /// A CPU for a new task; `None` when the backend cannot make one (`clone` answers `EAGAIN`).
    fn new_cpu(&self) -> Option<Box<dyn GuestCpu>> {
        let backend = self.backend.as_ref()?;
        let config = GuestThreadConfig::guest_managed(GuestAddressSpace::of(self.mem.space()).ok()?);
        backend.create_thread(config).ok()
    }

    /// A task has ended: a thread's `exit` clears and wakes its `clear_child_tid` (what
    /// `pthread_join` waits on); anything else ends the process -- every other task is halted and
    /// every wait interrupted. Answers the status the task ended with.
    fn task_finished(&self, task: &Task, status: ExitStatus, asked: Option<Exit>) -> ExitStatus {
        match asked {
            Some(Exit::Thread(_)) => {
                if task.clear_child_tid != 0 {
                    let _ = self.mem.write_u32(task.clear_child_tid, 0);
                    let _ = self.futexes.wake(task.clear_child_tid, 1, u32::MAX);
                }
            }
            _ => self.end_group(status.clone()),
        }
        let mut tasks = self.tasks.lock();
        tasks.remove(&task.tid);
        self.task_ended.notify_all();
        status
    }

    /// Freeze every task but `me` for a fork: each is asked to stop, and this returns once none
    /// runs guest code (a task in a syscall parks as it returns).
    pub(crate) fn freeze_others(&self, me: i32) {
        *self.freeze.lock() = Some(me);
        let handles: Vec<(omni_cpu::HaltHandle, Arc<std::sync::atomic::AtomicU8>)> =
            self.tasks.lock().iter().filter(|(tid, _)| **tid != me).map(|(_, h)| (h.halt.clone(), Arc::clone(&h.state))).collect();
        for (halt, _) in &handles {
            halt.request();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while handles.iter().any(|(_, s)| s.load(std::sync::atomic::Ordering::SeqCst) == IN_GUEST) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }

    /// Let the frozen tasks run again.
    pub(crate) fn thaw(&self) {
        *self.freeze.lock() = None;
        self.thawed.notify_all();
    }

    /// Park the task while the process is frozen for another task's fork (not when it is ending).
    fn park_if_frozen(&self, tid: i32, state: &std::sync::atomic::AtomicU8) {
        let mut freeze = self.freeze.lock();
        while freeze.is_some_and(|forking| forking != tid) && self.group_exit.lock().is_none() {
            state.store(PARKED, std::sync::atomic::Ordering::SeqCst);
            self.thawed.wait_for(&mut freeze, std::time::Duration::from_millis(100));
        }
        state.store(IN_KERNEL, std::sync::atomic::Ordering::SeqCst);
    }

    /// End the process from outside, as `kill` does: every task halts and `run` returns `status`.
    pub fn end(&self, status: ExitStatus) {
        self.end_group(status);
    }

    /// End the whole process with `status` (the first ending wins).
    fn end_group(&self, status: ExitStatus) {
        {
            let mut ending = self.group_exit.lock();
            if ending.is_none() {
                *ending = Some(status);
            }
        }
        for handle in self.tasks.lock().values() {
            handle.halt.request();
        }
        self.futexes.interrupt_all();
    }

    /// `clone` with the thread flags: a new task on a new host thread, continuing after the
    /// parent's `SVC` with `x0 = 0`, `sp = stack` (or the parent's) and `TPIDR_EL0 = tls`.
    pub(crate) fn spawn_thread(self: &Arc<Self>, parent: &Task, tid: i32, stack: u64, tls: Option<u64>, clear_child_tid: u64) -> Result<(), crate::errno::Errno> {
        let (regs, parent_sp) = parent.clone_regs.ok_or(crate::errno::EINVAL)?;
        let mut cpu = self.new_cpu().ok_or(crate::errno::EAGAIN)?;
        for (n, value) in regs.iter().enumerate() {
            cpu.set_x(XReg::new(n as u8).expect("x0..x30"), *value);
        }
        cpu.set_x(XReg::new(0).expect("x0"), 0);
        cpu.set_sp(if stack == 0 { parent_sp } else { stack } as usize);
        // Without CLONE_SETTLS the child keeps its parent's thread pointer, as on Linux.
        if let Some(tls) = tls.or(parent.clone_tpidr) {
            cpu.set_tpidr_el0(tls as usize);
        }
        let mut task = Task::new(tid, Arc::clone(self));
        task.clear_child_tid = clear_child_tid;
        task.sigmask = parent.sigmask;
        let pending = Arc::clone(&task.pending);
        let state = Arc::clone(&task.state);
        let task: *mut Task = Box::into_raw(Box::new(task));
        cpu.set_svc_handler(on_svc, ThunkContext(task as usize)).expect("the syscall entry");
        self.tasks.lock().insert(tid, TaskHandle { halt: cpu.halt_handle(), pending, state });
        let pc = parent.pc + 4;
        let process = Arc::clone(self);
        let task_addr = task as usize;
        let spawned = std::thread::Builder::new().name(format!("omni-linux-{tid}")).spawn(move || {
            let task = task_addr as *mut Task;
            let (status, asked) = process.run_task(&mut *cpu, task, pc);
            drop(cpu);
            // SAFETY: from `Box::into_raw` above; the CPU that could reach it is dropped.
            let task = unsafe { Box::from_raw(task) };
            process.task_finished(&task, status, asked);
        });
        spawned.map(|_| ()).map_err(|_| {
            self.tasks.lock().remove(&tid);
            crate::errno::EAGAIN
        })
    }

    /// Post a signal with its own `siginfo` to task `tid` (a timer's): delivered with it, or handed
    /// to `rt_sigtimedwait` with it.
    pub(crate) fn post_signal_info(&self, tid: i32, info: crate::signal::SigInfo) {
        self.queued_infos.lock().push((tid, info));
        self.post_signal(tid, info.signo);
    }

    /// The `siginfo` queued for `sig` on task `tid`, if one was ([`Process::post_signal_info`]).
    /// A timer's is its timer's no longer (`crate::timer`: an expiry meanwhile is an overrun).
    pub(crate) fn take_info(&self, tid: i32, sig: i32) -> Option<crate::signal::SigInfo> {
        let info = {
            let mut q = self.queued_infos.lock();
            let at = q.iter().position(|(t, i)| *t == tid && i.signo == sig)?;
            q.remove(at).1
        };
        if info.code == crate::signal::SI_TIMER {
            return Some(self.timers.dequeued(info));
        }
        Some(info)
    }

    /// Post `sig` to task `tid`: pending there, its run stopped so the loop delivers it, and its
    /// futex wait (if any) ended with `EINTR`.
    pub(crate) fn post_signal(&self, tid: i32, sig: i32) {
        if let Some(handle) = self.tasks.lock().get(&tid) {
            handle.pending.fetch_or(1 << (sig - 1), std::sync::atomic::Ordering::SeqCst);
            handle.halt.request();
        }
        self.futexes.interrupt(tid);
    }

    /// The next thread id.
    pub(crate) fn allocate_tid(&self) -> i32 {
        self.next_tid.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// The tids of every live task, lowest first.
    #[must_use]
    pub fn tids(&self) -> Vec<i32> {
        let mut tids: Vec<i32> = self.tasks.lock().keys().copied().collect();
        if tids.is_empty() {
            tids.push(self.sys.pid); // a process that has not started runs as its main task
        }
        tids.sort_unstable();
        tids
    }

    fn run_task(&self, cpu: &mut dyn GuestCpu, task: *mut Task, mut pc: u64) -> (ExitStatus, Option<Exit>) {
        // SAFETY: `task` is live for the whole loop (see `run`).
        let state = unsafe { Arc::clone(&(*task).state) };
        // `OMNI_THREAD_CPU`: this host thread's time, sampled for as long as it runs the task.
        struct Profiled(i32);
        impl Drop for Profiled {
            fn drop(&mut self) {
                crate::cpuprof::stopped(self.0);
            }
        }
        // SAFETY: as above.
        let _profiled = unsafe {
            crate::cpuprof::started((*task).tid, &(*task).name, &state);
            Profiled((*task).tid)
        };
        loop {
            state.store(IN_GUEST, std::sync::atomic::Ordering::SeqCst);
            let ran = cpu.run(pc as usize, RunLimit::Unlimited);
            state.store(IN_KERNEL, std::sync::atomic::Ordering::SeqCst);
            let exit = match ran {
                Ok(e) => e,
                Err(e) => {
                    let detail = format!("the CPU backend: {e}\n{}", self.registers(cpu));
                    return (ExitStatus::Killed { signal: 6, pc: cpu.pc() as u64, detail }, None);
                }
            };
            // SAFETY: `task` is live for the whole loop (see `run`), and no reference to it is held
            // here; the volatile read is what makes `on_svc`'s write inside `cpu.run` visible.
            let asked = unsafe { std::ptr::read_volatile(std::ptr::addr_of!((*task).exit)) };
            match (exit, asked) {
                (ExitReason::UnsupportedInstruction { .. }, Some(Exit::Group(code) | Exit::Thread(code))) => {
                    return (ExitStatus::Exited(code & 0xff), asked);
                }
                (ExitReason::UnsupportedInstruction { pc: at, .. }, Some(Exit::Signal(signal))) => {
                    let detail = format!("the default action of signal {signal}\n{}", self.registers(cpu));
                    return (ExitStatus::Killed { signal, pc: at as u64, detail }, asked);
                }
                (ExitReason::UnsupportedInstruction { pc: at, encoding }, None) if encoding & 0xFFE0_001F == 0xD400_0001 => {
                    // A syscall deferred for signals: `rt_sigreturn`, or one left deliverable.
                    // SAFETY: as for `asked` above.
                    let returning = unsafe { std::mem::replace(&mut (*task).sigreturn, false) };
                    pc = if returning { self.sigreturn(cpu, task) } else { at as u64 + 4 };
                    match self.deliver_pending(cpu, task, pc) {
                        Ok(next) => pc = next,
                        Err(killed) => return (killed, None),
                    }
                }
                (ExitReason::Halted { pc: at }, _) => {
                    // Another task ended the process, or posted a signal to this one.
                    if let Some(ending) = self.group_exit.lock().clone() {
                        return (ending, Some(Exit::Group(0)));
                    }
                    cpu.halt_handle().clear();
                    // SAFETY: as for `asked` above.
                    self.park_if_frozen(unsafe { (*task).tid }, &state);
                    match self.deliver_pending(cpu, task, at as u64) {
                        Ok(next) => pc = next,
                        Err(killed) => return (killed, None),
                    }
                }
                (ExitReason::MemoryFault { pc: at, address, access }, _) => {
                    let address = crate::guest::untag(address as u64);
                    let mapped = self.mem.space().region_at(address as usize).is_some_and(|r| r.mapping.is_some());
                    let code = if mapped { crate::signal::SEGV_ACCERR } else { crate::signal::SEGV_MAPERR };
                    let info = crate::signal::SigInfo { signo: 11, code, addr: address, pid: 0, uid: 0, ..crate::signal::SigInfo::default() };
                    let what = format!("{access:?} at {address:#x}");
                    match self.fault(cpu, task, info, at as u64, address, &what) {
                        Ok(next) => pc = next,
                        Err(killed) => return (killed, None),
                    }
                }
                (ExitReason::UnsupportedInstruction { pc: at, encoding }, None) => {
                    // `brk` (which dynarmic reports as unsupported) is SIGTRAP with TRAP_BRKPT; any
                    // other undefined instruction is SIGILL, as the kernel's undef handler raises it.
                    let info = if encoding & 0xFFE0_001F == 0xD420_0000 {
                        crate::signal::SigInfo { signo: 5, code: crate::signal::TRAP_BRKPT, addr: at as u64, pid: 0, uid: 0, ..crate::signal::SigInfo::default() }
                    } else {
                        crate::signal::SigInfo { signo: 4, code: crate::signal::ILL_ILLOPC, addr: at as u64, pid: 0, uid: 0, ..crate::signal::SigInfo::default() }
                    };
                    let what = format!("the guest executed an unsupported instruction {encoding:#010x} at {at:#x}");
                    match self.fault(cpu, task, info, at as u64, 0, &what) {
                        Ok(next) => pc = next,
                        Err(killed) => return (killed, None),
                    }
                }
                (ExitReason::Breakpoint { pc: at }, _) => {
                    // `brk`: SIGTRAP with TRAP_BRKPT; the frame's pc is the `brk` itself.
                    let info = crate::signal::SigInfo { signo: 5, code: crate::signal::TRAP_BRKPT, addr: at as u64, pid: 0, uid: 0, ..crate::signal::SigInfo::default() };
                    match self.fault(cpu, task, info, at as u64, 0, &format!("breakpoint at {at:#x}")) {
                        Ok(next) => pc = next,
                        Err(killed) => return (killed, None),
                    }
                }
                (other, _) => {
                    let detail = format!("{other}\n{}", self.registers(cpu));
                    return (ExitStatus::Killed { signal: 4, pc: other.pc() as u64, detail }, None);
                }
            }
        }
    }

    /// A synchronous signal the task's own instruction raised. As the kernel's `force_sig_fault`:
    /// if the task blocks it or has no handler for it (default or ignored), it cannot be deferred
    /// or dropped -- re-running the instruction would fault again -- so the process is killed.
    fn fault(&self, cpu: &mut dyn GuestCpu, task: *mut Task, info: crate::signal::SigInfo, pc: u64, fault_address: u64, what: &str) -> Result<u64, ExitStatus> {
        let sig = info.signo;
        // SAFETY: as in `deliver_pending`.
        let blocked = unsafe { (*task).sigmask } & (1 << (sig - 1)) != 0;
        if blocked || self.sys.action(sig).0 <= 1 {
            let detail = format!("{what}\n{}", self.registers(cpu));
            return Err(ExitStatus::Killed { signal: sig, pc, detail });
        }
        self.deliver(cpu, task, info, pc, fault_address)
    }

    /// The task's registers now, with `pc` where it resumes.
    fn read_regs(cpu: &dyn GuestCpu, pc: u64) -> crate::signal::Regs {
        let mut regs = crate::signal::Regs { sp: cpu.sp() as u64, pc, pstate: cpu.nzcv().to_pstate(), ..Default::default() };
        for (n, x) in regs.x.iter_mut().enumerate() {
            *x = cpu.x(XReg::new(n as u8).expect("x0..x30"));
        }
        for (n, v) in regs.v.iter_mut().enumerate() {
            *v = cpu.v(omni_cpu::VReg::new(n as u8).expect("v0..v31"));
        }
        regs
    }

    /// Deliver the lowest-numbered pending signal the task does not block, if any; answers where
    /// the task resumes.
    fn deliver_pending(&self, cpu: &mut dyn GuestCpu, task: *mut Task, pc: u64) -> Result<u64, ExitStatus> {
        // SAFETY: `task` is live for the loop and no reference to it is held across `cpu.run`.
        let (pending, mask) = unsafe { (Arc::clone(&(*task).pending), (*task).sigmask) };
        let ready = pending.load(std::sync::atomic::Ordering::SeqCst) & !mask;
        if ready == 0 {
            return Ok(pc);
        }
        let sig = ready.trailing_zeros() as i32 + 1;
        pending.fetch_and(!(1u64 << (sig - 1)), std::sync::atomic::Ordering::SeqCst);
        // SAFETY: as above.
        let queued = unsafe { (*task).queued_info.take() }.filter(|i| i.signo == sig);
        // SAFETY: as above.
        let tid = unsafe { (*task).tid };
        let queued = queued.or_else(|| self.take_info(tid, sig));
        let info = queued.unwrap_or(crate::signal::SigInfo { signo: sig, code: crate::signal::SI_TKILL, pid: self.sys.pid, uid: self.sys.uid(), ..crate::signal::SigInfo::default() });
        self.deliver(cpu, task, info, pc, 0)
    }

    /// **Diagnostic** (`OMNI_DUMP_ON_SEGV=<name part>`, with `OMNI_DUMP_DIR`; off by default): the
    /// first `SIGSEGV` whose pc or lr is in a mapping so named writes every mapping of that name, as
    /// memory holds it now (a packed library's decrypted code and its data), and the registers.
    fn dump_on_segv(&self, pc: u64, lr: u64, x: &[u64], fault_address: u64) {
        static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let (Ok(want), Ok(dir)) = (std::env::var("OMNI_DUMP_ON_SEGV"), std::env::var("OMNI_DUMP_DIR")) else { return };
        let named = |a: u64| self.mm.name_at(a).is_some_and(|(n, _)| n.windows(want.len()).any(|w| w == want.as_bytes()));
        if !(named(pc) || named(lr)) || DONE.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let pid = self.sys.pid;
        let mut index = format!("pid {pid} pc {pc:#x} lr {lr:#x} fault {fault_address:#x}\n");
        for (n, v) in x.iter().enumerate() {
            index += &format!("x{n} {v:#x} {}\n", self.mm.describe(*v).unwrap_or_default());
        }
        for (start, len, name, offset) in self.mm.file_mappings() {
            if !name.windows(want.len()).any(|w| w == want.as_bytes()) {
                continue;
            }
            // 4 KiB at a time: a PROT_NONE or unmapped piece reads as zeros rather than failing all.
            let mut bytes = vec![0u8; len as usize];
            for at in (0..len).step_by(4096) {
                let n = 4096.min(len - at) as usize;
                if let Ok(b) = self.mem.read(start + at, n) {
                    bytes[at as usize..at as usize + n].copy_from_slice(&b);
                }
            }
            let file = dir.join(format!("{pid}-{start:x}-off{offset:x}.bin"));
            let _ = std::fs::write(&file, &bytes);
            index += &format!("{start:#x}+{len:#x} off {offset:#x} {} -> {}\n", String::from_utf8_lossy(&name), file.display());
        }
        let _ = std::fs::write(dir.join(format!("{pid}-index.txt")), index);
        eprintln!("[dump] {pid}: {} written", dir.display());
    }

    /// Run the guest's handler for `info.signo`: build the kernel's frame below `sp` (or on the
    /// alternate stack), point the task at the handler, and block what the action asks.
    fn deliver(&self, cpu: &mut dyn GuestCpu, task: *mut Task, info: crate::signal::SigInfo, pc: u64, fault_address: u64) -> Result<u64, ExitStatus> {
        const SA_ONSTACK: u64 = 0x0800_0000;
        const SA_RESTORER: u64 = 0x0400_0000;
        const SA_NODEFER: u64 = 0x4000_0000;
        const SA_RESETHAND: u64 = 0x8000_0000;
        let sig = info.signo;
        let (handler, flags, restorer, sa_mask) = self.sys.action(sig);
        let killed = |detail: String| ExitStatus::Killed { signal: sig, pc, detail };
        match handler {
            0 if (17..=28).contains(&sig) && matches!(sig, 17 | 18 | 23 | 28) => return Ok(pc),
            0 => return Err(killed(format!("the default action of signal {sig}\n{}", self.registers(cpu)))),
            1 => return Ok(pc),
            _ => {}
        }
        let mut regs = Self::read_regs(cpu, pc);
        regs.fault_address = fault_address;
        if sig == 11 {
            self.dump_on_segv(pc, regs.x[30], &regs.x, fault_address);
        }
        if self.trace || signal_trace() {
            let at = |a: u64| self.mm.describe(a).map_or_else(String::new, |d| format!(" ({d})"));
            eprintln!(
                "[deliver] signal {sig} code {} addr {:#x} handler {handler:#x} flags {flags:#x} mask {sa_mask:#x} pc {pc:#x}{} lr {:#x}{} sp {:#x}",
                info.code, info.addr, at(pc), regs.x[30], at(regs.x[30]), regs.sp
            );
            if info.code > 0 {
                // A fault: the registers too, each labelled when it points into a mapping.
                for (n, x) in regs.x.iter().enumerate() {
                    eprintln!("  x{n:<2} {x:#018x}{}", at(*x));
                }
                // The code around the fault, as it is in memory now (a packed library's is not
                // what its file holds): 24 instructions before the pc, 8 from it.
                if let Ok(code) = self.mem.read(pc.wrapping_sub(96) & !3, 128) {
                    let words: Vec<String> = code.chunks_exact(4).map(|w| format!("{:08x}", u32::from_le_bytes(w.try_into().expect("4")))).collect();
                    eprintln!("  code at {:#x}: {}", pc.wrapping_sub(96) & !3, words.join(" "));
                }
                // The frame-pointer chain: AOSP builds arm64 with frame pointers.
                let mut fp = regs.x[29];
                for depth in 0..64 {
                    let (Ok(next), Ok(lr)) = (self.mem.read_u64(fp), self.mem.read_u64(fp + 8)) else { break };
                    if lr == 0 {
                        break;
                    }
                    eprintln!("  #{depth:02} {lr:#x}{}", at(crate::guest::untag(lr)));
                    if next <= fp {
                        break;
                    }
                    fp = next;
                }
            }
        }
        // SAFETY: as in `deliver_pending`.
        let (mask, altstack) = unsafe { ((*task).saved_sigmask.take().unwrap_or((*task).sigmask), (*task).altstack) };
        let at = crate::signal::placement(regs.sp, altstack, flags & SA_ONSTACK != 0);
        let frame = crate::signal::Frame::build(&regs, &info, mask, altstack);
        if self.mem.write(at, &frame).is_err() {
            return Err(killed(format!("signal {sig}: no room for its frame at {at:#x}\n{}", self.registers(cpu))));
        }
        let x = |n: u8| XReg::new(n).expect("a general-purpose register");
        cpu.set_x(x(0), sig as u64);
        cpu.set_x(x(1), at);
        cpu.set_x(x(2), at + crate::signal::UCONTEXT_OFFSET as u64);
        cpu.set_x(x(29), at + crate::signal::RECORD_OFFSET as u64);
        let sigtramp = self.sigtramp.load(std::sync::atomic::Ordering::Relaxed);
        cpu.set_x(x(30), if flags & SA_RESTORER != 0 { restorer } else { sigtramp });
        cpu.set_sp(at as usize);
        let block = sa_mask | if flags & SA_NODEFER == 0 { 1 << (sig - 1) } else { 0 };
        // SAFETY: as in `deliver_pending`.
        unsafe { (*task).sigmask = mask | (block & !((1 << 8) | (1 << 18))) };
        if flags & SA_RESETHAND != 0 {
            self.sys.reset_action(sig);
        }
        Ok(handler)
    }

    /// `rt_sigreturn`: every register, the flags, the vector registers and the mask back from the
    /// frame at `sp`; answers where the task resumes.
    fn sigreturn(&self, cpu: &mut dyn GuestCpu, task: *mut Task) -> u64 {
        let sp = cpu.sp() as u64;
        let Ok(bytes) = self.mem.read(sp, crate::signal::FRAME_BYTES) else {
            self.refusals.record(format!("rt_sigreturn: no frame at sp {sp:#x}"), 0, 0);
            return cpu.pc() as u64;
        };
        let (regs, mask) = crate::signal::Frame::parse(&bytes);
        for (n, v) in regs.x.iter().enumerate() {
            cpu.set_x(XReg::new(n as u8).expect("x0..x30"), *v);
        }
        for (n, v) in regs.v.iter().enumerate() {
            cpu.set_v(omni_cpu::VReg::new(n as u8).expect("v0..v31"), *v);
        }
        cpu.set_sp(regs.sp as usize);
        cpu.set_nzcv(omni_cpu::Nzcv::from_pstate(regs.pstate));
        // SAFETY: as in `deliver_pending`.
        unsafe { (*task).sigmask = mask & !((1 << 8) | (1 << 18)) };
        regs.pc
    }

    /// `pc`, `lr` and `sp` labelled from the file mappings, then `x0`..`x28`: a fault's evidence.
    fn registers(&self, cpu: &dyn GuestCpu) -> String {
        let label = |a: u64| self.mm.describe(a).map_or_else(String::new, |d| format!(" ({d})"));
        let x = |n: u8| cpu.x(XReg::new(n).expect("a general-purpose register"));
        let mut out = format!(
            "  pc {:#x}{}
  lr {:#x}{}
  sp {:#x}
",
            cpu.pc(), label(cpu.pc() as u64), x(30), label(x(30)), cpu.sp()
        );
        for n in 0..29u8 {
            out += &format!("  x{n:<2} {:#018x}{}", x(n), if n % 4 == 3 { "
" } else { "" });
        }
        out + "
"
    }

    pub fn report(&self) -> String {
        let status = self.exit.lock().clone();
        let refused = self.refusals.report();
        format!(
            "omni-linux: {}\n{}",
            status.map_or_else(|| "not run".to_string(), |s| format!("{s:?}")),
            if refused.is_empty() { "  nothing refused\n".to_string() } else { format!("refused:\n{refused}") }
        )
    }

    /// A process with no program, for handler tests.
    pub fn for_tests(vfs: Vfs, stdout: Output) -> Arc<Self> {
        let space = Arc::new(small_space());
        let scratch = space
            .map_anonymous(omni_mem::Placement::Anywhere { align: space.page_size() }, 1 << 20, omni_mem::Protection::ReadWrite, omni_mem::CommitPolicy::Lazy)
            .expect("scratch") as u64;
        let argv = vec![vfs.exe().to_vec()];
        Self::assemble(space, vfs, argv, stdout.clone(), stdout, false, None, scratch, UID)
    }

    pub fn scratch(&self) -> u64 {
        self.scratch
    }

    pub fn test_task(self: &Arc<Self>) -> Task {
        Task::new(self.sys.pid, Arc::clone(self))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.trace {
            eprintln!("[process] {} dropped", self.sys.pid);
        }
        // An image `execve` replaced leaves its pid to the image that replaced it; a stand-in's
        // pid is its own host process's.
        if !self.family.superseded() && !self.family.is_stand_in() {
            LIVE_PIDS.lock().remove(&self.sys.pid);
            crate::locks::process_ended(self.sys.pid);
        }
    }
}

/// `OMNI_SPLIT_REPORT=<seconds>`: every that many seconds, each process of this host process whose
/// 4 KiB overlay has served accesses says how many (per second since the last line), and which
/// host pages, by the mapping they belong to (D42). Off by default.
fn split_report_start() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let Some(secs) = std::env::var("OMNI_SPLIT_REPORT").ok().and_then(|v| v.parse::<u64>().ok()).filter(|&s| s > 0) else { return };
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new().name("split-report".into()).spawn(move || {
            let mut last: std::collections::HashMap<i32, u64> = std::collections::HashMap::new();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                let live: Vec<Arc<Process>> = ALL.lock().iter().filter_map(std::sync::Weak::upgrade).collect();
                for p in live {
                    let st = p.mem.space().split_stats();
                    if st.served_total == 0 && st.tracked == 0 {
                        continue;
                    }
                    let before = last.insert(p.sys.pid, st.served_total).unwrap_or(0);
                    let rate = st.served_total.saturating_sub(before) / secs;
                    let top: Vec<String> = st
                        .top
                        .iter()
                        .take(4)
                        .map(|(page, n)| format!("{page:#x}x{n} {}", p.mm.describe(*page as u64).unwrap_or_default()))
                        .collect();
                    eprintln!("[split] pid {} tracked {} trapping {} served {} ({rate}/s) top: {}", p.sys.pid, st.tracked, st.trapping, st.served_total, top.join(", "));
                }
            }
        });
    });
}
