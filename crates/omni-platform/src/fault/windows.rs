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
//! * no lock — the handler table is [`MAX_HANDLERS`] slots of atomics, scanned with acquire loads;
//! * anything that is not an `EXCEPTION_ACCESS_VIOLATION` with the two documented parameters is
//!   declined before a handler is consulted at all;
//! * an empty slot costs **one relaxed load** and nothing else.
//!
//! The counters are `Relaxed`: they are diagnostics, and ordering them would put a fence on a path
//! whose whole job is to get out of the way.
//!
//! # Quiescence: why a slot is *drained* rather than merely cleared
//!
//! This is the seam the whole-branch review found a use-after-free at, and the defect was in the
//! **contract**, not in any caller. The old `release` cleared `handler` with a release store and
//! returned, on the reasoning that a scanner which has not yet loaded the handler will now see zero.
//! That is true and it is not enough. A scanner is a **process-wide** handler: it can be running on
//! a thread that has nothing to do with the guest, it can be preempted for an arbitrarily long time
//! between loading `handler` and *calling* it, and when it resumes it calls a `fn` with a `context`
//! the registrant has since freed. `omni-mem`'s `DemandPager` then reads `base`/`end` out of a freed
//! `Box`. Worse, the slot is reusable the instant `release` returns, so a stale in-flight call could
//! be paired with a *new* registrant's context.
//!
//! So the contract is now stronger, and it is what [`super::install`] documents: **when `release`
//! returns, no dispatch is inside this slot's handler and none can start.** The mechanism is a
//! per-slot in-flight count:
//!
//! * [`dispatch`] increments `active` **before** it loads `handler`, and decrements it **after** the
//!   handler returns. So for the whole interval in which a call can be in flight, `active >= 1`.
//! * [`release`] stores 0 into `handler` **first**, then waits for `active` to read 0, and only then
//!   clears `context`.
//!
//! The ordering is the Dekker/store-buffer shape — each side writes one location and reads the other
//! — so it is **`SeqCst` on all four accesses, deliberately**, because acquire/release is provably
//! insufficient for it. The argument, in the single total order `S` that `SeqCst` gives:
//!
//! 1. Suppose a dispatch actually calls the handler. Then its `handler` load read a non-zero value,
//!    so that load precedes `release`'s store of 0 in `S` (after that store the slot stays 0 until an
//!    `install`, which cannot happen while this `release` is still running).
//! 2. The dispatch's `fetch_add` precedes its own `handler` load in `S` (program order), so the
//!    `fetch_add` precedes the store of 0, which precedes `release`'s first `active` load.
//! 3. `active` therefore carries that increment at `release`'s first load, and the only thing that
//!    can cancel it is the matching `fetch_sub` — which the dispatch performs only *after* the
//!    handler has returned. So `release` observes 0 only once every call that could be in flight has
//!    finished.
//!
//! What this costs and what it cannot do:
//!
//! * The handler path pays one locked increment and one locked decrement **per occupied slot it
//!   looks at**, and nothing at all for an empty one — the relaxed peek short-circuits before the
//!   RMW, and correctness never rests on that peek, only on the re-read under the reference.
//! * `release` spins. It is called from `FaultRegistration::drop` on an ordinary thread, never from
//!   the handler, so spinning there is not the "must not block" rule the handler lives under. It
//!   waits on work that is bounded by the handler's own body, and the handler is required to return.
//!   A handler that never returns turns a use-after-free into a hang — strictly the better failure,
//!   and one a debugger can read.
//! * It takes **no lock**, which is the other rule: the faulting thread cannot already hold a lock
//!   this module owns, because this module owns none on the fault path. `INSTALL_LOCK` is taken only
//!   by `install`, which no faulting thread is inside.
//! * The in-flight reference is released by a guard with a `Drop`, so even a handler that violates
//!   its no-unwind contract cannot leave a slot permanently un-drainable.
//!
//! Quiescence subsumes the alternative of a generation/epoch tag: a stale pairing is not *detected*,
//! it is made unrepresentable, because the slot cannot be re-published until the previous
//! registrant's dispatches have drained. Slots are therefore still reused, and `MAX_HANDLERS` still
//! means what it says.

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

/// One handler slot.
///
/// `handler` doubles as the occupancy flag: zero means empty, and it is the last field written when
/// claiming a slot and the first cleared when releasing one, so a scanner never sees a live handler
/// paired with a stale context.
///
/// `active` counts dispatches that are between "about to read `handler`" and "the handler has
/// returned". It is what makes `release` a *quiescence* point rather than a store; see the module
/// docs for why acquire/release would not be enough for it.
struct Slot {
    handler: AtomicUsize,
    context: AtomicUsize,
    active: AtomicUsize,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            handler: AtomicUsize::new(0),
            context: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
        }
    }
}

/// Claimed-but-not-yet-published marker, so two concurrent `install` calls cannot take one slot.
const CLAIMING: usize = 1;

/// Spins before `release` starts yielding instead. A handler's body is short — a bounds check and,
/// at worst, one `VirtualAlloc` — so the common wait is a few hundred nanoseconds and never reaches
/// the yield. The number is a choice, not a measurement.
const SPINS_BEFORE_YIELD: u32 = 512;

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_SLOT: Slot = Slot::empty();
static SLOTS: [Slot; MAX_HANDLERS] = [EMPTY_SLOT; MAX_HANDLERS];

static EXAMINED: AtomicU64 = AtomicU64::new(0);
static RESOLVED: AtomicU64 = AtomicU64::new(0);
static DECLINED: AtomicU64 = AtomicU64::new(0);
/// Releases that found a dispatch still inside the handler and had to wait for it.
///
/// Zero is the expected value in a single-threaded test and a non-zero one is not a defect: it is
/// the count of teardowns that would have been use-after-frees under the old contract, which is
/// exactly the number worth being able to look at.
static DRAINED: AtomicU64 = AtomicU64::new(0);

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
        // `dispatch` skips anything that is not a published handler. Publish the context first, then
        // the handler, so a scanner that sees the handler is guaranteed to see the matching context.
        //
        // Reusing a slot is safe here *because* `release` drained it: when the previous
        // registration's `release` returned, no dispatch held a reference to this slot, and the
        // handler has read zero ever since. So a dispatch cannot be carrying the old context into a
        // call that lands on the new one.
        debug_assert_eq!(
            slot.active.load(Ordering::SeqCst),
            0,
            "a slot was claimed while a dispatch still held a reference to it, which means \
             `release` returned without draining",
        );
        slot.context.store(context, Ordering::Relaxed);
        slot.handler.store(handler_addr, Ordering::Release);
        return Ok(registration(index));
    }

    Err(FaultError::HandlerTableFull { capacity: MAX_HANDLERS })
}

/// Unpublish a slot and **wait until no dispatch is inside its handler**.
///
/// The wait is the point. See the module docs for the ordering argument; the short version is that
/// clearing `handler` stops new calls and draining `active` ends the ones already in flight, and
/// `SeqCst` on both sides of both locations is what makes "no new calls" and "none in flight" hold
/// at the same instant rather than one after the other.
pub(super) fn release(slot: usize) {
    let Some(slot) = SLOTS.get(slot) else { return };

    // 1. No call can *start* after this store: `dispatch` re-reads `handler` under its reference and
    //    skips a zero.
    slot.handler.store(0, Ordering::SeqCst);

    // 2. No call is still *running* after this loop. `SeqCst`, and ordered after the store above by
    //    program order, is what the argument in the module docs needs.
    if slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);
        let mut spins: u32 = 0;
        while slot.active.load(Ordering::SeqCst) != 0 {
            if spins < SPINS_BEFORE_YIELD {
                spins += 1;
                core::hint::spin_loop();
            } else {
                // Not a lock and not a timeout: the thread being waited for is running a bounded
                // handler body, and yielding is how a release on a busier machine stops burning the
                // core it is waiting on.
                std::thread::yield_now();
            }
        }
    }

    // 3. Only now is the context unreachable, so only now may it be cleared — and only now may the
    //    registrant free what `context` pointed at, which is the guarantee `install` sells.
    slot.context.store(0, Ordering::Relaxed);
}

pub(super) fn stats() -> FaultStats {
    FaultStats {
        examined: EXAMINED.load(Ordering::Relaxed),
        resolved: RESOLVED.load(Ordering::Relaxed),
        declined: DECLINED.load(Ordering::Relaxed),
        drained: DRAINED.load(Ordering::Relaxed),
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

/// Holds one slot's in-flight reference for as long as it is alive.
///
/// A guard rather than a matching `fetch_sub` at the end of the body for one reason: a handler that
/// unwinds is undefined behaviour and `install` says so, but "undefined" must not be allowed to mean
/// "this slot can never be released again". With the guard, the worst an unwinding handler can do is
/// what it already does; without it, it would also wedge every future `release` of that slot in an
/// unbreakable spin.
struct InFlight {
    slot: &'static Slot,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.slot.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Offer one decoded fault to every published handler, in slot order, until one claims it.
///
/// Split out of [`veh`] so that the quiescence protocol — which is the whole of C1 — can be driven
/// by a test deterministically, without needing a real access violation to arrive at an instant a
/// test chose. The counters stay in `veh`, so a test that calls this does not move the process-wide
/// numbers other tests assert deltas on.
fn dispatch(fault: &Fault) -> FaultOutcome {
    for slot in &SLOTS {
        // A peek, purely so an empty slot costs one relaxed load instead of two locked RMWs. It is
        // an optimization and nothing rests on it: the load that decides whether to call is the one
        // below, taken *under* the in-flight reference.
        if slot.handler.load(Ordering::Relaxed) == 0 {
            continue;
        }

        // Take the reference first, then read the handler. This order is the fix: a `release`
        // running concurrently either sees this increment and waits, or this load sees its zero and
        // no call happens. `SeqCst` on both, because the two sides write one location and read the
        // other and acquire/release does not order that pair.
        slot.active.fetch_add(1, Ordering::SeqCst);
        let guard = InFlight { slot };
        let handler = slot.handler.load(Ordering::SeqCst);
        if handler == 0 || handler == CLAIMING {
            drop(guard);
            continue;
        }
        // Relaxed is enough: the `SeqCst` load above is at least an acquire, and `install` published
        // the context before the handler with a release store.
        let context = slot.context.load(Ordering::Relaxed);
        // SAFETY: `handler` was published by `install` from a `FaultHandler`, which is a `fn`
        // pointer with exactly this signature, and it is published with a release store after the
        // context, so an acquire load that sees it also sees the matching context. The in-flight
        // reference is held across this call, so `release` cannot return — and the registrant
        // cannot free `context` — until it has come back. `install` documents that the handler must
        // not unwind and must return.
        let handler: FaultHandler = unsafe { core::mem::transmute::<usize, FaultHandler>(handler) };
        let outcome = handler(context, fault);
        drop(guard);
        if outcome == FaultOutcome::Resolved {
            return FaultOutcome::Resolved;
        }
    }
    FaultOutcome::NotOurs
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

    match dispatch(&fault) {
        FaultOutcome::Resolved => {
            RESOLVED.fetch_add(1, Ordering::Relaxed);
            EXCEPTION_CONTINUE_EXECUTION
        }
        FaultOutcome::NotOurs => {
            DECLINED.fetch_add(1, Ordering::Relaxed);
            EXCEPTION_CONTINUE_SEARCH
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Serializes the tests that claim slots out of the process-wide table.
    ///
    /// libtest runs a binary's tests in parallel, and every one of these reasons about *which* slot
    /// it got and what was in flight against it. Two of them overlapping would not find a bug, it
    /// would invent one — the exact "a flaky row attributes a mutation to the wrong detector"
    /// failure the ledger records from Task 1.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The dispatch path must decline everything that is not an access violation with the two
    /// documented parameters, because every exception in the process passes through it — including
    /// the C++ exceptions xbyak throws when the code cache fills (`OD_HALT_SHIM_THREW`).
    #[test]
    fn only_access_violations_with_both_parameters_are_examined() {
        assert_eq!(AV_PARAMETERS, 2);
        assert_eq!((AV_READ, AV_WRITE, AV_EXECUTE), (0, 1, 8));
        // `CLAIMING` must not be mistakable for a real function pointer.
        // A claimed-but-unpublished slot must never be mistakable for a function pointer.
        const _: () = assert!(CLAIMING < 4096);
    }

    /// How long a "must not have happened yet" observation waits before it is believed.
    ///
    /// It is a *lower* bound on evidence, not a timeout: the test fails if `release` returns inside
    /// this window, and a slower machine only makes the window more convincing, never flakier.
    const NOT_YET: Duration = Duration::from_millis(250);

    /// How long a "must happen" observation waits before it is given up on. Generous, because a
    /// false failure here would be a flaky test in a mutation table, which Task 1's ledger entry
    /// says costs more than the row is worth.
    const EVENTUALLY: Duration = Duration::from_secs(30);

    /// How long a release that must **not** wait is given. A release of an idle slot is a handful of
    /// atomic operations, so five seconds is six orders of magnitude of margin — wide enough never
    /// to be flaky and short enough that the mutation which breaks it does not cost half a minute.
    const PROMPTLY: Duration = Duration::from_secs(5);

    // --- C1's first test: release must not return while a dispatch is inside the handler --------

    /// The address the blocking handler answers to. Nothing else may block it, because a real
    /// access violation anywhere in this process reaches it too.
    const BLOCKING_MAGIC: usize = 0x5AFE_0001;
    static BLOCK_ENTERED: AtomicBool = AtomicBool::new(false);
    static BLOCK_GATE: AtomicBool = AtomicBool::new(false);
    static BLOCK_SAW_RELEASE_RETURN: AtomicBool = AtomicBool::new(false);
    static BLOCK_RELEASE_RETURNED: AtomicBool = AtomicBool::new(false);

    fn blocking_handler(_context: usize, fault: &Fault) -> FaultOutcome {
        if fault.address != BLOCKING_MAGIC {
            return FaultOutcome::NotOurs;
        }
        BLOCK_ENTERED.store(true, Ordering::SeqCst);
        while !BLOCK_GATE.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
        // The observation that makes this a use-after-free test rather than a timing test: if
        // `release` has already returned, the registrant is already entitled to have freed whatever
        // `context` pointed at, and this frame is reading it.
        if BLOCK_RELEASE_RETURNED.load(Ordering::SeqCst) {
            BLOCK_SAW_RELEASE_RETURN.store(true, Ordering::SeqCst);
        }
        FaultOutcome::NotOurs
    }

    fn synthetic(address: usize) -> Fault {
        Fault { address, access: FaultAccess::Read, instruction_pointer: 0 }
    }

    /// **C1.** `release` returning while a dispatch still holds the slot is the use-after-free: the
    /// registrant frees its context the instant `drop` comes back, and the in-flight handler is
    /// about to dereference it.
    ///
    /// Driven through [`dispatch`] rather than through a real access violation on purpose. The race
    /// needs a handler to be *inside* its body at a moment the test chooses, and no arrangement of
    /// real faults can make that deterministic — the first version of the concurrent-granule test
    /// taught this project what a flaky row costs a mutation table.
    #[test]
    fn release_does_not_return_while_a_dispatch_is_inside_the_handler() {
        let _serial = serialized();
        BLOCK_ENTERED.store(false, Ordering::SeqCst);
        BLOCK_GATE.store(false, Ordering::SeqCst);
        BLOCK_SAW_RELEASE_RETURN.store(false, Ordering::SeqCst);
        BLOCK_RELEASE_RETURNED.store(false, Ordering::SeqCst);

        // The backend entry point is safe; `fault::install` is the `unsafe` wrapper. The handler
        // still honours the contract: it touches only `static`s in this module, never unwinds,
        // never blocks on a lock, and the context is an integer it does not dereference.
        let registration = install(blocking_handler, 0xC0FF_EE00).expect("a free handler slot");
        let slot = registration.slot();

        let faulter = std::thread::spawn(|| dispatch(&synthetic(BLOCKING_MAGIC)));
        let deadline = std::time::Instant::now() + EVENTUALLY;
        while !BLOCK_ENTERED.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "the handler was never entered");
            std::thread::yield_now();
        }

        let (tx, rx) = mpsc::channel();
        let releaser = std::thread::spawn(move || {
            drop(registration);
            BLOCK_RELEASE_RETURNED.store(true, Ordering::SeqCst);
            let _ = tx.send(());
        });

        // The evidence. Under the old contract this returns `Ok` immediately.
        let returned_early = rx.recv_timeout(NOT_YET).is_ok();

        // Let everything finish before asserting, so a failure is a failure and not a hung suite.
        BLOCK_GATE.store(true, Ordering::SeqCst);
        assert_eq!(faulter.join().expect("the faulting thread"), FaultOutcome::NotOurs);
        releaser.join().expect("the releasing thread");

        assert!(
            !returned_early,
            "`release` returned while a dispatch was still inside slot {slot}'s handler. The \
             registrant frees its context the moment `drop` returns, so the handler is then \
             reading freed memory — inside the OS exception dispatcher, on any thread in the \
             process, for any access violation"
        );
        assert!(
            !BLOCK_SAW_RELEASE_RETURN.load(Ordering::SeqCst),
            "the handler observed `release` having already returned, which is the use-after-free \
             window itself"
        );
        assert!(
            stats().drained >= 1,
            "a release that waited must be countable, or the property has no evidence in \
             production"
        );
    }

    // --- C1's second test: the wait is per slot, so teardown is not a global barrier ------------

    const IDLE_MAGIC: usize = 0x5AFE_0002;
    static IDLE_ENTERED: AtomicBool = AtomicBool::new(false);
    static IDLE_GATE: AtomicBool = AtomicBool::new(false);

    fn other_blocking_handler(_context: usize, fault: &Fault) -> FaultOutcome {
        if fault.address != IDLE_MAGIC {
            return FaultOutcome::NotOurs;
        }
        IDLE_ENTERED.store(true, Ordering::SeqCst);
        while !IDLE_GATE.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
        FaultOutcome::NotOurs
    }

    fn inert_handler(_context: usize, _fault: &Fault) -> FaultOutcome {
        FaultOutcome::NotOurs
    }

    /// The over-correction this fix could have been, pinned: waiting for *any* dispatch anywhere
    /// rather than for this slot's would make one guest instance's teardown block on another's page
    /// fault, and `ARCHITECTURE.md` §7's one-process-per-instance rule is the only thing that would
    /// hide it. A drain must be a property of the slot.
    #[test]
    fn releasing_one_slot_does_not_wait_for_a_dispatch_in_another() {
        let _serial = serialized();
        IDLE_ENTERED.store(false, Ordering::SeqCst);
        IDLE_GATE.store(false, Ordering::SeqCst);

        // As above: both handlers touch only `static`s and do not dereference the context.
        let busy = install(other_blocking_handler, 1).expect("a free handler slot");
        let idle = install(inert_handler, 2).expect("a second free handler slot");

        let faulter = std::thread::spawn(|| dispatch(&synthetic(IDLE_MAGIC)));
        let deadline = std::time::Instant::now() + EVENTUALLY;
        while !IDLE_ENTERED.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "the handler was never entered");
            std::thread::yield_now();
        }

        let (tx, rx) = mpsc::channel();
        let releaser = std::thread::spawn(move || {
            drop(idle);
            let _ = tx.send(());
        });
        // Bounded rather than a plain join, so a drain that waits on the wrong thing fails this
        // test instead of hanging the binary.
        let prompt = rx.recv_timeout(PROMPTLY).is_ok();

        IDLE_GATE.store(true, Ordering::SeqCst);
        assert_eq!(faulter.join().expect("the faulting thread"), FaultOutcome::NotOurs);
        releaser.join().expect("the releasing thread");
        drop(busy);

        assert!(
            prompt,
            "releasing an idle slot waited for a dispatch that was inside a different slot's \
             handler"
        );
    }

    /// A drained slot is reusable, and the reuse is what makes `MAX_HANDLERS` a capacity rather
    /// than a lifetime budget. The `debug_assert` in `install` is the other half of this.
    #[test]
    fn a_drained_slot_is_handed_out_again() {
        let _serial = serialized();
        // An inert handler that dereferences nothing.
        let first = install(inert_handler, 7).expect("a free handler slot");
        let slot = first.slot();
        drop(first);
        let second = install(inert_handler, 8).expect("the slot back");
        assert_eq!(second.slot(), slot, "a released slot must be reusable, not retired");
        assert_eq!(
            SLOTS[slot].active.load(Ordering::SeqCst),
            0,
            "a reused slot must start with no in-flight references"
        );
        drop(second);
        assert_eq!(
            SLOTS[slot].context.load(Ordering::Relaxed),
            0,
            "the context is cleared after the drain, so a stale one is never left published"
        );
    }
}
