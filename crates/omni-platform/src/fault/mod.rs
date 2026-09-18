//! Guest memory faults: the platform seam that lets Omnidroid see an access violation **before**
//! anyone else does.
//!
//! # Why this exists, and why it must be *vectored*
//!
//! D4 settled identity mapping: the guest's virtual address *is* the host's, and dynarmic emits
//! `mov reg, [r13 + vaddr]` with `r13 = 0`. A guest load from an address the guest has mapped but
//! Omnidroid has not committed yet is therefore an ordinary host access violation inside
//! JIT-generated code — and whoever handles it owns guest demand paging.
//!
//! dynarmic's own Windows fault handling is **frame-based** SEH: `exception_handler_windows.cpp`
//! builds a `RUNTIME_FUNCTION` and calls `RtlAddFunctionTable` over its code cache. Windows runs
//! **every vectored handler before any frame-based one**, so a handler installed here sees the fault
//! first and can resolve it without dynarmic ever learning it happened. That ordering is not a
//! convention we are relying on by luck: it is the documented dispatch order, it was verified in the
//! D4 spike (`veh_hits = 1`, dynarmic's slow path never entered), and [`stats`] exists so a test can
//! keep verifying it.
//!
//! D10 requires Omnidroid to keep ownership of guest paging, and this is the mechanism.
//!
//! # What a handler may and may not do
//!
//! A vectored handler runs on the faulting thread, at the faulting instruction, with that thread's
//! state frozen mid-instruction. Three rules follow, and none of them is negotiable:
//!
//! * **It must not unwind.** A Rust panic escaping into the OS's exception dispatcher is undefined
//!   behaviour. Handlers are `fn` pointers, not closures, and [`install`] documents that the body
//!   must be panic-free or catch its own panics.
//! * **It must not take a lock the faulting thread might already hold.** The fault can arrive at any
//!   instruction, so any lock the handler takes must be one that is never held across guest
//!   execution. This is a statement about the *caller's* design, which is why the context is an
//!   opaque `usize` rather than something this module can dereference.
//! * **It must be quick and it must decline quickly.** Every access violation in the whole process
//!   passes through here, including ones belonging to other code entirely. A handler that cannot
//!   immediately recognise the address as its own returns [`FaultOutcome::NotOurs`], which lets
//!   normal dispatch continue as though this module were not installed.
//! * **It must return.** Dropping a [`FaultRegistration`] waits for every dispatch already inside
//!   that slot's handler to come back before it lets the registrant free the context, so a handler
//!   that never returns turns a teardown into a hang. That is the price of the guarantee below, and
//!   it is the right way round: the failure it replaces was a use-after-free inside an exception
//!   dispatcher.
//!
//! # The guarantee a registrant gets back
//!
//! **When the registration's `drop` returns, no dispatch is inside that handler and none can
//! start.** The registrant may then free the context, and a slot the table hands out afterwards
//! cannot be paired with the old one.
//!
//! This is stated here because the earlier version of this contract was **insufficient rather than
//! violated**, which is why no review of either side alone could see it. It asked only that the
//! context stay valid "until the registration is dropped", and `omni-mem` satisfied that exactly —
//! it declared the registration field before the state, so the slot was cleared first. But clearing
//! a slot only stops calls that have not yet read it. A vectored handler is **process-wide**: it
//! runs on whatever thread faulted, for whatever reason, and can be preempted between loading a
//! handler pointer and calling it. Nothing a registrant can write makes that window safe, so the
//! window is closed here instead. See `windows.rs` for the mechanism and its ordering argument.
//!
//! # Scope
//!
//! Implemented and measured on Windows. On Linux and macOS every entry point returns
//! [`FaultError::Unsupported`], exactly as [`vm`](crate::vm) does, so a build
//! for those targets fails at the first call rather than appearing to work. The POSIX shape is a
//! `SIGSEGV` handler with `SA_SIGINFO` reading `si_addr`, which is a different enough mechanism —
//! signal-safety rules, no equivalent of "continue execution" beyond returning from the handler,
//! and per-thread alternate stacks — that guessing at it here would be worse than leaving it typed.

use crate::vm::OsError;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(not(target_os = "windows"))]
mod unsupported;
#[cfg(not(target_os = "windows"))]
use unsupported as backend;

/// What the faulting instruction was trying to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultAccess {
    /// A load.
    Read,
    /// A store.
    Write,
    /// An instruction fetch from a page with no execute permission (DEP).
    Execute,
}

impl core::fmt::Display for FaultAccess {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            FaultAccess::Read => "read",
            FaultAccess::Write => "write",
            FaultAccess::Execute => "instruction fetch",
        })
    }
}

/// One access violation, as the handler sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    /// The address that could not be accessed.
    pub address: usize,
    /// What was being done to it.
    pub access: FaultAccess,
    /// The instruction pointer of the faulting instruction — inside JIT-generated code when the
    /// fault came from guest code. Diagnostic only: it is not meaningful to Omnidroid's own code,
    /// and it must never be used to decide whether a fault is ours, because a code cache's address
    /// range is not a thing this crate knows.
    pub instruction_pointer: usize,
}

/// What a handler decided about a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum FaultOutcome {
    /// The handler recognised the address, made it accessible, and the faulting instruction should
    /// be retried. Maps to `EXCEPTION_CONTINUE_EXECUTION`.
    ///
    /// Returning this without actually making the address accessible produces an infinite fault
    /// loop, not a crash, which is the harder failure to diagnose of the two — so a handler that is
    /// unsure must return [`NotOurs`](FaultOutcome::NotOurs).
    Resolved,
    /// The handler does not own this address. Dispatch continues to the next vectored handler and
    /// then to frame-based handlers, exactly as if nothing were installed. Maps to
    /// `EXCEPTION_CONTINUE_SEARCH`.
    NotOurs,
}

/// A fault handler: a plain `fn` pointer plus an opaque context.
///
/// Deliberately not a closure and not a trait object. A vectored handler runs with the faulting
/// thread frozen mid-instruction, so the dispatch path must not allocate, must not take a lock this
/// module owns, and must not touch anything whose validity depends on a `Drop` having not yet run.
/// A raw `fn` and a `usize` are the smallest thing that can be published to the handler table with a
/// single atomic store.
///
/// The "`Drop` having not yet run" clause is now *enforced* for the context rather than merely
/// asked for: dropping a [`FaultRegistration`] does not return until this handler is quiescent.
pub type FaultHandler = fn(context: usize, fault: &Fault) -> FaultOutcome;

/// How many handlers can be installed at once.
///
/// One per guest address space, and `ARCHITECTURE.md` §7 gives each guest *instance* its own
/// process, so in production this is 1. The slack is for tests, which build several spaces in one
/// process. A fixed array is what lets the dispatch path be a lock-free scan of a few atomics.
///
/// **Raised from 8 to 32 in Task 4**, because 8 was not in fact enough slack for the thing the
/// slack exists for: `libtest` runs a binary's tests in parallel, and `omni-cpu`'s suites build one
/// guest address space per test, so a binary with ten tests could exhaust the table. The symptom
/// was the worst available — the ninth backend silently ran without demand paging, which puts every
/// guest fault on dynarmic's own handler and its 30-49x recompiled callback path — and it was
/// intermittent, because it depended on how libtest happened to schedule. The table is now refused
/// rather than swallowed by `omni-cpu` as well; see `DynarmicBackend::new`.
pub const MAX_HANDLERS: usize = 32;

/// Counters for the dispatch path, so the ordering claim above can be tested rather than asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaultStats {
    /// Access violations this module's vectored handler examined.
    pub examined: u64,
    /// Of those, how many a handler claimed and resolved.
    pub resolved: u64,
    /// Of those, how many every handler declined, so that dispatch continued elsewhere.
    pub declined: u64,
    /// Releases that found a dispatch still inside the handler and waited for it to return.
    ///
    /// Every one of these would have been a use-after-free under the pre-quiescence contract, so it
    /// is the one counter here that measures a *hazard avoided* rather than work done. It is
    /// legitimately zero on a quiet run, and a non-zero value is not a defect.
    pub drained: u64,
}

/// Result alias for this module.
pub type FaultResult<T> = Result<T, FaultError>;

/// Everything installing a fault handler can refuse (Global Constraint 7).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FaultError {
    /// This target has no implementation.
    #[error(
        "guest fault handling is not implemented on {platform}: omni-platform's {platform} \
         backend is structural only and has never been run. The POSIX shape is a SIGSEGV handler \
         with SA_SIGINFO, which is a different enough mechanism that it needs its own measurement"
    )]
    Unsupported {
        /// The target the backend was compiled for, e.g. `"linux"`.
        platform: &'static str,
    },

    /// [`MAX_HANDLERS`] handlers are already installed.
    ///
    /// A fixed table rather than a growable one because the dispatch path runs inside the OS
    /// exception dispatcher, where allocating is not something to do.
    #[error(
        "all {capacity} guest-fault handler slots are in use; one address space needs one slot, \
         and ARCHITECTURE.md section 7 gives each guest instance its own process"
    )]
    HandlerTableFull {
        /// [`MAX_HANDLERS`].
        capacity: usize,
    },

    /// The OS refused to install the process-wide vectored handler.
    #[error("`AddVectoredExceptionHandler` failed: {source}")]
    Os {
        /// The code the OS returned.
        source: OsError,
    },
}

impl FaultError {
    /// True when this failure means "this backend has no implementation".
    #[must_use]
    pub fn is_unsupported(&self) -> bool {
        matches!(self, FaultError::Unsupported { .. })
    }
}

/// Install a fault handler. It is removed when the returned registration is dropped, and **the drop
/// does not return until the handler is quiescent**.
///
/// The process-wide vectored handler is installed on the first call and is **never removed**, even
/// after the last registration is dropped: removing it would open a window in which a fault could
/// arrive between the last slot being cleared and the handler being unregistered, and the cost of
/// leaving it is one predictable branch on a path that only runs when something has already gone
/// wrong. Individual slots are cleared on drop, so a dropped registration stops receiving faults
/// immediately.
///
/// # Safety
///
/// This is `unsafe` because `context` is an opaque `usize` that `handler` will dereference, and
/// because of *when* `handler` runs. The caller guarantees all of:
///
/// * **`context` stays valid and pinned** until the returned [`FaultRegistration`] is dropped.
///   Nothing here can check that — the whole point of a `usize` is that this module never looks at
///   it — and a handler reading a freed context is a use-after-free inside an exception dispatcher.
///   Dropping the registration is a **quiescence point**: it blocks until every dispatch already
///   inside `handler` has returned, so freeing the context immediately afterwards is sound. Before
///   the drop it is not, whatever the caller's field order is, because the dispatcher is
///   process-wide and a call can be in flight on a thread the caller knows nothing about.
/// * **`handler` does not unwind.** It runs inside the OS exception dispatcher, across a frame
///   boundary with no unwind tables. A Rust panic escaping it is undefined behaviour, not a crash
///   that can be caught, so a handler that can panic must contain its own panics.
/// * **`handler` does not take a lock that the faulting thread might already hold**, does not
///   block, and **returns**. An access violation arrives at an arbitrary instruction, so any lock it
///   takes must be one that is never held across the work that can fault; and since a teardown waits
///   for it, a handler that does not return converts that teardown into a hang.
/// * **`handler` returns [`FaultOutcome::Resolved`] only if the address really was made
///   accessible.** Resolving without fixing anything produces an infinite fault loop rather than a
///   crash, which is the harder of the two to diagnose.
///
/// # Errors
///
/// [`FaultError::Unsupported`], [`FaultError::HandlerTableFull`] or [`FaultError::Os`].
pub unsafe fn install(
    handler: FaultHandler,
    context: usize,
) -> FaultResult<FaultRegistration> {
    backend::install(handler, context)
}

/// Dispatch counters. See [`FaultStats`].
pub fn stats() -> FaultStats {
    backend::stats()
}

/// Whether this target has a real implementation. `false` means every call returns
/// [`FaultError::Unsupported`].
#[must_use]
pub fn available() -> bool {
    backend::AVAILABLE
}

/// A live handler registration. Dropping it stops the handler being called **and waits for any call
/// already in flight to return**.
///
/// Not `Clone`: the slot is released exactly once.
#[derive(Debug)]
pub struct FaultRegistration {
    slot: usize,
}

impl FaultRegistration {
    /// Which slot this registration holds, for diagnostics.
    #[must_use]
    pub fn slot(&self) -> usize {
        self.slot
    }
}

impl Drop for FaultRegistration {
    /// Unpublishes the slot and then **waits until the handler is quiescent**.
    ///
    /// The wait is the whole point, and it is why this is not just a store: see the module docs. The
    /// registrant's `context` may be freed the instant this returns, and may not be freed before it.
    fn drop(&mut self) {
        backend::release(self.slot);
    }
}

/// Construct a registration. Only the backends call this.
fn registration(slot: usize) -> FaultRegistration {
    FaultRegistration { slot }
}
