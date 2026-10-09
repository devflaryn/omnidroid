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
    /// Set on a fork child for as long as it shares its parent's memory: ended with it.
    pair: Mutex<Option<Arc<ForkPair>>>,
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
        // The child is done sharing: the memory goes back to the parent before anything else, so a
        // parent waiting at the gate finds its own view in it.
        let pair = self.pair.lock().take();
        if let Some(pair) = pair {
            pair.finish();
        }
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
    // OMNI_FORK_NOCHILD=1 -- a diagnostic, never a default: the child is never started and the
    // parent runs on at once, with the child recorded as having exited 0. It answers one question,
    // "is this fork the only thing holding the app up?", for an app whose fork child never executes
    // a program (Clash of Clans' anti-tamper watchdog), which this fork deadlocks on by design.
    if nochild() {
        let child = parent.fork_child();
        let pid = child.sys.pid;
        parent.family.children.lock().insert(pid, Child::Ended(0, child.sys.uid()));
        eprintln!("[fork] OMNI_FORK_NOCHILD: {} forked {pid}, which is not started", p.sys.pid);
        if flags & CLONE_PARENT_SETTID != 0 {
            p.mem.write_u32(a[2], pid as u32).map_err(|_| EFAULT)?;
        }
        return Ok(pid as u64);
    }
    let tpidr = t.clone_tpidr;
    let frozen_at = std::time::Instant::now();
    parent.freeze_others(t.tid);
    p.mem.start_journal();
    let snapshot = Snapshot::take(p);
    let taken_at = std::time::Instant::now();
    let child = parent.fork_child();
    let pid = child.sys.pid;
    let sp = if a[1] == 0 { parent_sp } else { a[1] };
    if p.trace {
        eprintln!("[fork] child {pid} of {}: {} bytes of private memory kept", t.tid, snapshot.bytes());
    }
    let kept_bytes = snapshot.bytes();
    let pair = ForkPair::new(snapshot, &parent, &child);
    *child.family.pair.lock() = Some(Arc::clone(&pair));
    let started = start_child(p, t, &parent, &child, flags, a, regs, sp, tpidr);
    drop(child);
    // Until the child executes a program or ends (a dropped sender -- the child's thread gone --
    // ends the wait too), or until the child blocks instead and the pair hands the memory back.
    let mut shared = false;
    if let Ok(rx) = &started {
        let server = Arc::clone(&pair);
        let _ = std::thread::Builder::new().name(format!("omni-linux-fork-pair-{pid}")).spawn(move || server.serve());
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if pair.handed_over() {
                        shared = true;
                        break;
                    }
                }
            }
        }
    }
    let released_at = std::time::Instant::now();
    // Handed back: the pair put the fork-time image in the memory and thawed this process already,
    // and the child lives on with a view of its own. Otherwise this was a vfork after all.
    if !shared {
        pair.finish();
        pair.restore_parent(p);
        parent.thaw();
    }
    // OMNI_FORK_TRACE=1: each fork, what it kept, and how long its parent stood still.
    if fork_trace() {
        eprintln!(
            "[fork] {} forked {pid}{}: {} MiB kept, parent frozen {} ms (snapshot {} ms, child to exec {} ms, restore {} ms)",
            p.sys.pid,
            if shared { ", which lives on beside it" } else { "" },
            kept_bytes >> 20,
            frozen_at.elapsed().as_millis(),
            (taken_at - frozen_at).as_millis(),
            (released_at - taken_at).as_millis(),
            released_at.elapsed().as_millis()
        );
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

/// `OMNI_FORK_NOCHILD=1`: the diagnostic above.
fn nochild() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_FORK_NOCHILD").as_deref() == Ok("1"))
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

    /// Take the resident side's divergence from this image out of the address space, leaving the
    /// image itself in it: every page that differs is answered (with that side's content) and
    /// written back from the image. The caller holds the layout lock exclusively, so no copy
    /// interleaves. This is [`restore`](Self::restore) with the old content kept rather than
    /// dropped -- the half of a switch that shelves a side.
    fn shelve_holding_layout(&self, p: &Process, journal: &[(u64, usize)]) -> Shelf {
        const PAGE: usize = 4096;
        let keep_kernel_writes = |at: u64, page: &mut [u8], current: &[u8]| {
            let end = at + page.len() as u64;
            for &(w, len) in journal {
                let (from, to) = (w.max(at), (w + len as u64).min(end));
                if from < to {
                    let (i, j) = ((from - at) as usize, (to - at) as usize);
                    page[i..j].copy_from_slice(&current[i..j]);
                }
            }
        };
        let mut shelf = Shelf::new();
        for (start, bytes) in &self.kept {
            let whole = p.mem.read_holding_layout(*start, bytes.len()).ok();
            for (n, was) in bytes.chunks(PAGE).enumerate() {
                let at = start + (n * PAGE) as u64;
                let now = match &whole {
                    Some(w) => std::borrow::Cow::Borrowed(&w[n * PAGE..n * PAGE + was.len()]),
                    None => match p.mem.read_holding_layout(at, was.len()) {
                        Ok(v) => std::borrow::Cow::Owned(v),
                        Err(_) => continue,
                    },
                };
                if *now != *was {
                    shelf.push((at, now.to_vec()));
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
                            shelf.push((page_at, now.clone()));
                            let mut page = vec![0u8; len];
                            keep_kernel_writes(page_at, &mut page, &now);
                            let _ = p.mem.write_holding_layout(page_at, &page);
                        }
                    }
                }
                at = next;
            }
        }
        shelf
    }

    /// Put back every page that changed, but for the ranges the kernel wrote for the parent
    /// meanwhile (its journal); a page untouched at the fork that the child touched reads as
    /// zeros again. Under the layout lock, so no write of the kernel's interleaves.
    fn restore(&self, p: &Process) {
        const PAGE: usize = 4096;
        let _layout = p.mem.layout().write();
        // The journal ends now, but the remote direct-access gate stays shut until the write-back is
        // done: a direct write from the system host (a binder reply) landing in between would be
        // overwritten by it, not being journaled (`GuestMem::take_journal_gated`).
        let journal = p.mem.take_journal_gated();
        struct GateBack<'a>(&'a crate::guest::GuestMem);
        impl Drop for GateBack<'_> {
            fn drop(&mut self) {
                self.0.journal_done();
            }
        }
        let _gate = GateBack(&p.mem);
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
            // The range as it is now, read at once (a read a page is ten times slower), then
            // compared page by page: most pages are as they were.
            let whole = p.mem.read_holding_layout(*start, bytes.len()).ok();
            for (n, was) in bytes.chunks(PAGE).enumerate() {
                let at = start + (n * PAGE) as u64;
                let now = match &whole {
                    Some(w) => std::borrow::Cow::Borrowed(&w[n * PAGE..n * PAGE + was.len()]),
                    None => match p.mem.read_holding_layout(at, was.len()) {
                        Ok(v) => std::borrow::Cow::Owned(v),
                        Err(_) => continue,
                    },
                };
                if *now != *was {
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

/// Which of a fork pair's two views is meant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Side {
    Parent,
    Child,
}

/// A side's divergence from the fork-time image: its content for the pages it differs on.
type Shelf = Vec<(u64, Vec<u8>)>;

/// How long a fork child that has not executed a program must have sat still before the pair is
/// made live -- the answer to "is this child going to execute a program, or is it staying?".
///
/// Half a second. Every fork the boot makes -- vold's `vold_prepare_subdirs`, installd's
/// `dex2oat`, a shell's pipeline -- reaches `execve` in a few milliseconds, so none of them is ever
/// made live, and that path keeps exactly the cost and the code it had. A child still sitting there
/// half a second later is waiting on its parent, and the only way it will ever stop waiting is to
/// be given a view of its own. Its parent's `fork` pays this once, well inside the 60 s
/// ActivityManager allows a process to start in.
/// `OMNI_FORK_SHARE_AFTER_MS` overrides it, for measuring what a shorter or longer wait costs: the
/// parent's `fork` takes this long to answer, and an app that times its own fork can see it.
const CHILD_IS_STAYING_MS: u64 = 500;

fn child_is_staying() -> std::time::Duration {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    std::time::Duration::from_millis(*MS.get_or_init(|| std::env::var("OMNI_FORK_SHARE_AFTER_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(CHILD_IS_STAYING_MS)))
}

/// Once the pair is live, how long the side holding the memory must have been out of guest code
/// before it is handed over. Short: by now both sides are known to take turns, and this is the
/// pause between one side stopping and the other starting.
const IDLE_BEFORE_HANDOVER: std::time::Duration = std::time::Duration::from_millis(10);

/// A child that never blocks still gives the memory back to its parent this long after taking it:
/// a turn is a turn, and a parent must not be stopped indefinitely by one.
const CHILD_TURN_AT_MOST: std::time::Duration = std::time::Duration::from_secs(2);

/// **The one memory, time-shared between a fork parent and a child that has not executed a
/// program.** A guest address is a host address and both live in one host process, so only one
/// side's view can be in the address space at a time; this holds the other's.
///
/// A side's view is always the fork-time `image` plus its own writes, which is what `fork` promises
/// -- kept over the pair's whole life rather than only across the vfork window. The pair is made
/// live only when the child is seen to block instead of executing a program, so the fork-exec path
/// every boot takes is reached and finished before any of this is attached.
pub(crate) struct ForkPair {
    /// The parent's private writable memory as the fork found it.
    image: Snapshot,
    inner: Mutex<Pair>,
    changed: Condvar,
    parent: Weak<Process>,
    child: Weak<Process>,
}

struct Pair {
    /// Whose view is in the address space.
    resident: Side,
    /// The side that is not resident, as it left the memory.
    parent_shelf: Shelf,
    child_shelf: Shelf,
    /// Each side's standing claim on the memory. The child's is set when a call completes for it
    /// or it is about to run; the parent's whenever it loses the memory, because its tasks are
    /// then parked and cannot ask for themselves.
    parent_wants: bool,
    child_wants: bool,
    /// Live once the first handover has happened; until then this is an ordinary vfork.
    live: bool,
    /// The child executed a program or ended: the memory is the parent's for good.
    over: bool,
}

impl Pair {
    /// How many pages the given side has shelved (0 for whichever side is resident).
    fn shelf_len(&self, side: Side) -> usize {
        match side {
            Side::Parent => self.parent_shelf.len(),
            Side::Child => self.child_shelf.len(),
        }
    }
}

/// The gate `GuestMem` waits at before a copy (`crate::guest::Resident`).
struct Seat {
    pair: Arc<ForkPair>,
    side: Side,
}

impl crate::guest::Resident for Seat {
    fn ensure(&self) {
        self.pair.ensure(self.side);
    }
}

impl ForkPair {
    fn new(image: Snapshot, parent: &Arc<Process>, child: &Arc<Process>) -> Arc<Self> {
        Arc::new(Self {
            image,
            inner: Mutex::new(Pair { resident: Side::Child, parent_shelf: Vec::new(), child_shelf: Vec::new(), parent_wants: false, child_wants: false, live: false, over: false }),
            changed: Condvar::new(),
            parent: Arc::downgrade(parent),
            child: Arc::downgrade(child),
        })
    }

    fn process(&self, side: Side) -> Option<Arc<Process>> {
        match side {
            Side::Parent => self.parent.upgrade(),
            Side::Child => self.child.upgrade(),
        }
    }

    /// Block until `side`'s view is the one in the address space. A side that is already resident,
    /// or a pair that is over, costs a lock and nothing else.
    fn ensure(&self, side: Side) {
        let mut inner = self.inner.lock();
        if inner.over || inner.resident == side {
            return;
        }
        match side {
            Side::Parent => inner.parent_wants = true,
            Side::Child => inner.child_wants = true,
        }
        self.changed.notify_all();
        while !inner.over && inner.resident != side {
            self.changed.wait(&mut inner);
        }
    }

    /// The memory has been handed back to the parent: its forking thread may return.
    fn handed_over(&self) -> bool {
        let inner = self.inner.lock();
        inner.live && inner.resident == Side::Parent
    }

    /// The fork ended as a vfork does (the child executed a program or exited) and the pair was
    /// never made live: put the parent's memory back exactly as it did before.
    fn restore_parent(&self, p: &Process) {
        self.image.restore(p);
    }

    /// The pair is over: the parent keeps the memory and both views stop waiting. If the child had
    /// the memory when it stopped sharing, the parent's view is put back into it first -- otherwise
    /// the parent would run on in the child's.
    fn finish(&self) {
        let give_back = {
            let inner = self.inner.lock();
            inner.live && !inner.over && inner.resident == Side::Child
        };
        if give_back {
            self.switch_to(Side::Parent);
        }
        let mut inner = self.inner.lock();
        inner.over = true;
        self.changed.notify_all();
        drop(inner);
        for side in [Side::Parent, Side::Child] {
            if let Some(p) = self.process(side) {
                p.mem.unshare();
            }
        }
    }

    /// Hand the memory to `to`, which the other side has. The side losing it is stopped first (no
    /// task of it in guest code), and the exchange is made under the layout lock, which every copy
    /// holds shared -- so neither a running task nor a completing call can see a half-swapped view.
    fn switch_to(&self, to: Side) {
        let from = match to {
            Side::Parent => Side::Child,
            Side::Child => Side::Parent,
        };
        let (Some(losing), Some(gaining)) = (self.process(from), self.process(to)) else { return };
        // Stop the side that has the memory: `freeze_others(0)` exempts no task, and it waits for
        // each one to leave guest code.
        losing.freeze_others(0);
        {
            let _layout = losing.mem.layout().write();
            let mut inner = self.inner.lock();
            if inner.over || inner.resident == to {
                drop(inner);
                losing.thaw();
                return;
            }
            // The journal is the parent's: the ranges the kernel wrote for it while the child had
            // the memory. It is emptied at the first handover and is empty after that, because
            // from then on a call completing for a shelved side waits for its turn instead.
            let journal = self.process(Side::Parent).map(|p| p.mem.take_journal()).unwrap_or_default();
            let shelved = self.image.shelve_holding_layout(&losing, &journal);
            let taking = match to {
                Side::Parent => std::mem::take(&mut inner.parent_shelf),
                Side::Child => std::mem::take(&mut inner.child_shelf),
            };
            apply_shelf_holding_layout(&gaining, &taking);
            match from {
                Side::Parent => inner.parent_shelf = shelved,
                Side::Child => inner.child_shelf = shelved,
            }
            inner.resident = to;
            match to {
                Side::Parent => inner.parent_wants = false,
                Side::Child => inner.child_wants = false,
            }
            // The parent has just been stopped mid-run and its tasks are parked, so it cannot ask
            // for the memory itself: its claim stands from here until it has it back.
            if from == Side::Parent {
                inner.parent_wants = true;
            }
        }
        gaining.thaw();
        self.changed.notify_all();
        if fork_trace() {
            let inner = self.inner.lock();
            eprintln!("[fork] the memory goes to the {to:?}: it kept {} pages, the {from:?} left {} behind", inner.shelf_len(to), inner.shelf_len(from));
        }
    }

    /// Every task of `side` is out of guest code (it has at least one task, and none is running).
    fn idle(&self, side: Side) -> bool {
        self.process(side).is_some_and(|p| p.no_task_in_guest())
    }

    /// The pair's own thread: it makes every switch, so no task ever performs one itself and the
    /// lock order is the same everywhere. It hands the memory over when the side that has it is
    /// blocked, and when the other side asks.
    /// The two sides are not symmetric, and the policy says so. The **parent owns the memory**: it
    /// keeps it while it runs, and gets it back as soon as the child stops using it. The **child
    /// is a guest**: it asks, is let in at once -- the parent would otherwise never yield, having
    /// no reason to -- does its turn, and blocks, which gives the memory straight back.
    ///
    /// So the costly half of a switch, shelving a parent that has been running and has diverged
    /// from the fork-time image, is paid only when the child actually has something to do, and
    /// never merely because the parent waited on a binder call for a moment.
    fn serve(self: Arc<Self>) {
        let mut idle_since: Option<std::time::Instant> = None;
        let mut child_since: Option<std::time::Instant> = None;
        loop {
            let (over, resident, parent_wants, child_wants, live) = {
                let inner = self.inner.lock();
                (inner.over, inner.resident, inner.parent_wants, inner.child_wants, inner.live)
            };
            if over {
                return;
            }
            let idle = self.idle(resident);
            if idle {
                idle_since.get_or_insert_with(std::time::Instant::now);
            } else {
                idle_since = None;
            }
            let still_for = if live { IDLE_BEFORE_HANDOVER } else { child_is_staying() };
            let settled = idle_since.is_some_and(|t| t.elapsed() >= still_for);
            let switch = match resident {
                // The child wants in: let it, running parent or not.
                Side::Parent if child_wants && self.process(Side::Child).is_some() => Some(Side::Child),
                // The child has stopped: the parent takes its memory back. Before the pair is live
                // this is the first handover, and what makes it live.
                Side::Child if settled && (parent_wants || !live) => Some(Side::Parent),
                // A child that never blocks must not keep the memory from its parent for ever.
                Side::Child if parent_wants && child_since.is_some_and(|t| t.elapsed() >= CHILD_TURN_AT_MOST) => Some(Side::Parent),
                _ => None,
            };
            if let Some(to) = switch {
                if !live {
                    self.go_live();
                }
                self.switch_to(to);
                idle_since = None;
                child_since = (to == Side::Child).then(std::time::Instant::now);
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// The child blocked rather than executing a program: from here both sides take turns, and
    /// every copy either makes waits for its turn.
    fn go_live(self: &Arc<Self>) {
        let mut inner = self.inner.lock();
        if inner.live || inner.over {
            return;
        }
        inner.live = true;
        drop(inner);
        for side in [Side::Parent, Side::Child] {
            if let Some(p) = self.process(side) {
                p.mem.share_with(Arc::new(Seat { pair: Arc::clone(self), side }));
            }
        }
        if fork_trace() {
            eprintln!("[fork] the child blocked: parent and child now take turns in the memory");
        }
        // OMNI_FORK_TRACE_CALLS=1: from here, both sides' system calls are traced. The window that
        // matters for an app whose fork child stays -- what the parent does once its fork has
        // answered -- without the cost of tracing the whole start of the app into it.
        if std::env::var("OMNI_FORK_TRACE_CALLS").as_deref() == Ok("1") {
            for side in [Side::Parent, Side::Child] {
                if let Some(p) = self.process(side) {
                    p.trace_calls(true);
                }
            }
        }
    }
}

/// Write a shelved side's pages back into the address space.
fn apply_shelf_holding_layout(p: &Process, shelf: &Shelf) {
    for (at, bytes) in shelf {
        let _ = p.mem.write_holding_layout(*at, bytes);
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
    let loading = std::time::Instant::now();
    let image = me.exec_image(&path, &argv, &envp).map_err(|e| {
        p.refusals.record(format!("execve: {e}"), t.pc, t.lr);
        ENOEXEC
    })?;
    if fork_trace() {
        eprintln!("[fork] {} executes {}: image made in {} ms", p.sys.pid, String::from_utf8_lossy(&path), loading.elapsed().as_millis());
    }
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

/// Wait for a stop from a process this one traces (`crate::ptrace`). A tracee is not a child, so
/// this is reached only once `wait_child` has answered `ECHILD`; with no such tracee either, that
/// `ECHILD` stands. The wait itself is a poll: a stop is posted by the attach, not by a scheduler
/// this layer has.
fn wait_tracee(p: &Process, t: &Task, want: &impl Fn(i32) -> bool, nohang: bool) -> Result<Option<(i32, i32, u32)>, Errno> {
    let me = p.sys.pid;
    loop {
        if let Some((pid, status)) = crate::ptrace::tracee_stop(me, want) {
            return Ok(Some((pid, status, p.sys.uid())));
        }
        if !crate::ptrace::traces_any(me, want) {
            return Err(ECHILD);
        }
        if nohang {
            return Ok(None);
        }
        if t.pending.load(Ordering::SeqCst) & !t.sigmask != 0 {
            return Err(EINTR);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// `wait4(pid, wstatus, options, rusage)`: `pid` > 0 that child, anything else any child (there
/// are no process groups of children here). `rusage` is zeroed.
fn sys_wait4(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    const WNOHANG: u64 = 1;
    let target = a[0] as i64 as i32;
    let want = |pid: i32| target <= 0 || pid == target;
    let found = match wait_child(p, t, want, a[2] & WNOHANG != 0, false) {
        Ok(found) => found,
        // A tracer waits for its tracee as it does for a child, even when the tracee is its own
        // parent (`crate::ptrace`: the self-debugging watchdog).
        Err(e) if e == ECHILD => wait_tracee(p, t, &want, a[2] & WNOHANG != 0)?,
        Err(e) => return Err(e),
    };
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
