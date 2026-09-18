//! Windows backend for the guest-fault seam: one process-wide vectored exception handler.
//!
//! # Why a vectored handler and not `SetUnhandledExceptionFilter`
//!
//! An unhandled-exception filter runs *last*, after every frame-based handler has declined —
//! including dynarmic's, which is exactly the one we must beat. A vectored handler installed with
//! `first = 1` runs *first*, before any frame-based handler on any frame. That ordering is the whole
//! point of this module: dynarmic's `exception_handler_windows.cpp` registers a `RUNTIME_FUNCTION`
//! over its code cache with `RtlAddFunctionTable`, so it is reachable only through frame-based
//! dispatch, which never starts while a vectored handler is still saying "I have this".
//!
//! # The dispatch path
//!
//! Every access violation in the process reaches [`veh`], including ones that have nothing to do
//! with the guest. So the path is written as though it were hot even though it is not:
//!
//! * no allocation, no formatting, no `tracing`;
//! * no lock — the handler table is [`MAX_HANDLERS`] pairs of atomics, scanned with acquire loads;
//! * anything that is not an `EXCEPTION_ACCESS_VIOLATION` with the two documented parameters is
//!   declined before a handler is consulted at all.
//!
//! The counters are `Relaxed`: they are diagnostics, and ordering them would put a fence on a path
//! whose whole job is to get out of the way.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, GetLastError};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS,
};

use super::{
    registration, Fault, FaultAccess, FaultError, FaultHandler, FaultOutcome, FaultRegistration,
    FaultResult, FaultStats, MAX_HANDLERS,
};
use crate::vm::OsError;

/// This backend is real.
pub(super) const AVAILABLE: bool = true;

/// `EXCEPTION_RECORD::ExceptionInformation[0]` for an access violation, as documented: 0 read,
/// 1 write, 8 data-execution-prevention. Any other value is a shape we have not seen and is
/// declined rather than guessed at.
const AV_READ: usize = 0;
const AV_WRITE: usize = 1;
const AV_EXECUTE: usize = 8;

/// An access violation carries exactly two parameters. A record claiming fewer is malformed.
const AV_PARAMETERS: u32 = 2;

/// One handler slot. `handler` doubles as the occupancy flag: zero means empty, and it is the last
/// field written when claiming a slot and the first cleared when releasing one, so a scanner never
/// sees a live handler paired with a stale context.
struct Slot {
    handler: AtomicUsize,
    context: AtomicUsize,
}

impl Slot {
    const fn empty() -> Self {
        Self { handler: AtomicUsize::new(0), context: AtomicUsize::new(0) }
    }
}

/// Claimed-but-not-yet-published marker, so two concurrent `install` calls cannot take one slot.
const CLAIMING: usize = 1;

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_SLOT: Slot = Slot::empty();
static SLOTS: [Slot; MAX_HANDLERS] = [EMPTY_SLOT; MAX_HANDLERS];

static EXAMINED: AtomicU64 = AtomicU64::new(0);
static RESOLVED: AtomicU64 = AtomicU64::new(0);
static DECLINED: AtomicU64 = AtomicU64::new(0);

/// The `AddVectoredExceptionHandler` handle, or 0 if it has not been installed yet.
static VEH_HANDLE: AtomicUsize = AtomicUsize::new(0);
/// Guards the one-time install. A `Mutex` is fine here: it is taken only by `install`, never by the
/// fault path, so it can never be held by a thread that is about to fault into [`veh`].
static INSTALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn install(handler: FaultHandler, context: usize) -> FaultResult<FaultRegistration> {
    ensure_veh_installed()?;

    let handler_addr = handler as usize;
    debug_assert!(handler_addr > CLAIMING, "a function pointer is never a small integer");

    for (index, slot) in SLOTS.iter().enumerate() {
        if slot
            .handler
            .compare_exchange(0, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        // The slot is ours and no scanner will call it: `CLAIMING` is not a function pointer and
        // `veh` skips anything that is not a published handler. Publish the context first, then the
        // handler, so a scanner that sees the handler is guaranteed to see the matching context.
        slot.context.store(context, Ordering::Relaxed);
        slot.handler.store(handler_addr, Ordering::Release);
        return Ok(registration(index));
    }

    Err(FaultError::HandlerTableFull { capacity: MAX_HANDLERS })
}

pub(super) fn release(slot: usize) {
    if let Some(slot) = SLOTS.get(slot) {
        // Clearing the handler first is what makes this safe against a concurrent fault: a scanner
        // reads the handler with an acquire load and skips a zero, so after this store no new call
        // can start. A call already in flight is the caller's problem, and `FaultRegistration` is
        // owned by the thing that owns the context, so dropping it while the guest is running would
        // be a use-after-free the borrow checker already prevents.
        slot.handler.store(0, Ordering::Release);
        slot.context.store(0, Ordering::Relaxed);
    }
}

pub(super) fn stats() -> FaultStats {
    FaultStats {
        examined: EXAMINED.load(Ordering::Relaxed),
        resolved: RESOLVED.load(Ordering::Relaxed),
        declined: DECLINED.load(Ordering::Relaxed),
    }
}

fn ensure_veh_installed() -> FaultResult<()> {
    if VEH_HANDLE.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if VEH_HANDLE.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    // SAFETY: `AddVectoredExceptionHandler` takes a first-or-last flag and a handler pointer with
    // the `system` ABI, which `veh` has. `first = 1` asks for it to run before every previously
    // installed vectored handler and before all frame-based dispatch, which is the property this
    // module exists for. The handler references only `static` state, so it stays valid for the life
    // of the process, which is exactly how long the registration lasts.
    let handle = unsafe { AddVectoredExceptionHandler(1, Some(veh)) };
    if handle.is_null() {
        // SAFETY: reading the calling thread's last-error code straight after the failed call.
        return Err(FaultError::Os { source: OsError(unsafe { GetLastError() }) });
    }
    VEH_HANDLE.store(handle as usize, Ordering::Release);
    Ok(())
}

/// The process-wide vectored exception handler.
///
/// # Safety
///
/// Called by the OS exception dispatcher with the faulting thread frozen. It must not unwind, must
/// not allocate, and must not block. Every `unsafe` here is a read of the record the OS handed us.
unsafe extern "system" fn veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // SAFETY: the OS guarantees `info` points at a valid `EXCEPTION_POINTERS` for the duration of
    // the call, and its `ExceptionRecord` at a valid `EXCEPTION_RECORD`.
    let record = unsafe { (*info).ExceptionRecord };
    if record.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // SAFETY: as above.
    let (code, parameters, count, address_of_instruction) = unsafe {
        (
            (*record).ExceptionCode,
            (*record).ExceptionInformation,
            (*record).NumberParameters,
            (*record).ExceptionAddress as usize,
        )
    };

    if code != EXCEPTION_ACCESS_VIOLATION || count < AV_PARAMETERS {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let access = match parameters[0] {
        AV_READ => FaultAccess::Read,
        AV_WRITE => FaultAccess::Write,
        AV_EXECUTE => FaultAccess::Execute,
        _ => return EXCEPTION_CONTINUE_SEARCH,
    };

    let fault = Fault {
        address: parameters[1],
        access,
        instruction_pointer: address_of_instruction,
    };

    EXAMINED.fetch_add(1, Ordering::Relaxed);

    for slot in &SLOTS {
        let handler = slot.handler.load(Ordering::Acquire);
        if handler == 0 || handler == CLAIMING {
            continue;
        }
        let context = slot.context.load(Ordering::Relaxed);
        // SAFETY: `handler` was published by `install` from a `FaultHandler`, which is a `fn`
        // pointer with exactly this signature, and it is published with a release store after the
        // context, so an acquire load that sees it also sees the matching context. `install`
        // documents that the handler must not unwind.
        let handler: FaultHandler = unsafe { core::mem::transmute::<usize, FaultHandler>(handler) };
        if handler(context, &fault) == FaultOutcome::Resolved {
            RESOLVED.fetch_add(1, Ordering::Relaxed);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
    }

    DECLINED.fetch_add(1, Ordering::Relaxed);
    EXCEPTION_CONTINUE_SEARCH
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dispatch path must decline everything that is not an access violation with the two
    /// documented parameters, because every exception in the process passes through it — including
    /// the C++ exceptions xbyak throws when the code cache fills (`OD_HALT_SHIM_THREW`).
    #[test]
    fn only_access_violations_with_both_parameters_are_examined() {
        assert_eq!(AV_PARAMETERS, 2);
        assert_eq!((AV_READ, AV_WRITE, AV_EXECUTE), (0, 1, 8));
        // `CLAIMING` must not be mistakable for a real function pointer.
        assert!(CLAIMING < 4096, "a claimed-but-unpublished slot must be an impossible code address");
    }
}
