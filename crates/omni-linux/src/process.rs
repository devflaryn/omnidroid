//! A guest process: its address space, descriptors, and tasks, and the loop that runs a task.
use std::path::PathBuf;
use std::sync::Arc;

use omni_cpu::dynarmic::{DynarmicBackend, DynarmicOptions};
use omni_cpu::{ExitReason, GuestAddressSpace, GuestCpu, GuestCpuBackend, GuestThreadConfig, RunLimit, ThunkCall, ThunkContext};
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
    pub trace: bool,
    backend: Option<DynarmicBackend>,
    start: Mutex<Option<(u64, u64)>>, // (pc, sp) of the main task
    exit: Mutex<Option<ExitStatus>>,
    scratch: u64,
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
}

const GUEST_SPACE_BYTES: usize = 64 << 30;
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
        Self { tid, process, pc: 0, lr: 0, clear_child_tid: 0, sigmask: 0, altstack: altstack_disabled(), name: Vec::new(), exit: None }
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
    let process = Arc::clone(&task.process);
    let result = process.syscall(task, number, args);
    if process.trace {
        eprintln!("[{}] {}({:#x}, {:#x}, {:#x}, {:#x}) = {:#x}", task.tid, name_of(number), args[0], args[1], args[2], args[3], result);
    }
    call.set_x(0, result);
    if task.exit.is_some() {
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

    fn assemble(space: Arc<GuestSpace>, vfs: Vfs, stdout: Output, stderr: Output, trace: bool, backend: Option<DynarmicBackend>, scratch: u64) -> Arc<Self> {
        let mut table = Table::new();
        crate::install_all(&mut table);
        Arc::new(Self {
            mem: GuestMem::new(Arc::clone(&space)),
            table,
            refusals: Refusals::default(),
            vfs,
            fds: FdTable::standard(stdout, stderr),
            cwd: Mutex::new(b"/".to_vec()),
            mm: Mm::new(space),
            sys: SysState::new(PID, UID),
            trace,
            backend,
            start: Mutex::new(None),
            exit: Mutex::new(None),
            scratch,
        })
    }

    pub fn spawn(config: SpawnConfig) -> Result<Arc<Self>, String> {
        let sysroot = Sysroot::open(&config.sysroot)?;
        let exe = config.argv.first().ok_or("no program: argv is empty")?.clone();
        for dir in ["data", "tmp"] {
            std::fs::create_dir_all(config.instance_dir.join(dir)).map_err(|e| format!("{}: {e}", config.instance_dir.display()))?;
        }
        let vfs = Vfs::new(
            sysroot,
            vec![(b"/data".to_vec(), config.instance_dir.join("data")), (b"/tmp".to_vec(), config.instance_dir.join("tmp"))],
            exe.clone(),
        );
        let space = Arc::new(
            GuestSpace::with_config(GuestSpaceConfig { size: GUEST_SPACE_BYTES, ..GuestSpaceConfig::default() })
                .map_err(|e| format!("reserve the guest address space: {e}"))?,
        );
        let backend = DynarmicBackend::new(Arc::clone(&space), DynarmicOptions::default()).map_err(|e| format!("the CPU backend: {e}"))?;
        let p = Self::assemble(space, vfs, config.stdout, config.stderr, config.trace, Some(backend), 0);
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
        let stack = p.mm.map(&p, &loader, MapRequest { addr: 0, len: STACK_BYTES + 4096, prot: 3, flags: 0x22 | 0x20000, fd: -1, offset: 0 }).map_err(|e| format!("the main stack: {e:?}"))?;
        p.mm.protect(stack, 4096, 0).map_err(|e| format!("the stack guard: {e:?}"))?;
        let top = stack + STACK_BYTES + 4096;
        let mut random = [0u8; 16];
        omni_platform::process::random_bytes(&mut random).map_err(|e| format!("AT_RANDOM: {e}"))?;
        let auxv = [
            (AT_PHDR, program.phdr), (AT_PHENT, 56), (AT_PHNUM, program.phnum), (AT_PAGESZ, 4096),
            (AT_BASE, base), (AT_FLAGS, 0), (AT_ENTRY, program.entry), (AT_UID, u64::from(UID)),
            (AT_EUID, u64::from(UID)), (AT_GID, u64::from(UID)), (AT_EGID, u64::from(UID)),
            (AT_HWCAP, HWCAP), (AT_HWCAP2, 0), (AT_CLKTCK, 100), (AT_SECURE, 0),
        ];
        let (bytes, sp) = exec::build_stack(top, &config.argv, &config.envp, &auxv, random, &exe);
        p.mem.write(top - bytes.len() as u64, &bytes).map_err(|e| format!("the initial stack: {e:?}"))?;
        loader.exit = None;
        *p.start.lock() = Some((entry, sp));
        Ok(p)
    }

    /// Run the main task to its end; `exit_group` or its last `exit` ends the process.
    pub fn run(self: &Arc<Self>) -> ExitStatus {
        let (pc, sp) = self.start.lock().expect("spawned");
        let backend = self.backend.as_ref().expect("a spawned process has a backend");
        let config = GuestThreadConfig::guest_managed(GuestAddressSpace::of(self.mem.space()).expect("the space's extent"));
        let mut cpu = backend.create_thread(config).expect("the main thread");
        let mut task = Box::new(Task::new(PID, Arc::clone(self)));
        cpu.set_svc_handler(on_svc, ThunkContext(&mut *task as *mut Task as usize)).expect("the syscall entry");
        cpu.set_sp(sp as usize);
        let status = self.run_task(&mut *cpu, &mut task, pc);
        *self.exit.lock() = Some(status.clone());
        status
    }

    fn run_task(&self, cpu: &mut dyn GuestCpu, task: &mut Task, mut pc: u64) -> ExitStatus {
        loop {
            let exit = match cpu.run(pc as usize, RunLimit::Unlimited) {
                Ok(e) => e,
                Err(e) => return ExitStatus::Killed { signal: 6, pc, detail: format!("the CPU backend: {e}") },
            };
            match (exit, task.exit) {
                (ExitReason::UnsupportedInstruction { .. }, Some(Exit::Group(code) | Exit::Thread(code))) => {
                    return ExitStatus::Exited(code);
                }
                (ExitReason::UnsupportedInstruction { pc: at, encoding }, None) if encoding & 0xFFE0_001F == 0xD400_0001 => {
                    // An SVC deferred for another reason (A5: signal delivery). None exist in A1.
                    pc = at as u64 + 4;
                }
                (ExitReason::MemoryFault { pc: at, address, access }, _) => {
                    return ExitStatus::Killed { signal: 11, pc: at as u64, detail: format!("{access:?} at {address:#x}") };
                }
                (other, _) => {
                    return ExitStatus::Killed { signal: 4, pc: other.pc() as u64, detail: format!("{other}") };
                }
            }
        }
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
        Self::assemble(space, vfs, stdout.clone(), stdout, false, None, scratch)
    }

    pub fn scratch(&self) -> u64 {
        self.scratch
    }

    pub fn test_task(self: &Arc<Self>) -> Task {
        Task::new(PID, Arc::clone(self))
    }
}
