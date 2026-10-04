//! `ptrace`, as far as **one process claiming another as its tracer** goes -- and no further.
//!
//! The guest kernel has no debugger machinery: a task's registers and memory are the host's, a
//! stopped tracee would have to be stopped by this layer, and nothing here needs that. What does
//! need `ptrace` is the other thing it is used for: **the self-debugging anti-tamper**, where an
//! app forks a watchdog, the parent allows it with `prctl(PR_SET_PTRACER, child)`, and the child
//! does `PTRACE_ATTACH` on its parent so that the one tracer slot is taken and no real debugger
//! can have it. Clash of Clans does exactly this, and kills itself when the attach fails
//! (`docs/superpowers/specs/2026-10-04-fork-timeshare-design.md` has the trace).
//!
//! So this keeps the part of `ptrace` that is bookkeeping -- who traces whom, one tracer at a time,
//! and only with the tracee's leave -- and refuses the part that is debugging. The property the
//! app is buying is real: once its watchdog holds the slot, a second `PTRACE_ATTACH` is `EPERM`.
//!
//! **An attach here does not stop the tracee.** Real `PTRACE_ATTACH` leaves the tracee in signal-
//! delivery-stop for its tracer to collect with `wait`. Stopping a whole app's threads with no way
//! to resume them is worse than not stopping them, and a watchdog that only wants the slot never
//! looks. A tracer that does look is refused by name, so it shows up in the refusals rather than
//! being quietly lied to.
use std::sync::atomic::{AtomicI32, Ordering};

use crate::errno::{Errno, SysResult, EINVAL, EPERM, ESRCH};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

/// `ptrace` requests, as `<sys/ptrace.h>` numbers them.
mod req {
    pub const TRACEME: u64 = 0;
    pub const CONT: u64 = 7;
    pub const KILL: u64 = 8;
    pub const ATTACH: u64 = 16;
    pub const DETACH: u64 = 17;
    pub const SETOPTIONS: u64 = 0x4200;
    pub const SEIZE: u64 = 0x4206;
    pub const INTERRUPT: u64 = 0x4207;
}

/// `EIO`, what `ptrace` answers for a request it cannot carry out on this tracee.
const EIO: Errno = Errno(5);

/// The wait status of a process stopped by a signal: `(signal << 8) | 0x7f`.
const fn stopped_by(signal: i32) -> i32 {
    (signal << 8) | 0x7f
}

/// SIGSTOP, which `PTRACE_ATTACH` leaves a tracee stopped in for its tracer to collect.
const SIGSTOP: i32 = 19;

/// Who traces a process, if anyone, and the stops its tracer has not collected.
///
/// **Attaching is per thread.** A debugger reads `/proc/<pid>/task` and attaches to every tid it
/// finds, then waits for each one's stop; a tracer whose wait answers `ECHILD` concludes the thread
/// is gone and kills it. So each attached tid carries its own stop, even though the tracer slot is
/// the process's.
#[derive(Default)]
pub struct Traced {
    tracer: AtomicI32,
    /// Each attached tid, and the wait status its tracer has not taken yet (0: none waiting).
    attached: parking_lot::Mutex<std::collections::BTreeMap<i32, i32>>,
}

impl Traced {
    /// The tracer's pid, or `None`.
    pub(crate) fn tracer(&self) -> Option<i32> {
        match self.tracer.load(Ordering::SeqCst) {
            0 => None,
            pid => Some(pid),
        }
    }

    /// Attach `tracer` to the thread `tid`. The first attach takes the process's one tracer slot;
    /// the rest must be the same tracer. Each leaves the stop that tracer is about to wait for.
    fn claim(&self, tracer: i32, tid: i32) -> bool {
        let mine = self.tracer.compare_exchange(0, tracer, Ordering::SeqCst, Ordering::SeqCst).is_ok() || self.tracer() == Some(tracer);
        if mine {
            self.attached.lock().insert(tid, stopped_by(SIGSTOP));
        }
        mine
    }

    /// Give the slot up, if `tracer` holds it.
    fn release(&self, tracer: i32) -> bool {
        let gave = self.tracer.compare_exchange(tracer, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok();
        if gave {
            self.attached.lock().clear();
        }
        gave
    }

    /// A stop to collect from an attached thread `want` accepts, taken.
    fn take_stop(&self, want: &impl Fn(i32) -> bool) -> Option<(i32, i32)> {
        let mut attached = self.attached.lock();
        let tid = attached.iter().find(|(tid, status)| **status != 0 && want(**tid)).map(|(tid, _)| *tid)?;
        attached.insert(tid, 0);
        Some((tid, stopped_by(SIGSTOP)))
    }

    /// Whether any attached thread `want` accepts is still here to be waited for.
    fn has_attached(&self, want: &impl Fn(i32) -> bool) -> bool {
        self.attached.lock().keys().any(|tid| want(*tid))
    }
}

/// A stop to collect from a thread of a process `me` traces, taken.
pub(crate) fn tracee_stop(me: i32, want: &impl Fn(i32) -> bool) -> Option<(i32, i32)> {
    crate::process::all_live().into_iter().filter(|p| p.traced.tracer() == Some(me)).find_map(|p| p.traced.take_stop(want))
}

/// Whether `me` traces a thread `want` accepts -- whether a `wait` for one has anything to wait
/// for, or should answer `ECHILD`.
pub(crate) fn traces_any(me: i32, want: &impl Fn(i32) -> bool) -> bool {
    crate::process::all_live().iter().any(|p| p.traced.tracer() == Some(me) && p.traced.has_attached(want))
}

/// The process with this pid, among those of this host process. A fork child and its parent are
/// always here together -- they are one host process by construction -- which is the whole of what
/// the self-debugging shape needs.
/// A debugger attaches to **threads**, not processes: it reads `/proc/<pid>/task` and attaches to
/// each tid it finds. A tid names its process here.
fn process_with(pid: i32) -> Option<std::sync::Arc<Process>> {
    let live = crate::process::all_live();
    if let Some(p) = live.iter().find(|p| p.sys.pid == pid) {
        return Some(std::sync::Arc::clone(p));
    }
    live.into_iter().find(|p| p.tids().contains(&pid))
}

/// The process `me` traces that `pid` names, by its pid or one of its tids -- what lets a tracer
/// signal a tracee's thread, which is not its child and not its own (`send_signal`).
pub(crate) fn tracee_of(me: i32, pid: i32) -> Option<std::sync::Arc<Process>> {
    process_with(pid).filter(|p| p.traced.tracer() == Some(me))
}

/// `ptrace(request, pid, addr, data)`.
fn sys_ptrace(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let (request, pid) = (a[0], a[1] as i64 as i32);
    let me = p.sys.pid;
    match request {
        // "Trace me": the caller's parent becomes its tracer.
        req::TRACEME => {
            let parent = p.family.ppid();
            if p.traced.claim(parent, t.tid) {
                Ok(0)
            } else {
                Err(EPERM)
            }
        }
        req::ATTACH | req::SEIZE => {
            if pid == me {
                return Err(EPERM);
            }
            let Some(target) = process_with(pid) else { return Err(ESRCH) };
            // One tracer at a time -- the rule the app is attaching in order to rely on -- but every
            // thread of that process may be attached by the tracer that holds it.
            if target.traced.claim(me, pid) {
                Ok(0)
            } else {
                Err(EPERM)
            }
        }
        req::DETACH => {
            let Some(target) = process_with(pid) else { return Err(ESRCH) };
            if target.traced.release(me) {
                Ok(0)
            } else {
                Err(EPERM)
            }
        }
        // Nothing is ever stopped here, so "carry on" and "set options" have nothing to do -- but
        // they must come from the tracer, and they must name a tracee.
        req::CONT | req::SETOPTIONS | req::INTERRUPT => {
            let Some(target) = process_with(pid) else { return Err(ESRCH) };
            if target.traced.tracer() == Some(me) {
                Ok(0)
            } else {
                Err(ESRCH)
            }
        }
        req::KILL => {
            let Some(target) = process_with(pid) else { return Err(ESRCH) };
            if target.traced.tracer() != Some(me) {
                return Err(ESRCH);
            }
            crate::fork::signal_process(&target, 9);
            Ok(0)
        }
        // Reading or writing a tracee's registers or memory, and the stop-and-step requests: this
        // layer has no tracee stops, so a caller is told so rather than given an invented answer.
        _ => {
            p.refusals.record(format!("ptrace: request {request:#x}"), t.pc, t.lr);
            Err(if pid == 0 { EINVAL } else { EIO })
        }
    }
}

pub fn install(table: &mut Table) {
    table.set(nr::PTRACE, sys_ptrace);
}
