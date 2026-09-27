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
    backend: Option<DynarmicBackend>,
    pub(crate) start: Mutex<Option<(u64, u64)>>, // (pc, sp) of the main task
    exit: Mutex<Option<ExitStatus>>,
    scratch: u64,
}

/// What other tasks reach of a task: the handle that stops its run and its pending signals.
struct TaskHandle {
    halt: omni_cpu::HaltHandle,
    pending: Arc<std::sync::atomic::AtomicU64>,
}

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
/// and a guest address is a host address (D4). On Windows 2 GiB for now: `KUSER_SHARED_DATA` sits
/// at `0x7FFE0000` in every process, so a space from lower down must step around it.
#[cfg(windows)]
const GUEST_SPACE_LOW_BASE: usize = 0x8000_0000;
#[cfg(not(windows))]
const GUEST_SPACE_LOW_BASE: usize = 0x1000_0000;

/// The guest space, at [`GUEST_SPACE_LOW_BASE`] if the host has that free, else wherever it
/// chooses (a program that needs no low memory still runs; ART will not).
fn reserve_space() -> Result<GuestSpace, omni_mem::MemError> {
    let config = |base| GuestSpaceConfig { base, size: GUEST_SPACE_BYTES, ..GuestSpaceConfig::default() };
    GuestSpace::with_config(config(Some(GUEST_SPACE_LOW_BASE))).or_else(|e| {
        tracing::warn!(%e, "no guest space below 4 GiB; reserving where the host chooses");
        GuestSpace::with_config(config(None))
    })
}
const STACK_BYTES: u64 = 8 << 20;
const PID: i32 = 1000;
const UID: u32 = 10000;

fn altstack_disabled() -> [u8; 24] {
    let mut s = [0u8; 24];
    s[8..12].copy_from_slice(&2i32.to_le_bytes()); // SS_DISABLE
    s
}

impl Task {
    #[must_use]
    pub fn new(tid: i32, process: Arc<Process>) -> Self {
        Self { tid, process, pc: 0, lr: 0, clear_child_tid: 0, sigmask: 0, altstack: altstack_disabled(), name: Vec::new(), exit: None, clone_regs: None, pending: Arc::default(), sigreturn: false }
    }
}

/// The in-loop syscall entry (`GuestCpu::set_svc_handler`): `ThunkContext` is the task's address.
fn on_svc(call: &mut ThunkCall<'_>) {
    // SAFETY: the context is `&mut Task` of the task this CPU runs, set by `run_task`, which owns
    // the task for exactly as long as the CPU can call this.
    let task = unsafe { &mut *(call.context().0 as *mut Task) };
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
    }
    let process = Arc::clone(&task.process);
    let result = process.syscall(task, number, args);
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
    let deliverable = task.pending.load(std::sync::atomic::Ordering::SeqCst) & !task.sigmask != 0;
    if task.exit.is_some() || task.sigreturn || deliverable {
        call.defer_to_caller();
    }
}

impl Process {
    pub fn syscall(&self, task: &mut Task, number: u64, args: [u64; 6]) -> u64 {
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
    fn assemble(space: Arc<GuestSpace>, vfs: Vfs, argv: Vec<Vec<u8>>, stdout: Output, stderr: Output, trace: bool, backend: Option<DynarmicBackend>, scratch: u64) -> Arc<Self> {
        let mut table = Table::new();
        crate::install_all(&mut table);
        let (properties, dropped) = crate::props::Properties::from_sysroot(vfs.sysroot());
        let props = crate::procfs::PropFiles {
            info: crate::props::property_info_bytes(),
            serial: crate::props::serial_area_bytes(),
            area: properties.area_bytes(),
            apex_info: crate::apex::apex_info_list(vfs.sysroot()),
        };
        let comm = argv.first().map_or_else(Vec::new, |a| {
            a.rsplit(|&b| b == b'/').next().unwrap_or(a).iter().copied().take(15).collect()
        });
        let layout = crate::guest::Layout::default();
        let p = Arc::new(Self {
            mem: GuestMem::new(Arc::clone(&space), Arc::clone(&layout)),
            table,
            refusals: Refusals::default(),
            vfs,
            fds: FdTable::standard(stdout, stderr),
            cwd: Mutex::new(b"/".to_vec()),
            mm: Mm::new(space, layout),
            sys: SysState::new(PID, UID),
            futexes: crate::futex::Futexes::default(),
            tasks: Mutex::new(std::collections::HashMap::new()),
            task_ended: parking_lot::Condvar::new(),
            next_tid: std::sync::atomic::AtomicI32::new(PID + 1),
            group_exit: Mutex::new(None),
            trace,
            argv,
            comm: Mutex::new(comm),
            props,
            sigtramp: std::sync::atomic::AtomicU64::new(0),
            backend,
            start: Mutex::new(None),
            exit: Mutex::new(None),
            scratch,
        });
        // `/proc` and `/sys` are generated from the process itself (`procfs`).
        let proc: Arc<dyn crate::procfs::ProcFs> = Arc::clone(&p) as Arc<dyn crate::procfs::ProcFs>;
        p.vfs.attach_proc(Arc::downgrade(&proc));
        for name in dropped {
            p.refusals.record(format!("property dropped (too long for its kind): {name}"), 0, 0);
        }
        p
    }

    pub fn spawn(config: SpawnConfig) -> Result<Arc<Self>, String> {
        let sysroot = Sysroot::open(&config.sysroot)?;
        let exe = config.argv.first().ok_or("no program: argv is empty")?.clone();
        // The instance's writable state: what a device keeps on its data partition and tmpfs, and
        // `/linkerconfig`, which `linkerconfig` writes at boot for `linker64` to read (sub-project B).
        let writable_dirs = ["data", "tmp", "linkerconfig"];
        let mut writable = Vec::new();
        for dir in writable_dirs {
            let host = config.instance_dir.join(dir);
            std::fs::create_dir_all(&host).map_err(|e| format!("{}: {e}", host.display()))?;
            writable.push((format!("/{dir}").into_bytes(), host));
        }
        let vfs = Vfs::new(sysroot, writable, exe.clone());
        let space = Arc::new(reserve_space().map_err(|e| format!("reserve the guest address space: {e}"))?);
        // Top Byte Ignore: arm64 Linux gives user space TBI, and Android's heap depends on it.
        // 128 guest threads: ART alone starts about twenty, and Roblox runs dozens.
        let mut options = DynarmicOptions { top_byte_ignore: true, max_threads: 128, ..DynarmicOptions::default() };
        // OMNI_DYNARMIC_OPT=<hex mask>: run with only these (safe) JIT optimizations -- 0 for none,
        // to tell a translation fault from a kernel one.
        if let Some(mask) = std::env::var("OMNI_DYNARMIC_OPT").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
            options.optimizations_override = Some(mask);
        }
        let backend = DynarmicBackend::new(Arc::clone(&space), options).map_err(|e| format!("the CPU backend: {e}"))?;
        let p = Self::assemble(space, vfs, config.argv.clone(), config.stdout, config.stderr, config.trace, Some(backend), 0);
        let mut loader = Task::new(PID, Arc::clone(&p));
        let name = |e| format!("{}: {e:?}", String::from_utf8_lossy(&exe));
        let program = exec::load_elf(&p, &loader, &exe).map_err(name)?;
        let (entry, base) = match &program.interp {
            Some(interp) => {
                let i = exec::load_elf(&p, &loader, interp).map_err(|e| format!("{}: {e:?}", String::from_utf8_lossy(interp)))?;
                (i.entry, i.bias)
            }
            None => (program.entry, 0),
        };
        let page = p.mm.page_size();
        let stack = p.mm.map(&p, &loader, MapRequest { addr: 0, len: STACK_BYTES + page, prot: 3, flags: 0x22 | 0x20000, fd: -1, offset: 0 }).map_err(|e| format!("the main stack: {e:?}"))?;
        p.mm.protect(stack, page, 0).map_err(|e| format!("the stack guard: {e:?}"))?;
        p.mm.label(stack + page, STACK_BYTES, b"[stack]");
        // A one-page `[vdso]` holding the kernel's signal trampoline: `mov x8, #139; svc #0`.
        let vdso = p.mm.map(&p, &loader, MapRequest { addr: 0, len: page, prot: 3, flags: 0x22, fd: -1, offset: 0 }).map_err(|e| format!("the vdso page: {e:?}"))?;
        let trampoline: Vec<u8> = [0xD280_1168u32, 0xD400_0001].iter().flat_map(|w| w.to_le_bytes()).collect();
        p.mem.write(vdso, &trampoline).map_err(|e| format!("the vdso page: {e:?}"))?;
        p.mm.protect(vdso, page, 5).map_err(|e| format!("the vdso page: {e:?}"))?;
        p.mm.label(vdso, page, b"[vdso]");
        p.sigtramp.store(vdso, std::sync::atomic::Ordering::Relaxed);
        let top = stack + STACK_BYTES + page;
        let mut random = [0u8; 16];
        omni_platform::process::random_bytes(&mut random).map_err(|e| format!("AT_RANDOM: {e}"))?;
        let auxv = [
            (AT_PHDR, program.phdr), (AT_PHENT, 56), (AT_PHNUM, program.phnum), (AT_PAGESZ, page),
            (AT_BASE, base), (AT_FLAGS, 0), (AT_ENTRY, program.entry), (AT_UID, u64::from(UID)),
            (AT_EUID, u64::from(UID)), (AT_GID, u64::from(UID)), (AT_EGID, u64::from(UID)),
            (AT_HWCAP, HWCAP), (AT_HWCAP2, 0), (AT_CLKTCK, 100), (AT_SECURE, 0),
        ];
        let (bytes, sp) = exec::build_stack(top, &config.argv, &config.envp, &auxv, random, &exe);
        p.mem.write(top - bytes.len() as u64, &bytes).map_err(|e| format!("the initial stack: {e:?}"))?;
        loader.exit = None;
        if p.trace {
            eprintln!("[exec] stack [{stack:#x}, {top:#x}) guard [{stack:#x}, {:#x}) sp {sp:#x} entry {entry:#x}", stack + page);
        }
        *p.start.lock() = Some((entry, sp));
        Ok(p)
    }

    /// Run the main task to its end; `exit_group` or its last `exit` ends the process.
    /// Run the program to its end: `exit_group`, a fatal signal or fault, or its last thread's
    /// `exit`. Every other thread is stopped and joined before this returns.
    pub fn run(self: &Arc<Self>) -> ExitStatus {
        let (pc, sp) = self.start.lock().expect("spawned");
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
        let task: *mut Task = Box::into_raw(Box::new(Task::new(PID, Arc::clone(self))));
        cpu.set_svc_handler(on_svc, ThunkContext(task as usize)).expect("the syscall entry");
        cpu.set_sp(sp as usize);
        // SAFETY: `task` is live (just made); nothing else reaches it yet.
        let pending = Arc::clone(unsafe { &(*task).pending });
        self.tasks.lock().insert(PID, TaskHandle { halt: cpu.halt_handle(), pending });
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
        while !tasks.is_empty() {
            self.task_ended.wait_for(&mut tasks, std::time::Duration::from_millis(200));
            if let Some(ending) = self.group_exit.lock().clone() {
                for handle in tasks.values() {
                    handle.halt.request();
                }
                let _ = ending;
            }
        }
        drop(tasks);
        let status = self.group_exit.lock().clone().unwrap_or(status);
        *self.exit.lock() = Some(status.clone());
        status
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
        if let Some(tls) = tls {
            cpu.set_tpidr_el0(tls as usize);
        }
        let mut task = Task::new(tid, Arc::clone(self));
        task.clear_child_tid = clear_child_tid;
        task.sigmask = parent.sigmask;
        let pending = Arc::clone(&task.pending);
        let task: *mut Task = Box::into_raw(Box::new(task));
        cpu.set_svc_handler(on_svc, ThunkContext(task as usize)).expect("the syscall entry");
        self.tasks.lock().insert(tid, TaskHandle { halt: cpu.halt_handle(), pending });
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
            tids.push(PID); // a process that has not started runs as its main task
        }
        tids.sort_unstable();
        tids
    }

    fn run_task(&self, cpu: &mut dyn GuestCpu, task: *mut Task, mut pc: u64) -> (ExitStatus, Option<Exit>) {
        loop {
            let exit = match cpu.run(pc as usize, RunLimit::Unlimited) {
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
                    match self.deliver_pending(cpu, task, at as u64) {
                        Ok(next) => pc = next,
                        Err(killed) => return (killed, None),
                    }
                }
                (ExitReason::MemoryFault { pc: at, address, access }, _) => {
                    let address = crate::guest::untag(address as u64);
                    let mapped = self.mem.space().region_at(address as usize).is_some_and(|r| r.mapping.is_some());
                    let code = if mapped { crate::signal::SEGV_ACCERR } else { crate::signal::SEGV_MAPERR };
                    let info = crate::signal::SigInfo { signo: 11, code, addr: address, pid: 0, uid: 0 };
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
                        crate::signal::SigInfo { signo: 5, code: crate::signal::TRAP_BRKPT, addr: at as u64, pid: 0, uid: 0 }
                    } else {
                        crate::signal::SigInfo { signo: 4, code: crate::signal::ILL_ILLOPC, addr: at as u64, pid: 0, uid: 0 }
                    };
                    let what = format!("the guest executed an unsupported instruction {encoding:#010x} at {at:#x}");
                    match self.fault(cpu, task, info, at as u64, 0, &what) {
                        Ok(next) => pc = next,
                        Err(killed) => return (killed, None),
                    }
                }
                (ExitReason::Breakpoint { pc: at }, _) => {
                    // `brk`: SIGTRAP with TRAP_BRKPT; the frame's pc is the `brk` itself.
                    let info = crate::signal::SigInfo { signo: 5, code: crate::signal::TRAP_BRKPT, addr: at as u64, pid: 0, uid: 0 };
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
        let info = crate::signal::SigInfo { signo: sig, code: crate::signal::SI_TKILL, addr: 0, pid: self.sys.pid, uid: self.sys.uid };
        self.deliver(cpu, task, info, pc, 0)
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
        let (mask, altstack) = unsafe { ((*task).sigmask, (*task).altstack) };
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
        let space = Arc::new(GuestSpace::new().expect("a guest space"));
        let scratch = space
            .map_anonymous(omni_mem::Placement::Anywhere { align: space.page_size() }, 1 << 20, omni_mem::Protection::ReadWrite, omni_mem::CommitPolicy::Lazy)
            .expect("scratch") as u64;
        let argv = vec![vfs.exe().to_vec()];
        Self::assemble(space, vfs, argv, stdout.clone(), stdout, false, None, scratch)
    }

    pub fn scratch(&self) -> u64 {
        self.scratch
    }

    pub fn test_task(self: &Arc<Self>) -> Task {
        Task::new(PID, Arc::clone(self))
    }
}
