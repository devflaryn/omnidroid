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
//!
//! # Scope
//!
//! Implemented and measured on Windows. On Linux and macOS every entry point returns
//! [`VmError::Unsupported`](vm::VmError::Unsupported), exactly as [`vm`](crate::vm) does, so a build
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
pub type FaultHandler = fn(context: usize, fault: &Fault) -> FaultOutcome;

/// How many handlers can be installed at once.
///
/// One per guest address space, and `ARCHITECTURE.md` §7 gives each guest *instance* its own
/// process, so in production this is 1. The slack is for tests, which build several spaces in one
/// process. A fixed array is what lets the dispatch path be a lock-free scan of a few atomics.
pub const MAX_HANDLERS: usize = 8;

/// Counters for the dispatch path, so the ordering claim above can be tested rather than asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaultStats {
    /// Access violations this module's vectored handler examined.
    pub examined: u64,
    /// Of those, how many a handler claimed and resolved.
    pub resolved: u64,
    /// Of those, how many every handler declined, so that dispatch continued elsewhere.
    pub declined: u64,
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

/// Install a fault handler. It is removed when the returned registration is dropped.
///
/// The process-wide vectored handler is installed on the first call and is **never removed**, even
/// after the last registration is dropped: removing it would open a window in which a fault could
/// arrive between the last slot being cleared and the handler being unregistered, and the cost of
/// leaving it is one predictable branch on a path that only runs when something has already gone
/// wrong. Individual slots are cleared on drop, so a dropped registration stops receiving faults
/// immediately.
///
/// # Errors
///
/// [`FaultError::Unsupported`] on a target with no implementation, [`FaultError::HandlerTableFull`]
/// if [`MAX_HANDLERS`] are already installed, or [`FaultError::Os`] if the OS refused the handler.
///
/// # Panics
///
/// Never. But `handler` itself must not panic: it runs inside the OS exception dispatcher, where
/// unwinding is undefined behaviour.
pub fn install(handler: FaultHandler, context: usize) -> FaultResult<FaultRegistration> {
    backend::install(handler, context)
}

/// Dispatch counters. See [`FaultStats`].
pub fn stats() -> FaultStats {
    backend::stats()
}

/// Whether this target has a real implementation. `false` means every call returns
/// [`VmError::Unsupported`].
#[must_use]
pub fn available() -> bool {
    backend::AVAILABLE
}

/// A live handler registration. Dropping it stops the handler being called.
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
    fn drop(&mut self) {
        backend::release(self.slot);
    }
}

/// Construct a registration. Only the backends call this.
fn registration(slot: usize) -> FaultRegistration {
    FaultRegistration { slot }
}
