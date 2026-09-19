//! The translating backend: `GuestCpu` over the pinned dynarmic (D5), configured the way D4 and
//! D13 require and **asserted** to be so before any guest code runs.
//!
//! # What this module is for
//!
//! Three settings decide whether Omnidroid is fast, safe, or runs at all, and none of them fails
//! loudly on its own:
//!
//! * **Identity mapping** (D4). `fastmem_pointer = 0` with `fastmem_address_space_bits = 64` emits
//!   `mov reg, [r13 + vaddr]` with `r13 = 0`. The default width is 36, which still produces correct
//!   results while costing **30-49x** (n = 31, two loop shapes). [`crate::require_identity_mapping`]
//!   runs against what dynarmic
//!   reports back, once per context, in [`DynarmicBackend::create_thread`].
//! * **The bionic thread pointer** (D13). Every context gets a [`crate::GuestTls`] block with a
//!   stack guard at `+0x28` and `TPIDR_EL0` pointing at it, before it can run an instruction.
//! * **Guest faults** (D10, Global Constraint 11). `check_halt_on_memory_access` makes a guest
//!   access to an unmapped address stop *at the faulting instruction*, which is what turns it into a
//!   typed [`ExitReason::MemoryFault`] rather than a run that carries on with garbage.
//!
//! # The three FFI hazards, and what handles each
//!
//! `dynarmic-sys` states them; this is where they are paid for.
//!
//! 1. **Re-entrancy.** The callback context is a separate heap allocation from [`DynarmicCpu`],
//!    reached only
//!    through a raw pointer, so no `&mut` to it exists at an `od_jit_run` call site. Every callback
//!    forms its `&mut` for the body of that callback and never stores it.
//! 2. **Unwinding.** Every callback body runs inside [`catch_unwind`]. A panic is recorded, the jit
//!    is halted, and the panic is re-raised by `run` *after* the generated frames are gone.
//! 3. **Pinned pointers.** `TPIDR_EL0` and `TPIDRRO_EL0` are inlined into generated code, so they
//!    live in `Box`es owned by the context and are never moved.
//!
//! # Why the run loop is sliced
//!
//! See [`crate::run`]. In short: the emitted block-linking terminal checks the cycle counter **or**
//! the halt flag and never both, so an external halt cannot stop a block-linked direct-branch loop.
//! The watchdog is therefore a short budget expiring, checked in Rust between slices.

use std::cell::UnsafeCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use dynarmic_sys::{
    optimization, od_jit_clear_halt, od_jit_effective_config, od_jit_free, od_jit_get_pc,
    od_jit_get_pstate, od_jit_get_reg, od_jit_get_sp, od_jit_get_vec, od_jit_halt,
    od_jit_invalidate_range, od_jit_new, od_jit_reset_stats, od_jit_run, od_jit_set_pc,
    od_jit_set_pstate, od_jit_set_reg, od_jit_set_sp, od_jit_set_vec, od_jit_slow_path_total,
    od_jit_stats, od_monitor_free,
    od_monitor_new, OdConfig, OdEffectiveConfig, OdStats, OD_DYNARMIC_ABI_VERSION,
    OD_HALT_CACHE_INVALIDATION, OD_HALT_MEMORY_ABORT, OD_HALT_SHIM_REENTERED, OD_HALT_SHIM_THREW,
    OD_HALT_USER1, OD_HALT_USER8, OD_FIXED_PER_JIT_BYTES,
};
use omni_mem::{DemandPager, FaultAccess, GuestAddr, GuestSpace, PagerStats, Protection};

use crate::context::{ContextCost, GuestAddressSpace, GuestRange, GuestThreadConfig};
use crate::cpu::{Capabilities, GuestCpu, GuestCpuBackend, HaltHandle, InlineThunkCounts};
use crate::error::{CpuError, CpuResult};
use crate::exit::{AccessKind, ExitReason, RunLimit};
use crate::fastmem::{require_identity_mapping, MemoryMapping};
use crate::regs::{Nzcv, VReg, XReg};
use crate::run::Budget;
use crate::thunk::{ThunkContext, ThunkFn, ThunkRegs};
use crate::tls::{GuestTls, TlsArena};

mod callbacks;

pub use callbacks::BACKEND_NAME;

/// `SVC #0xFFFF`, planted by [`read_code`](callbacks) at a thunk or at the return sentinel.
///
/// Why an `SVC` rather than a halt at translation time: `read_code` runs while dynarmic is
/// *translating*, which can be arbitrarily far ahead of execution, so stopping there would stop at
/// the wrong moment. `SVC`'s terminal in dynarmic's A64 frontend is `CheckHalt{PopRSBHint}`, which
/// tests `halt_reason` immediately after the callback returns — so a halt raised inside `call_svc`
/// stops at exactly the planted instruction, whatever the optimization flags are.
///
/// The immediate is not what identifies the stop. A guest is free to execute `SVC #0xFFFF` of its
/// own, so `call_svc` decides by looking up the *address*, which only Omnidroid can have registered.
const STOP_SVC: u32 = 0xD41F_FFE1;

/// `BRK #0`, planted at a breakpoint. Raises `exception::BREAKPOINT` *without executing the
/// instruction it replaced*, which is what [`GuestCpu::add_breakpoint`] promises.
const BREAKPOINT_BRK: u32 = 0xD420_0000;

/// Halt bit the callbacks raise for a stop that is not a memory abort.
const HALT_EXIT: u32 = OD_HALT_USER1;
/// Halt bit raised when a callback panicked.
const HALT_PANIC: u32 = OD_HALT_USER8;

/// Every halt bit this backend raises or expects, for clearing between slices.
const HALT_OURS: u32 =
    HALT_EXIT | HALT_PANIC | OD_HALT_MEMORY_ABORT | OD_HALT_CACHE_INVALIDATION;


/// How the translating backend is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DynarmicOptions {
    /// Bytes of code cache per guest thread. 0 selects dynarmic's 128 MiB default.
    ///
    /// D5 measured **20-35 MiB committed per thread** against that default, and Windows commits the
    /// cache incrementally (`BlockOfCode::EnsureMemoryCommitted`), so this is a reservation ceiling
    /// rather than a charge. It is still a real knob, because the reservation is per thread and
    /// Roblox is heavily multithreaded.
    pub code_cache_size: u64,
    /// How many guest threads this backend will be asked for. Sizes the exclusive monitor and the
    /// TLS arena.
    pub max_threads: u32,
    /// Whether to clear the two optimization flags whose terminal handlers check neither the cycle
    /// counter nor the halt flag.
    ///
    /// **Default `true`, and that is a deliberate trade.** Task 2 measured a guest `BR X30`
    /// branching to itself to be stoppable by *nothing* under the default flags — not a budget, not
    /// a halt — and `optimization::INTERRUPTIBLE` fixes it. The cost is about **3.9 ns per indirect
    /// transfer** (n = 31 per configuration): nothing at all on a guest with no indirect branches,
    /// up to roughly 5x on an indirect-saturated one. Roblox's real branch mix is reported in the
    /// Task 3 report.
    ///
    /// Availability beats throughput here because the failure modes are not comparable: an
    /// unstoppable guest thread is a denial of service on the host from untrusted input (Global
    /// Constraint 11), while the alternative is a runtime that is slower on one class of code.
    pub interruptible: bool,
    /// Whether to check the memory-abort halt bit after every guest data access.
    ///
    /// **Default `true`.** Without it, a slow-path callback that detects an unmapped guest address
    /// can halt, but the current block runs on to its terminal and — with cycle counting on — links
    /// straight into the next block, so the guest keeps executing after the fault. With it, the
    /// emitter plants a `test`/`jz` on the **abort path only**, not on the fastmem fast path
    /// (`EmitCheckMemoryAbort` is emitted inside the deferred abort block), so the cost on the hot
    /// path is zero.
    ///
    /// It is not free, though: `A64::Jit::Impl` skips `GetSetElimination` entirely when this is set.
    /// The measured cost of that is in the Task 3 report.
    pub check_halt_on_memory_access: bool,
    /// Whether to check, **per run slice**, that guest memory never went through a host callback
    /// unless the slice ended in a memory fault.
    ///
    /// **Default `true`.** See [`CpuError::DegradedMemoryPath`] for the defect class this exists
    /// for and why it is stated per slice rather than as "the counter stays at zero". The cost is
    /// one load per slice — [`od_jit_slow_path_total`] rather than a 72-byte struct copy — and a
    /// slice is a million guest instructions by default, so it is not a hot path.
    ///
    /// It is **automatically disarmed** when this backend does not own guest paging, because the
    /// callback path is then the designed route for a first touch rather than a degradation.
    /// [`DynarmicBackend::slice_invariant_armed`] reports what is actually in force.
    pub assert_callback_free_slices: bool,
}

impl Default for DynarmicOptions {
    fn default() -> Self {
        Self {
            // 8 MiB is dynarmic's documented minimum. The 128 MiB default would reserve 4 GiB
            // across 32 guest threads, and D10 makes address space cheap but not free.
            code_cache_size: 8 << 20,
            max_threads: 32,
            interruptible: true,
            check_halt_on_memory_access: true,
            assert_callback_free_slices: true,
        }
    }
}

impl DynarmicOptions {
    /// The `OptimizationFlag` bitmask these options select.
    #[must_use]
    pub const fn optimizations(&self) -> u32 {
        if self.interruptible {
            optimization::INTERRUPTIBLE
        } else {
            optimization::ALL_SAFE
        }
    }
}

/// Deliberate breakages of the guest memory path, for
/// [`DynarmicBackend::create_misconfigured_thread`].
///
/// Every field is `None` or `false` in [`Default`], which is the conforming configuration — so the
/// only way to build a broken context is to say, field by field, exactly what is being broken.
///
/// **Behind the non-default `test-support` feature**, and `#[doc(hidden)]`, because the thing it
/// configures is a bypass of the startup assertion. A production build cannot reach it at all. See
/// [`DynarmicBackend::create_misconfigured_thread`] for why it exists.
#[cfg(feature = "test-support")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryPathOverrides {
    /// Override `fastmem_address_space_bits`. **36** is dynarmic's own default, and the value D4
    /// says degrades a high guest address onto the 30-49x-slower callback path while still producing
    /// correct results.
    pub address_space_bits: Option<u32>,
    /// Override `fastmem_enabled`. `Some(false)` routes every guest access through a callback.
    pub direct_access: Option<bool>,
    /// Override `silently_mirror_fastmem`. `Some(true)` masks a wild guest address into range
    /// instead of faulting.
    pub mirrors_out_of_range: Option<bool>,
    /// Hand dynarmic a **null** `TPIDR_EL0` pointer, so guest code cannot read the thread pointer
    /// at all.
    ///
    /// This is the one D13 is about, and it is here because without it the assertion's seventh
    /// check could never fire in this backend: `DynarmicCpu` always passes the address of a `Box`,
    /// which is never null, so the check was unreachable and therefore unproven. The shim accepts a
    /// null pointer (its header says the guest read then faults into `exception_raised`), so this
    /// is a configuration a backend could really reach by mistake.
    pub null_thread_pointer: bool,
}

#[cfg(feature = "test-support")]
impl MemoryPathOverrides {
    fn into_internal(self) -> Overrides {
        Overrides {
            address_space_bits: self.address_space_bits,
            direct_access: self.direct_access,
            mirrors_out_of_range: self.mirrors_out_of_range,
            null_thread_pointer: self.null_thread_pointer,
        }
    }
}

/// The always-compiled form of the above. Private, and `Default` is the only value the normal
/// construction path can produce.
#[derive(Debug, Clone, Copy, Default)]
struct Overrides {
    address_space_bits: Option<u32>,
    direct_access: Option<bool>,
    mirrors_out_of_range: Option<bool>,
    null_thread_pointer: bool,
}

/// Owns the exclusive monitor, which every guest thread of one address space shares.
struct Monitor(*mut c_void);

// SAFETY: dynarmic's `ExclusiveMonitor` is designed to be shared between the jits of several guest
// threads and does its own locking; the handle is only ever passed to `od_jit_new` and freed once,
// in `Drop`, after every jit that holds it. D5 records that it anti-scales 21x from 1 to 16 threads,
// which is a performance problem and not a soundness one.
unsafe impl Send for Monitor {}
// SAFETY: as above.
unsafe impl Sync for Monitor {}

impl Drop for Monitor {
    fn drop(&mut self) {
        // SAFETY: the handle came from `od_monitor_new` and every jit using it has been freed —
        // `DynarmicBackend` hands out contexts that hold an `Arc` of the shared state, so the
        // monitor outlives them all.
        unsafe { od_monitor_free(self.0) };
    }
}

/// What every context of one guest address space shares.
struct Shared {
    space: Arc<GuestSpace>,
    extent: GuestAddressSpace,
    monitor: Monitor,
    tls: TlsArena,
    options: DynarmicOptions,
    /// Installed once per backend. `None` on a target with no vectored-handler implementation, or
    /// when the caller installed one of its own; the difference is reported by
    /// [`DynarmicBackend::owns_guest_paging`].
    _pager: Option<DemandPager>,
    owns_guest_paging: bool,
    /// Processor ids handed back by dropped contexts.
    ///
    /// Recycled rather than monotonic, because a `processor_id` indexes into the shared exclusive
    /// monitor and the monitor is sized once, at backend creation. A runtime whose guest threads
    /// come and go — which is every runtime — would otherwise exhaust the ids while holding far
    /// fewer threads than it was sized for, and the symptom would be a thread creation that fails
    /// for a reason unrelated to how many threads are actually live.
    free_processors: parking_lot::Mutex<Vec<u32>>,
    next_processor: AtomicU32,
    /// Processor ids handed back while the jit that held them was still alive. **Always 0.**
    ///
    /// A witness, not a statistic. `release_processor_id` cannot check the ordering itself — it is
    /// handed an integer — so the caller passes what it knows, and this counts the times that claim
    /// was false. It exists because the failure it guards has no symptom of its own: an id recycled
    /// early lets another thread build a jit against the *same* entry of the shared
    /// `ExclusiveMonitor` as a jit that is still live, and two guest threads on one monitor entry
    /// makes `STXR` succeed where the architecture requires it to fail. Correct-looking results,
    /// silently wrong, in D5's risk 3.
    ids_released_early: AtomicU64,
}

impl Shared {
    fn take_processor_id(&self) -> Option<u32> {
        if let Some(id) = self.free_processors.lock().pop() {
            return Some(id);
        }
        let id = self.next_processor.fetch_add(1, Ordering::Relaxed);
        if id >= self.options.max_threads.max(1) {
            self.next_processor.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(id)
    }

    /// Hand a processor id back for reuse.
    ///
    /// `no_jit_holds_it` is the caller's statement that nothing can still be pointed at this entry
    /// of the shared exclusive monitor: either the jit has been freed, or it was never created. It
    /// is recorded rather than asserted — an `assert!` in a `Drop` would turn a bookkeeping mistake
    /// into a panic during unwinding — and `DynarmicBackend::processor_ids_released_early` is what
    /// a test reads.
    fn release_processor_id(&self, id: u32, no_jit_holds_it: bool) {
        if !no_jit_holds_it {
            self.ids_released_early.fetch_add(1, Ordering::Relaxed);
        }
        self.free_processors.lock().push(id);
    }
}

/// Makes [`DynarmicCpu`] contexts and holds everything they share.
pub struct DynarmicBackend {
    shared: Arc<Shared>,
}

impl DynarmicBackend {
    /// Bring up the translating backend for one guest address space.
    ///
    /// Installs a [`DemandPager`] so that Omnidroid, and not dynarmic's frame-based SEH, owns guest
    /// page faults (D10). If the platform has no vectored-handler implementation the backend still
    /// works — a guest fault then reaches the slow-path callback and becomes a typed exit — but
    /// demand paging is not available and [`owns_guest_paging`](Self::owns_guest_paging) says so.
    ///
    /// # Errors
    ///
    /// [`CpuError::Backend`] if the exclusive monitor could not be allocated,
    /// [`CpuError::InvalidAddressSpace`] for a space that cannot be described, or
    /// [`CpuError::Memory`] if the TLS arena could not be reserved.
    pub fn new(space: Arc<GuestSpace>, options: DynarmicOptions) -> CpuResult<Self> {
        let extent = GuestAddressSpace::of(&space)?;
        let tls = TlsArena::new(&space, options.max_threads.max(1) as usize)?;

        // SAFETY: freed exactly once, in `Monitor::drop`, after every jit that references it.
        let raw = unsafe { od_monitor_new(u64::from(options.max_threads.max(1))) };
        if raw.is_null() {
            return Err(CpuError::Backend {
                backend: BACKEND_NAME,
                operation: "allocate the shared exclusive monitor",
                detail: format!(
                    "od_monitor_new({}) returned null",
                    options.max_threads.max(1)
                ),
            });
        }

        // D10 requires Omnidroid to own guest page faults. Two failures, and they are not the
        // same failure: a platform with **no vectored-handler implementation** is a known,
        // documented state the backend still works in, while a platform that *has* one and could
        // not give us a slot is a resource exhaustion whose only symptom would be that every guest
        // fault goes to dynarmic's own handler and permanently recompiles the block onto the
        // 30-49x callback path. Swallowing the second was a real defect — it made `omni-cpu`'s own
        // suite intermittently run without a pager — so it is refused.
        let pager = match DemandPager::install(Arc::clone(&space)) {
            Ok(pager) => Some(pager),
            Err(e) if e.is_unsupported() => None,
            Err(e) => {
                // SAFETY-adjacent note: the monitor allocated above has not been wrapped in a
                // `Monitor` yet, so it would leak on this path. Free it here.
                // SAFETY: `raw` came from `od_monitor_new` and no jit references it.
                unsafe { od_monitor_free(raw) };
                return Err(CpuError::Backend {
                    backend: BACKEND_NAME,
                    operation: "install the guest demand pager",
                    detail: format!(
                        "{e}. D10 requires Omnidroid to take guest faults ahead of dynarmic's own \
                         handler; without that every guest fault recompiles its block onto the \
                         callback path, measured 30-49x slower with correct results"
                    ),
                });
            }
        };
        let owns_guest_paging = pager.is_some();

        Ok(Self {
            shared: Arc::new(Shared {
                space,
                extent,
                monitor: Monitor(raw),
                tls,
                options,
                _pager: pager,
                owns_guest_paging,
                free_processors: parking_lot::Mutex::new(Vec::new()),
                next_processor: AtomicU32::new(0),
                ids_released_early: AtomicU64::new(0),
            }),
        })
    }

    /// Processor ids that were handed back while a jit still referenced their monitor entry.
    ///
    /// **Always 0**, and it is a test's job to keep saying so. See `Shared::ids_released_early` for
    /// why the thing being counted has no other symptom.
    #[must_use]
    pub fn processor_ids_released_early(&self) -> u64 {
        self.shared.ids_released_early.load(Ordering::Relaxed)
    }

    /// Whether this backend took ownership of guest page faults, as D10 requires.
    ///
    /// `false` means the platform has no vectored-handler implementation. Guest faults still
    /// produce typed exits; what is missing is demand paging, so every guest page must be committed
    /// before the guest touches it.
    #[must_use]
    pub fn owns_guest_paging(&self) -> bool {
        self.shared.owns_guest_paging
    }

    /// The options in force.
    #[must_use]
    pub fn options(&self) -> DynarmicOptions {
        self.shared.options
    }

    /// The TLS arena every guest thread's block comes from.
    #[must_use]
    pub fn tls(&self) -> &TlsArena {
        &self.shared.tls
    }

    /// What this backend's demand pager has done, or `None` if it has none.
    #[must_use]
    pub fn pager_stats(&self) -> Option<PagerStats> {
        self.shared._pager.as_ref().map(DemandPager::stats)
    }

    /// Whether the per-slice callback invariant is actually in force.
    ///
    /// Both halves must hold: the option must be on, *and* this backend must own guest paging. It
    /// is reported rather than inferred because a check that has been disarmed by a platform
    /// detail is not a check, and the difference has to be visible to the test that claims it.
    #[must_use]
    pub fn slice_invariant_armed(&self) -> bool {
        self.shared.options.assert_callback_free_slices && self.shared.owns_guest_paging
    }

    /// Build a context with a **deliberately broken** memory path, handing back both the context and
    /// the refusal [`create_thread`](GuestCpuBackend::create_thread) would have produced for it.
    ///
    /// This exists for one reason, and it is Global Constraint 13: a test that cannot fail is worse
    /// than no test. The D4 startup assertion is the *entire* defence against a silent 30-49x
    /// regression, so it has to be shown to fire — and, separately, shown to be guarding something
    /// real, which needs a misconfigured context that can actually be run and measured.
    ///
    /// Both halves come back together on purpose. There is no way to obtain a context this way
    /// without also obtaining the error saying why it should not exist.
    ///
    /// **That is not enough on its own, and this is gated because of it.** `CpuError` is not
    /// `#[must_use]`, so `let (cpu, _) = …` hands back a runnable context the startup assertion
    /// refused, with the refusal dropped on the floor — a real bypass rather than a theoretical one.
    /// `#[cfg(test)]` cannot close it, because integration tests are separate crates and would lose
    /// access along with everyone else, so it sits behind the non-default `test-support` feature,
    /// which this crate turns on for its own test targets through a dev-dependency on itself. A
    /// production build cannot call it.
    ///
    /// # Errors
    ///
    /// [`CpuError::Unsupported`] if `overrides` would have produced a **conforming** configuration —
    /// this is not a back door to building an ordinary context — plus everything
    /// [`create_thread`](GuestCpuBackend::create_thread) can fail with.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn create_misconfigured_thread(
        &self,
        overrides: MemoryPathOverrides,
    ) -> CpuResult<(DynarmicCpu, CpuError)> {
        let tls = self.shared.tls.allocate(&self.shared.space)?;
        let config = GuestThreadConfig::new(self.shared.extent, tls.thread_pointer())?;
        let processor_id = self.shared.take_processor_id().ok_or(CpuError::Unsupported {
            backend: BACKEND_NAME,
            operation: "create another guest thread",
            reason: "the shared exclusive monitor is sized at backend creation and every live \
                     guest thread needs a distinct processor id within it",
        })?;
        // Every exit from here on has to give the id back. `build` does this with `inspect_err`;
        // this path has two failure exits rather than one, so it is written out.
        let built = DynarmicCpu::build_unchecked(
            Arc::clone(&self.shared),
            config,
            Some(tls),
            processor_id,
            overrides.into_internal(),
        );
        let cpu = match built {
            Ok(cpu) => cpu,
            Err(error) => {
                // `build_unchecked` failed, so no jit was ever created against this id.
                self.shared.release_processor_id(processor_id, true);
                return Err(error);
            }
        };
        match require_identity_mapping(&cpu.memory_mapping(), self.shared.extent) {
            Err(error) => Ok((cpu, error)),
            Ok(()) => {
                // `cpu` is dropped here, and `DynarmicCpu::drop` returns the id and the TLS block.
                Err(CpuError::Unsupported {
                    backend: BACKEND_NAME,
                    operation: "build a deliberately misconfigured context",
                    reason: "the overrides produced a configuration that satisfies D4, so there is \
                             nothing for the startup assertion to refuse and nothing to measure",
                })
            }
        }
    }

    /// Bring up a context with a freshly-allocated bionic TLS block (D13).
    ///
    /// This is the call a runtime uses. [`create_thread`](GuestCpuBackend::create_thread) exists for
    /// callers that have already built their own block and want to hand it over.
    ///
    /// # Errors
    ///
    /// As [`create_thread`](GuestCpuBackend::create_thread), plus [`CpuError::Memory`] if the TLS
    /// block could not be committed.
    pub fn create_thread_with_tls(&self) -> CpuResult<DynarmicCpu> {
        let tls = self.shared.tls.allocate(&self.shared.space)?;
        let config = GuestThreadConfig::new(self.shared.extent, tls.thread_pointer())?;
        self.build(config, Some(tls))
    }

    fn build(
        &self,
        config: GuestThreadConfig,
        tls: Option<GuestTls>,
    ) -> CpuResult<DynarmicCpu> {
        let processor_id = self.shared.take_processor_id().ok_or(CpuError::Unsupported {
            backend: BACKEND_NAME,
            operation: "create another guest thread",
            reason: "the shared exclusive monitor is sized at backend creation and every live \
                     guest thread needs a distinct processor id within it",
        })?;
        DynarmicCpu::new(Arc::clone(&self.shared), config, tls, processor_id).inspect_err(|_| {
            // Either the jit was never created or `DynarmicCpu::drop` has already freed it: an
            // `Err` out of `new` leaves no live jit holding this id either way.
            self.shared.release_processor_id(processor_id, true);
        })
    }
}

impl GuestCpuBackend for DynarmicBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn create_thread(&self, config: GuestThreadConfig) -> CpuResult<Box<dyn GuestCpu>> {
        Ok(Box::new(self.build(config, None)?))
    }

    fn shared_cost(&self) -> ContextCost {
        self.shared.tls.cost()
    }
}

impl core::fmt::Debug for DynarmicBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DynarmicBackend")
            .field("extent", &self.shared.extent)
            .field("options", &self.shared.options)
            .field("owns_guest_paging", &self.shared.owns_guest_paging)
            .finish()
    }
}

/// What a callback decided the run should stop with. Read by `run` once the generated frames are
/// gone, so that building an [`ExitReason`] never happens inside guest execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingExit {
    Returned { pc: GuestAddr },
    Thunk { pc: GuestAddr },
    Unsupported { pc: GuestAddr, encoding: u32 },
    /// `pc` is `None` for a *data* fault, which is filled in by `run` from the guest PC after the
    /// run has returned. That is the honest source: `check_halt_on_memory_access` makes the emitter
    /// store the faulting instruction's PC and force-return, so it is exact afterwards — whereas
    /// reading it inside the callback would give whatever the current block last wrote, which under
    /// block linking is the entry PC of the block and not the instruction.
    Fault { pc: Option<GuestAddr>, address: GuestAddr, access: AccessKind },
    Breakpoint { pc: GuestAddr },
}

/// The SSE control word, and the guard that keeps the guest's out of host code.
///
/// # Why this is in the dispatcher and not in each handler
///
/// `BlockOfCode::GenRunCode` does `stmxcsr` of the host's word and `ldmxcsr` of `guest_MXCSR` before
/// jumping into translated code, and restores the host's **only** on the two `FORCE_RETURN` paths.
/// `A64EmitX64::EmitA64CallSupervisor` calls `Devirtualize<CallSVC>::EmitCall` with no
/// `code.SwitchMxcsrOnExit()` in front of it — the only terminal in the whole A64 emitter that
/// switches before a host call is `IR::Term::Interpret` — and `return_from_run_code[0]`, the
/// dispatcher an inline thunk returns through, never touches it either.
///
/// So a host callback reached from generated code inherits the guest's rounding mode and its
/// flush-to-zero and denormals-are-zero bits, which `A64JitState::SetFpcr` maps out of `FPCR`. Rust's
/// `f32`/`f64` compile to SSE, and `exp`, `log`, `powf` and `sincosf` are all among the imports the
/// 3,594 static initializers reach — so a host `powf` serviced inline would compute with denormals
/// flushed and return a plausible number, with no error anywhere.
///
/// **The guard therefore lives here, at the one place every inline handler passes through, and not in
/// the handlers.** A guard a handler is supposed to remember is a guard that is silently absent from
/// the handler that forgot it, and that is exactly the defect class this one exists to close. It
/// restores the **guest's** word on the way out as well as installing the host's on the way in,
/// because the guest resumes inside the same `od_jit_run` and nothing else will put it back.
///
/// Not covered, and stated rather than implied: the x87 control word. Neither dynarmic nor Rust's
/// `f32`/`f64` codegen uses x87 on x86-64, so there is nothing to switch; if that ever stops being
/// true this is where it goes.
pub(crate) mod mxcsr {
    /// Read `MXCSR`.
    ///
    /// `_mm_getcsr` is deprecated in favour of exactly this instruction.
    #[must_use]
    pub(crate) fn read() -> u32 {
        let mut out: u32 = 0;
        // SAFETY: SSE2 is baseline on x86-64, and this module is only compiled there. `stmxcsr`
        // writes four bytes to a `u32` this frame owns.
        unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut out, options(nostack)) };
        out
    }

    /// Write `MXCSR`.
    pub(crate) fn write(value: u32) {
        // SAFETY: as `read`. `ldmxcsr` reads four bytes from a `u32` this frame owns. A reserved bit
        // would fault, and every value written here was read out of `MXCSR` in the first place.
        unsafe { core::arch::asm!("ldmxcsr [{}]", in(reg) &value, options(nostack)) };
    }

    /// Installs the host's `MXCSR` for the body of a host callback and puts the guest's back.
    ///
    /// Nothing is switched when the two words are already equal, which is the common case — the guest
    /// has not touched `FPCR` — so the guard costs one `stmxcsr` and a compare on that path.
    pub(crate) struct Guard {
        guest: u32,
        switched: bool,
    }

    impl Guard {
        pub(crate) fn enter(host: u32) -> Self {
            let guest = read();
            let switched = guest != host;
            if switched {
                write(host);
            }
            Self { guest, switched }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.switched {
                write(self.guest);
            }
        }
    }
}

/// [`mxcsr::Guard`], for the benchmark that prices it.
///
/// The guard itself stays crate-private — it is the dispatcher's business and nothing else should be
/// constructing one — but "a correctness fix whose cost is unknown" is how an argument gets had
/// later, so `tests/thunk.rs` is given a door to measure it through. It is `#[doc(hidden)]` and named
/// after what it is for.
#[doc(hidden)]
#[must_use]
pub fn mxcsr_guard_for_measurement(host_mxcsr: u32) -> impl Drop {
    mxcsr::Guard::enter(host_mxcsr)
}

/// The guest register file at a thunk, over dynarmic's `JitState`.
///
/// Reads and writes go straight to `JitState`, which is where the A64 emitter keeps guest registers
/// at every callback boundary, so a write here is what the resumed guest sees. It is the translating
/// backend's [`ThunkRegs`] and is handed to a handler as a [`ThunkCall`](crate::ThunkCall), which is the shape the
/// compatibility layer is written against — see `crate::thunk` for why that indirection is not
/// optional.
pub struct JitRegs<'a> {
    jit: *mut c_void,
    _borrow: core::marker::PhantomData<&'a mut ()>,
}

impl JitRegs<'_> {
    pub(crate) fn new(jit: *mut c_void) -> Self {
        Self { jit, _borrow: core::marker::PhantomData }
    }
}

impl ThunkRegs for JitRegs<'_> {
    /// Read `X{index}`. An index above 30 reads zero, which the shim enforces.
    fn x(&self, index: u32) -> u64 {
        // SAFETY: the jit is live -- this runs inside one of its own callbacks -- and the shim
        // bounds-checks the index.
        unsafe { od_jit_get_reg(self.jit, index) }
    }

    /// Write `X{index}`. An index above 30 is ignored.
    fn set_x(&mut self, index: u32, value: u64) {
        // SAFETY: as `x`.
        unsafe { od_jit_set_reg(self.jit, index, value) }
    }

    /// Read `V{index}` as its full 128 bits. An index above 31 reads zero.
    ///
    /// Needed as much as [`x`](Self::x): AAPCS64 passes floating-point and vector arguments in
    /// `V0`-`V7` and returns in `V0`, so a marshal that could only reach the general-purpose
    /// registers would silently drop every `double` argument. `A64EmitX64::EmitA64SetQ` stores to
    /// `JitState.vec` with a `movaps` exactly as `EmitA64SetX` stores to `JitState.reg`, so the vector
    /// file is coherent at a callback for the same reason the integer file is — and
    /// `an_inline_handler_sees_and_writes_the_guest_vector_file` establishes it rather than trusting
    /// the symmetry.
    fn v(&self, index: u32) -> u128 {
        let mut halves = [0u64; 2];
        // SAFETY: the jit is live and the shim bounds-checks the index and writes both halves.
        unsafe { od_jit_get_vec(self.jit, index, halves.as_mut_ptr()) };
        u128::from(halves[0]) | (u128::from(halves[1]) << 64)
    }

    /// Write `V{index}`. An index above 31 is ignored.
    fn set_v(&mut self, index: u32, value: u128) {
        let halves = [value as u64, (value >> 64) as u64];
        // SAFETY: as `v`.
        unsafe { od_jit_set_vec(self.jit, index, halves.as_ptr()) };
    }

    /// Read `SP`.
    ///
    /// The ninth and later AAPCS64 arguments live at `[SP]` upward, and a variadic call's overflow
    /// area is there too, so a register file without this could marshal at most eight arguments —
    /// and would do it silently, reading whatever `X0`-`X7` happened to hold for the ninth.
    fn sp(&self) -> GuestAddr {
        // SAFETY: as `x`. `SP` is a field of `JitState` like any other.
        unsafe { od_jit_get_sp(self.jit) as GuestAddr }
    }

    /// Write `SP`.
    fn set_sp(&mut self, value: GuestAddr) {
        // SAFETY: as `x`.
        unsafe { od_jit_set_sp(self.jit, value as u64) }
    }
}

/// Host state reachable from generated guest code.
///
/// A separate allocation from [`DynarmicCpu`] on purpose: see the module docs on re-entrancy.
pub(crate) struct CpuCtx {
    pub(crate) space: Arc<GuestSpace>,
    pub(crate) extent: GuestAddressSpace,
    pub(crate) jit: *mut c_void,

    /// The last region `read_code` looked up, so that translating a run of instructions in one
    /// function does not take the space's lock once per instruction.
    ///
    /// Invalidated by [`GuestCpu::invalidate_code`] and at the top of every `run`, which are the two
    /// moments the guest's own mappings can have changed underneath it. A stale entry here would
    /// mean fetching an instruction from a range that has since been unmapped, so it is cleared
    /// eagerly rather than validated lazily.
    ///
    /// The third element is **whether the region was committed end to end** when it was cached, and
    /// it is the one thing that makes the cache safe to use. Without it the cache short-circuited
    /// `ensure_committed` for every fetch after the first inside a region, so a lazily-committed
    /// anonymous executable region larger than the 64 KiB commit granule would be read from *Rust*
    /// code at an uncommitted address — which dynarmic's frame-based handler does not cover, so it
    /// would rely on the demand pager existing, and `owns_guest_paging()` can be false. Not reachable
    /// for M2's file-backed image; reachable the moment a guest JIT exists. It was also written and
    /// never read, which is how it survived review.
    pub(crate) executable_cache: Option<(GuestAddr, GuestAddr, bool)>,

    pub(crate) thunks: BTreeSet<GuestAddr>,
    /// Thunks serviced **inside** the run loop rather than by exiting to the caller. See
    /// [`DynarmicCpu::add_inline_thunk`].
    pub(crate) inline_thunks: BTreeMap<GuestAddr, (ThunkFn, ThunkContext)>,
    /// How many inline thunks have been serviced, so a measurement can prove the path ran.
    pub(crate) inline_calls: u64,
    /// How many of those asked to be handed back to the caller. See
    /// [`ThunkCall::defer_to_caller`].
    pub(crate) inline_deferred: u64,
    /// The host thread's `MXCSR`, captured at the top of every `run`. See [`mxcsr`].
    pub(crate) host_mxcsr: u32,
    pub(crate) breakpoints: BTreeSet<GuestAddr>,
    /// The sentinel return address planted in `X30`, if one is armed.
    pub(crate) sentinel: Option<GuestAddr>,
    /// A breakpoint to ignore for exactly one fetch, so that resuming from a breakpoint executes the
    /// instruction under it rather than tripping over it again.
    pub(crate) suppressed_breakpoint: Option<GuestAddr>,

    pub(crate) pending: Option<PendingExit>,
    pub(crate) ticks_remaining: u64,
    pub(crate) ticks_used: u64,
    pub(crate) panic_msg: Option<String>,
}

impl CpuCtx {
    /// Whether `address` is in a mapped region this access is allowed by, committing it if the
    /// mapping is lazy and the granule is not committed yet.
    ///
    /// **The policy is not here.** It is [`omni_mem::admit`], which is the same function the demand
    /// pager asks — see that module for the two divergent copies this replaced and for the one place
    /// the callers still legitimately differ. What is left here is translating dynarmic's vocabulary
    /// into the policy's, and the decision about what to cache.
    ///
    /// Returns the region's extent, and **whether the region was already committed end to end**. The
    /// second element is what makes caching sound: see [`CpuCtx::fetch`].
    pub(crate) fn resolve(
        &self,
        address: GuestAddr,
        len: usize,
        want: Protection,
    ) -> Option<(GuestAddr, GuestAddr, bool)> {
        // Kept, and redundant on purpose. `admit`'s first rule refuses an unmapped address from the
        // region map, which is authoritative; this is the cheap reject for an address that cannot
        // possibly be in the space, on a path every guest fault takes.
        if !self.extent.contains(address) {
            return None;
        }
        // dynarmic asks in terms of the protection it wants; the policy asks in terms of the access
        // being attempted. The mapping is total and is the only translation between the two.
        let access = match want {
            Protection::ReadExecute => FaultAccess::Execute,
            Protection::ReadWrite => FaultAccess::Write,
            Protection::Read | Protection::None => FaultAccess::Read,
        };
        // Committing here rather than leaving it to the fault handler keeps this path working on a
        // platform with no vectored-handler implementation, where `owns_guest_paging()` is false.
        let admitted = omni_mem::admit(&self.space, address, len, access).ok()?;
        Some((admitted.start, admitted.end, admitted.fully_committed))
    }
}

/// One guest thread's CPU.
pub struct DynarmicCpu {
    jit: *mut c_void,
    ctx: Box<UnsafeCell<CpuCtx>>,
    /// dynarmic inlines this pointer into generated code, so the box must never move.
    tpidr_el0: Box<u64>,
    tpidrro_el0: Box<u64>,
    shared: Arc<Shared>,
    tls: Option<GuestTls>,
    processor_id: u32,
    halt: HaltHandle,
    cost: ContextCost,
    /// Slices whose callback-path delta broke the invariant. See [`Self::degraded_slices`].
    degraded_slices: u64,
    /// Guest instructions the most recent [`GuestCpu::run`] executed. See
    /// [`Self::last_run_instructions`].
    last_run_instructions: u64,
    /// Whether the invariant is armed for this context. Copied from the backend at construction
    /// so the hot path does not chase an `Arc` per slice.
    slice_invariant_armed: bool,
}

// SAFETY: `GuestCpu` is `Send` and not `Sync`, which is exactly this type's contract: one context
// belongs to one guest thread and is moved to whichever thread runs it. Everything reachable from
// it is owned by it — the jit, the context allocation and the two pinned registers — except the
// `Arc<Shared>`, whose contents are themselves `Send + Sync`. Nothing is shared with another
// `DynarmicCpu`, so moving one to another thread hands over the whole graph.
unsafe impl Send for DynarmicCpu {}

impl DynarmicCpu {
    fn new(
        shared: Arc<Shared>,
        config: GuestThreadConfig,
        tls: Option<GuestTls>,
        processor_id: u32,
    ) -> CpuResult<Self> {
        let extent = shared.extent;
        let cpu =
            Self::build_unchecked(shared, config, tls, processor_id, Overrides::default())?;
        // **The startup assertion.** Read back from the live `UserConfig` rather than echoed from
        // what was asked for, and run before the context is handed to anyone, so a context that
        // exists is a context whose memory path is D4's.
        require_identity_mapping(&cpu.memory_mapping(), extent)?;
        Ok(cpu)
    }

    fn build_unchecked(
        shared: Arc<Shared>,
        config: GuestThreadConfig,
        tls: Option<GuestTls>,
        processor_id: u32,
        overrides: Overrides,
    ) -> CpuResult<Self> {
        let ctx = Box::new(UnsafeCell::new(CpuCtx {
            space: Arc::clone(&shared.space),
            extent: config.space(),
            jit: core::ptr::null_mut(),
            executable_cache: None,
            thunks: BTreeSet::new(),
            inline_thunks: BTreeMap::new(),
            inline_calls: 0,
            inline_deferred: 0,
            host_mxcsr: mxcsr::read(),
            breakpoints: BTreeSet::new(),
            sentinel: None,
            suppressed_breakpoint: None,
            pending: None,
            ticks_remaining: 0,
            ticks_used: 0,
            panic_msg: None,
        }));

        // D13: the thread pointer is programmed *before* the jit exists, so there is no window in
        // which a context could be run with a zero one.
        let mut tpidr_el0 = Box::new(config.tpidr_el0() as u64);
        let tpidrro_el0 = Box::new(config.tpidr_el0() as u64);

        let options = shared.options;
        let cfg = OdConfig {
            abi_version: OD_DYNARMIC_ABI_VERSION,
            callbacks: &callbacks::CALLBACKS,
            ctx: ctx.get().cast::<c_void>(),
            // D13. The shim accepts a null pointer here — its header says the guest's read then
            // faults into `exception_raised` — which is precisely the configuration the startup
            // assertion's `TPIDR_EL0 storage` check exists to refuse, so it has to be reachable for
            // that check to be provable. Only `create_misconfigured_thread` can ask for it.
            tpidr_el0: if overrides.null_thread_pointer {
                core::ptr::null_mut()
            } else {
                &mut *tpidr_el0
            },
            tpidrro_el0: if overrides.null_thread_pointer {
                core::ptr::null()
            } else {
                &*tpidrro_el0
            },
            // D4, all four fields together. `fastmem_pointer = 0` with 64 bits is the identity
            // mapping; mirroring is off so a wild guest address faults instead of aliasing a valid
            // page; recompiling on a fastmem failure is what routes a declined fault to the slow
            // path, where it becomes a typed exit.
            fastmem_enabled: i32::from(overrides.direct_access.unwrap_or(true)),
            fastmem_pointer: 0,
            fastmem_address_space_bits: overrides.address_space_bits.unwrap_or(64),
            silently_mirror_fastmem: i32::from(overrides.mirrors_out_of_range.unwrap_or(false)),
            recompile_on_fastmem_failure: 1,
            // Task 2's review measured this: off gives 1 slow-path read plus 1 exclusive callback
            // per `LDXR`, on gives 0, which matters because D5 lists the global exclusive monitor's
            // 21x anti-scaling as a primary risk.
            fastmem_exclusive_access: 1,
            monitor: shared.monitor.0,
            processor_id,
            code_cache_size: options.code_cache_size,
            // Programmed rather than left at 0, which would select dynarmic's own default. The
            // default is the same 600 MHz, so nothing a guest can read changes -- but the counter
            // `cb_get_cntpct` returns is scaled by this same constant, and two defaults that happen
            // to agree is not the same thing as one constant used twice. See `crate::clock`.
            cntfrq_el0: crate::clock::CNTFRQ_HZ,
            ctr_el0: 0,
            dczid_el0: 4,
            // The watchdog. See `crate::run`.
            enable_cycle_counting: 1,
            wall_clock_cntpct: 0,
            hook_hint_instructions: 0,
            define_unpredictable_behaviour: 0,
            check_halt_on_memory_access: i32::from(options.check_halt_on_memory_access),
            unsafe_optimizations: 0,
            optimizations: options.optimizations(),
        };

        // SAFETY: `cfg` is fully initialised; `callbacks` is a `'static` constant; `ctx`, the two
        // register boxes and the monitor all outlive the jit, which is freed in `Drop` before any of
        // them. dynarmic copies `cfg` and keeps the pointers.
        let jit = unsafe { od_jit_new(&cfg) };
        if jit.is_null() {
            return Err(CpuError::Backend {
                backend: BACKEND_NAME,
                operation: "create a jit",
                detail: format!(
                    "od_jit_new rejected the configuration (code_cache_size = {}, processor_id = \
                     {processor_id})",
                    options.code_cache_size
                ),
            });
        }

        // SAFETY: nothing is executing yet, so no callback can hold a reference.
        unsafe {
            (*ctx.get()).jit = jit;
        }

        let armed = options.assert_callback_free_slices && shared.owns_guest_paging;

        Ok(Self {
            jit,
            ctx,
            tpidr_el0,
            tpidrro_el0,
            shared,
            cost: ContextCost {
                // The guest's TLS block, plus the per-jit state this pin allocates unconditionally.
                // Both are derived rather than measured: the first is one page by construction, the
                // second is `sizeof(FastDispatchEntry) * fast_dispatch_table_size` from the pin,
                // checked against the vendored header by `dynarmic-sys`'s `pin_constants` test. The
                // code cache's committed high-water mark is still missing; see `cost`.
                private_committed: tls
                    .as_ref()
                    .map_or(0, GuestTls::len)
                    .saturating_add(OD_FIXED_PER_JIT_BYTES),
                shared_committed: 0,
            },
            tls,
            processor_id,
            halt: HaltHandle::new(),
            degraded_slices: 0,
            last_run_instructions: 0,
            slice_invariant_armed: armed,
        })
    }

    /// What dynarmic reports it is actually configured with, in Omnidroid's vocabulary.
    #[must_use]
    pub fn memory_mapping(&self) -> MemoryMapping {
        let observed = self.effective_config();
        MemoryMapping {
            direct_access: observed.fastmem_enabled != 0,
            host_base: observed.fastmem_pointer,
            address_bits: observed.fastmem_address_space_bits,
            mirrors_out_of_range: observed.silently_mirror_fastmem != 0,
            page_table_present: observed.page_table_present != 0,
            counts_instructions: observed.enable_cycle_counting != 0,
            tpidr_el0_slot: observed.tpidr_el0_ptr,
        }
    }

    /// dynarmic's live configuration, unfiltered. For tests and diagnostics.
    #[must_use]
    pub fn effective_config(&self) -> OdEffectiveConfig {
        let mut out = OdEffectiveConfig::default();
        // SAFETY: `self.jit` is live for `self`'s lifetime and `out` is writable.
        unsafe { od_jit_effective_config(self.jit, &mut out) };
        out
    }

    /// Callback-entry counters. `slow_path_total` is the one D4's assertion cares about: under
    /// identity fastmem it must stay **zero** for code that only touches mapped memory.
    #[must_use]
    pub fn stats(&self) -> OdStats {
        let mut out = OdStats::default();
        // SAFETY: as `effective_config`.
        unsafe { od_jit_stats(self.jit, &mut out) };
        out
    }

    /// Zero the callback-entry counters, so one loop can be measured on its own.
    pub fn reset_stats(&self) {
        // SAFETY: as `effective_config`.
        unsafe { od_jit_reset_stats(self.jit) };
    }

    /// How many times generated code has entered a data-memory callback.
    ///
    /// One load, not a struct copy, because [`run`](GuestCpu::run) reads it twice per slice. Under
    /// D4's identity mapping this stays at zero for guest code that only touches mapped memory:
    /// see [`CpuError::DegradedMemoryPath`].
    #[must_use]
    pub fn slow_path_entries(&self) -> u64 {
        // SAFETY: the jit is live, and `&self` cannot overlap a `run` — `run` takes `&mut self`.
        // The counter is non-atomic and written only by callbacks, which run on this thread.
        unsafe { od_jit_slow_path_total(self.jit) }
    }

    /// How many run slices were found to have degraded onto the callback path.
    ///
    /// Non-zero only when the invariant is disarmed, since an armed one turns the first violation
    /// into [`CpuError::DegradedMemoryPath`] and there is no second.
    #[must_use]
    pub fn degraded_slices(&self) -> u64 {
        self.degraded_slices
    }

    /// Read `TPIDRRO_EL0`, the read-only alias of the thread pointer.
    ///
    /// Bionic gives both registers the same value on AArch64, and D5 confirmed dynarmic supports
    /// both. It is exposed separately because guest code can read `TPIDRRO_EL0` from EL0 while
    /// `TPIDR_EL0` is the one it may write, so a guest that finds them disagreeing would be seeing a
    /// state no real kernel produces.
    #[must_use]
    pub fn tpidrro_el0(&self) -> GuestAddr {
        *self.tpidrro_el0 as GuestAddr
    }

    /// The bionic TLS block this context owns, if it allocated one.
    #[must_use]
    pub fn tls(&self) -> Option<&GuestTls> {
        self.tls.as_ref()
    }

    fn with_ctx<R>(&self, f: impl FnOnce(&mut CpuCtx) -> R) -> R {
        // SAFETY: `&self` here can never overlap a `run`, because `run` takes `&mut self` and
        // therefore no `&self` borrow is live while generated code is on the stack. That is the
        // discipline `dynarmic-sys` asks for, expressed as a borrow rather than a comment.
        f(unsafe { &mut *self.ctx.get() })
    }

    /// Drop the translation covering one instruction. Used whenever a thunk, breakpoint or sentinel
    /// is added or removed, because `read_code` only runs at translation time.
    fn invalidate_word(&self, address: GuestAddr) -> CpuResult<()> {
        // SAFETY: `self.jit` is live. `od_jit_invalidate_range` is documented as safe from any
        // thread and from inside a callback, and the shim clamps the length.
        unsafe { od_jit_invalidate_range(self.jit, address as u64, 4) };
        Ok(())
    }

    fn take_panic(&self) -> CpuResult<()> {
        let msg = self.with_ctx(|ctx| ctx.panic_msg.take());
        match msg {
            None => Ok(()),
            Some(detail) => Err(CpuError::Backend {
                backend: BACKEND_NAME,
                operation: "run guest code",
                detail: format!("a callback panicked and was contained at the FFI boundary: {detail}"),
            }),
        }
    }
}

impl Drop for DynarmicCpu {
    /// **Order is load-bearing, and it was wrong.**
    ///
    /// The jit is freed *first*, and only then is the processor id given back. The other order is
    /// what this used to do, and it opens a window: `release_processor_id` puts the id on the free
    /// list, another thread's `build` takes it, and for as long as `od_jit_free` has not run there
    /// are two live jits pointed at the **same entry of the shared `ExclusiveMonitor`**. Two guest
    /// threads on one monitor entry makes `STXR` succeed where the architecture requires it to fail
    /// — a silent wrong answer in the subsystem D5 lists as risk 3 of 4, with no error anywhere.
    ///
    /// The TLS block needs no statement here and that is the point: `tls` is a field, so it is
    /// dropped after this body, and [`GuestTls`]'s own `Drop` returns it to the arena. It used to be
    /// freed explicitly at the top, which is both the earliest safe moment and the one that stopped
    /// being reached when a constructor failed halfway.
    fn drop(&mut self) {
        // SAFETY: `&mut self` means nothing is executing, and the jit is freed exactly once. It is
        // freed before `ctx`, `tpidr_el0` and `tpidrro_el0` — which are dropped after this — and
        // before the `Arc<Shared>` that owns the monitor it points at.
        unsafe { od_jit_free(self.jit) };
        // Nulled so that `self.jit.is_null()` below *is* the statement "the jit is gone" rather
        // than a comment claiming it, and so a use-after-free of this field would be a null
        // dereference rather than a dangling one.
        self.jit = core::ptr::null_mut();
        // Only now: no jit can reference this processor's monitor entry any more.
        self.shared.release_processor_id(self.processor_id, self.jit.is_null());
    }
}

impl GuestCpu for DynarmicCpu {
    fn backend_name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            counted_step_limit: true,
            // Honest rather than optimistic. A halt is observed **between slices**, so it stops any
            // guest whose slice budget can expire. Under the default optimization flags a guest
            // `BR X30` branching to itself expires no budget and checks no halt flag, so nothing
            // stops it — which is precisely what `interruptible` exists to fix, and why this field
            // is computed rather than hard-coded to `true`.
            asynchronous_halt: self.shared.options.interruptible,
            breakpoints: true,
            // See `add_inline_thunk`: the `SVC` terminal's `CheckHalt{PopRSBHint}` is what makes it
            // possible, and `INTERRUPTIBLE` is what makes `PopRSBHint` reach the dispatcher rather
            // than a return-stack-buffer guess. With the flag cleared the resume would be to
            // whatever the RSB predicted, which is not the address the handler wrote.
            inline_thunks: self.shared.options.interruptible,
        }
    }

    fn space(&self) -> GuestAddressSpace {
        self.shared.extent
    }

    fn run(&mut self, from: GuestAddr, limit: RunLimit) -> CpuResult<ExitReason> {
        self.set_pc(from);
        self.with_ctx(|ctx| {
            ctx.executable_cache = None;
            ctx.pending = None;
            ctx.panic_msg = None;
            // Resuming from a breakpoint must execute the instruction under it, or a caller could
            // never step past one. Exactly one fetch is suppressed, and only at the entry address.
            ctx.suppressed_breakpoint =
                ctx.breakpoints.contains(&from).then_some(from);
        });
        if self.with_ctx(|ctx| ctx.suppressed_breakpoint.is_some()) {
            self.invalidate_word(from)?;
        }

        // **On the early returns below, and why there is no halt-clearing here.**
        //
        // Four exits from the loop return before the `od_jit_clear_halt` further down —
        // `take_panic`, the two shim failures and `DegradedMemoryPath` — and the whole-branch review
        // read that as leaving the context poisoned: the next `run` would return from `od_jit_run`
        // having executed nothing, and the classifier at the bottom of this loop would report it as
        // "halted with reason … which this backend does not raise and cannot classify", blaming
        // dynarmic for a bit this backend left behind.
        //
        // **That does not hold on this pin, and the emitted dispatcher is where to see it.**
        // `BlockOfCode::GenRunCode` ends every return path with `xor eax, eax; lock xchg
        // [r15 + halt_reason], eax` (`block_of_code.cpp:403-405`): the halt reason is read *and
        // cleared*, atomically, by the generated code, and handed back as `Run`'s return value. So a
        // context always re-enters `Jit::Run` with `halt_reason == 0` however this loop left it, and
        // `tests/lifecycle.rs` runs a context twice across a `DegradedMemoryPath` to keep saying so.
        //
        // An entry clear was written, measured against that, and removed. It changed no behaviour,
        // and it is not free: it is a lock-prefixed RMW on the per-call path, and the guest call
        // boundary M3 budgets against is about 33 ns in total for an inline thunk -- see
        // `tests/thunk.rs`, which replaced D5 amendment 2's "under 53 ns" with a measured round trip.
        // A lock-prefixed RMW is a material fraction of that, which is why this stayed removed.
        //
        // The residual, stated rather than swept up: `OD_HALT_SHIM_REENTERED` and
        // `OD_HALT_SHIM_THREW` never reach that `xchg` — the first never calls `Run`, and the second
        // unwinds out of it — so a bit set before either can survive. Both already declare the jit
        // uncharacterised and not to be reused, which is a stronger statement than a stale halt bit.
        // The host thread's SSE control word, captured **here** rather than at construction,
        // because a context is created on whichever thread brings the guest thread up and moved to
        // the one that runs it, and the two can have different words. Everything an inline thunk
        // handler runs is host code, and [`mxcsr::Guard`] puts this back for it.
        let host_mxcsr = mxcsr::read();
        self.with_ctx(|ctx| ctx.host_mxcsr = host_mxcsr);

        let mut budget = Budget::new(limit);
        self.last_run_instructions = 0;
        loop {
            if self.halt.is_requested() {
                return Ok(ExitReason::Halted { pc: self.pc() });
            }
            let Some(slice) = budget.slice() else {
                return Ok(ExitReason::StepLimitReached {
                    pc: self.pc(),
                    executed: budget.executed(),
                });
            };
            self.with_ctx(|ctx| {
                ctx.ticks_remaining = slice;
                ctx.ticks_used = 0;
            });

            // The per-slice callback invariant (`CpuError::DegradedMemoryPath`). One load before
            // and one after, on the jit's own thread, around a slice that is a million guest
            // instructions by default.
            let callbacks_before = self.slice_invariant_armed.then(|| self.slow_path_entries());

            // SAFETY: the jit is live; `&mut self` means no `&mut CpuCtx` is outstanding at this
            // call site; every callback contains its own panics. This executes attacker-controlled
            // guest code, which is the point: the memory it can reach is the guest space plus
            // whatever else identity mapping exposes, and the containment for that is the
            // one-process-per-instance boundary in `ARCHITECTURE.md` section 7.
            let halt_reason = unsafe { od_jit_run(self.jit) };

            let used = self.with_ctx(|ctx| ctx.ticks_used);
            budget.charge(used);
            self.last_run_instructions = budget.executed();
            self.take_panic()?;

            if halt_reason & OD_HALT_SHIM_REENTERED != 0 {
                return Err(CpuError::Backend {
                    backend: BACKEND_NAME,
                    operation: "run guest code",
                    detail: "od_jit_run was called while this jit was already executing".into(),
                });
            }
            if halt_reason & OD_HALT_SHIM_THREW != 0 {
                return Err(CpuError::Backend {
                    backend: BACKEND_NAME,
                    operation: "run guest code",
                    detail: "a C++ exception escaped Jit::Run and was caught at the shim; the jit \
                             is in an uncharacterised state and must not be reused"
                        .into(),
                });
            }
            // The per-slice callback invariant, checked **after** the two shim failures above:
            // a re-entered jit and an escaped C++ exception both mean the jit is in an
            // uncharacterised state, which subsumes anything this could say about it.
            if let Some(before) = callbacks_before {
                let delta = self.slow_path_entries().saturating_sub(before);
                if delta != 0 {
                    // The one exemption, and it is narrow on purpose: a genuine guest fault
                    // *arrives* through the callback, so it increments the counter. Anything else
                    // that increments it is a block that used to reach memory directly and no
                    // longer does.
                    let exit = self.with_ctx(|ctx| match ctx.pending {
                        Some(PendingExit::Fault { .. }) => None,
                        Some(PendingExit::Returned { .. }) => Some("the guest returned"),
                        Some(PendingExit::Thunk { .. }) => Some("the guest reached a thunk"),
                        Some(PendingExit::Unsupported { .. }) => {
                            Some("an unsupported instruction")
                        }
                        Some(PendingExit::Breakpoint { .. }) => Some("a breakpoint"),
                        None => Some("the slice ran to the end of its budget"),
                    });
                    if let Some(exit) = exit {
                        self.degraded_slices += 1;
                        return Err(CpuError::DegradedMemoryPath {
                            pc: self.pc(),
                            callbacks: delta,
                            exit,
                        });
                    }
                }
            }

            if halt_reason & HALT_OURS != 0 {
                // SAFETY: the jit is live and not executing.
                unsafe { od_jit_clear_halt(self.jit, HALT_OURS) };
            }

            self.with_ctx(|ctx| ctx.suppressed_breakpoint = None);

            if let Some(pending) = self.with_ctx(|ctx| ctx.pending.take()) {
                return Ok(match pending {
                    PendingExit::Returned { pc } => ExitReason::Returned { pc },
                    PendingExit::Thunk { pc } => ExitReason::Thunk { pc },
                    PendingExit::Unsupported { pc, encoding } => {
                        ExitReason::UnsupportedInstruction { pc, encoding }
                    }
                    PendingExit::Fault { pc, address, access } => ExitReason::MemoryFault {
                        pc: pc.unwrap_or_else(|| self.pc()),
                        address,
                        access,
                    },
                    PendingExit::Breakpoint { pc } => {
                        // The instruction at `pc` has not run. dynarmic advanced the guest PC past
                        // the `BRK` before raising, so put it back: `GuestCpu::add_breakpoint`
                        // promises that resuming from `pc` runs the instruction.
                        self.set_pc(pc);
                        ExitReason::Breakpoint { pc }
                    }
                });
            }

            if halt_reason & OD_HALT_CACHE_INVALIDATION != 0 {
                continue;
            }
            if budget.is_exhausted() {
                return Ok(ExitReason::StepLimitReached {
                    pc: self.pc(),
                    executed: budget.executed(),
                });
            }
            if halt_reason != 0 {
                return Err(CpuError::Backend {
                    backend: BACKEND_NAME,
                    operation: "run guest code",
                    detail: format!(
                        "the jit halted with reason {halt_reason:#010x}, which this backend does \
                         not raise and cannot classify"
                    ),
                });
            }
            // Otherwise the slice's budget expired with no stop of its own: go round again.
        }
    }

    fn last_run_instructions(&self) -> u64 {
        self.last_run_instructions
    }

    fn halt_handle(&self) -> HaltHandle {
        self.halt.clone()
    }

    fn x(&self, reg: XReg) -> u64 {
        // SAFETY: the jit is live and the shim bounds-checks the index.
        unsafe { od_jit_get_reg(self.jit, u32::from(reg.index())) }
    }

    fn set_x(&mut self, reg: XReg, value: u64) {
        // SAFETY: as `x`.
        unsafe { od_jit_set_reg(self.jit, u32::from(reg.index()), value) };
    }

    fn sp(&self) -> GuestAddr {
        // SAFETY: the jit is live.
        unsafe { od_jit_get_sp(self.jit) as GuestAddr }
    }

    fn set_sp(&mut self, value: GuestAddr) {
        // SAFETY: the jit is live.
        unsafe { od_jit_set_sp(self.jit, value as u64) };
    }

    fn pc(&self) -> GuestAddr {
        // SAFETY: the jit is live. Note D4: the value comes back sign-extended from 56 bits.
        unsafe { od_jit_get_pc(self.jit) as GuestAddr }
    }

    fn set_pc(&mut self, value: GuestAddr) {
        // SAFETY: the jit is live.
        unsafe { od_jit_set_pc(self.jit, value as u64) };
    }

    fn nzcv(&self) -> Nzcv {
        // SAFETY: the jit is live.
        Nzcv::from_pstate(u64::from(unsafe { od_jit_get_pstate(self.jit) }))
    }

    fn set_nzcv(&mut self, value: Nzcv) {
        // SAFETY: the jit is live. Only the four condition bits are replaced; the rest of PSTATE is
        // read back and preserved, because `Nzcv` deliberately cannot name them.
        unsafe {
            let pstate = od_jit_get_pstate(self.jit);
            od_jit_set_pstate(self.jit, (pstate & !Nzcv::MASK) | (value.to_pstate() as u32));
        }
    }

    fn v(&self, reg: VReg) -> u128 {
        let mut out = [0u64; 2];
        // SAFETY: the jit is live and `out` is two writable `u64`.
        unsafe { od_jit_get_vec(self.jit, u32::from(reg.index()), out.as_mut_ptr()) };
        u128::from(out[0]) | (u128::from(out[1]) << 64)
    }

    fn set_v(&mut self, reg: VReg, value: u128) {
        let parts = [value as u64, (value >> 64) as u64];
        // SAFETY: the jit is live and `parts` is two readable `u64`.
        unsafe { od_jit_set_vec(self.jit, u32::from(reg.index()), parts.as_ptr()) };
    }

    fn tpidr_el0(&self) -> GuestAddr {
        *self.tpidr_el0 as GuestAddr
    }

    fn set_tpidr_el0(&mut self, value: GuestAddr) {
        // The boxes are not reallocated, so the pointers dynarmic inlined into generated code stay
        // valid. This is how a guest thread that calls `__set_tls` re-points itself.
        //
        // Both registers move together. On AArch64 bionic reads TLS through `TPIDR_EL0` and the
        // kernel keeps `TPIDRRO_EL0` in step; leaving the read-only alias behind would give guest
        // code a view no real kernel produces, and the guest has no way to tell that it is looking
        // at a stale value rather than a second thread's.
        *self.tpidr_el0 = value as u64;
        *self.tpidrro_el0 = value as u64;
    }

    fn invalidate_code(&mut self, range: GuestRange) -> CpuResult<()> {
        self.with_ctx(|ctx| ctx.executable_cache = None);
        // SAFETY: the jit is live; the shim clamps a zero or overflowing length, which matters
        // because this range comes from guest `mprotect`, guest `munmap` and the guest's own
        // `IC IVAU` (Global Constraint 11).
        unsafe { od_jit_invalidate_range(self.jit, range.start() as u64, range.len() as u64) };
        Ok(())
    }

    fn add_thunk(&mut self, address: GuestAddr) -> CpuResult<()> {
        self.with_ctx(|ctx| ctx.thunks.insert(address));
        self.invalidate_word(address)
    }

    fn remove_thunk(&mut self, address: GuestAddr) -> CpuResult<bool> {
        let had = self.with_ctx(|ctx| ctx.thunks.remove(&address));
        self.invalidate_word(address)?;
        Ok(had)
    }

    /// # Why this backend can service a thunk without leaving the run loop
    ///
    /// Because a thunk is a planted `SVC` (`STOP_SVC`, this module's own constant) and `SVC`'s
    /// terminal in dynarmic's A64
    /// frontend is `CheckHalt{PopRSBHint}`. A callback that does **not** raise a halt falls through
    /// `CheckHalt` into `PopRSBHint`, which with `ReturnStackBuffer` cleared —
    /// `optimization::INTERRUPTIBLE`, which this backend sets by default (D16) — emits
    /// `ReturnFromRunCode`. And `ReturnFromRunCode` is **not** a return to the caller: it is the top
    /// of the emitted dispatcher loop (`block_of_code.cpp`, `GenRunCode`), which re-reads
    /// `halt_reason` and `cycles_remaining`, calls `LookupBlock` and jumps straight to the next
    /// block. So writing the guest `PC` from inside the callback and returning quietly resumes the
    /// guest without unwinding the generated frame, without the `AddTicks`/`GetTicksRemaining`
    /// callbacks, without the `lock xchg` on `halt_reason`, and without re-entering `Jit::Run`.
    ///
    /// Guest registers are coherent in `JitState` at every callback — the A64 emitter stores each
    /// guest register write straight to memory — so the handler reads and writes them through
    /// [`JitRegs`] and the resumed guest sees them.
    ///
    /// The guest resumes at `X30`, which is what a `BL` into the thunk region leaves there.
    fn add_inline_thunk(
        &mut self,
        address: GuestAddr,
        handler: ThunkFn,
        context: ThunkContext,
    ) -> CpuResult<()> {
        // **Refused rather than registered when the flag is clear**, because the resume would be a
        // return-stack-buffer prediction rather than the address the handler wrote: the guest would
        // carry on somewhere plausible with a register file the handler had already changed. That is
        // Global Constraint 1's failure shape exactly, so the capability is checked here and not only
        // advertised.
        if !self.shared.options.interruptible {
            return Err(CpuError::Unsupported {
                backend: BACKEND_NAME,
                operation: "dispatch a thunk inside the run loop",
                reason: "`DynarmicOptions::interruptible` is false, so `PopRSBHint` does not reach \
                         the emitted dispatcher and the guest would not resume at the address the \
                         handler wrote",
            });
        }
        self.with_ctx(|ctx| ctx.inline_thunks.insert(address, (handler, context)));
        self.invalidate_word(address)
    }

    fn remove_inline_thunk(&mut self, address: GuestAddr) -> CpuResult<bool> {
        let had = self.with_ctx(|ctx| ctx.inline_thunks.remove(&address).is_some());
        self.invalidate_word(address)?;
        Ok(had)
    }

    fn inline_thunk_calls(&self) -> InlineThunkCounts {
        self.with_ctx(|ctx| InlineThunkCounts {
            serviced: ctx.inline_calls,
            deferred: ctx.inline_deferred,
        })
    }

    fn set_return_sentinel(&mut self, address: GuestAddr) -> CpuResult<()> {
        self.with_ctx(|ctx| ctx.sentinel = Some(address));
        self.invalidate_word(address)
    }

    fn return_sentinel(&self) -> Option<GuestAddr> {
        self.with_ctx(|ctx| ctx.sentinel)
    }

    fn add_breakpoint(&mut self, address: GuestAddr) -> CpuResult<()> {
        self.with_ctx(|ctx| ctx.breakpoints.insert(address));
        self.invalidate_word(address)
    }

    fn remove_breakpoint(&mut self, address: GuestAddr) -> CpuResult<bool> {
        let had = self.with_ctx(|ctx| ctx.breakpoints.remove(&address));
        self.invalidate_word(address)?;
        Ok(had)
    }

    /// What this context costs, **and what this figure still does not include**.
    ///
    /// Two terms, both *derived* rather than measured, so this is a floor that does not depend on
    /// what the guest has done:
    ///
    /// * the guest's bionic TLS block — one page, by construction;
    /// * [`OD_FIXED_PER_JIT_BYTES`], the 16 MiB `FastDispatchEntry` table `A64EmitX64` holds as a
    ///   by-value member, constructed and zeroed whether or not the optimization that uses it is
    ///   enabled, which this backend disables (D16). `dynarmic-sys`'s `pin_constants` test reads
    ///   both factors back out of the vendored header, so a re-pin cannot move it silently.
    ///
    /// **The missing term is dynarmic's code cache**, which on Windows commits incrementally as code
    /// is emitted (`BlockOfCode::EnsureMemoryCommitted`), so the figure that matters is a high-water
    /// mark. It is a private member of `BlockOfCode` that `A64::Jit` does not expose, and reading it
    /// would mean patching the vendored pin. It is **bounded above** by
    /// [`DynarmicOptions::code_cache_size`], and M2's gate asserts both ends: the measured
    /// per-thread charge is under a ceiling, and the gap between it and this figure is under
    /// `code_cache_size`. So the omission is bounded and asserted rather than merely admitted.
    ///
    /// Measured against this: **24.5 MiB** per guest thread at the 8 MiB default cache (n = 8
    /// threads, serialized), of which this reports 16.004 MiB. D5's 20-35 MiB band was measured
    /// against the 128 MiB default.
    fn cost(&self) -> ContextCost {
        self.cost
    }
}

impl core::fmt::Debug for DynarmicCpu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DynarmicCpu")
            .field("pc", &format_args!("{:#x}", self.pc()))
            .field("tpidr_el0", &format_args!("{:#x}", self.tpidr_el0()))
            .field("cost", &self.cost)
            .finish()
    }
}

/// Re-raise a callback's panic on the caller's thread, where unwinding is legal.
pub(crate) fn record_panic(ctx: &mut CpuCtx, payload: &(dyn core::any::Any + Send)) {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic>".to_string());
    ctx.panic_msg = Some(msg);
    if !ctx.jit.is_null() {
        // SAFETY: `od_jit_halt` is documented as callable from inside a callback; it sets an atomic
        // flag and nothing else.
        unsafe { od_jit_halt(ctx.jit, HALT_PANIC) };
    }
}

/// Run a callback body with the re-entrancy and panic discipline `dynarmic-sys` requires.
///
/// # Safety
///
/// `ctx` must be the pointer given to `od_jit_new` as `OdConfig::ctx`, which is always
/// `Box<UnsafeCell<CpuCtx>>::get()` for a box that outlives the jit.
pub(crate) unsafe fn with<R>(
    ctx: *mut c_void,
    fallback: R,
    f: impl FnOnce(&mut CpuCtx) -> R,
) -> R {
    // SAFETY: the caller's contract. Going through `UnsafeCell` is what makes forming `&mut` legal
    // while `DynarmicCpu` is only shared-borrowed.
    let raw: *mut CpuCtx = unsafe { (*(ctx as *const UnsafeCell<CpuCtx>)).get() };

    // SAFETY: no other reference to `*raw` can be live. dynarmic never nests callbacks and never
    // runs them off-thread; `run` takes `&mut self`, so no `&mut CpuCtx` exists at the call site;
    // and `with_ctx` would have to run on this thread, which is inside `run`.
    match catch_unwind(AssertUnwindSafe(|| f(unsafe { &mut *raw }))) {
        Ok(value) => value,
        Err(payload) => {
            // SAFETY: as above; the closure's borrow has ended.
            record_panic(unsafe { &mut *raw }, &*payload);
            fallback
        }
    }
}

/// Stop the run with `exit`, from inside a callback.
pub(crate) fn stop(ctx: &mut CpuCtx, exit: PendingExit, halt_bit: u32) {
    // First one wins: a second stop in the same slice would overwrite the reason the run actually
    // stopped for, and the first is the one that happened.
    if ctx.pending.is_none() {
        ctx.pending = Some(exit);
    }
    if !ctx.jit.is_null() {
        // SAFETY: callable from inside a callback; sets an atomic flag.
        unsafe { od_jit_halt(ctx.jit, halt_bit) };
    }
}
