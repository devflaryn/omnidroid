//! Linux backend for the guest-fault seam: a process-wide `SIGSEGV`/`SIGBUS` handler that keeps
//! **first place** in the chain.
//!
//! # What "first" means on Linux, and why it is not automatic
//!
//! On Windows every vectored handler runs before any frame-based one, so a VEH installed at any time
//! sees a fault before dynarmic's `RtlAddFunctionTable` handler. POSIX has no such ordering: a
//! signal has exactly **one** disposition, the last `sigaction` wins, and anyone before it runs only
//! if the winner chooses to call it. dynarmic's POSIX handler
//! (`backend/exception_handler_posix.cpp`) is installed **lazily, once per process, when the first
//! `Jit` is constructed**, saving whatever it displaced; for a fault whose RIP is inside its code
//! cache it takes the fastmem fallback -- recompiles the block onto the callback path, the 30-49x
//! degradation D4 amendment 2 guards against -- and **never chains**. `omni-cpu` installs the demand
//! pager before its first jit exists, so an ordinary `sigaction` here puts dynarmic in front of us
//! the moment the first guest thread is created, and every demand-paged guest access in JIT code is
//! then served by dynarmic's slow path. Correct results, silently slower: the exact failure mode of
//! D4 amendment 2, reached by a route the startup assertion cannot see.
//!
//! So first place is **re-asserted**: [`reassert_precedence`] reads the current disposition and, if
//! it is not this module's handler, installs this module's handler again on top of it and chains to
//! it. `omni-cpu` calls it straight after every `od_jit_new`. It is idempotent and costs one
//! `sigaction` query per signal when nothing has changed.
//!
//! # The chain, and the loop it has to break
//!
//! After a re-assertion the chain is: **this handler -> dynarmic's -> (dynarmic's saved "old") ->
//! ...**, and dynarmic's saved "old" is *this handler*, from before dynarmic was installed. A naive
//! chain therefore loops: this handler declines, calls dynarmic's, which (not its code) calls its
//! old, which is this handler, which declines again, and so on until the stack is gone.
//!
//! The loop is broken with a per-thread chain depth. When this handler forwards a fault it has
//! declined, it marks the thread as "chaining"; if it is entered again while the mark is set, it is
//! being called *as somebody else's predecessor*, so it does not dispatch a second time -- it
//! forwards straight to the disposition that was in place before this module first installed itself
//! (Rust's own stack-overflow handler, normally). The result is exactly the chain a Windows process
//! has: Omnidroid's handler first, dynarmic's second, the runtime's own last.
//!
//! Forwarding follows dynarmic's `exception_handler_posix.cpp`, and the kernel's own rules where
//! those are stricter: an `SA_SIGINFO` action is called with the three arguments; a plain
//! `sa_handler` is called with the signal; `SIG_IGN` for a *kernel* fault is what the kernel itself
//! does not honour (a fault it cannot deliver is fatal), so it is treated as `SIG_DFL`; and
//! `SIG_DFL` resets the disposition and returns, so the faulting instruction faults again and the
//! default action -- termination -- happens at the real fault, with the real registers in the core.
//! A `SIGSEGV` that was *sent* (`kill`, `si_code <= 0`) is re-raised after the reset instead,
//! because returning would simply lose it. The forwarded action's `sa_mask` is blocked for the
//! duration of the call, as delivery would have blocked it.
//!
//! # The dispatch path, and signal safety
//!
//! The slot table and its quiescence protocol are **the same protocol as `fault/windows.rs`, copied
//! and not changed**: a per-slot in-flight count taken before the handler pointer is read, a
//! `DRAINING` marker so a slot being drained cannot be handed out, and `SeqCst` on the four accesses
//! that form the store-buffer shape. The argument is written out there and applies unchanged; the
//! same three tests pin it at the bottom of this file.
//!
//! What is Linux-specific is **what may run inside the handler**. POSIX allows only
//! async-signal-safe functions in a signal handler, and neither the pager behind this seam
//! (`omni-mem`'s `DemandPager`: a `parking_lot` mutex, a `BTreeMap`, `catch_unwind`) nor the
//! `vm` ledger (a `Mutex<BTreeMap>`) is async-signal-safe in that sense. What makes them sound
//! here is a narrower property, and it is worth stating exactly because it is the whole argument:
//!
//! * **The signals are synchronous.** A `SIGSEGV`/`SIGBUS` that this module dispatches is a
//!   kernel-generated page fault (`si_code > 0`, trap 14), delivered to the thread that faulted,
//!   *at* the faulting instruction. A signal sent with `kill` is declined before any handler sees
//!   it. So the code interrupted is always the faulting instruction's, never an arbitrary point.
//! * **The faulting instruction is guest code, or host code reading guest memory.** Neither is
//!   inside the allocator or holding the space's lock or the ledger's lock -- the pager's invariant
//!   (`omni-mem/src/pager.rs`, "the thread running guest code must not hold this space's internal
//!   lock") is exactly the condition that makes taking those locks here deadlock-free, and it is
//!   the same condition the Windows VEH already depends on, because a VEH also runs on the faulting
//!   thread.
//! * **So the one hazard is a fault *inside* the allocator or under one of those locks**, which is
//!   a host defect (heap corruption), not guest input -- and the process is lost either way.
//!
//! That is the same bargain the Windows backend makes, and nothing more. What the handler itself
//! does is async-signal-safe: atomics, `sigaction`, `pthread_sigmask`, `raise`, and `errno` is saved
//! and restored around the whole body so an interrupted `errno` read is not corrupted.
//!
//! # The alternate stack
//!
//! Installed `SA_ONSTACK`, so a fault that is a **stack overflow** can still be delivered -- on the
//! alternate stack -- and forwarded to Rust's own handler, which prints which thread overflowed. A
//! thread with no alternate stack simply runs the handler on its own stack, which is fine for every
//! fault but an overflow. Rust gives every `std::thread` an alternate stack when its own handler is
//! installed at startup (it is, in every Rust binary), sized `max(SIGSTKSZ, AT_MINSIGSTKSZ)`, which
//! is 8 KiB on this host plus a guard page; dynarmic gives the thread that builds the first jit a
//! 2 MiB one. The pager path's real need was **measured** against that, not assumed:
//! `tests/fault_linux.rs` paints an alternate stack, serves a real demand fault through the whole
//! `DemandPager` path on it, and reads the high-water mark back. The figure is in
//! `docs/ports/linux-notes/mem.md`.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;

use super::{
    registration, Fault, FaultAccess, FaultError, FaultHandler, FaultOutcome, FaultRegistration,
    FaultResult, FaultStats, MAX_HANDLERS,
};
use crate::vm::OsError;

/// This backend is real.
pub(super) const AVAILABLE: bool = true;

// -------------------------------------------------------------------------------------------
// The slot table and its quiescence protocol: fault/windows.rs's, unchanged. See that file for the
// ordering argument; the comments here say only where the code is.
// -------------------------------------------------------------------------------------------

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

/// Claimed-but-not-yet-published marker.
const CLAIMING: usize = 1;
/// Being drained: unpublished and unclaimable until [`release`]'s last store.
const DRAINING: usize = 2;
/// Every slot value at or below this is a state marker rather than a handler pointer.
const MAX_MARKER: usize = DRAINING;
/// Spins before `release` starts yielding. A choice, not a measurement, as on Windows.
const SPINS_BEFORE_YIELD: u32 = 512;

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_SLOT: Slot = Slot::empty();
static SLOTS: [Slot; MAX_HANDLERS] = [EMPTY_SLOT; MAX_HANDLERS];

static EXAMINED: AtomicU64 = AtomicU64::new(0);
static RESOLVED: AtomicU64 = AtomicU64::new(0);
static DECLINED: AtomicU64 = AtomicU64::new(0);
static DRAINED: AtomicU64 = AtomicU64::new(0);

pub(super) fn install(handler: FaultHandler, context: usize) -> FaultResult<FaultRegistration> {
    ensure_installed()?;

    let handler_addr = handler as usize;
    debug_assert!(handler_addr > MAX_MARKER, "a function pointer is never a small integer");

    for (index, slot) in SLOTS.iter().enumerate() {
        if slot
            .handler
            .compare_exchange(0, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        // As fault/windows.rs: the slot cannot be the previous registration's mid-drain, because
        // the only store of 0 is `release`'s last action.
        slot.context.store(context, Ordering::Relaxed);
        slot.handler.store(handler_addr, Ordering::Release);
        return Ok(registration(index));
    }

    Err(FaultError::HandlerTableFull { capacity: MAX_HANDLERS })
}

/// Unpublish, drain, then free. fault/windows.rs's `release`, step for step.
pub(super) fn release(slot: usize) {
    let Some(slot) = SLOTS.get(slot) else { return };
    slot.handler.store(DRAINING, Ordering::SeqCst);
    if slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);
        let mut spins: u32 = 0;
        while slot.active.load(Ordering::SeqCst) != 0 {
            if spins < SPINS_BEFORE_YIELD {
                spins += 1;
                core::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }
    slot.context.store(0, Ordering::Relaxed);
    slot.handler.store(0, Ordering::Release);
}

pub(super) fn stats() -> FaultStats {
    FaultStats {
        examined: EXAMINED.load(Ordering::Relaxed),
        resolved: RESOLVED.load(Ordering::Relaxed),
        declined: DECLINED.load(Ordering::Relaxed),
        drained: DRAINED.load(Ordering::Relaxed),
    }
}

struct InFlight {
    slot: &'static Slot,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.slot.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Offer one decoded fault to every published handler, in slot order. fault/windows.rs's
/// `dispatch`, unchanged: a relaxed peek, then the in-flight reference, then the `SeqCst` re-read.
fn dispatch(fault: &Fault) -> FaultOutcome {
    for slot in &SLOTS {
        if slot.handler.load(Ordering::Relaxed) <= MAX_MARKER {
            continue;
        }
        slot.active.fetch_add(1, Ordering::SeqCst);
        let guard = InFlight { slot };
        let handler = slot.handler.load(Ordering::SeqCst);
        if handler <= MAX_MARKER {
            drop(guard);
            continue;
        }
        let context = slot.context.load(Ordering::Relaxed);
        // SAFETY: as fault/windows.rs: `handler` was published by `install` from a `FaultHandler`
        // after its context, and the in-flight reference keeps `release` from returning until this
        // call has come back.
        let handler: FaultHandler = unsafe { core::mem::transmute::<usize, FaultHandler>(handler) };
        let outcome = handler(context, fault);
        drop(guard);
        if outcome == FaultOutcome::Resolved {
            return FaultOutcome::Resolved;
        }
    }
    FaultOutcome::NotOurs
}

// -------------------------------------------------------------------------------------------
// The signal side
// -------------------------------------------------------------------------------------------

/// The two signals a guest memory fault can arrive as: `SIGSEGV` for an unmapped or protected page,
/// `SIGBUS` for a file-backed page past the end of its file.
const SIGNALS: [libc::c_int; 2] = [libc::SIGSEGV, libc::SIGBUS];

fn signal_index(signal: libc::c_int) -> Option<usize> {
    SIGNALS.iter().position(|&s| s == signal)
}

fn signal_name(index: usize) -> &'static str {
    ["SIGSEGV", "SIGBUS"][index]
}

/// How many dispositions per signal this module will remember: the one it first displaced, and one
/// per re-assertion that found somebody else on top. In a process that behaves there are two (Rust's
/// handler, then dynarmic's). Running out means something keeps displacing this handler, which is
/// reported as [`FaultError::PrecedenceContested`] rather than papered over.
const SNAPSHOTS: usize = 16;

/// A disposition that some forward may still read, written once and never again.
struct Snapshot(UnsafeCell<MaybeUninit<libc::sigaction>>);

// SAFETY: each cell is written exactly once, under `INSTALL_LOCK`, *before* its index is published
// with a release store; every reader loads the index with acquire first and only reads cells whose
// index it has seen. No cell is ever written after publication.
unsafe impl Sync for Snapshot {}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_SNAPSHOT: Snapshot = Snapshot(UnsafeCell::new(MaybeUninit::uninit()));
#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_ROW: [Snapshot; SNAPSHOTS] = [EMPTY_SNAPSHOT; SNAPSHOTS];
static SAVED: [[Snapshot; SNAPSHOTS]; 2] = [EMPTY_ROW, EMPTY_ROW];

/// How many snapshots of each signal are written. Only `INSTALL_LOCK`'s holder writes it.
static SAVED_COUNT: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

/// The snapshot a declined fault is forwarded to: the disposition this handler most recently
/// displaced. `usize::MAX` until the first install has recorded one.
static NEXT: [AtomicUsize; 2] = [AtomicUsize::new(usize::MAX), AtomicUsize::new(usize::MAX)];

/// Snapshot 0 of each signal is the disposition in place before this module ever installed itself:
/// what a re-entered handler forwards to. It is always index 0.
const ORIGINAL: usize = 0;

static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Taken only by `install` and `reassert_precedence`, never on the fault path, so a thread that is
/// about to fault can never be holding it.
static INSTALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

std::thread_local! {
    /// Set while this thread is forwarding a fault this module declined. Const-initialised and
    /// without a destructor, so reading it from a signal handler is a plain TLS load.
    static CHAINING: Cell<u32> = const { Cell::new(0) };
}

fn record_snapshot(index: usize, action: &libc::sigaction) -> FaultResult<usize> {
    let count = SAVED_COUNT[index].load(Ordering::Relaxed);
    if count >= SNAPSHOTS {
        return Err(FaultError::PrecedenceContested {
            signal: signal_name(index),
            displacements: count,
        });
    }
    // SAFETY: under INSTALL_LOCK; this cell has never been written or published (see `Snapshot`).
    unsafe { (*SAVED[index][count].0.get()).write(*action) };
    SAVED_COUNT[index].store(count + 1, Ordering::Release);
    Ok(count)
}

fn our_action() -> libc::sigaction {
    // SAFETY: `sigaction` is plain data; all-zero is a valid (empty-mask, no-flag) value that every
    // field below is then set on.
    let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
    action.sa_sigaction = on_signal as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
    // Both fault signals are blocked while the handler runs, so a fault *inside* it is fatal at
    // once (the kernel forces the default action on a blocked synchronous fault) instead of
    // recursing through the chain. The pager's re-entrancy guard is the second line.
    // SAFETY: `sa_mask` is a live sigset_t owned by `action`.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaddset(&mut action.sa_mask, libc::SIGSEGV);
        libc::sigaddset(&mut action.sa_mask, libc::SIGBUS);
    }
    action
}

fn query(signal: libc::c_int, index: usize) -> FaultResult<libc::sigaction> {
    // SAFETY: plain data, filled in by the call.
    let mut current: libc::sigaction = unsafe { core::mem::zeroed() };
    // SAFETY: a NULL new action only reads the current disposition into `current`.
    if unsafe { libc::sigaction(signal, core::ptr::null(), &mut current) } != 0 {
        return Err(sigaction_failed(index));
    }
    Ok(current)
}

fn sigaction_failed(index: usize) -> FaultError {
    FaultError::Signal {
        signal: signal_name(index),
        source: OsError(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32),
    }
}

fn is_ours(action: &libc::sigaction) -> bool {
    action.sa_sigaction == on_signal as *const () as usize
}

/// Put this handler on top of `signal`'s chain, recording what it displaces as the next link.
/// Caller holds `INSTALL_LOCK`.
fn put_on_top(signal: libc::c_int, index: usize) -> FaultResult<()> {
    let current = query(signal, index)?;
    if is_ours(&current) {
        return Ok(());
    }
    let next = record_snapshot(index, &current)?;
    NEXT[index].store(next, Ordering::Release);
    let ours = our_action();
    // SAFETY: plain data, filled in by the call.
    let mut displaced: libc::sigaction = unsafe { core::mem::zeroed() };
    // SAFETY: `ours` is a fully initialised action whose handler is `on_signal`, which has the
    // SA_SIGINFO signature and references only statics.
    if unsafe { libc::sigaction(signal, &ours, &mut displaced) } != 0 {
        return Err(sigaction_failed(index));
    }
    // Somebody may have installed between the query and the swap. Then *that* is the next link.
    if displaced.sa_sigaction != current.sa_sigaction || displaced.sa_flags != current.sa_flags {
        let next = record_snapshot(index, &displaced)?;
        NEXT[index].store(next, Ordering::Release);
    }
    Ok(())
}

fn ensure_installed() -> FaultResult<()> {
    if INSTALLED.load(Ordering::Acquire) {
        return Ok(());
    }
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if INSTALLED.load(Ordering::Acquire) {
        return Ok(());
    }
    for (index, &signal) in SIGNALS.iter().enumerate() {
        // The first snapshot of each signal is ORIGINAL, by construction.
        debug_assert_eq!(SAVED_COUNT[index].load(Ordering::Relaxed), ORIGINAL);
        put_on_top(signal, index)?;
    }
    INSTALLED.store(true, Ordering::Release);
    Ok(())
}

/// Put this module's handler back in first place if something has installed over it since.
///
/// A no-op (one `sigaction` query per signal) when nothing has, and when nothing has been installed
/// yet at all -- there is then no place to defend, and the first `install` takes it.
pub(super) fn reassert_precedence() -> FaultResult<()> {
    if !INSTALLED.load(Ordering::Acquire) {
        return Ok(());
    }
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for (index, &signal) in SIGNALS.iter().enumerate() {
        put_on_top(signal, index)?;
    }
    Ok(())
}

// `si_code` values that name a real page fault. From <asm-generic/siginfo.h>.
const SEGV_MAPERR: libc::c_int = 1;
const SEGV_ACCERR: libc::c_int = 2;
const BUS_ADRERR: libc::c_int = 2;
/// The x86 page-fault vector: `REG_TRAPNO` for every fault this module decodes.
const X86_TRAP_PF: i64 = 14;
/// Page-fault error code bits (`REG_ERR`), from the Intel SDM vol. 3 section 4.7.
const PF_WRITE: i64 = 1 << 1;
const PF_INSTR: i64 = 1 << 4;

/// Turn a delivered signal into a [`Fault`], or `None` for anything that is not a kernel page
/// fault with a known access kind -- a sent signal, a general-protection fault (a non-canonical
/// address, `si_addr == 0`), an alignment trap. Those are declined before any handler is consulted,
/// as fault/windows.rs declines anything that is not an access violation with both parameters.
///
/// # Safety
///
/// `info` and `context` are the pointers the kernel passed to an `SA_SIGINFO` handler.
unsafe fn decode(signal: libc::c_int, info: *const libc::siginfo_t, context: *const libc::c_void) -> Option<Fault> {
    if info.is_null() || context.is_null() {
        return None;
    }
    // SAFETY: the kernel's siginfo for this delivery.
    let (code, address) = unsafe { ((*info).si_code, (*info).si_addr() as usize) };
    let is_page_fault = match signal {
        libc::SIGSEGV => code == SEGV_MAPERR || code == SEGV_ACCERR,
        libc::SIGBUS => code == BUS_ADRERR,
        _ => false,
    };
    if !is_page_fault {
        return None;
    }
    // SAFETY: an SA_SIGINFO handler's third argument is the interrupted thread's ucontext_t.
    let context = unsafe { &*context.cast::<libc::ucontext_t>() };
    let registers = &context.uc_mcontext.gregs;
    if registers[libc::REG_TRAPNO as usize] != X86_TRAP_PF {
        return None;
    }
    let error = registers[libc::REG_ERR as usize];
    let access = if error & PF_INSTR != 0 {
        FaultAccess::Execute
    } else if error & PF_WRITE != 0 {
        FaultAccess::Write
    } else {
        FaultAccess::Read
    };
    Some(Fault {
        address,
        access,
        instruction_pointer: registers[libc::REG_RIP as usize] as usize,
    })
}

/// The process-wide handler.
///
/// # Safety
///
/// Called by the kernel as an `SA_SIGINFO` handler. It must not unwind, and everything it does
/// itself is async-signal-safe; see the module docs for what the handlers it dispatches to rely on.
unsafe extern "C" fn on_signal(signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    // SAFETY: __errno_location returns this thread's errno slot, valid for the thread's life.
    let errno = unsafe { libc::__errno_location() };
    // SAFETY: as above.
    let saved_errno = unsafe { *errno };
    let Some(index) = signal_index(signal) else { return };

    if CHAINING.with(Cell::get) > 0 {
        // Entered as somebody else's predecessor while forwarding: dispatching again would be the
        // loop. Go straight to what was there before this module.
        // SAFETY: the kernel's own arguments, passed on unchanged.
        unsafe { forward(index, ORIGINAL, signal, info, context) };
        // SAFETY: as above.
        unsafe { *errno = saved_errno };
        return;
    }

    // SAFETY: the kernel's arguments to this SA_SIGINFO handler.
    if let Some(fault) = unsafe { decode(signal, info, context) } {
        EXAMINED.fetch_add(1, Ordering::Relaxed);
        if dispatch(&fault) == FaultOutcome::Resolved {
            RESOLVED.fetch_add(1, Ordering::Relaxed);
            // SAFETY: as above.
            unsafe { *errno = saved_errno };
            return;
        }
        DECLINED.fetch_add(1, Ordering::Relaxed);
    }

    let next = NEXT[index].load(Ordering::Acquire);
    CHAINING.with(|depth| depth.set(depth.get() + 1));
    // SAFETY: as above.
    unsafe { forward(index, next, signal, info, context) };
    CHAINING.with(|depth| depth.set(depth.get() - 1));
    // SAFETY: as above.
    unsafe { *errno = saved_errno };
}

/// Hand a declined fault to snapshot `which` of this signal's chain.
///
/// # Safety
///
/// The kernel's arguments to an `SA_SIGINFO` handler, unchanged.
unsafe fn forward(
    index: usize,
    which: usize,
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let published = SAVED_COUNT[index].load(Ordering::Acquire);
    let action = if which < published {
        // SAFETY: cell `which` was written before SAVED_COUNT was raised past it, and is never
        // written again.
        unsafe { (*SAVED[index][which].0.get()).assume_init_ref() }
    } else {
        // Nothing recorded to forward to: behave as SIG_DFL would.
        // SAFETY: the kernel's arguments.
        unsafe { default_action(signal, info) };
        return;
    };
    let handler = action.sa_sigaction;
    let sent = !info.is_null() && {
        // SAFETY: the kernel's siginfo.
        unsafe { (*info).si_code <= 0 }
    };
    if handler == libc::SIG_DFL || (handler == libc::SIG_IGN && !sent) {
        // SIG_IGN is not honoured for a kernel fault by the kernel either: an ignored synchronous
        // fault is forced to the default action.
        // SAFETY: the kernel's arguments.
        unsafe { default_action(signal, info) };
        return;
    }
    if handler == libc::SIG_IGN {
        return;
    }
    // Block the forwarded action's mask for the call, as delivery would have.
    // SAFETY: plain data.
    let mut previous: libc::sigset_t = unsafe { core::mem::zeroed() };
    // SAFETY: both sets are live; pthread_sigmask is async-signal-safe.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &action.sa_mask, &mut previous) };
    if action.sa_flags & libc::SA_SIGINFO != 0 {
        // SAFETY: an SA_SIGINFO action's handler has this signature by definition.
        let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
            unsafe { core::mem::transmute(handler) };
        f(signal, info, context);
    } else {
        // SAFETY: a plain action's handler has this signature by definition.
        let f: extern "C" fn(libc::c_int) = unsafe { core::mem::transmute(handler) };
        f(signal);
    }
    // SAFETY: restores the mask this handler was running with.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, core::ptr::null_mut()) };
}

/// `SIG_DFL`: reset the disposition, and let the default action happen at the fault.
///
/// # Safety
///
/// The kernel's siginfo for this delivery (or null).
unsafe fn default_action(signal: libc::c_int, info: *const libc::siginfo_t) {
    // SAFETY: plain data; SIG_DFL with an empty mask and no flags.
    let mut default: libc::sigaction = unsafe { core::mem::zeroed() };
    default.sa_sigaction = libc::SIG_DFL;
    // SAFETY: installs SIG_DFL; async-signal-safe.
    unsafe { libc::sigaction(signal, &default, core::ptr::null_mut()) };
    let sent = !info.is_null() && {
        // SAFETY: the kernel's siginfo.
        unsafe { (*info).si_code <= 0 }
    };
    if sent {
        // A sent signal does not recur on return, so it is raised again; it is blocked until this
        // handler returns, and then delivered with the default action.
        // SAFETY: async-signal-safe.
        unsafe { libc::raise(signal) };
    }
    // A kernel fault recurs on return, now with the default action: termination at the real fault.
}

#[cfg(test)]
mod tests {
    //! fault/windows.rs's three quiescence tests, driven through [`dispatch`] exactly as there, plus
    //! the chain decisions that are Linux's own.
    use super::*;
    use core::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::time::Duration;

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    const NOT_YET: Duration = Duration::from_millis(250);
    const EVENTUALLY: Duration = Duration::from_secs(30);
    const PROMPTLY: Duration = Duration::from_secs(5);

    fn synthetic(address: usize) -> Fault {
        Fault { address, access: FaultAccess::Read, instruction_pointer: 0 }
    }

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
        if BLOCK_RELEASE_RETURNED.load(Ordering::SeqCst) {
            BLOCK_SAW_RELEASE_RETURN.store(true, Ordering::SeqCst);
        }
        FaultOutcome::NotOurs
    }

    /// C1: `release` must not return while a dispatch is inside the handler.
    #[test]
    fn release_does_not_return_while_a_dispatch_is_inside_the_handler() {
        let _serial = serialized();
        BLOCK_ENTERED.store(false, Ordering::SeqCst);
        BLOCK_GATE.store(false, Ordering::SeqCst);
        BLOCK_SAW_RELEASE_RETURN.store(false, Ordering::SeqCst);
        BLOCK_RELEASE_RETURNED.store(false, Ordering::SeqCst);

        let registration = install(blocking_handler, 0xC0FF_EE00).expect("a free handler slot");
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
        let returned_early = rx.recv_timeout(NOT_YET).is_ok();
        BLOCK_GATE.store(true, Ordering::SeqCst);
        assert_eq!(faulter.join().expect("the faulting thread"), FaultOutcome::NotOurs);
        releaser.join().expect("the releasing thread");
        assert!(!returned_early, "`release` returned while a dispatch was inside the handler");
        assert!(!BLOCK_SAW_RELEASE_RETURN.load(Ordering::SeqCst));
        assert!(stats().drained >= 1, "a release that waited must be countable");
    }

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

    /// The drain is per slot, so one instance's teardown does not wait on another's fault.
    #[test]
    fn releasing_one_slot_does_not_wait_for_a_dispatch_in_another() {
        let _serial = serialized();
        IDLE_ENTERED.store(false, Ordering::SeqCst);
        IDLE_GATE.store(false, Ordering::SeqCst);
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
        let prompt = rx.recv_timeout(PROMPTLY).is_ok();
        IDLE_GATE.store(true, Ordering::SeqCst);
        assert_eq!(faulter.join().expect("the faulting thread"), FaultOutcome::NotOurs);
        releaser.join().expect("the releasing thread");
        drop(busy);
        assert!(prompt, "releasing an idle slot waited for another slot's dispatch");
    }

    const REUSE_MAGIC: usize = 0x5AFE_0003;
    const REUSE_CONTEXT: usize = 0xBEEF_BEEF;
    const INTRUDER_CONTEXT: usize = 0xFACE_FACE;
    static REUSE_ENTERED: AtomicBool = AtomicBool::new(false);
    static REUSE_GATE: AtomicBool = AtomicBool::new(false);
    static REUSE_SEEN_CONTEXT: AtomicUsize = AtomicUsize::new(0);

    fn reuse_handler(context: usize, fault: &Fault) -> FaultOutcome {
        if fault.address != REUSE_MAGIC {
            return FaultOutcome::NotOurs;
        }
        REUSE_ENTERED.store(true, Ordering::SeqCst);
        while !REUSE_GATE.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
        REUSE_SEEN_CONTEXT.store(context, Ordering::SeqCst);
        FaultOutcome::NotOurs
    }

    /// C1, the reuse half: a slot being drained cannot be handed out or have its context cleared.
    #[test]
    fn a_slot_being_drained_cannot_be_handed_out_or_have_its_context_cleared() {
        let _serial = serialized();
        REUSE_ENTERED.store(false, Ordering::SeqCst);
        REUSE_GATE.store(false, Ordering::SeqCst);
        REUSE_SEEN_CONTEXT.store(0, Ordering::SeqCst);

        let registration = install(reuse_handler, REUSE_CONTEXT).expect("a free handler slot");
        let index = registration.slot();
        let faulter = std::thread::spawn(|| dispatch(&synthetic(REUSE_MAGIC)));
        let deadline = std::time::Instant::now() + EVENTUALLY;
        while !REUSE_ENTERED.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "the handler was never entered");
            std::thread::yield_now();
        }
        let (tx, rx) = mpsc::channel();
        let releaser = std::thread::spawn(move || {
            drop(registration);
            let _ = tx.send(());
        });
        let handler_address = reuse_handler as *const () as usize;
        while SLOTS[index].handler.load(Ordering::SeqCst) == handler_address {
            assert!(std::time::Instant::now() < deadline, "`release` never unpublished the slot");
            std::thread::yield_now();
        }
        let marker = SLOTS[index].handler.load(Ordering::SeqCst);
        let intruder = install(inert_handler, INTRUDER_CONTEXT).expect("some other free slot");
        let intruder_slot = intruder.slot();
        let context_during_drain = SLOTS[index].context.load(Ordering::Relaxed);
        let released_early = rx.recv_timeout(NOT_YET).is_ok();

        REUSE_GATE.store(true, Ordering::SeqCst);
        assert_eq!(faulter.join().expect("the faulting thread"), FaultOutcome::NotOurs);
        releaser.join().expect("the releasing thread");

        assert_eq!(marker, DRAINING, "a slot whose drain is in progress must be marked as such");
        assert_ne!(intruder_slot, index, "`install` took a slot that was still draining");
        assert_eq!(context_during_drain, REUSE_CONTEXT, "the draining slot's context changed");
        assert!(!released_early, "`release` returned while the handler was still running");
        assert_eq!(REUSE_SEEN_CONTEXT.load(Ordering::SeqCst), REUSE_CONTEXT);
        drop(intruder);
        assert_eq!(SLOTS[index].handler.load(Ordering::SeqCst), 0, "the slot is claimable again");
        assert_eq!(SLOTS[index].context.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_drained_slot_is_handed_out_again() {
        let _serial = serialized();
        let first = install(inert_handler, 7).expect("a free handler slot");
        let slot = first.slot();
        drop(first);
        let second = install(inert_handler, 8).expect("the slot back");
        assert_eq!(second.slot(), slot, "a released slot must be reusable, not retired");
        assert_eq!(SLOTS[slot].active.load(Ordering::SeqCst), 0);
        drop(second);
        assert_eq!(SLOTS[slot].context.load(Ordering::Relaxed), 0);
    }

    /// The decode constants are the kernel's and Intel's, not guesses: `si_code` for a page fault,
    /// the page-fault vector, and the two error-code bits the access kind is read from.
    #[test]
    fn the_decode_constants_are_the_documented_ones() {
        assert_eq!((SEGV_MAPERR, SEGV_ACCERR, BUS_ADRERR), (1, 2, 2));
        assert_eq!(X86_TRAP_PF, 14);
        assert_eq!((PF_WRITE, PF_INSTR), (2, 16));
        const _: () = assert!(CLAIMING < 4096);
    }
}
