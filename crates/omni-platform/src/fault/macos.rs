//! macOS backend for the guest-fault seam: **thread-level Mach exception ports**, and handlers run
//! on the faulting thread.
//!
//! # Why Mach, and why the *thread* level
//!
//! On macOS a bad access is a Mach exception before it is ever a signal. The kernel offers it to
//! the faulting **thread's** exception port, then to the **task's**, then to the host's, and only
//! when all of them decline does it become `SIGSEGV`/`SIGBUS`. dynarmic's macOS handler
//! (`backend/exception_handler_macos.cpp`) takes the **task** port for `EXC_BAD_ACCESS` when its
//! first code cache is made, and resolves faults inside its code by redirecting them onto its
//! callback path -- which is exactly the permanent 30-49x deoptimisation D4 and D10 say Omnidroid
//! must pre-empt by seeing the fault first.
//!
//! A signal handler would see it last, after dynarmic. A task port would be taken back from us by
//! dynarmic whenever its first JIT is created. **A thread port is consulted before any task port,
//! whoever installed that and whenever** -- the same "first, by the documented dispatch order" that
//! a first vectored handler has on Windows. So this backend sets its port as every thread's
//! `EXC_BAD_ACCESS` (and `EXC_BREAKPOINT`, below) port: the threads alive at the first
//! [`install`], enumerated with `task_threads`, and every thread started afterwards, from a
//! `pthread_introspection_hook_install` hook that runs on the new thread before its start routine.
//! A decline returns `KERN_FAILURE`, and the kernel moves on to the task port -- dynarmic's, when it
//! has one -- then to the host, then to the signal. That is Windows' vectored-then-frame-based
//! order, reproduced rather than approximated.
//!
//! # Why the handlers run on the faulting thread, and how
//!
//! A Mach exception is a *message*: the faulting thread is suspended and a server thread receives
//! it. The seam's handlers were written for Windows, where a vectored handler runs **on the faulting
//! thread**, and `omni-mem`'s pager depends on it (its re-entrancy guard and its retry record are
//! thread-locals of the faulting thread). Running them on the server thread would quietly break
//! both. So the server does not run them. It **redirects** the faulting thread into a trampoline:
//!
//! 1. `EXC_BAD_ACCESS` arrives. The server saves the thread's general, NEON and exception state
//!    into a slot from a preallocated pool, points the thread's `pc` at the trampoline, its `sp` at
//!    the slot's own stack and `x0` at the slot, and replies success.
//! 2. The thread runs the trampoline, which calls [`dispatch`] **on the faulting thread** -- its own
//!    thread-locals, its own identity -- and stores the outcome in the slot. Then it executes
//!    `brk #0xfa17`.
//! 3. `EXC_BREAKPOINT` arrives at the same port. The server checks the `pc` is that instruction,
//!    restores the saved NEON and general state wholesale (every register, `sp`, `pc`, flags), and
//!    replies success. The thread resumes at the faulting instruction.
//! 4. **Resolved**: the instruction now succeeds. **Declined**: the server recorded a decline for
//!    this thread, address and `pc` before restoring; the instruction faults again, the server finds
//!    the record, and replies `KERN_FAILURE` at once -- so the kernel offers the fault to the task
//!    port, exactly as if this backend had not been installed.
//!
//! A `brk` that is not the trampoline's is declined, so a debugger's breakpoints reach the debugger
//! (which holds the task port).
//!
//! # What the server may not do
//!
//! The faulting thread is frozen at an arbitrary instruction, possibly inside `malloc` or holding
//! any lock. So the server **allocates nothing and takes no lock** on the fault path: the slot pool,
//! its stacks and the decline records are preallocated, and only the server thread touches them. It
//! makes Mach calls (`thread_get_state`, `thread_set_state`, `mach_msg`), none of which allocate in
//! this process. The server thread itself never has this port, so it cannot fault into itself.
//!
//! # The handler table and quiescence
//!
//! The same slot table and the same `SeqCst` drain protocol as the Windows backend, for the reasons
//! written out there at length: [`release`] publishes `DRAINING`, waits for every dispatch already
//! inside the slot's handler to return, and only then frees the slot. `dispatch` runs on the
//! faulting thread, so a registrant's context is read on that thread, exactly as on Windows.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use super::{
    registration, Fault, FaultAccess, FaultError, FaultHandler, FaultOutcome, FaultRegistration,
    FaultResult, FaultStats, MAX_HANDLERS,
};
use crate::vm::OsError;

/// This backend is real.
pub(super) const AVAILABLE: bool = true;

// ---------------------------------------------------------------------------------------------
// Mach declarations (none of these are in `libc`, or only deprecated there).
// ---------------------------------------------------------------------------------------------

type MachPort = u32;
type KernReturn = i32;

const KERN_SUCCESS: KernReturn = 0;
const KERN_FAILURE: KernReturn = 5;
const KERN_INVALID_ADDRESS: i64 = 1;
const KERN_PROTECTION_FAILURE: i64 = 2;
const MIG_BAD_ID: KernReturn = -303;

const MACH_PORT_RIGHT_RECEIVE: u32 = 1;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_RCV_MSG: i32 = 0x2;
const MACH_RCV_LARGE: i32 = 0x4;
const MACH_SEND_MSG: i32 = 0x1;
const MACH_PORT_LIMITS_INFO: i32 = 1;
const MACH_PORT_QLIMIT_LARGE: u32 = 1024;

const EXC_BAD_ACCESS: i32 = 1;
const EXC_BREAKPOINT: i32 = 6;
const EXC_MASK_BAD_ACCESS: u32 = 1 << EXC_BAD_ACCESS;
const EXC_MASK_BREAKPOINT: u32 = 1 << EXC_BREAKPOINT;
const EXCEPTION_DEFAULT: i32 = 1;
const MACH_EXCEPTION_CODES: i32 = 0x8000_0000_u32 as i32;
/// `mach_exception_raise`, the `EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES` request.
const MACH_EXCEPTION_RAISE_ID: i32 = 2405;

const ARM_THREAD_STATE64: i32 = 6;
const ARM_EXCEPTION_STATE64: i32 = 7;
const ARM_NEON_STATE64: i32 = 17;
const THREAD_STATE_NONE: i32 = 5;
const THREAD_IDENTIFIER_INFO: i32 = 4;

const PTHREAD_INTROSPECTION_THREAD_START: u32 = 2;

/// `arm_thread_state64_t` for an arm64 (not arm64e) process: plain registers, no signed pointers.
#[repr(C)]
#[derive(Clone, Copy)]
struct ThreadState {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    flags: u32,
}

/// `arm_exception_state64_t`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ExceptionState {
    far: u64,
    esr: u32,
    exception: u32,
}

/// `arm_neon_state64_t`: the 32 vector registers, FPSR and FPCR.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct NeonState {
    v: [u128; 32],
    fpsr: u32,
    fpcr: u32,
}

const fn words<T>() -> u32 {
    (core::mem::size_of::<T>() / 4) as u32
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MachMsgHeader {
    bits: u32,
    size: u32,
    remote_port: MachPort,
    local_port: MachPort,
    voucher_port: MachPort,
    id: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PortDescriptor {
    name: MachPort,
    pad1: u32,
    pad2: u16,
    disposition: u8,
    kind: u8,
}

/// `__Request__mach_exception_raise_t` (`#pragma pack(4)`).
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct RaiseRequest {
    head: MachMsgHeader,
    descriptor_count: u32,
    thread: PortDescriptor,
    task: PortDescriptor,
    ndr: [u8; 8],
    exception: i32,
    code_count: u32,
    code: [i64; 2],
}

/// `__Reply__mach_exception_raise_t`.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct RaiseReply {
    head: MachMsgHeader,
    ndr: [u8; 8],
    ret_code: KernReturn,
}

extern "C" {
    static mach_task_self_: MachPort;
    static NDR_record: [u8; 8];
    fn mach_thread_self() -> MachPort;
    fn mach_port_allocate(task: MachPort, right: u32, name: *mut MachPort) -> KernReturn;
    fn mach_port_insert_right(task: MachPort, name: MachPort, poly: MachPort, kind: u32) -> KernReturn;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    fn mach_port_set_attributes(
        task: MachPort,
        name: MachPort,
        flavor: i32,
        info: *const u32,
        count: u32,
    ) -> KernReturn;
    fn mach_msg(
        msg: *mut MachMsgHeader,
        option: i32,
        send_size: u32,
        rcv_size: u32,
        rcv_name: MachPort,
        timeout: u32,
        notify: MachPort,
    ) -> KernReturn;
    fn thread_set_exception_ports(
        thread: MachPort,
        mask: u32,
        port: MachPort,
        behavior: i32,
        flavor: i32,
    ) -> KernReturn;
    fn thread_get_state(thread: MachPort, flavor: i32, state: *mut u32, count: *mut u32) -> KernReturn;
    fn thread_set_state(thread: MachPort, flavor: i32, state: *const u32, count: u32) -> KernReturn;
    fn thread_info(thread: MachPort, flavor: i32, info: *mut u32, count: *mut u32) -> KernReturn;
    fn task_threads(task: MachPort, list: *mut *mut MachPort, count: *mut u32) -> KernReturn;
    fn mach_vm_deallocate(task: MachPort, address: u64, size: u64) -> KernReturn;
    fn pthread_introspection_hook_install(hook: IntrospectionHook) -> Option<IntrospectionHook>;
}

type IntrospectionHook =
    unsafe extern "C" fn(event: u32, thread: libc::pthread_t, address: *mut libc::c_void, size: usize);

fn task_self() -> MachPort {
    // SAFETY: set by libSystem before any Rust code runs and never written afterwards.
    unsafe { mach_task_self_ }
}

/// The kernel's 64-bit identifier of a thread, stable for its life (unlike a port name).
fn thread_id(thread: MachPort) -> Option<u64> {
    let mut info = [0u32; 6];
    let mut count = 6u32;
    // SAFETY: `info` holds THREAD_IDENTIFIER_INFO_COUNT (6) words and `count` says so.
    let kr = unsafe { thread_info(thread, THREAD_IDENTIFIER_INFO, info.as_mut_ptr(), &mut count) };
    (kr == KERN_SUCCESS).then(|| u64::from(info[0]) | (u64::from(info[1]) << 32))
}

// ---------------------------------------------------------------------------------------------
// The trampoline.
// ---------------------------------------------------------------------------------------------

core::arch::global_asm!(
    ".p2align 2",
    ".globl _omni_platform_fault_trampoline",
    "_omni_platform_fault_trampoline:",
    // x0 = the slot; sp = the slot's stack, 16-aligned. x19 is callee-saved across the call; the
    // thread's own x19 is in the slot and comes back with everything else.
    "mov x19, x0",
    "mov x29, xzr",
    "mov x30, xzr",
    "bl {dispatch}",
    "mov x0, x19",
    ".globl _omni_platform_fault_trampoline_brk",
    "_omni_platform_fault_trampoline_brk:",
    "brk #0xfa17",
    // Never resumed here: the server restores the saved state. If it ever were, trap again rather
    // than run off the end.
    "b _omni_platform_fault_trampoline_brk",
    dispatch = sym trampoline_dispatch,
);

extern "C" {
    fn omni_platform_fault_trampoline();
    static omni_platform_fault_trampoline_brk: u32;
}

fn trampoline_entry() -> u64 {
    omni_platform_fault_trampoline as unsafe extern "C" fn() as usize as u64
}

fn trampoline_brk() -> u64 {
    // Taking the address of a code label; nothing is read.
    core::ptr::addr_of!(omni_platform_fault_trampoline_brk) as u64
}

/// What a faulting thread carries through the trampoline. Written by the server before the thread
/// is redirected and after it traps back; written by the thread (the outcome) in between. The
/// exception messages between those steps order the accesses.
#[repr(C)]
struct Slot {
    saved: ThreadState,
    neon: NeonState,
    fault: Fault,
    thread: u64,
    outcome: u32,
    in_use: bool,
}

const SLOTS: usize = 256;
const SLOT_STACK: usize = 256 * 1024;
const OUTCOME_RESOLVED: u32 = 1;
const OUTCOME_DECLINED: u32 = 2;

/// Server-only state: the slot pool, the stacks under it and the decline records. Touched by the
/// server thread only, except that a redirected thread writes its own slot's `outcome` -- see
/// [`Slot`].
struct ServerState {
    slots: [Slot; SLOTS],
    stacks: usize,
    declines: [(u64, u64, u64); SLOTS],
}

struct ServerCell(UnsafeCell<Option<Box<ServerState>>>);
// SAFETY: only the server thread dereferences it (see `ServerState`).
unsafe impl Sync for ServerCell {}

static SERVER: ServerCell = ServerCell(UnsafeCell::new(None));
static PORT: AtomicU32 = AtomicU32::new(0);
static SERVER_THREAD: AtomicU64 = AtomicU64::new(0);
static INSTALL_LOCK: Mutex<Option<KernReturn>> = Mutex::new(None);
static PREVIOUS_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Called on the faulting thread, on its slot's stack, by the trampoline.
extern "C" fn trampoline_dispatch(slot: *mut Slot) {
    // SAFETY: the server handed this thread exactly this slot and will not touch it until the
    // thread traps back; the fault description is plain data.
    let fault = unsafe { (*slot).fault };
    EXAMINED.fetch_add(1, Ordering::Relaxed);
    // The handler contract forbids unwinding; a violation is contained here rather than unwinding
    // off the top of a stack with no caller.
    let outcome = std::panic::catch_unwind(|| dispatch(&fault)).unwrap_or(FaultOutcome::NotOurs);
    let code = match outcome {
        FaultOutcome::Resolved => {
            RESOLVED.fetch_add(1, Ordering::Relaxed);
            OUTCOME_RESOLVED
        }
        FaultOutcome::NotOurs => {
            DECLINED.fetch_add(1, Ordering::Relaxed);
            OUTCOME_DECLINED
        }
    };
    // SAFETY: as above.
    unsafe { (*slot).outcome = code };
}

// ---------------------------------------------------------------------------------------------
// The handler table (identical protocol to the Windows backend).
// ---------------------------------------------------------------------------------------------

struct HandlerSlot {
    handler: AtomicUsize,
    context: AtomicUsize,
    active: AtomicUsize,
}

impl HandlerSlot {
    const fn empty() -> Self {
        Self { handler: AtomicUsize::new(0), context: AtomicUsize::new(0), active: AtomicUsize::new(0) }
    }
}

const CLAIMING: usize = 1;
const DRAINING: usize = 2;
const MAX_MARKER: usize = DRAINING;
const SPINS_BEFORE_YIELD: u32 = 512;

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_SLOT: HandlerSlot = HandlerSlot::empty();
static HANDLERS: [HandlerSlot; MAX_HANDLERS] = [EMPTY_SLOT; MAX_HANDLERS];

static EXAMINED: AtomicU64 = AtomicU64::new(0);
static RESOLVED: AtomicU64 = AtomicU64::new(0);
static DECLINED: AtomicU64 = AtomicU64::new(0);
static DRAINED: AtomicU64 = AtomicU64::new(0);

pub(super) fn install(handler: FaultHandler, context: usize) -> FaultResult<FaultRegistration> {
    ensure_server()?;
    let handler_addr = handler as usize;
    debug_assert!(handler_addr > MAX_MARKER, "a function pointer is never a small integer");
    for (index, slot) in HANDLERS.iter().enumerate() {
        if slot
            .handler
            .compare_exchange(0, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        slot.context.store(context, Ordering::Relaxed);
        slot.handler.store(handler_addr, Ordering::Release);
        return Ok(registration(index));
    }
    Err(FaultError::HandlerTableFull { capacity: MAX_HANDLERS })
}

pub(super) fn release(slot: usize) {
    let Some(slot) = HANDLERS.get(slot) else { return };
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
    slot: &'static HandlerSlot,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.slot.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn dispatch(fault: &Fault) -> FaultOutcome {
    for slot in &HANDLERS {
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
        // SAFETY: a value above MAX_MARKER in `handler` was stored from a `FaultHandler` by
        // `install` and cannot be replaced until the drain this `active` reference holds off.
        let handler: FaultHandler = unsafe { core::mem::transmute::<usize, FaultHandler>(handler) };
        let outcome = handler(context, fault);
        drop(guard);
        if outcome == FaultOutcome::Resolved {
            return FaultOutcome::Resolved;
        }
    }
    FaultOutcome::NotOurs
}

fn any_handler() -> bool {
    HANDLERS.iter().any(|slot| slot.handler.load(Ordering::Relaxed) > MAX_MARKER)
}

// ---------------------------------------------------------------------------------------------
// Installation: the port, the server, and the port on every thread.
// ---------------------------------------------------------------------------------------------

fn claim_thread(thread: MachPort, port: MachPort) -> KernReturn {
    // SAFETY: a thread port this process holds a right to, and a port with a send right.
    unsafe {
        thread_set_exception_ports(
            thread,
            EXC_MASK_BAD_ACCESS | EXC_MASK_BREAKPOINT,
            port,
            EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES,
            THREAD_STATE_NONE,
        )
    }
}

/// Runs on every new thread before its start routine.
unsafe extern "C" fn on_thread_event(
    event: u32,
    thread: libc::pthread_t,
    address: *mut libc::c_void,
    size: usize,
) {
    if event == PTHREAD_INTROSPECTION_THREAD_START {
        let port = PORT.load(Ordering::Acquire);
        if port != 0 {
            // SAFETY: this thread's own port; the extra reference `mach_thread_self` takes is
            // given back straight away.
            unsafe {
                let me = mach_thread_self();
                claim_thread(me, port);
                mach_port_deallocate(task_self(), me);
            }
        }
    }
    let previous = PREVIOUS_HOOK.load(Ordering::Acquire);
    if previous != 0 {
        // SAFETY: the value `pthread_introspection_hook_install` returned, a hook of this type.
        let previous: IntrospectionHook = unsafe { core::mem::transmute(previous) };
        // SAFETY: forwarding the event exactly as received.
        unsafe { previous(event, thread, address, size) };
    }
}

fn ensure_server() -> FaultResult<()> {
    let mut installed = INSTALL_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match *installed {
        Some(KERN_SUCCESS) => return Ok(()),
        Some(code) => return Err(FaultError::Os { source: OsError(code as u32) }),
        None => {}
    }
    let result = start_server();
    *installed = Some(result.err().unwrap_or(KERN_SUCCESS));
    result.map_err(|code| FaultError::Os { source: OsError(code as u32) })
}

fn start_server() -> Result<(), KernReturn> {
    let mut port: MachPort = 0;
    // SAFETY: out-parameter to a live local.
    let kr = unsafe { mach_port_allocate(task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port) };
    if kr != KERN_SUCCESS {
        return Err(kr);
    }
    // SAFETY: a receive right just allocated; the send right lets the kernel send to it.
    let kr = unsafe { mach_port_insert_right(task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND) };
    if kr != KERN_SUCCESS {
        return Err(kr);
    }
    // Room for many threads faulting at once: the default queue of 5 would make the sixth block
    // in the kernel until the server drains.
    let limit = [MACH_PORT_QLIMIT_LARGE];
    // SAFETY: one word of MACH_PORT_LIMITS_INFO.
    let kr = unsafe {
        mach_port_set_attributes(task_self(), port, MACH_PORT_LIMITS_INFO, limit.as_ptr(), 1)
    };
    if kr != KERN_SUCCESS {
        return Err(kr);
    }

    // The pool, preallocated so the fault path allocates nothing. Stacks are one reservation, backed
    // on first touch, with a PROT_NONE guard page under each.
    let page = crate::vm::page_size();
    let stride = SLOT_STACK + page;
    // SAFETY: an anonymous private mapping the kernel places.
    let stacks = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            SLOTS * stride,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if stacks == libc::MAP_FAILED {
        return Err(KERN_FAILURE);
    }
    for index in 0..SLOTS {
        // SAFETY: the guard page at the bottom of each slot's stack, inside the mapping above.
        unsafe { libc::mprotect(stacks.cast::<u8>().add(index * stride).cast(), page, libc::PROT_NONE) };
    }
    // Zeroed in place rather than built on this stack (it is a quarter of a mebibyte): all-zero
    // is a valid ServerState -- unused slots, no stacks yet, no decline records.
    // SAFETY: every field of ServerState is plain data for which all-zero bytes are valid.
    let mut state: Box<ServerState> = unsafe { Box::<ServerState>::new_zeroed().assume_init() };
    state.stacks = stacks as usize;
    // SAFETY: the server thread has not been started, so nothing else can see the cell yet.
    unsafe { *SERVER.0.get() = Some(state) };

    // The server thread, started **before** the hook is installed and skipped by the enumeration,
    // so it is the one thread without this port: a fault on it can never be sent to itself.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("omni-fault-server".into())
        .spawn(move || {
            // SAFETY: this thread's own port; the reference is returned at once.
            let id = unsafe {
                let me = mach_thread_self();
                let id = thread_id(me);
                mach_port_deallocate(task_self(), me);
                id
            };
            SERVER_THREAD.store(id.unwrap_or(0), Ordering::Release);
            let _ = ready_tx.send(());
            serve(port);
        })
        .map_err(|_| KERN_FAILURE)?;
    ready_rx.recv().map_err(|_| KERN_FAILURE)?;
    PORT.store(port, Ordering::Release);

    // New threads first, then the ones already running, so no thread falls between the two.
    // SAFETY: installs a hook that forwards to whatever was there before.
    let previous = unsafe { pthread_introspection_hook_install(on_thread_event) };
    PREVIOUS_HOOK.store(previous.map_or(0, |hook| hook as usize), Ordering::Release);

    let server = SERVER_THREAD.load(Ordering::Acquire);
    let mut list: *mut MachPort = core::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out-parameters to live locals; the list is released below.
    let kr = unsafe { task_threads(task_self(), &mut list, &mut count) };
    if kr != KERN_SUCCESS {
        return Err(kr);
    }
    for index in 0..count as usize {
        // SAFETY: `task_threads` returned `count` thread ports at `list`.
        let thread = unsafe { *list.add(index) };
        if thread_id(thread) != Some(server) {
            claim_thread(thread, port);
        }
        // SAFETY: each port in the list carries a reference this function owns.
        unsafe { mach_port_deallocate(task_self(), thread) };
    }
    // SAFETY: the list is `count` port names allocated by the kernel in this task.
    unsafe {
        mach_vm_deallocate(
            task_self(),
            list as u64,
            u64::from(count) * core::mem::size_of::<MachPort>() as u64,
        )
    };
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The server.
// ---------------------------------------------------------------------------------------------

#[repr(C, align(8))]
struct Buffer([u8; 1024]);

fn serve(port: MachPort) -> ! {
    let mut buffer = Buffer([0; 1024]);
    loop {
        let head = buffer.0.as_mut_ptr().cast::<MachMsgHeader>();
        // SAFETY: `buffer` is 1024 writable bytes, 8-aligned; the kernel writes at most that much.
        let kr = unsafe { mach_msg(head, MACH_RCV_MSG | MACH_RCV_LARGE, 0, 1024, port, 0, 0) };
        if kr != KERN_SUCCESS {
            continue;
        }
        // SAFETY: a received message is at least a header, and a raise request is read only when
        // its id and size say it is one.
        let header = unsafe { *head };
        let mut ret = MIG_BAD_ID;
        if header.id == MACH_EXCEPTION_RAISE_ID
            && header.size as usize >= core::mem::size_of::<RaiseRequest>()
        {
            // SAFETY: as above.
            let request = unsafe { core::ptr::read_unaligned(head.cast::<RaiseRequest>()) };
            let thread = request.thread.name;
            ret = match request.exception {
                EXC_BAD_ACCESS if request.code_count >= 2 => {
                    on_bad_access(thread, request.code[0], request.code[1] as u64)
                }
                EXC_BREAKPOINT => on_breakpoint(thread),
                _ => KERN_FAILURE,
            };
            // SAFETY: the two port rights the kernel copied into this message are ours to drop.
            unsafe {
                mach_port_deallocate(task_self(), request.thread.name);
                mach_port_deallocate(task_self(), request.task.name);
            }
        }
        // SAFETY: NDR_record is libsystem's constant.
        let ndr = unsafe { NDR_record };
        let mut reply = RaiseReply {
            head: MachMsgHeader {
                // MACH_MSGH_BITS(remote disposition of the request's reply port, 0).
                bits: header.bits & 0x1f,
                size: core::mem::size_of::<RaiseReply>() as u32,
                remote_port: header.remote_port,
                local_port: 0,
                voucher_port: 0,
                id: header.id + 100,
            },
            ndr,
            ret_code: ret,
        };
        // SAFETY: a complete reply message on the stack; send-once to the request's reply port.
        unsafe {
            mach_msg(
                core::ptr::addr_of_mut!(reply).cast(),
                MACH_SEND_MSG,
                core::mem::size_of::<RaiseReply>() as u32,
                0,
                0,
                0,
                0,
            )
        };
    }
}

fn state(thread: MachPort, flavor: i32, out: *mut u32, words: u32) -> bool {
    let mut count = words;
    // SAFETY: `out` points at a value of `words` 32-bit words of the flavor's struct.
    unsafe { thread_get_state(thread, flavor, out, &mut count) == KERN_SUCCESS }
}

/// What an `ESR_EL1` says the faulting access was, or `None` for anything that is not a data or
/// instruction abort (an alignment fault, say), which is not an access violation.
fn access_from_esr(esr: u32) -> Option<FaultAccess> {
    let class = esr >> 26;
    match class {
        // Data abort, from a lower or the same exception level: WnR is bit 6.
        0x24 | 0x25 => Some(if esr & (1 << 6) != 0 { FaultAccess::Write } else { FaultAccess::Read }),
        // Instruction abort: a fetch.
        0x20 | 0x21 => Some(FaultAccess::Execute),
        _ => None,
    }
}

fn on_bad_access(thread: MachPort, code: i64, address: u64) -> KernReturn {
    if code != KERN_INVALID_ADDRESS && code != KERN_PROTECTION_FAILURE {
        return KERN_FAILURE;
    }
    // SAFETY: only this thread touches the server state.
    let Some(server) = (unsafe { (*SERVER.0.get()).as_mut() }) else { return KERN_FAILURE };
    let Some(id) = thread_id(thread) else { return KERN_FAILURE };
    let mut saved: ThreadState = unsafe { core::mem::zeroed() };
    if !state(thread, ARM_THREAD_STATE64, core::ptr::addr_of_mut!(saved).cast(), words::<ThreadState>()) {
        return KERN_FAILURE;
    }
    // A fault this backend already declined, faulting again as it was meant to: pass it on.
    if let Some(record) = server.declines.iter_mut().find(|record| record.0 == id) {
        let matches = record.1 == address && record.2 == saved.pc;
        *record = (0, 0, 0);
        if matches {
            return KERN_FAILURE;
        }
    }
    if !any_handler() {
        return KERN_FAILURE;
    }
    let mut exception = ExceptionState::default();
    if !state(thread, ARM_EXCEPTION_STATE64, core::ptr::addr_of_mut!(exception).cast(), words::<ExceptionState>()) {
        return KERN_FAILURE;
    }
    let Some(access) = access_from_esr(exception.esr) else { return KERN_FAILURE };
    let Some(index) = server.slots.iter().position(|slot| !slot.in_use) else {
        // Every slot is busy: decline rather than wait, which reaches dynarmic's handler and stays
        // correct.
        return KERN_FAILURE;
    };
    let stride = SLOT_STACK + crate::vm::page_size();
    let stack_top = (server.stacks + index * stride + stride) as u64 & !15;
    let slot = &mut server.slots[index];
    if !state(thread, ARM_NEON_STATE64, core::ptr::addr_of_mut!(slot.neon).cast(), words::<NeonState>()) {
        return KERN_FAILURE;
    }
    slot.saved = saved;
    slot.fault = Fault { address: address as usize, access, instruction_pointer: saved.pc as usize };
    slot.thread = id;
    slot.outcome = 0;
    slot.in_use = true;
    let mut redirected = saved;
    redirected.pc = trampoline_entry();
    redirected.sp = stack_top;
    redirected.x[0] = core::ptr::addr_of_mut!(*slot) as u64;
    redirected.fp = 0;
    redirected.lr = 0;
    // SAFETY: the thread is suspended in this exception; the state is a complete ThreadState.
    let kr = unsafe {
        thread_set_state(thread, ARM_THREAD_STATE64, core::ptr::addr_of!(redirected).cast(), words::<ThreadState>())
    };
    if kr != KERN_SUCCESS {
        slot.in_use = false;
        return KERN_FAILURE;
    }
    KERN_SUCCESS
}

fn on_breakpoint(thread: MachPort) -> KernReturn {
    // SAFETY: only this thread touches the server state.
    let Some(server) = (unsafe { (*SERVER.0.get()).as_mut() }) else { return KERN_FAILURE };
    let mut current: ThreadState = unsafe { core::mem::zeroed() };
    if !state(thread, ARM_THREAD_STATE64, core::ptr::addr_of_mut!(current).cast(), words::<ThreadState>()) {
        return KERN_FAILURE;
    }
    if current.pc != trampoline_brk() {
        // Somebody else's breakpoint: the debugger's, on the task port.
        return KERN_FAILURE;
    }
    let base = server.slots.as_ptr() as u64;
    let offset = current.x[0].wrapping_sub(base);
    let size = core::mem::size_of::<Slot>() as u64;
    if offset % size != 0 || offset / size >= SLOTS as u64 {
        return KERN_FAILURE;
    }
    let index = (offset / size) as usize;
    let Some(id) = thread_id(thread) else { return KERN_FAILURE };
    let slot = &mut server.slots[index];
    if !slot.in_use || slot.thread != id {
        return KERN_FAILURE;
    }
    // SAFETY: the thread is suspended in this exception; both states are complete structs saved
    // from it.
    let restored = unsafe {
        thread_set_state(thread, ARM_NEON_STATE64, core::ptr::addr_of!(slot.neon).cast(), words::<NeonState>())
            == KERN_SUCCESS
            && thread_set_state(
                thread,
                ARM_THREAD_STATE64,
                core::ptr::addr_of!(slot.saved).cast(),
                words::<ThreadState>(),
            ) == KERN_SUCCESS
    };
    let declined = slot.outcome != OUTCOME_RESOLVED;
    let record = (id, slot.fault.address as u64, slot.saved.pc);
    slot.in_use = false;
    if !restored {
        return KERN_FAILURE;
    }
    if declined {
        if let Some(free) = server.declines.iter_mut().find(|entry| entry.0 == 0 || entry.0 == id) {
            *free = record;
        }
    }
    KERN_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_declared_mach_structures_have_the_sdk_sizes() {
        // ARM_THREAD_STATE64_COUNT, ARM_EXCEPTION_STATE64_COUNT and ARM_NEON_STATE64_COUNT from
        // <mach/arm/thread_status.h>; the request and reply from mach_exc.defs' MIG output.
        assert_eq!(words::<ThreadState>(), 68);
        assert_eq!(words::<ExceptionState>(), 4);
        assert_eq!(words::<NeonState>(), 132);
        assert_eq!(core::mem::size_of::<RaiseRequest>(), 24 + 4 + 12 + 12 + 8 + 4 + 4 + 16);
        assert_eq!(core::mem::size_of::<RaiseReply>(), 36);
    }

    #[test]
    fn the_esr_decides_read_write_and_fetch() {
        assert_eq!(access_from_esr(0x9200_0007), Some(FaultAccess::Read));
        assert_eq!(access_from_esr(0x9200_0047), Some(FaultAccess::Write));
        assert_eq!(access_from_esr(0x9600_0047), Some(FaultAccess::Write));
        assert_eq!(access_from_esr(0x8200_000f), Some(FaultAccess::Execute));
        // A PC alignment fault (EC 0x22) is not an access violation.
        assert_eq!(access_from_esr(0x8a00_0000), None);
    }
}
