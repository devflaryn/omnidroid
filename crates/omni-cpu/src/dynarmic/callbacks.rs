//! The host side of the FFI boundary: every function generated guest code can call into.
//!
//! Each one follows the same three rules, stated once here rather than twenty-three times below:
//!
//! * the body runs inside [`with`], which forms the `&mut CpuCtx` for that body only and contains
//!   any panic;
//! * a callback never dereferences a guest address it has not checked against the guest address
//!   space *and* the region map first, because guest code is untrusted input and the whole point of
//!   the slow path is that it is where a bad address arrives;
//! * a callback that cannot satisfy the guest records a typed stop and halts, rather than returning
//!   a made-up value.
//!
//! # When the data callbacks run at all
//!
//! Under D4's identity mapping they should run **never**. A guest load compiles to
//! `mov reg, [r13 + vaddr]` with `r13 = 0`, so it reaches memory with no callback at all — that is
//! what `slow_path_total == 0` measures, and what the 30-49x is. They are reached only when that
//! instruction *faults*: the vectored handler gets it first (D10), and if Omnidroid's pager declines
//! it, dynarmic's frame-based handler redirects the access here with `recompile_on_fastmem_failure`.
//! So an entry to one of these is either a genuine guest fault or a regression in the configuration,
//! and both are worth the check that follows.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

use dynarmic_sys::{exception, OdCallbacks};
use omni_mem::{GuestAddr, Protection};

use crate::dynarmic::{
    mxcsr, stop, with, CpuCtx, JitRegs, PendingExit, BREAKPOINT_BRK, HALT_EXIT, STOP_SVC,
};
use crate::exit::AccessKind;
use crate::thunk::ThunkCall;

use dynarmic_sys::OD_HALT_MEMORY_ABORT;

/// The name this backend reports.
pub const BACKEND_NAME: &str = "dynarmic";

impl CpuCtx {
    /// Fetch the instruction word at `vaddr`, or `None` if it is not executable guest memory.
    fn fetch(&mut self, vaddr: u64) -> Option<u32> {
        let address = usize::try_from(vaddr).ok()?;
        if address % 4 != 0 {
            return None;
        }
        // `committed` is load-bearing and used to be written `true` and never read. The cache exists
        // so that translating a run of instructions in one function does not take the space's lock
        // once per instruction — but skipping `resolve` also skips `ensure_committed`, and that is
        // only harmless when there is nothing left to commit. For a lazily-committed anonymous
        // executable region larger than the commit granule it is not: the read below would touch an
        // uncommitted page from *Rust*, where dynarmic's frame-based handler does not reach, leaving
        // only the demand pager — which `owns_guest_paging()` may report absent. So a region that is
        // not fully committed is re-resolved on every fetch, which is correct and slower, and a fully
        // committed one keeps the fast path.
        let cached = self
            .executable_cache
            .is_some_and(|(start, end, committed)| {
                committed && address >= start && address + 4 <= end
            });
        if !cached {
            let (start, end, committed) = self.resolve(address, 4, Protection::ReadExecute)?;
            self.executable_cache = Some((start, end, committed));
        }
        // SAFETY: `resolve` established that `[address, address + 4)` lies inside a mapped,
        // executable region of this guest space, and committed it if the mapping was lazy. D4's
        // identity mapping makes the guest address a host address, so this is an ordinary read of
        // memory this process owns. Unaligned is impossible — the 4-byte alignment is checked
        // above — but `read_unaligned` costs nothing extra and does not rely on that check being
        // upstream of a future edit.
        Some(unsafe { (address as *const u32).read_unaligned() })
    }

    /// Resolve a guest data address for an access of `len` bytes, or record a fault and stop.
    ///
    /// Returns `None` having already halted the run, so every caller can simply return its
    /// fallback value.
    fn data_ptr(&mut self, vaddr: u64, len: usize, want: Protection) -> Option<*mut u8> {
        let access = if want == Protection::ReadWrite { AccessKind::Write } else { AccessKind::Read };
        let Ok(address) = usize::try_from(vaddr) else {
            self.fault(vaddr, access);
            return None;
        };
        if self.resolve(address, len, want).is_none() {
            self.fault(vaddr, access);
            return None;
        }
        Some(address as *mut u8)
    }

    /// Record a data fault and stop.
    ///
    /// The PC is deliberately **not** read here. `check_halt_on_memory_access` makes the emitter
    /// store the faulting instruction's PC and force-return as soon as it sees the memory-abort
    /// bit, so the guest PC is exact once the run has returned; reading it from inside the callback
    /// would give whatever the current block last wrote, which under block linking is the block's
    /// entry PC rather than the instruction's. So `run` fills it in.
    fn fault(&mut self, vaddr: u64, access: AccessKind) {
        stop(
            self,
            PendingExit::Fault { pc: None, address: vaddr as GuestAddr, access },
            OD_HALT_MEMORY_ABORT,
        );
    }
}

unsafe extern "C" fn cb_read_code(ctx: *mut c_void, vaddr: u64, out: *mut u32) -> i32 {
    // SAFETY: `ctx` is this backend's context; `out` is dynarmic's own stack slot.
    unsafe {
        with(ctx, 0, |c| {
            let address = vaddr as GuestAddr;
            // Order matters. The sentinel and thunks are addresses Omnidroid planted, so they win
            // over whatever the guest has there; a breakpoint is a debugging overlay on a real
            // instruction, so it comes next; and only then is guest memory read.
            if c.sentinel == Some(address)
                || c.thunks.contains(&address)
                || c.inline_thunks.contains_key(&address)
            {
                *out = STOP_SVC;
                return 1;
            }
            if c.breakpoints.contains(&address) && c.suppressed_breakpoint != Some(address) {
                *out = BREAKPOINT_BRK;
                return 1;
            }
            match c.fetch(vaddr) {
                Some(word) => {
                    *out = word;
                    1
                }
                // 0 makes dynarmic raise `NO_EXECUTE_FAULT`, which is how a guest branch into
                // unmapped memory becomes a typed exit instead of a host crash.
                None => 0,
            }
        })
    }
}

macro_rules! read_cb {
    ($name:ident, $ty:ty, $n:expr) => {
        unsafe extern "C" fn $name(ctx: *mut c_void, vaddr: u64) -> $ty {
            // SAFETY: `ctx` is this backend's context.
            unsafe {
                with(ctx, 0, |c| match c.data_ptr(vaddr, $n, Protection::Read) {
                    // SAFETY: `data_ptr` checked the range against the region map and committed it.
                    Some(ptr) => ptr.cast::<$ty>().read_unaligned(),
                    None => 0,
                })
            }
        }
    };
}

read_cb!(cb_read8, u8, 1);
read_cb!(cb_read16, u16, 2);
read_cb!(cb_read32, u32, 4);
read_cb!(cb_read64, u64, 8);

unsafe extern "C" fn cb_read128(ctx: *mut c_void, vaddr: u64, out: *mut u64) {
    // SAFETY: `ctx` is this backend's context; `out` is two writable `u64`.
    unsafe {
        with(ctx, (), |c| {
            let (lo, hi) = match c.data_ptr(vaddr, 16, Protection::Read) {
                // SAFETY: `data_ptr` checked all sixteen bytes.
                Some(ptr) => (
                    ptr.cast::<u64>().read_unaligned(),
                    ptr.add(8).cast::<u64>().read_unaligned(),
                ),
                None => (0, 0),
            };
            *out = lo;
            *out.add(1) = hi;
        })
    }
}

macro_rules! write_cb {
    ($name:ident, $ty:ty, $n:expr) => {
        unsafe extern "C" fn $name(ctx: *mut c_void, vaddr: u64, value: $ty) {
            // SAFETY: `ctx` is this backend's context.
            unsafe {
                with(ctx, (), |c| {
                    if let Some(ptr) = c.data_ptr(vaddr, $n, Protection::ReadWrite) {
                        // SAFETY: `data_ptr` checked the range and that it is writable.
                        ptr.cast::<$ty>().write_unaligned(value);
                    }
                })
            }
        }
    };
}

write_cb!(cb_write8, u8, 1);
write_cb!(cb_write16, u16, 2);
write_cb!(cb_write32, u32, 4);
write_cb!(cb_write64, u64, 8);

unsafe extern "C" fn cb_write128(ctx: *mut c_void, vaddr: u64, value: *const u64) {
    // SAFETY: `ctx` is this backend's context; `value` is two readable `u64`.
    unsafe {
        with(ctx, (), |c| {
            let (lo, hi) = (*value, *value.add(1));
            if let Some(ptr) = c.data_ptr(vaddr, 16, Protection::ReadWrite) {
                // SAFETY: `data_ptr` checked all sixteen bytes and that they are writable.
                ptr.cast::<u64>().write_unaligned(lo);
                ptr.add(8).cast::<u64>().write_unaligned(hi);
            }
        })
    }
}

/// Store-exclusive, as a real compare-and-swap on the guest's own memory.
///
/// dynarmic's own defaults return `false` unconditionally, which would turn the guest's standard
/// `LDXR`/`STXR` retry loop into an infinite loop — a guest that never makes progress, with no error
/// anywhere. `fastmem_exclusive_access` means these are normally unreached, but "normally unreached"
/// is not a reason to leave a trap in them.
macro_rules! exclusive_cb {
    ($name:ident, $ty:ty, $atomic:ty, $n:expr) => {
        unsafe extern "C" fn $name(
            ctx: *mut c_void,
            vaddr: u64,
            value: $ty,
            expected: $ty,
        ) -> i32 {
            // SAFETY: `ctx` is this backend's context.
            unsafe {
                with(ctx, 0, |c| {
                    let Some(ptr) = c.data_ptr(vaddr, $n, Protection::ReadWrite) else {
                        return 0;
                    };
                    #[allow(clippy::modulo_one)] // `$n` is 1 for the byte-wide instantiation,
                    // where the check is vacuously true; writing it out uniformly is what keeps the
                    // 16-, 32- and 64-bit cases from being the exception rather than the rule.
                    if ptr as usize % $n != 0 {
                        // An unaligned exclusive is `UNPREDICTABLE` in the architecture and cannot
                        // be done atomically on the host either. Fail the store, which is a legal
                        // outcome the guest's retry loop already handles.
                        return 0;
                    }
                    // SAFETY: `data_ptr` checked the range and writability, and the alignment check
                    // above makes the atomic well-defined. `Relaxed` is enough: the guest's own
                    // barriers are translated separately, and this operation's ordering is the
                    // guest's to state.
                    let cell = &*(ptr.cast::<$atomic>());
                    i32::from(
                        cell.compare_exchange(expected, value, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok(),
                    )
                })
            }
        }
    };
}

exclusive_cb!(cb_wx8, u8, AtomicU8, 1);
exclusive_cb!(cb_wx16, u16, AtomicU16, 2);
exclusive_cb!(cb_wx32, u32, AtomicU32, 4);
exclusive_cb!(cb_wx64, u64, AtomicU64, 8);

/// 128-bit store-exclusive.
///
/// **Not atomic**, and that is stated rather than hidden: stable Rust has no 128-bit
/// compare-and-swap, and `CMPXCHG16B` — which D2 guarantees is present — is not reachable without
/// inline assembly, which Global Constraint 4 keeps out of this crate. The guest sees a
/// compare-and-swap that is correct single-threaded and racy across guest threads sharing a
/// 16-byte object. `fastmem_exclusive_access` means the inline path handles the real cases; this is
/// the fallback, and the gap is recorded in the Task 3 report rather than papered over.
unsafe extern "C" fn cb_wx128(
    ctx: *mut c_void,
    vaddr: u64,
    value: *const u64,
    expected: *const u64,
) -> i32 {
    // SAFETY: `ctx` is this backend's context; both arrays are two readable `u64`.
    unsafe {
        with(ctx, 0, |c| {
            let (vlo, vhi) = (*value, *value.add(1));
            let (elo, ehi) = (*expected, *expected.add(1));
            let Some(ptr) = c.data_ptr(vaddr, 16, Protection::ReadWrite) else {
                return 0;
            };
            // SAFETY: `data_ptr` checked all sixteen bytes and that they are writable.
            let lo = ptr.cast::<u64>().read_unaligned();
            let hi = ptr.add(8).cast::<u64>().read_unaligned();
            if lo != elo || hi != ehi {
                return 0;
            }
            ptr.cast::<u64>().write_unaligned(vlo);
            ptr.add(8).cast::<u64>().write_unaligned(vhi);
            1
        })
    }
}

/// 231 of dynarmic's 874 A64 decoder entries are unimplemented (D5) and arrive here.
///
/// There is no A64 interpreter in Omnidroid, so the honest answer is a typed stop naming the
/// instruction — not silently skipping `num_insns` instructions, which would corrupt guest state
/// invisibly.
unsafe extern "C" fn cb_interpreter_fallback(ctx: *mut c_void, pc: u64, _num_insns: u64) {
    // SAFETY: `ctx` is this backend's context.
    unsafe {
        with(ctx, (), |c| {
            let encoding = c.fetch(pc).unwrap_or(0);
            stop(
                c,
                PendingExit::Unsupported { pc: pc as GuestAddr, encoding },
                HALT_EXIT,
            );
        })
    }
}

/// `SVC #imm`. Either one of ours — a thunk or the return sentinel — or the guest asking for a
/// syscall, which M2 has no layer for.
unsafe extern "C" fn cb_call_svc(ctx: *mut c_void, swi: u32) {
    // SAFETY: `ctx` is this backend's context.
    unsafe {
        with(ctx, (), |c| {
            // dynarmic sets the guest PC to the instruction *after* the `SVC` before calling, so the
            // site is four bytes back. The immediate is deliberately not what identifies the stop:
            // guest code may execute any `SVC` it likes, and only Omnidroid can have registered an
            // address.
            let site = (dynarmic_sys::od_jit_get_pc(c.jit) as GuestAddr).wrapping_sub(4);
            // Serviced here and resumed here: no halt is raised, so `CheckHalt` falls through into
            // `PopRSBHint`, which with `ReturnStackBuffer` cleared is the emitted dispatcher loop
            // and not a return to the caller. See `DynarmicCpu::add_inline_thunk`.
            if let Some((handler, context)) = c.inline_thunks.get(&site).copied() {
                c.inline_calls += 1;
                // The guest's SSE control word is live here -- generated code is still running and
                // `EmitA64CallSupervisor` does not switch it -- and everything the handler runs is
                // host code. See `dynarmic::mxcsr`. The guard is here, once, rather than in each
                // handler, because a forgotten guard is silent.
                let guard = mxcsr::Guard::enter(c.host_mxcsr);
                let mut regs = JitRegs::new(c.jit);
                let mut call = ThunkCall::new(&mut regs, site, context);
                handler(&mut call);
                let deferred = call.is_deferred();
                drop(guard);
                if deferred {
                    // The handler could not finish here: it needs guest code run for it, or it has a
                    // typed error to report and no channel to report it on. The guest `PC` is left at
                    // the `SVC`, so the caller sees the thunk's own address and the exit is
                    // resumable — a handler that deferred because it has to call into the guest
                    // resumes past the thunk itself once the caller has serviced it.
                    c.inline_deferred += 1;
                    stop(c, PendingExit::Thunk { pc: site }, HALT_EXIT);
                    return;
                }
                // A `BL` into the thunk region left the return address in `X30`. Writing `PC` is
                // what the dispatcher reads on its way to the next block.
                let resume = dynarmic_sys::od_jit_get_reg(c.jit, 30);
                dynarmic_sys::od_jit_set_pc(c.jit, resume);
                return;
            }
            let exit = if c.sentinel == Some(site) {
                PendingExit::Returned { pc: site }
            } else if c.thunks.contains(&site) {
                PendingExit::Thunk { pc: site }
            } else {
                // A genuine guest supervisor call. `UnsupportedInstruction` rather than a fabricated
                // success (Global Constraint 1): M3's syscall layer is what turns this into
                // something the guest can proceed from, and until then the runtime must be told
                // precisely which call it could not serve.
                PendingExit::Unsupported {
                    pc: site,
                    encoding: 0xD400_0001 | ((swi & 0xFFFF) << 5),
                }
            };
            stop(c, exit, HALT_EXIT);
        })
    }
}

/// Every hint continued through, across **every** context in the process.
///
/// # Why a global, when the context already counts them
///
/// `Context::hints` is per guest thread and is reachable only from that thread's `DynarmicCpu`.
/// The question a stall asks is the opposite one: *some* thread is burning a core and the asker
/// is a watchdog on a third thread that holds none of their contexts. A process-wide relaxed
/// counter answers it in one read.
///
/// It is a **watch, not a detector** (`docs/VERIFICATION.md` entry 11): it rises whenever a guest
/// spins on a `yield`, which is ordinary, and it stays where it is under every defect the hint
/// arm could have. What it is for is telling a *spin* from a *block* when the import census is
/// frozen and no instruction budget is being consumed — in that state a climbing hint count is
/// the guest looping through hints and nothing else, which no other counter in the runtime shows.
pub static HINTS_OBSERVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The architecture's **hint** instructions, and the reason this arm exists at all.
///
/// `YIELD`, `WFE`, `WFI`, `SEV` and `SEVL` are hints: A64 permits an implementation to execute
/// every one of them as a `NOP`, and none has an architectural effect a program can observe. So
/// the correct emulation is to **let execution continue**, and reporting them as
/// `UnsupportedInstruction` -- which is what the catch-all below did -- stops a guest on
/// instructions the guest is entitled to execute.
///
/// # This was not reachable by configuration, which is why it is handled here
///
/// `DynarmicOptions` sets `hook_hint_instructions: 0`, and the shim passes it into
/// `A64::UserConfig`. **The x64 A64 backend then does not forward it.**
/// `backend/x64/a64_interface.cpp:273` builds the translator's options as
///
/// ```text
/// A64::Translate(..., {conf.define_unpredictable_behaviour, conf.wall_clock_cntpct});
/// ```
///
/// -- two initialisers for a three-member aggregate, so `TranslationOptions::
/// hook_hint_instructions` keeps its declared default of **`true`**
/// (`frontend/A64/translate/a64_translate.h:37`). The A32 paths do forward it
/// (`backend/x64/a32_interface.cpp:216`), which is what makes this look like an oversight rather
/// than a decision. **MEASURED** on this pin: a three-instruction program `movz x0,#7; yield;
/// ret` exits `UnsupportedInstruction { encoding: 0xd503203f }` with
/// `interpreter_fallbacks: 0` and `exceptions: 1` -- so it arrives here, through
/// `ExceptionRaised`, with the config asking for the opposite.
///
/// Handled here rather than patched into the vendored tree because this arm is **correct
/// either way**: if a later pin forwards the flag the hints stop arriving and nothing here
/// changes, and if some other configuration turns hooking on deliberately, a hint still must not
/// stop the guest.
///
/// # What `RaiseException` has already done
///
/// `TranslatorVisitor::RaiseException` emits `SetPC(PC + 4)` before the exception and terminates
/// the block with `CheckHalt{ReturnToDispatch}`. So the guest `PC` is **already past the hint**
/// when this callback runs, and simply not halting resumes at the next instruction. There is
/// nothing to skip and nothing to fix up.
const fn is_hint(kind: u32) -> bool {
    matches!(
        kind,
        exception::YIELD
            | exception::WAIT_FOR_EVENT
            | exception::WAIT_FOR_INTERRUPT
            | exception::SEND_EVENT
            | exception::SEND_EVENT_LOCAL
    )
}

/// Whether a hint asks the host thread to give up its slice.
///
/// `YIELD` and `WFE`/`WFI` are emitted by spin loops -- the guest that made this reachable is
/// `libroblox.so` at `0x021eba20`, a three-instruction `ldr`/`cbz`/`yield` spin on a guard word
/// another guest thread owns. Yielding the *host* thread is what the hint is for, and it is the
/// difference between that loop costing a scheduler slice and costing a core.
///
/// `SEV`/`SEVL` signal rather than wait, so they continue immediately.
const fn hint_yields(kind: u32) -> bool {
    matches!(
        kind,
        exception::YIELD | exception::WAIT_FOR_EVENT | exception::WAIT_FOR_INTERRUPT
    )
}

unsafe extern "C" fn cb_exception_raised(ctx: *mut c_void, pc: u64, kind: u32) {
    // SAFETY: `ctx` is this backend's context.
    unsafe {
        with(ctx, (), |c| {
            if is_hint(kind) {
                c.hints += 1;
                HINTS_OBSERVED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if hint_yields(kind) {
                    std::thread::yield_now();
                }
                // **No `stop`.** The block terminated with `CheckHalt{ReturnToDispatch}` and no
                // halt bit is set, so the dispatcher continues from the PC `RaiseException`
                // already advanced past the hint.
                return;
            }
            let address = pc as GuestAddr;
            let exit = match kind {
                // The guest branched somewhere `read_code` refused: an instruction fetch from
                // memory that is not mapped executable.
                exception::NO_EXECUTE_FAULT => PendingExit::Fault {
                    // An instruction fetch names its own address, and dynarmic passes the
                    // instruction's PC here, so this one is known exactly at the callback.
                    pc: Some(address),
                    address,
                    access: AccessKind::Execute,
                },
                exception::BREAKPOINT if c.breakpoints.contains(&address) => {
                    PendingExit::Breakpoint { pc: address }
                }
                _ => {
                    let encoding = c.fetch(pc).unwrap_or(0);
                    PendingExit::Unsupported { pc: address, encoding }
                }
            };
            let halt = if matches!(exit, PendingExit::Fault { .. }) {
                // A no-execute fault is raised through `CheckHalt{ReturnToDispatch}`, which tests
                // the whole halt word, so any bit works. The memory-abort bit is used so that a
                // fault looks the same to a caller reading halt reasons whichever way it arrived.
                OD_HALT_MEMORY_ABORT
            } else {
                HALT_EXIT
            };
            stop(c, exit, halt);
        })
    }
}

/// `IC IVAU` / `IC IALLU` / `IC IALLUIS`: the guest telling us it wrote code.
///
/// Without this a guest that patches itself — every JIT the engine embeds — executes stale
/// translations with no error anywhere.
unsafe extern "C" fn cb_icache_op(ctx: *mut c_void, op: u32, vaddr: u64) {
    // SAFETY: `ctx` is this backend's context.
    unsafe {
        with(ctx, (), |c| {
            c.executable_cache = None;
            if c.jit.is_null() {
                return;
            }
            // `op` 0 is `IC IVAU` (one cache line); anything else is an all-instruction-cache
            // operation. dynarmic's own A64 frontend raises this from a `CheckHalt{ReturnToDispatch}`
            // terminal, so invalidating from here is safe.
            if op == 0 {
                // A cache line, not a word: the guest named a line and the architecture invalidates
                // the whole of it. 64 bytes is `CTR_EL0`'s default line size on this pin
                // (0x8444c004), and over-invalidating is correct-but-slower where
                // under-invalidating is silently wrong.
                dynarmic_sys::od_jit_invalidate_range(c.jit, vaddr & !63, 64);
            } else {
                dynarmic_sys::od_jit_clear_cache(c.jit);
            }
        })
    }
}

/// `CNTPCT_EL0`: the architectural counter, in `CNTFRQ_EL0` ticks since the process epoch.
///
/// **This returned `ctx.ticks_used` and was documented as monotonic.** It is not: `run` resets that
/// counter at the top of every slice, so the value sawtoothed every million guest instructions, and
/// its units were guest instructions against an advertised 600 MHz. See [`crate::clock`] for what a
/// guest does with the difference; the short version is that a guest clock running backwards
/// produces bugs that look like anything but a clock.
///
/// The context is not read at all now, which is why this takes no lock and cannot be affected by a
/// slice boundary. It still goes through [`with`] so that the FFI discipline is uniform and a panic
/// from `Instant::now` -- which does not panic -- could not unwind into generated code.
unsafe extern "C" fn cb_get_cntpct(ctx: *mut c_void) -> u64 {
    // SAFETY: `ctx` is this backend's context.
    unsafe { with(ctx, 0, |_| crate::clock::cntpct()) }
}

/// Charge a slice's worth of guest instructions.
///
/// Saturating on both sides: `ticks` comes from generated code, and a runtime whose budget wrapped
/// would turn a bounded run into an unbounded one (Global Constraint 11).
unsafe extern "C" fn cb_add_ticks(ctx: *mut c_void, ticks: u64) {
    // SAFETY: `ctx` is this backend's context.
    unsafe {
        with(ctx, (), |c| {
            c.ticks_used = c.ticks_used.saturating_add(ticks);
            c.ticks_remaining = c.ticks_remaining.saturating_sub(ticks);
        })
    }
}

unsafe extern "C" fn cb_get_ticks_remaining(ctx: *mut c_void) -> u64 {
    // SAFETY: `ctx` is this backend's context.
    unsafe { with(ctx, 0, |c| c.ticks_remaining) }
}

/// The complete table. Every slot is filled; `od_jit_new` rejects a table with a hole rather than
/// letting generated code call address zero.
pub(crate) const CALLBACKS: OdCallbacks = OdCallbacks {
    read_code: Some(cb_read_code),
    read8: Some(cb_read8),
    read16: Some(cb_read16),
    read32: Some(cb_read32),
    read64: Some(cb_read64),
    read128: Some(cb_read128),
    write8: Some(cb_write8),
    write16: Some(cb_write16),
    write32: Some(cb_write32),
    write64: Some(cb_write64),
    write128: Some(cb_write128),
    write_exclusive8: Some(cb_wx8),
    write_exclusive16: Some(cb_wx16),
    write_exclusive32: Some(cb_wx32),
    write_exclusive64: Some(cb_wx64),
    write_exclusive128: Some(cb_wx128),
    interpreter_fallback: Some(cb_interpreter_fallback),
    call_svc: Some(cb_call_svc),
    exception_raised: Some(cb_exception_raised),
    instruction_cache_op: Some(cb_icache_op),
    get_cntpct: Some(cb_get_cntpct),
    add_ticks: Some(cb_add_ticks),
    get_ticks_remaining: Some(cb_get_ticks_remaining),
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The two encodings this backend plants into the guest's instruction stream, spelled out so
    /// they can be checked against the ARM ARM by eye rather than trusted.
    #[test]
    fn the_planted_encodings_are_what_they_claim_to_be() {
        // SVC #imm16 is `1101 0100 000 imm16 00001`, i.e. 0xD4000001 | imm16 << 5.
        assert_eq!(STOP_SVC, 0xD400_0001 | (0xFFFFu32 << 5));
        assert_eq!(STOP_SVC, 0xD41F_FFE1);
        // BRK #imm16 is `1101 0100 001 imm16 00000`, i.e. 0xD4200000 | imm16 << 5.
        assert_eq!(BREAKPOINT_BRK, 0xD420_0000);
        // And they are different instructions, which the address-based dispatch relies on only for
        // clarity — but a typo that made them equal would make every breakpoint a thunk.
        assert_ne!(STOP_SVC, BREAKPOINT_BRK);
    }
}
