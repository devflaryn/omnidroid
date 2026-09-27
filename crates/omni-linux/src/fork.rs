//! `fork`, `execve` and `wait` (docs/superpowers/specs/2026-09-27-fork-exec-design.md).
//!
//! Every process of an instance lives in one host process, and a guest address is a host address,
//! so a fork child cannot have a copy of its parent's memory: it is a vfork child, running in its
//! parent's memory until it executes a program or ends, while the parent's calling thread waits.
//! What fork promises the parent -- that it finds its private memory as it left it, whatever the
//! child writes (its stack frames, the atfork handlers' state: libbinder marks its `ProcessState`
//! forked) -- is kept: the parent's other tasks are frozen while the child runs, its private
//! memory is saved, and what the child changed is put back.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use omni_cpu::XReg;
use parking_lot::{Condvar, Mutex};

use crate::errno::{Errno, SysResult, EAGAIN, EFAULT, EINTR, EINVAL, ENOENT};
use crate::process::{Exit, ExitStatus, Process, Task};
use crate::syscall::{nr, Table};
use crate::vfs::Node;

const ECHILD: Errno = Errno(10);
const ENOEXEC: Errno = Errno(8);
const EACCES: Errno = crate::errno::EACCES;
const SIGCHLD: i32 = 17;
const CLONE_PARENT_SETTID: u64 = 0x10_0000;
const CLONE_CHILD_SETTID: u64 = 0x100_0000;

/// A child as its parent's `wait` finds it.
enum Child {
    /// Running: the image under that pid now (the fork child, then the program it executed).
    Running(Weak<Process>),
    /// Ended, not yet reaped: its wait status word and uid.
    Ended(i32, u32),
}

/// A process's parent and children, and, for a vfork child, the parent thread waiting on it.
#[derive(Default)]
pub struct Family {
    parent: Mutex<Option<Weak<Process>>>,
    ppid: AtomicI32,
    children: Mutex<BTreeMap<i32, Child>>,
    changed: Condvar,
    /// Set while the parent's forking thread waits: released at `execve` or the child's end.
    release: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// An image `execve` replaced: its end is not the process's end.
    superseded: AtomicBool,
    /// The image that replaced this one: `run` answers how it ended.
    successor: Mutex<Option<Arc<Process>>>,
    /// A stand-in for another host process's process (`crate::remote`).
    stand_in: AtomicBool,
    /// The signal mask the main task starts with (a fork child's and an executed program's are
    /// the calling thread's).
    start_mask: AtomicU64,
}

impl Family {
    pub(crate) fn set_parent(&self, parent: &Arc<Process>) {
        *self.parent.lock() = Some(Arc::downgrade(parent));
        self.ppid.store(parent.sys.pid, Ordering::Relaxed);
    }

    /// An executed image's: the parent of the image it replaces.
    pub(crate) fn inherit(&self, old: &Family) {
        *self.parent.lock() = old.parent.lock().clone();
        self.ppid.store(old.ppid.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    fn parent(&self) -> Option<Arc<Process>> {
        self.parent.lock().as_ref().and_then(Weak::upgrade)
    }

    /// `getppid`: the parent's pid, or init's.
    pub(crate) fn ppid(&self) -> i32 {
        match self.ppid.load(Ordering::Relaxed) {
            0 => 1,
            pid => pid,
        }
    }

    pub(crate) fn mark_stand_in(&self) {
        self.stand_in.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_stand_in(&self) -> bool {
        self.stand_in.load(Ordering::Relaxed)
    }

    pub(crate) fn superseded(&self) -> bool {
        self.superseded.load(Ordering::SeqCst)
    }

    /// The image `execve` replaced this one with, if it did.
    pub(crate) fn successor(&self) -> Option<Arc<Process>> {
        self.successor.lock().clone()
    }

    pub(crate) fn start_mask(&self) -> u64 {
        self.start_mask.load(Ordering::Relaxed)
    }

    /// The running image of a child, by pid.
    pub(crate) fn child(&self, pid: i32) -> Option<Arc<Process>> {
        match self.children.lock().get(&pid) {
            Some(Child::Running(image)) => image.upgrade(),
            _ => None,
        }
    }

    fn release_parent(&self) {
        if let Some(tx) = self.release.lock().take() {
            let _ = tx.send(());
        }
    }
}

/// `clone` without `CLONE_THREAD` (`fork`; `vfork` is the same here): the child runs on a thread
/// of its own from the parent's registers with `x0 = 0`, in the parent's memory. Meanwhile the
/// parent's other tasks are frozen, and its private memory is kept: when the child executes a
/// program or ends, every page it changed is put back -- but for what the kernel wrote for the
/// parent's own blocked calls meanwhile -- and the parent's tasks run on. Answers the child's pid.
pub(crate) fn fork(p: &Process, t: &mut Task, flags: u64, a: [u64; 6]) -> SysResult {
    let parent = Arc::clone(&t.process);
    let (regs, parent_sp) = t.clone_regs.take().ok_or(EINVAL)?;
    let tpidr = t.clone_tpidr;
    let frozen_at = std::time::Instant::now();
    parent.freeze_others(t.tid);
    p.mem.start_journal();
    let snapshot = Snapshot::take(p);
    let child = parent.fork_child();
    let pid = child.sys.pid;
    let sp = if a[1] == 0 { parent_sp } else { a[1] };
    if p.trace {
        eprintln!("[fork] child {pid} of {}: {} bytes of private memory kept", t.tid, snapshot.bytes());
    }
    let started = start_child(p, t, &parent, &child, flags, a, regs, sp, tpidr);
    drop(child);
    // Until the child executes a program or ends (a dropped sender -- the child's thread gone --
    // ends the wait too).
    if let Ok(rx) = &started {
        let _ = rx.recv();
    }
    snapshot.restore(p);
    parent.thaw();
    // OMNI_FORK_TRACE=1: each fork, what it kept, and how long its parent stood still.
    if fork_trace() {
        eprintln!("[fork] {} forked {pid}: {} MiB kept, parent frozen {} ms", p.sys.pid, snapshot.bytes() >> 20, frozen_at.elapsed().as_millis());
    }
    started?;
    if flags & CLONE_PARENT_SETTID != 0 {
        p.mem.write_u32(a[2], pid as u32).map_err(|_| EFAULT)?;
    }
    Ok(pid as u64)
}

fn fork_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_FORK_TRACE").as_deref() == Ok("1"))
}

/// Start the fork child's thread; the receiver its release comes on.
#[allow(clippy::too_many_arguments)]
fn start_child(p: &Process, t: &Task, parent: &Arc<Process>, child: &Arc<Process>, flags: u64, a: [u64; 6], regs: [u64; 31], sp: u64, tpidr: Option<u64>) -> Result<std::sync::mpsc::Receiver<()>, Errno> {
    let pid = child.sys.pid;
    // The child's tid, in its own view of the memory (put back with the rest).
    if flags & CLONE_CHILD_SETTID != 0 {
        p.mem.write_u32(a[4], pid as u32).map_err(|_| EFAULT)?;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    *child.family.release.lock() = Some(tx);
    child.family.start_mask.store(t.sigmask, Ordering::Relaxed);
    parent.family.children.lock().insert(pid, Child::Running(Arc::downgrade(child)));
    let pc = t.pc + 4;
    let runner = Arc::clone(child);
    let spawned = std::thread::Builder::new().name(format!("omni-linux-fork-{pid}")).spawn(move || {
        let status = runner.run_from(pc, |cpu| {
            for (n, value) in regs.iter().enumerate() {
                cpu.set_x(XReg::new(n as u8).expect("x0..x30"), *value);
            }
            cpu.set_x(XReg::new(0).expect("x0"), 0);
            cpu.set_sp(sp as usize);
            if let Some(tls) = tpidr {
                cpu.set_tpidr_el0(tls as usize);
            }
        });
        ended(&runner, &status);
    });
    if spawned.is_err() {
        parent.family.children.lock().remove(&pid);
        return Err(EAGAIN);
    }
    Ok(rx)
}

/// The parent's private writable memory as the fork found it: what it had touched (committed),
/// and what it had not.
struct Snapshot {
    kept: Vec<(u64, Vec<u8>)>,
    untouched: Vec<(u64, u64)>,
}

impl Snapshot {
    /// Every committed range of a private, writable mapping -- anonymous, or a file's private view
    /// (a program's `.data`, which fork gives the child a copy of as much as its heap). A shared
    /// mapping is shared with the child under fork too.
    fn take(p: &Process) -> Self {
        let _layout = p.mem.layout().write();
        let space = p.mem.space();
        let mut snapshot = Self { kept: Vec::new(), untouched: Vec::new() };
        for region in space.mapped_regions() {
            let writable = matches!(region.protection, omni_mem::Protection::ReadWrite | omni_mem::Protection::ReadWriteExecute);
            let private = matches!(region.kind, omni_mem::RegionKind::Anonymous | omni_mem::RegionKind::File { shared: false, .. });
            if !writable || !private {
                continue;
            }
            let end = (region.start + region.len) as u64;
            let mut at = region.start as u64;
            while at < end {
                let Some(r) = space.region_at(at as usize) else { break };
                let next = ((r.start + r.len) as u64).min(end);
                // An untouched anonymous page reads as zeros; a file view's untouched page is the
                // file's, so it is kept by its content.
                if r.committed == 0 && matches!(region.kind, omni_mem::RegionKind::Anonymous) {
                    snapshot.untouched.push((at, next));
                } else if let Ok(bytes) = p.mem.read_holding_layout(at, (next - at) as usize) {
                    snapshot.kept.push((at, bytes));
                }
                at = next;
            }
        }
        snapshot
    }

    fn bytes(&self) -> usize {
        self.kept.iter().map(|(_, b)| b.len()).sum()
    }

    /// Put back every page that changed, but for the ranges the kernel wrote for the parent
    /// meanwhile (its journal); a page untouched at the fork that the child touched reads as
    /// zeros again. Under the layout lock, so no write of the kernel's interleaves.
    fn restore(&self, p: &Process) {
        const PAGE: usize = 4096;
        let _layout = p.mem.layout().write();
        let journal = p.mem.take_journal();
        let keep_kernel_writes = |at: u64, page: &mut [u8], current: &[u8]| {
            let end = at + page.len() as u64;
            for &(w, len) in &journal {
                let (from, to) = (w.max(at), (w + len as u64).min(end));
                if from < to {
                    let (i, j) = ((from - at) as usize, (to - at) as usize);
                    page[i..j].copy_from_slice(&current[i..j]);
                }
            }
        };
        for (start, bytes) in &self.kept {
            for (n, was) in bytes.chunks(PAGE).enumerate() {
                let at = start + (n * PAGE) as u64;
                let Ok(now) = p.mem.read_holding_layout(at, was.len()) else { continue };
                if now != was {
                    let mut page = was.to_vec();
                    keep_kernel_writes(at, &mut page, &now);
                    let _ = p.mem.write_holding_layout(at, &page);
                }
            }
        }
        for &(start, end) in &self.untouched {
            let mut at = start;
            while at < end {
                let Some(r) = p.mem.space().region_at(at as usize) else { break };
                let next = ((r.start + r.len) as u64).min(end);
                if r.committed != 0 {
                    for page_at in (at..next).step_by(PAGE) {
                        let len = PAGE.min((next - page_at) as usize);
                        let Ok(now) = p.mem.read_holding_layout(page_at, len) else { continue };
                        if now.iter().any(|b| *b != 0) {
                            let mut page = vec![0u8; len];
                            keep_kernel_writes(page_at, &mut page, &now);
                            let _ = p.mem.write_holding_layout(page_at, &page);
                        }
                    }
                }
                at = next;
            }
        }
    }
}

/// A process image ended with `status`: unless `execve` replaced it, its parent finds it ended
/// (and is sent `SIGCHLD`), and a parent still waiting on its fork is released.
pub(crate) fn ended(image: &Arc<Process>, status: &ExitStatus) {
    if image.trace {
        eprintln!("[fork] {} ended: {status:?}{} ({} references)", image.sys.pid, if image.family.superseded() { ", replaced by execve" } else { "" }, Arc::strong_count(image));
    }
    if image.family.superseded() {
        return;
    }
    let word = match status {
        ExitStatus::Exited(code) => (code & 0xff) << 8,
        ExitStatus::Killed { signal, .. } => signal & 0x7f,
    };
    let pid = image.sys.pid;
    if let Some(parent) = image.family.parent() {
        let (handler, flags, ..) = parent.sys.action(SIGCHLD);
        const SA_NOCLDWAIT: u64 = 2;
        {
            let mut children = parent.family.children.lock();
            // With SIGCHLD ignored no zombie is kept: the child is reaped as it ends.
            if handler == 1 || flags & SA_NOCLDWAIT != 0 {
                children.remove(&pid);
            } else {
                children.insert(pid, Child::Ended(word, image.sys.uid()));
            }
        }
        parent.family.changed.notify_all();
        if handler > 1 {
            parent.post_signal(parent.sys.pid, SIGCHLD);
        }
    }
    image.family.release_parent();
}

/// A NULL-terminated array of string pointers (`argv`, `envp`).
fn strings(p: &Process, mut at: u64) -> Result<Vec<Vec<u8>>, Errno> {
    let mut out = Vec::new();
    if at == 0 {
        return Ok(out);
    }
    loop {
        let s = p.mem.read_u64(at)?;
        if s == 0 {
            return Ok(out);
        }
        out.push(p.mem.read_cstr(s, 128 * 1024)?);
        if out.len() > 64 * 1024 {
            return Err(crate::errno::E2BIG);
        }
        at += 8;
    }
}

/// `execve(path, argv, envp)`: the program is loaded into a space of its own under this pid, and
/// the old image ends without a status: a parent waiting on its fork is released, and the old
/// image's `run` answers how the new one ends.
fn sys_execve(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let path = p.mem.read_cstr(a[0], 4096)?;
    let argv = strings(p, a[1])?;
    let envp = strings(p, a[2])?;
    let me = Arc::clone(&t.process);
    // What `execvp` tries path after path must be cheap to refuse.
    let cwd = p.cwd.lock().clone();
    match p.vfs.resolve(&cwd, &path, true)?.node {
        Node::Missing { .. } => return Err(ENOENT),
        Node::Dir | Node::HostDir { .. } => return Err(EACCES),
        _ => {}
    }
    let image = me.exec_image(&path, &argv, &envp).map_err(|e| {
        p.refusals.record(format!("execve: {e}"), t.pc, t.lr);
        ENOEXEC
    })?;
    image.family.start_mask.store(t.sigmask, Ordering::Relaxed);
    *me.family.successor.lock() = Some(Arc::clone(&image));
    me.family.superseded.store(true, Ordering::SeqCst);
    if let Some(parent) = me.family.parent() {
        parent.family.children.lock().insert(me.sys.pid, Child::Running(Arc::downgrade(&image)));
    }
    let runner = Arc::clone(&image);
    let spawned = std::thread::Builder::new().name(format!("omni-linux-exec-{}", image.sys.pid)).spawn(move || {
        let status = runner.run();
        ended(&runner, &status);
    });
    if spawned.is_err() {
        me.family.superseded.store(false, Ordering::SeqCst);
        *me.family.successor.lock() = None;
        return Err(EAGAIN);
    }
    me.family.release_parent();
    t.exit = Some(Exit::Group(0));
    Ok(0)
}

/// Wait for a child `want` accepts to have ended: its pid, wait status word and uid; `None` with
/// `nohang` when none has. `ECHILD` when there is no such child; `EINTR` when a signal comes.
fn wait_child(p: &Process, t: &Task, want: impl Fn(i32) -> bool, nohang: bool, keep: bool) -> Result<Option<(i32, i32, u32)>, Errno> {
    let mut children = p.family.children.lock();
    loop {
        if !children.keys().any(|pid| want(*pid)) {
            return Err(ECHILD);
        }
        let ended = children.iter().find_map(|(pid, c)| match c {
            Child::Ended(word, uid) if want(*pid) => Some((*pid, *word, *uid)),
            _ => None,
        });
        if let Some(found) = ended {
            if !keep {
                children.remove(&found.0);
            }
            return Ok(Some(found));
        }
        if nohang {
            return Ok(None);
        }
        if t.pending.load(Ordering::SeqCst) & !t.sigmask != 0 {
            return Err(EINTR);
        }
        p.family.changed.wait_for(&mut children, std::time::Duration::from_millis(50));
    }
}

/// `wait4(pid, wstatus, options, rusage)`: `pid` > 0 that child, anything else any child (there
/// are no process groups of children here). `rusage` is zeroed.
fn sys_wait4(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    const WNOHANG: u64 = 1;
    let target = a[0] as i64 as i32;
    let found = wait_child(p, t, |pid| target <= 0 || pid == target, a[2] & WNOHANG != 0, false)?;
    let Some((pid, word, _)) = found else { return Ok(0) };
    if a[1] != 0 {
        p.mem.write_u32(a[1], word as u32)?;
    }
    if a[3] != 0 {
        p.mem.write(a[3], &[0u8; 144])?;
    }
    Ok(pid as u64)
}

/// `waitid(idtype, id, infop, options, rusage)`: `P_PID` that child, `P_ALL`/`P_PGID` any;
/// `WNOHANG`, `WNOWAIT`. The `siginfo` says how it ended (`CLD_EXITED` or `CLD_KILLED`).
fn sys_waitid(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    const P_PID: u64 = 1;
    const WNOHANG: u64 = 1;
    const WNOWAIT: u64 = 0x0100_0000;
    let (idtype, id) = (a[0], a[1] as i64 as i32);
    let found = wait_child(p, t, |pid| idtype != P_PID || pid == id, a[3] & WNOHANG != 0, a[3] & WNOWAIT != 0)?;
    if a[2] != 0 {
        let mut info = [0u8; 128];
        if let Some((pid, word, uid)) = found {
            let (code, status) = if word & 0x7f == 0 { (1, (word >> 8) & 0xff) } else { (2, word & 0x7f) };
            info[0..4].copy_from_slice(&SIGCHLD.to_le_bytes());
            info[8..12].copy_from_slice(&(code as i32).to_le_bytes());
            info[16..20].copy_from_slice(&pid.to_le_bytes());
            info[20..24].copy_from_slice(&uid.to_le_bytes());
            info[24..28].copy_from_slice(&status.to_le_bytes());
        }
        p.mem.write(a[2], &info)?;
    }
    if a[4] != 0 {
        p.mem.write(a[4], &[0u8; 144])?;
    }
    Ok(0)
}

/// A signal to another process (a child): `SIGKILL`, or a default action that terminates, ends
/// it; an ignored one is dropped; a handled one is posted to its main thread.
pub(crate) fn signal_process(target: &Arc<Process>, sig: i32) {
    let (handler, ..) = target.sys.action(sig);
    let ignored_by_default = matches!(sig, 17 | 18 | 23 | 28);
    match handler {
        _ if sig == 9 => target.end(ExitStatus::Killed { signal: 9, pc: 0, detail: "killed by SIGKILL".into() }),
        1 => {}
        0 if ignored_by_default => {}
        0 => target.end(ExitStatus::Killed { signal: sig, pc: 0, detail: format!("the default action of signal {sig}, sent by another process") }),
        _ => target.post_signal(target.sys.pid, sig),
    }
}

pub fn install(table: &mut Table) {
    table.set(nr::EXECVE, sys_execve);
    table.set(nr::WAIT4, sys_wait4);
    table.set(nr::WAITID, sys_waitid);
}
