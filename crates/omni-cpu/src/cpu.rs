//! The `GuestCpu` trait: one guest thread's ARM64 CPU, and the backend that makes them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use omni_mem::GuestAddr;

use crate::context::{ContextCost, GuestAddressSpace, GuestRange, GuestThreadConfig};
use crate::error::CpuResult;
use crate::exit::{ExitReason, RunLimit};
use crate::regs::{Nzcv, VReg, XReg};
use crate::thunk::{ThunkContext, ThunkFn};

/// What the inline half of the thunk boundary has actually done, per context.
///
/// Two counters rather than one, because they answer different questions and a single total answers
/// neither: `serviced` says the fast path ran at all, and `deferred` says how many of those calls
/// escalated to the exit path. A boundary whose `deferred` count equals its `serviced` count is
/// paying for a dispatcher it is not using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InlineThunkCounts {
    /// Calls that entered an inline handler.
    pub serviced: u64,
    /// Of those, how many asked to be handed back to the caller through
    /// [`ThunkCall::defer_to_caller`](crate::ThunkCall::defer_to_caller).
    pub deferred: u64,
}

/// A way to stop a running guest thread from another thread.
///
/// Cloneable, `Send` and `Sync`, and obtained *before* [`GuestCpu::run`] is called — which it has to
/// be, because `run` takes `&mut self` and therefore nothing else can hold a reference to the
/// context while the guest is executing.
///
/// # Why this is not optional
///
/// Guest code is the ultimate untrusted input (Global Constraint 11): it will loop and recurse
/// without bound, and none of that may take down the host process. A counted step budget answers
/// that for a backend that can count instructions; a backend executing guest code natively on an
/// ARM64 host cannot count them at all. This is the mechanism that is available to both, and
/// [`Capabilities::asynchronous_halt`] says whether a given backend has really implemented it.
#[derive(Debug, Clone, Default)]
pub struct HaltHandle(Arc<AtomicBool>);

impl HaltHandle {
    /// A fresh handle, not halted. Backends construct one per context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the context to stop at its next opportunity.
    ///
    /// Idempotent, and safe to call when nothing is running: the flag stays set until
    /// [`clear`](HaltHandle::clear) takes it back, so a halt requested a moment before `run` starts
    /// stops that run rather than being lost. Losing it would make the halt a race, and a race in
    /// the mechanism that bounds untrusted code is not a mechanism.
    pub fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether a halt is outstanding. A backend polls this at its exit points.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Take an outstanding halt back, so the context can run again.
    ///
    /// Returns whether one was outstanding.
    pub fn clear(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}

/// What a backend can actually do, reported rather than assumed.
///
/// Two backends with the same trait do not have the same powers, and the differences are not
/// cosmetic: they decide whether the runtime can bound untrusted guest code. A caller that needs a
/// guarantee checks for it here instead of discovering at runtime that a call did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Whether [`RunLimit::Instructions`] is honoured.
    ///
    /// A translating backend can count for nothing, since it is rewriting every block anyway. A
    /// backend that executes guest code natively has no translator in which to put a counter, and
    /// says `false` here; asking it for a counted run returns
    /// [`CpuError::Unsupported`](crate::CpuError::Unsupported) rather than quietly running
    /// unbounded.
    pub counted_step_limit: bool,
    /// Whether [`HaltHandle::request`] actually stops guest code that is already running.
    ///
    /// If this is `false`, nothing in this trait can interrupt a guest infinite loop, and the caller
    /// is responsible for bounding it some other way — a watchdog that tears down the instance
    /// process, for example (`ARCHITECTURE.md` section 7 puts each instance in its own process
    /// precisely so that this is possible).
    pub asynchronous_halt: bool,
    /// Whether [`GuestCpu::add_breakpoint`] is implemented.
    pub breakpoints: bool,
    /// Whether [`GuestCpu::add_inline_thunk`] dispatches a thunk **without leaving the run loop**.
    ///
    /// D17 measured the two shapes at ≈33 ns and 80-105 ns per call, a factor of 3, so this is not a
    /// cosmetic difference — but it is also not something every backend can offer, and a boundary
    /// that assumed it would be a boundary that only works on one host. A backend answering `false`
    /// refuses [`add_inline_thunk`](GuestCpu::add_inline_thunk) with
    /// [`CpuError::Unsupported`](crate::CpuError::Unsupported), and the compatibility layer falls
    /// back to servicing every call through [`ExitReason::Thunk`](crate::ExitReason::Thunk) — slower,
    /// and correct.
    pub inline_thunks: bool,
}

/// One guest thread's ARM64 CPU.
///
/// # The two implementations this exists for
///
/// **On an ARM64 host**, guest code runs natively: there is no translation at all, the loader maps
/// the code executable and calls it, and a thunk is an ABI-compatible call
/// (`ARCHITECTURE.md` section 6). **On an x86-64 host**, an ARM64-to-x86-64 translator runs it
/// (D5: dynarmic, pinned as a fork). Nothing in this trait's shape may assume the second, or the
/// abstraction is not an abstraction. Concretely, that rules out three things this interface would
/// otherwise naturally have grown:
///
/// * **No code arena, code cache or emitted-code handle appears anywhere in it.** Those belong to a
///   backend that emits code; a native backend emits none. The arena is configured into the
///   translating backend, not passed through this trait.
/// * **No counted step budget as the only stop control.** See [`HaltHandle`] and
///   [`Capabilities::counted_step_limit`].
/// * **No "translate this range" or "how many blocks were flushed".**
///   [`invalidate_code`](GuestCpu::invalidate_code) is stated as *the guest changed these bytes*,
///   which is a fact about guest memory, rather than as *drop your translations*, which is an
///   instruction to a translator.
///
/// # Threading
///
/// `Send` but not `Sync`: a context belongs to one guest thread. It is created on whichever thread
/// brings the guest thread up and moved to the thread that runs it. Every method but
/// [`halt_handle`](GuestCpu::halt_handle) needs `&mut self` or a `&self` that cannot be held while
/// `run` has the context borrowed, which is what makes the register accessors safe to read straight
/// out of the backend's live state.
pub trait GuestCpu: Send {
    /// Which backend this is, for diagnostics. Stable across the process.
    fn backend_name(&self) -> &'static str;

    /// What this backend can do. See [`Capabilities`].
    fn capabilities(&self) -> Capabilities;

    /// The guest address space this context runs in.
    fn space(&self) -> GuestAddressSpace;

    /// Run guest code from `from` until something stops it.
    ///
    /// Sets `PC` to `from` and executes. On return, the register file holds the guest state at the
    /// stop, so a caller reads results out of [`x`](GuestCpu::x) afterwards.
    ///
    /// # Errors
    ///
    /// [`CpuError::Unsupported`](crate::CpuError::Unsupported) if `limit` is a counted budget and
    /// this backend cannot count — it refuses rather than running unbounded.
    /// [`CpuError::Backend`](crate::CpuError::Backend) if the backend itself failed, which is
    /// distinct from the guest failing: a guest that faults or executes garbage produces an
    /// [`ExitReason`], not an error.
    fn run(&mut self, from: GuestAddr, limit: RunLimit) -> CpuResult<ExitReason>;

    /// How many guest instructions the most recent [`run`](GuestCpu::run) executed.
    ///
    /// [`ExitReason::StepLimitReached`] carries this already, because a caller bounding untrusted code
    /// needs it at the moment the bound is hit. Every *other* exit drops it — and a caller that has to
    /// re-enter `run` repeatedly, which the thunk boundary does once per exit-path crossing, cannot
    /// otherwise subtract what has been spent. Without it a counted budget bounds each *segment* of a
    /// run rather than the run, and a guest that crosses the boundary N times gets N times the
    /// allowance the caller asked for.
    ///
    /// A backend counts at the end of a unit it handles as a whole, so this is what really executed and
    /// may exceed a budget. A backend with no counter — [`Capabilities::counted_step_limit`] `false` —
    /// returns 0, which is honest: it has nothing to report, and it also refuses counted budgets.
    fn last_run_instructions(&self) -> u64;

    /// A handle with which another thread can stop this one. See [`HaltHandle`].
    fn halt_handle(&self) -> HaltHandle;

    /// Read a general-purpose register, `X0` to `X30`.
    fn x(&self, reg: XReg) -> u64;

    /// Write a general-purpose register.
    fn set_x(&mut self, reg: XReg, value: u64);

    /// Read the stack pointer.
    ///
    /// Separate from [`x`](GuestCpu::x) because AArch64 register encoding 31 means `XZR` in most
    /// instructions and `SP` in a few, so `SP` is not `X31` — see [`XReg`].
    fn sp(&self) -> GuestAddr;

    /// Write the stack pointer.
    fn set_sp(&mut self, value: GuestAddr);

    /// Read the program counter.
    fn pc(&self) -> GuestAddr;

    /// Write the program counter. [`run`](GuestCpu::run) does this too; this is for setting it up
    /// before a resume, or for a debugger.
    fn set_pc(&mut self, value: GuestAddr);

    /// Read the four condition flags.
    fn nzcv(&self) -> Nzcv;

    /// Write the four condition flags. Only those four: see [`Nzcv`] for why the rest of `PSTATE`
    /// is not reachable through here.
    fn set_nzcv(&mut self, value: Nzcv);

    /// Read a SIMD/floating-point register, `V0` to `V31`, as its full 128 bits.
    fn v(&self, reg: VReg) -> u128;

    /// Write a SIMD/floating-point register.
    fn set_v(&mut self, reg: VReg, value: u128);

    /// Read the bionic thread pointer, `TPIDR_EL0`.
    ///
    /// Mandatory, not optional (D13). `libroblox.so` holds 1,282 `MRS Xt, TPIDR_EL0` instructions,
    /// 1,276 of which go straight on to load `[Xt, #0x28]` — bionic's `TLS_SLOT_STACK_GUARD` — and
    /// the first of them runs before `JNI_OnLoad` and before the first of the 3,594 static
    /// initializers. Every guest thread is started with one, which
    /// [`GuestThreadConfig`] enforces; this is how it is read back and how a guest thread that calls
    /// `__set_tls` re-points it.
    fn tpidr_el0(&self) -> GuestAddr;

    /// Write the bionic thread pointer.
    fn set_tpidr_el0(&mut self, value: GuestAddr);

    /// Tell the CPU that the guest's own bytes in `range` have changed.
    ///
    /// Stated as a fact about guest memory rather than as an instruction to a translator, because
    /// both backends need it and they need it for different reasons: a translating backend must drop
    /// whatever it compiled from those bytes, and a native ARM64 backend must invalidate the host
    /// instruction cache for them (AArch64 does not have a coherent instruction cache; `IC IVAU`,
    /// `DSB ISH`, `ISB` is the sequence, and omitting it means the old code keeps running with no
    /// error anywhere). Callers are the guest's `mprotect`, `munmap`, `dlclose`, and the guest's own
    /// cache-maintenance instructions.
    ///
    /// # Scope
    ///
    /// Declared per context, because a context is what a caller holds. Whether it *takes effect*
    /// per context or across the whole process is the backend's business and genuinely differs: a
    /// translating backend with unshared per-thread code caches (D5) must be told once per thread,
    /// while on an ARM64 host the instruction cache is a property of the machine, so one context's
    /// `IC IVAU` serves every thread and the others' calls are redundant rather than wrong. A caller
    /// must therefore call it on every context it wants invalidated, and a backend must tolerate
    /// being told something it already knows. The same is true of
    /// [`add_thunk`](GuestCpu::add_thunk) and [`add_breakpoint`](GuestCpu::add_breakpoint), which a
    /// native backend implements by patching guest memory that every context shares.
    ///
    /// # Errors
    ///
    /// [`CpuError::Backend`](crate::CpuError::Backend) if the backend could not carry it out. Note
    /// that a range outside the guest address space is *not* an error: a range can legitimately
    /// cover memory this context never executed.
    fn invalidate_code(&mut self, range: GuestRange) -> CpuResult<()>;

    /// Stop with [`ExitReason::Thunk`] when the guest reaches `address`.
    ///
    /// The boundary M3's imported-symbol layer is built on. Idempotent, and per context in the same
    /// qualified sense as [`invalidate_code`](GuestCpu::invalidate_code): a native backend plants a
    /// veneer in guest memory, which every context of that guest sees.
    ///
    /// # Errors
    ///
    /// [`CpuError::Backend`](crate::CpuError::Backend) if the backend could not install it.
    fn add_thunk(&mut self, address: GuestAddr) -> CpuResult<()>;

    /// Stop treating `address` as a thunk. Returns whether it was one.
    ///
    /// # Errors
    ///
    /// [`CpuError::Backend`](crate::CpuError::Backend) if the backend could not remove it.
    fn remove_thunk(&mut self, address: GuestAddr) -> CpuResult<bool>;

    /// Service the thunk at `address` with `handler`, **inside** the run loop, instead of returning
    /// [`ExitReason::Thunk`] to the caller.
    ///
    /// This is the fast half of the boundary: D17 measured ≈33 ns against 80-105 ns for exiting to
    /// Rust per call, a factor of 3 paid by every one of the imported symbols all 3,594 static
    /// initializers reach. `context` is handed back to the handler at every call and is how a bare
    /// `fn` finds shared state; see [`ThunkContext`].
    ///
    /// A handler that cannot finish the call here — because it needs guest code run, or because it
    /// has a typed error to report — calls
    /// [`ThunkCall::defer_to_caller`](crate::ThunkCall::defer_to_caller), and the call becomes an
    /// ordinary [`ExitReason::Thunk`] at the same address.
    ///
    /// Idempotent, and per context in the same qualified sense as
    /// [`add_thunk`](GuestCpu::add_thunk).
    ///
    /// # Errors
    ///
    /// [`CpuError::Unsupported`](crate::CpuError::Unsupported) if
    /// [`Capabilities::inline_thunks`] is `false` — it refuses rather than silently registering a
    /// handler that never runs, which would return a fabricated zero to the guest for every imported
    /// call. [`CpuError::Backend`](crate::CpuError::Backend) if the backend could not install it.
    fn add_inline_thunk(
        &mut self,
        address: GuestAddr,
        handler: ThunkFn,
        context: ThunkContext,
    ) -> CpuResult<()>;

    /// Stop servicing `address` inline. Returns whether it was.
    ///
    /// # Errors
    ///
    /// As [`add_inline_thunk`](GuestCpu::add_inline_thunk).
    fn remove_inline_thunk(&mut self, address: GuestAddr) -> CpuResult<bool>;

    /// How many inline thunks this context has serviced, of which how many were deferred.
    ///
    /// Not a statistic: it is what lets a test tell *dispatched inline* apart from *took the exit
    /// path and produced the same answer more slowly*, which is a distinction the whole of D17 rests
    /// on and which no assertion about the guest's result can make (Global Constraint 13).
    fn inline_thunk_calls(&self) -> InlineThunkCounts;

    /// Arm a sentinel return address: when the guest branches to `address`,
    /// [`run`](GuestCpu::run) returns [`ExitReason::Returned`].
    ///
    /// This is how a *call into* guest code finishes, and therefore how the host-to-guest half of the
    /// thunk boundary works at all — a `qsort` comparator, an `atexit` handler, a `pthread` entry
    /// point. The caller puts `address` in `X30` and the guest's own `RET` lands there.
    ///
    /// On the trait rather than on one backend because the compatibility layer needs it, and the
    /// compatibility layer holds `&mut dyn GuestCpu`. An ARM64-native backend implements it by
    /// planting a veneer at `address`, exactly as it does for a thunk.
    ///
    /// # Errors
    ///
    /// [`CpuError::Backend`](crate::CpuError::Backend) if the backend could not install it.
    fn set_return_sentinel(&mut self, address: GuestAddr) -> CpuResult<()>;

    /// The armed sentinel, if there is one.
    ///
    /// Exists because a nested call into guest code has to put back whatever the outer call armed:
    /// without this the inner call's sentinel would still be armed when the outer guest frame
    /// returned, and the outer return would be reported at an address the caller no longer expects.
    fn return_sentinel(&self) -> Option<GuestAddr>;

    /// Stop with [`ExitReason::Breakpoint`] when the guest reaches `address`, without executing the
    /// instruction there. Idempotent, and per context with the same qualification as
    /// [`add_thunk`](GuestCpu::add_thunk): a backend that implements breakpoints by writing `BRK`
    /// into guest memory is writing memory every context shares.
    ///
    /// # Errors
    ///
    /// [`CpuError::Unsupported`](crate::CpuError::Unsupported) if
    /// [`Capabilities::breakpoints`] is `false`, or
    /// [`CpuError::Backend`](crate::CpuError::Backend) if the backend could not install it.
    fn add_breakpoint(&mut self, address: GuestAddr) -> CpuResult<()>;

    /// Remove a breakpoint. Returns whether there was one.
    ///
    /// # Errors
    ///
    /// As [`add_breakpoint`](GuestCpu::add_breakpoint).
    fn remove_breakpoint(&mut self, address: GuestAddr) -> CpuResult<bool>;

    /// What this context costs, split into the half the OS counter sees and the half it does not.
    ///
    /// D5 measured **20-35 MiB per guest thread** of unshared code cache, and D15 established that a
    /// pagefile-backed section is charged against the system commit limit while being invisible to
    /// `process_commit_charge`. Between them, the fastest-growing consumer in the runtime is one the
    /// process counter shows as flat — which is why this is on the trait at all rather than being
    /// left to whoever remembers to measure. See [`ContextCost`].
    fn cost(&self) -> ContextCost;
}

/// What makes [`GuestCpu`] contexts, and holds whatever they share.
///
/// # Why a factory rather than a constructor on the trait
///
/// Because of what D5 measured: dynarmic gives each guest thread a **fully duplicated** code cache,
/// committing 20-35 MiB per thread regardless of code volume — 0.6-1.1 GiB across 32 threads, and
/// Roblox is heavily multithreaded. That is recorded as a primary risk against D10, and the
/// mitigation is sharing. Per-thread state that is *sometimes* shared cannot be expressed by a
/// per-thread constructor: something has to own the shared part and hand out contexts that reference
/// it. Making that explicit now is what lets the sharing arrive later without changing every caller.
///
/// It is also where the translating backend's code arena lives, which is how the arena stays out of
/// [`GuestCpu`] and therefore out of the ARM64-native path.
pub trait GuestCpuBackend: Send + Sync {
    /// Which backend this is. Stable across the process.
    fn name(&self) -> &'static str;

    /// Bring up one guest thread's CPU context.
    ///
    /// `config` has already established that the thread has a `TPIDR_EL0` (D13) — it cannot be
    /// constructed otherwise — so a backend may take that as given and program it.
    ///
    /// # Errors
    ///
    /// [`CpuError::Memory`](crate::CpuError::Memory) if the context's memory could not be obtained,
    /// or [`CpuError::Backend`](crate::CpuError::Backend) if the backend failed to initialise one.
    fn create_thread(&self, config: GuestThreadConfig) -> CpuResult<Box<dyn GuestCpu>>;

    /// Bring up one guest thread's CPU context **on a bionic TLS block this backend allocates**.
    ///
    /// The call a runtime uses when the guest itself asks for a thread — `pthread_create` — as
    /// against [`create_thread`](GuestCpuBackend::create_thread), which is for a caller that has
    /// already built a block and wants to hand it over.
    ///
    /// # Why the backend allocates the block rather than the caller
    ///
    /// **The stack-guard value has to be the same in every thread of one address space**, and it
    /// is a property of the arena the block came from. Bionic reads its guard once per process
    /// from `getauxval(AT_RANDOM)` and copies that one value into every thread's slot 5; a
    /// function that stores the canary on its frame in one thread and checks it in another would
    /// fail otherwise, and `__stack_chk_fail` is a *termination*. So a caller that allocated its
    /// own [`TlsArena`](crate::TlsArena) beside the backend's would be introducing a second guard
    /// value into one guest process, and the symptom would be an occasional inexplicable stack
    /// check failure in a thread that did nothing wrong. Keeping the allocation on this side
    /// means there is one arena per backend and therefore one guard per address space, which is
    /// what D13 describes.
    ///
    /// The returned context **owns** its block and returns it to the arena when dropped.
    ///
    /// # Errors
    ///
    /// [`CpuError::Unsupported`](crate::CpuError::Unsupported) from the default body, which is
    /// what a backend that does not manage TLS blocks answers — a refusal naming the backend,
    /// never a context without a thread pointer. Otherwise as
    /// [`create_thread`](GuestCpuBackend::create_thread), plus
    /// [`CpuError::Memory`](crate::CpuError::Memory) if the block could not be committed and
    /// `Unsupported` when the arena is full, which is how a guest asking for more threads than
    /// this backend was sized for is refused rather than handed a shared block.
    fn create_guest_thread(&self) -> CpuResult<Box<dyn GuestCpu>> {
        Err(crate::error::CpuError::Unsupported {
            backend: "guest-cpu-backend",
            operation: "create a guest thread with a TLS block of this backend's own",
            reason: "this backend does not own a TLS arena, so it has no stack-guard value to \
                     give a new thread and cannot satisfy D13 on its own. A caller that has its \
                     own bionic TLS block builds a GuestThreadConfig and calls create_thread",
        })
    }

    /// What this backend costs *once*, not per thread: anything shared between contexts.
    ///
    /// Reported separately from [`GuestCpu::cost`] so that summing contexts does not count shared
    /// memory once per guest thread, which is exactly the mistake that would make D5's per-thread
    /// figure unfalsifiable.
    fn shared_cost(&self) -> ContextCost;
}

/// `GuestCpu` and `GuestCpuBackend` must stay object-safe: the runtime holds whichever backend the
/// host has, and it must not be generic over it — that is the whole point of the trait
/// (`ARCHITECTURE.md` section 2: "nothing in the core knows which backend it has").
const _: () = {
    const fn assert_object_safe(_: Option<&dyn GuestCpu>, _: Option<&dyn GuestCpuBackend>) {}
    assert_object_safe(None, None);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// A halt requested before a run starts must still be outstanding when the run begins, or the
    /// only bound on untrusted guest code is a race.
    #[test]
    fn a_halt_request_is_sticky_until_it_is_cleared() {
        let handle = HaltHandle::new();
        assert!(!handle.is_requested());
        assert!(!handle.clear(), "clearing an unset flag reports that nothing was outstanding");

        let other_thread = handle.clone();
        other_thread.request();
        assert!(handle.is_requested(), "a clone shares the flag");
        other_thread.request();
        assert!(handle.is_requested(), "requesting twice is idempotent");

        assert!(handle.clear(), "clearing reports that a halt was outstanding");
        assert!(!handle.is_requested());
        assert!(!other_thread.is_requested(), "and the clone sees it cleared");
    }

    /// The handle has to cross a thread boundary to be useful at all.
    #[test]
    fn a_halt_handle_crosses_a_thread_boundary() {
        let handle = HaltHandle::new();
        let moved = handle.clone();
        std::thread::spawn(move || moved.request()).join().expect("the requesting thread");
        assert!(handle.is_requested(), "a halt requested from another thread must be visible here");
    }
}
