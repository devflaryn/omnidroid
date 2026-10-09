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
//! # What fastmem does not check, and why every reader of this file needs to know
//!
//! **`admit` does not govern the guest's own loads and stores. It never has.**
//!
//! With `fastmem_pointer = 0` and `fastmem_address_space_bits = 64`, a guest `ldr x0, [x1]` is
//! compiled to a host load at *exactly* `x1`. There is no bounds check, no mask and no table
//! lookup in the generated code -- that absence is the 30-49x this setting buys. The only thing
//! that can interrupt such an access is a **host** page fault, so:
//!
//! * an address with nothing mapped behind it **in this whole process** faults,
//!   [`omni_platform::fault`] hands it to the pager, the pager answers `NotOurs` for anything
//!   outside `GuestSpace`, and `check_halt_on_memory_access` turns it into a typed
//!   [`ExitReason::MemoryFault`]. This is the case the comment beside `fastmem_pointer` describes
//!   and it is real;
//! * an address that **is** mapped in this process but is not part of `GuestSpace` -- this crate's
//!   code cache, the boundary's thunk region, the Rust heap, a loaded DLL, a graphics driver's
//!   mapped memory -- **does not fault, and is read or written directly.**
//!
//! `omni_mem::admit` is checked by [`CpuCtx::resolve`] on the *slow* path and by every handler in
//! `omni-android` that touches a guest pointer. Those are this layer's own accesses. The guest's
//! instruction stream does not go through either.
//!
//! So the sentence "`admit` refuses it, therefore the guest cannot touch it" is **false**, and it
//! has been written down in this project more than once. What `admit` gives is that *this layer*
//! will not be tricked into dereferencing a guest-chosen number -- which is Global Constraint 11,
//! and which is a different and narrower claim than guest memory isolation.
//!
//! **Consequences, stated so they are not rediscovered:**
//!
//! 1. Handing the guest any host address in a register (a `vkMapMemory` result, a host callback
//!    pointer, an allocator's return) makes that memory *work* for the guest, silently, until some
//!    shim in `omni-android` re-validates the same pointer and refuses it a long way from the
//!    cause. Preferring memory that is already inside `GuestSpace` is therefore not a safety
//!    nicety, it is what keeps one story true in both places.
//! 2. Guest code is untrusted by design (D6). Under identity fastmem, guest code that computes
//!    an address reaches whatever is at it. This runtime is a **compatibility layer, not a
//!    sandbox**, and nothing in it should be described as confining guest execution.
//! 3. Turning this into isolation is not a patch. It would mean giving up identity mapping
//!    (`fastmem_pointer` to a reserved base, `address_space_bits` down to the guest's real width so
//!    out-of-range wraps into the reservation) and paying the 30-49x, or reserving the entire
//!    address range around `GuestSpace` so that everything outside it is guard pages. Both are D4
//!    decisions to reopen with measurements, not edits to make in passing.
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
    optimization, od_code_cache_clear, od_code_cache_evict_to, od_code_cache_free, od_code_cache_invalidate_range, od_code_cache_new,
    od_code_cache_guest_pcs_of, od_code_cache_stats_of, od_code_cache_tables_of, od_jit_clear_halt,
    od_jit_effective_config,
    od_jit_free, od_jit_get_pc, od_jit_get_pstate, od_jit_get_reg, od_jit_get_sp, od_jit_get_vec,
    od_jit_halt, od_jit_invalidate_range, od_jit_new, od_jit_new_shared, od_jit_reset_stats, od_jit_run,
    od_jit_set_pc, od_jit_set_pstate, od_jit_set_reg, od_jit_set_sp, od_jit_set_vec,
    od_jit_slow_path_total, od_jit_stats, od_monitor_free, od_monitor_layout_of, od_monitor_new,
    OdCodeCacheStats, OdCodeCacheTables, OdConfig, OdEffectiveConfig, OdMonitorLayout, OdStats,
    OD_DYNARMIC_ABI_VERSION, OD_HALT_CACHE_INVALIDATION, OD_HALT_MEMORY_ABORT,
    OD_HALT_SHIM_REENTERED, OD_HALT_SHIM_THREW, OD_HALT_USER1, OD_HALT_USER8,
    OD_FIXED_PER_JIT_BYTES,
};
use omni_mem::{DemandPager, FaultAccess, GuestAddr, GuestSpace, PagerStats, Protection};

use crate::context::{ContextCost, GuestAddressSpace, GuestRange, GuestThreadConfig};
use crate::cpu::{
    Capabilities, GuestCpu, GuestCpuBackend, HaltHandle, InlineThunkCounts, JitCounters,
};
use crate::error::{CpuError, CpuResult};
use crate::exit::{AccessKind, ExitReason, RunLimit};
use crate::fastmem::{require_memory_path, MemoryMapping};
use crate::regs::{Nzcv, VReg, XReg};
use crate::run::Budget;
use crate::thunk::{ThunkContext, ThunkFn, ThunkRegs};
use crate::tls::{GuestTls, TlsArena};

mod callbacks;
mod inline_table;

pub use callbacks::HINTS_OBSERVED;

pub use callbacks::BACKEND_NAME;
pub use callbacks::Hle;

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


/// How guest exclusive loads and stores (`LDXR`/`STXR`, `LDAXP`/`STLXP` and the rest) are made
/// atomic across guest threads.
///
/// Both arms perform every exclusive store as **one host compare-and-swap** (`lock cmpxchg`, and
/// `cmpxchg16b` for a pair, on x64; an `LDAXR`/`STLXR` loop, `LDAXP`/`STLXP` for a pair, on arm64)
/// against the value the thread's own exclusive load read, so both are value-compare at the
/// memory word. On arm64 `ValueCompare` has meant what it says only since vendored patch 0021: the
/// backend's inline path ignored dynarmic's flag before, and took the lock and scanned regardless
/// (D31 amendment 1). They differ in what surrounds it -- `docs/DECISIONS.md` (D31) has the argument and
/// `tests/exclusive.rs` the lost-update stress test and the one behaviour that differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusiveMonitor {
    /// dynarmic's global monitor: every exclusive load and store takes one **process-wide** spin
    /// lock, and every exclusive store also clears every other processor's reservation of the same
    /// address -- a scan emitted **inline and unrolled over every slot the monitor was sized for**
    /// (`EmitExclusiveTestAndClear`), so it costs `max_threads` compares whether or not those
    /// threads exist.
    Global,
    /// No global lock and no scan (dynarmic's `Unsafe_IgnoreGlobalMonitor`): each processor keeps
    /// its own reservation, and an exclusive store succeeds iff the reserved address matches and
    /// the word still holds the reserved value. The one observable difference from `Global` is
    /// ABA across another thread's **exclusive** store, which `Global` fails and this succeeds.
    ValueCompare,
}

/// Slots between two processors' monitor entries under [`ExclusiveMonitor::ValueCompare`].
///
/// dynarmic keeps the reservations in two dense arrays (8-byte addresses, 16-byte values), so
/// adjacent processors share cache lines, and every exclusive load by one writes a line that the
/// others' exclusive loads also write. Eight slots apart puts each processor on lines of its own.
/// Free under `ValueCompare`, which emits no scan; under `Global` the scan is unrolled over every
/// slot, so there the stride stays 1.
const VALUE_COMPARE_SLOT_STRIDE: u32 = 8;

/// The address space a shared code cache reserves by default: 1 GiB, committed as code is emitted
/// (D38). Only [`SHARED_CODE_LIVE_BYTES`] of it holds live code; the rest is room for regions to
/// move into while a retired one waits to be given back.
pub const SHARED_CODE_CACHE_BYTES: u64 = 1 << 30;
/// A shared cache's region: the unit it fills, and retires oldest first (vendored patch 0028, D38
/// amendment 3). Small, so that a retirement forgets little and is quick.
pub const SHARED_CODE_REGION_BYTES: u64 = 16 << 20;
/// The code a shared cache keeps live by default: past it, the oldest region is retired and its
/// blocks translated again if they are still run. A game world emits ~245 MiB in its first minutes
/// (w27-w30). D38 amendment 4, measured: at 128 MiB (amendment 3) PS99's settled world kept
/// evicting all session (w35, 30 min: 31 PERF lines with evictions, 12-50k blocks translated again
/// per line, and ~60 s at 0-18 fps around +1310-1370 s with no window event), against w32 at the
/// old whole-cache capacity: no eviction, 3 windows under 20 fps. 256 MiB holds that working set
/// and keeps amendment 3's point -- a full region is never a flush. A memory-first instance (the
/// owner's 30-35-instance case) can set `OMNI_JIT_SHARED_CACHE_LIVE_MB=128` and accept the churn.
pub const SHARED_CODE_LIVE_BYTES: u64 = 256 << 20;
/// The smallest shared cache `OMNI_JIT_SHARED_CACHE_MB` may ask for: two 8 MiB regions and the
/// prelude.
pub const SHARED_CODE_CACHE_MIN_BYTES: u64 = 64 << 20;
/// The largest: the x64 backend reaches its prelude from every block with a 32-bit displacement.
pub const SHARED_CODE_CACHE_MAX_BYTES: u64 = 2 << 30;

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
    /// Whether to run under `optimization::INTERRUPTIBLE`: `ALL_SAFE` without the optimization
    /// flags whose terminal handlers check neither the cycle counter nor the halt flag.
    ///
    /// **Default `true`, and that is a deliberate trade.** Task 2 measured a guest `BR X30`
    /// branching to itself to be stoppable by *nothing* under upstream's default flags — not a
    /// budget, not a halt — and `INTERRUPTIBLE` fixed it by clearing `ReturnStackBuffer` and
    /// `FastDispatch` (D16), at about **3.9 ns per indirect transfer**. Vendored patches since gave
    /// those handlers both checks -- 0018 the return-stack buffer's on x64 (D33), 0019 the
    /// fast-dispatch handler's and 0020 the return-stack buffer's on arm64 (D35) -- so on x64
    /// `INTERRUPTIBLE` is now `ALL_SAFE` itself, and on arm64 `ALL_SAFE` less `FastDispatch`, which
    /// that backend does not implement. The flag is kept because it is what this backend
    /// *promises* (`capabilities().asynchronous_halt`, the in-loop thunk path), and what it selects
    /// is `dynarmic-sys`'s to say per architecture.
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
    /// It was not free, though: upstream skips `GetSetElimination` entirely when this is set (the
    /// cost is in the Task 3 report). Patch 0037 runs a precise form of the pass instead, while
    /// [`set_precise_get_set`] is on (the default on x64).
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
    /// How exclusive loads and stores are made atomic. See [`ExclusiveMonitor`].
    pub exclusive_monitor: ExclusiveMonitor,
    /// **A measurement switch, `None` in every shipped configuration**: the raw dynarmic
    /// optimization mask (safe bits only) to use instead of the one
    /// [`interruptible`](Self::interruptible) selects. Set from `OMNI_JIT_OPTIMIZATIONS` by
    /// [`with_environment`](Self::with_environment), which says so.
    pub optimizations_override: Option<u32>,
    /// **One translation cache for every guest thread of this address space** (vendored patch
    /// 0022, `docs/research/shared-jit-cache.md`, D38) instead of one per thread, so a block one
    /// thread translated is run by all. **Default `true` on x64 hosts** (D38 amendment 2) and
    /// `false` elsewhere; `OMNI_JIT_SHARED_CACHE=0|1` overrides it.
    ///
    /// x64 hosts only: on arm64 the backend has no shared cache, and asking for one gives
    /// per-thread caches, said so on stderr. With it on, [`code_cache_size`](Self::code_cache_size)
    /// no longer applies (a context has no cache of its own), breakpoints are refused (a breakpoint
    /// for one thread cannot be planted in code every thread runs), and the addresses this backend
    /// plants -- thunks, inline thunks, the return sentinel -- are a property of the space: what a
    /// context reaches at one is still decided by that context's own registrations.
    pub shared_code_cache: bool,
    /// Bytes of address space the shared cache reserves, committed as code is emitted.
    /// [`SHARED_CODE_CACHE_BYTES`] by default; `OMNI_JIT_SHARED_CACHE_MB` sets it.
    pub shared_code_cache_bytes: u64,
    /// The shared cache's region size (at least 8 MiB, at least two regions in the cache).
    /// [`SHARED_CODE_REGION_BYTES`] by default; `OMNI_JIT_SHARED_CACHE_REGION_MB` sets it.
    pub shared_code_region_bytes: u64,
    /// The code the shared cache keeps live, rounded down to whole regions (at least one; at most
    /// all but one). [`SHARED_CODE_LIVE_BYTES`] by default; `OMNI_JIT_SHARED_CACHE_LIVE_MB` sets
    /// it. Committed code stays within this and a region (the one retired, until it is given back).
    pub shared_code_live_bytes: u64,
    /// Top Byte Ignore, as arm64 Linux enables it for user space: a data access through an
    /// address with bits 56-63 set reaches the address with them cleared. Off by default, so the
    /// Roblox path keeps D4's full 64-bit identity mapping; the Linux personality (`omni-linux`)
    /// turns it on, because Android's scudo tags every heap pointer with 0x02 in the top byte.
    ///
    /// On, the direct path covers 56 address bits and masks the tag off (`shl`/`shr` before the
    /// access, the cost of D4's rule for below-64-bit configurations) and the callback path
    /// clears it before resolving.
    pub top_byte_ignore: bool,
    /// With [`top_byte_ignore`](Self::top_byte_ignore): whether the **direct** path masks the tag
    /// (56 bits, mirrored: `mov`/`shl`/`shr` before every guest access). **Default `true`**.
    ///
    /// `false` (x64 hosts only) is the configuration's form of what [`set_tbi_unmasked`] does at run
    /// time, **without** patch 0041's learning: D4's 64-bit identity on the direct path -- measured
    /// 15-25% faster on load/store loops (`tests/bench.rs::the_cost_of_top_byte_ignore`) -- and a
    /// tagged address left to the host: on x86-64 any non-zero top byte makes it non-canonical, the
    /// access takes a general-protection fault (Windows: an access violation at "address"
    /// `u64::MAX`; Linux: `SIGSEGV` at 0), which the demand pager declines and dynarmic's handler
    /// turns, by the faulting instruction's address, into the callback path -- where the tag is
    /// cleared and the access served, every time. Correct, and **~2.4 us each** (MEASURED,
    /// `bench.rs`), and Android 15's scudo reaches every chunk header through a `0x02`-tagged
    /// pointer (`omni-linux/tests/tbi_off.rs`), so `omni-linux` uses the live switch instead, whose
    /// sites learn the mask after their first tagged access. Each such access is counted ([`tagged_accesses`]) and exempt
    /// from the per-slice degraded-memory invariant. Ignored (treated as `true`) on other hosts: an
    /// arm64 host would apply its own TBI to a tagged address, which in a low window (D41) is the
    /// wrong memory.
    pub tbi_direct_mask: bool,
    /// Whether a guest access the demand pager declines -- a fault the guest meant, such as ART's
    /// implicit null check, which loads through a null object and turns the `SIGSEGV` into a
    /// `NullPointerException` -- also moves that instruction onto the callback path **for good**
    /// (dynarmic's `recompile_on_fastmem_failure`). Either way that one access reaches the callback
    /// and becomes a typed exit.
    ///
    /// **Default `true`**, as the Roblox path always ran. The Linux personality turns it off: its
    /// guests fault on purpose, and each such instruction's next, valid execution would take the
    /// callback path -- which the per-slice invariant ([`CpuError::DegradedMemoryPath`]) rightly
    /// kills, and which on arm64 (MEASURED, D41: `b_hello_dex`, 4 of 6 runs killed at `exit_group`
    /// after one null check) nothing else prevents. x64's shared code cache never recompiles, so
    /// this only changes what arm64 hosts do.
    pub recompile_on_declined_fault: bool,
    /// **Entries in each guest thread's fast-dispatch table** (patch 0035, x64 with the shared
    /// cache): a power of two from 0x40 to 0x10000, 16 bytes each; 0 is the pin's 0x1000 (64 KiB a
    /// thread). A smaller table costs a process of many mostly idle threads less (the system's host
    /// process runs ~900) and misses more often, a miss costing a lookup in the cache's block map.
    pub fast_dispatch_entries: u32,
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
            // D31, decided 2026-09-24: a game world's busy workers spent 3-8.5% of their samples
            // at the global monitor (one process-wide lock plus a 256-slot inline scan per store);
            // value-compare costs a tenth per atomic and scales. `OMNI_JIT_EXCLUSIVE_MONITOR=global`
            // is the way back, announced.
            exclusive_monitor: ExclusiveMonitor::ValueCompare,
            top_byte_ignore: false,
            tbi_direct_mask: true,
            recompile_on_declined_fault: true,
            optimizations_override: None,
            // D38 amendment 2, decided 2026-09-25 on x64: in PS99 with w20's drag script, w27/w29
            // (shared) against w28 (per-thread) -- 0 s under 20 fps during input against 14 s
            // (minimum 38-42 against 2 fps), translation peaking at 16 against 326 kinsn/s,
            // 3.2 against 4.1 GiB private, settled fps unchanged (48-52 against 49), no region
            // retired in either run. arm64 has no shared cache, so it stays per-thread there.
            // `OMNI_JIT_SHARED_CACHE=0` is the way back, announced.
            shared_code_cache: cfg!(target_arch = "x86_64"),
            shared_code_cache_bytes: SHARED_CODE_CACHE_BYTES,
            shared_code_region_bytes: SHARED_CODE_REGION_BYTES,
            shared_code_live_bytes: SHARED_CODE_LIVE_BYTES,
            fast_dispatch_entries: 0,
        }
    }
}

/// **The unsafe floating-point flags every backend of this process emits with from now on**
/// (patch 0034, x64): `optimization::UNSAFE_FP`'s bits, others dropped; returns the mask in force
/// (0 on arm64, where it does nothing). Blocks already translated keep what they were translated
/// with -- [`DynarmicBackend::clear_code_cache`] has them translated again. Every backend here opens
/// dynarmic's unsafe gate (for [`ExclusiveMonitor::ValueCompare`]), so the flags apply wherever
/// that is the monitor, which it is by default.
pub fn set_live_fp_optimizations(mask: u32) -> u32 {
    // SAFETY: stores one process-wide atomic; no pointer crosses.
    unsafe { dynarmic_sys::od_set_live_fp_optimizations(mask) }
}

/// **Whether `GetSetElimination` runs, in its precise form, where
/// [`check_halt_on_memory_access`](DynarmicOptions::check_halt_on_memory_access) is set** (patch
/// 0037, x64): process-wide, for every block translated from now on; returns what is in force
/// (`false` on arm64, where it does nothing). Blocks already translated keep what they were
/// translated with -- [`DynarmicBackend::clear_code_cache`] has them translated again.
///
/// Upstream skips the pass under the memory-abort check, so every guest register read is a load
/// from `JitState` and every write a store, because the pass erases a write that a later write to
/// the same register overwrites and a fault between the two would then see the older value. The
/// precise form keeps every write that comes before a guest data access (or anything else that can
/// leave the block or call out) and still forwards known values to later reads, so a fault stops
/// with exactly the state it stopped with before (`tests/precise_getset.rs`).
///
/// **On by default**; `OMNI_JIT_PRECISE_GETSET=0` (announced by
/// [`DynarmicOptions::with_environment`]) or `omni-linux`'s `jit_getset=0` lever turns it off.
pub fn set_precise_get_set(on: bool) -> bool {
    // SAFETY: stores one process-wide atomic; no pointer crosses.
    unsafe { dynarmic_sys::od_set_precise_get_set(u32::from(on)) != 0 }
}

/// **Whether Top Byte Ignore's mask on the direct path is one `and`** (patch 0040, x64) against a
/// pool constant rather than dynarmic's `shl`/`shr` pair: the same address, one cycle less on its
/// path. Process-wide, for blocks emitted from now on; returns what is in force (`false` on arm64).
/// Off by default; `OMNI_JIT_TBI_AND=1` (announced by [`DynarmicOptions::with_environment`]) or
/// `omni-linux`'s `jit_tbiand=1` lever turns it on.
pub fn set_fastmem_mask_by_and(on: bool) -> bool {
    // SAFETY: stores one process-wide atomic; no pointer crosses.
    unsafe { dynarmic_sys::od_set_fastmem_mask_by_and(u32::from(on)) != 0 }
}

/// **Top Byte Ignore's mask off the direct path, switched while running** (patch 0040, x64): the
/// live form of [`DynarmicOptions::tbi_direct_mask`] `false`, for blocks emitted from now on (clear
/// the cache to have every block again): a context configured with the mask emits its accesses
/// unmasked, and a tagged one faults to the slow path, which clears the tag and counts it
/// ([`tagged_accesses`]). **Patch 0041**: that instruction is then noted (by guest location,
/// process-wide; [`tbi_sites_noted`]), the run leaves at its next halt check, its translations
/// are dropped, and it is emitted masked from then on -- so a process pays one fault per
/// instruction that ever meets a tag (MEASURED: 279 per `toybox` process, the same for a 7x
/// larger listing), not one per access, and every other access runs at D4's identity. Returns
/// what is in force (`false` on arm64). Off by default; `omni-linux`'s `jit_tbi=0` lever (or
/// `OMNI_JIT_TBI=0` from the start) turns it on (`jit_tbi=1` back).
pub fn set_tbi_unmasked(on: bool) -> bool {
    // SAFETY: stores one process-wide atomic; no pointer crosses.
    unsafe { dynarmic_sys::od_set_tbi_unmasked(u32::from(on)) != 0 }
}

/// **The return-stack buffer's and fast-dispatch table's hit paths inside each block** (patch
/// 0042, x64): a `RET`/`BR`/`BLR` block checks the buffer or probes the thread's table itself, from
/// the target PC still in a register and with an indirect jump of its own (the host predicts it
/// per site), instead of jumping to one shared handler that reloads the PC from `JitState`. Same
/// lookups, same budget and halt checks, a miss continues in the shared handler. Process-wide, for
/// blocks emitted from now on; returns what is in force (`false` on arm64). MEASURED
/// (`dynarmic-sys/tests/codegen_bench.rs`, shared cache): 8 calls + 8 returns 35.6 -> 28.5 ns, a
/// threaded interpreter's dispatch 13.2 -> 12.8 ns/op (random opcodes) and 3.20 -> 2.91 (cyclic).
/// Off by default; `OMNI_JIT_FASTDISP=1` (announced by [`DynarmicOptions::with_environment`]) or
/// `omni-linux`'s `jit_fastdisp=1` lever turns it on.
pub fn set_fast_dispatch_inline(on: bool) -> bool {
    // SAFETY: stores one process-wide atomic; no pointer crosses.
    unsafe { dynarmic_sys::od_set_fast_dispatch_inline(u32::from(on)) != 0 }
}

/// **The shared caches' maps shrink with what they hold** (patch 0066, x64): after an eviction
/// (code aging, the live limit) or an invalidation forgets blocks, the block map, the link heads
/// and the guest-range page index are rehashed down when they hold at most half of what their
/// bucket arrays could. A robin_map never shrinks by itself, so a cache aged from its busiest
/// moment kept that moment's arrays (a world's game host: a 28 MiB block map at 2^20 buckets for
/// 636k blocks). The same lookups either way; a rehash costs ~20-40 ns an entry, under the cache's
/// lock. Process-wide; returns what is in force (`false` on arm64). Off by default;
/// `OMNI_JIT_TABLE_SHRINK=1` or `omni-linux`'s `jit_table_shrink=1` lever turns it on.
pub fn set_shrink_tables(on: bool) -> bool {
    // SAFETY: stores one process-wide atomic; no pointer crosses.
    unsafe { dynarmic_sys::od_set_shrink_tables(u32::from(on)) != 0 }
}

/// **Smaller translated code** (patch 0061, x64): process-wide bits, for blocks emitted from now
/// on; returns the bits in force (0 on arm64). The same behaviour either way.
///
/// - `1` (`dynarmic_sys::OD_COMPACT_FAULT_STUBS`): each fastmem site's out-of-line slow path calls
///   one shared memory-abort check (the guest PC as 8 bytes of data) instead of carrying ~38 bytes
///   of it, and keeps no call into its fallback where nothing but a host fault reaches it.
/// - `2` (`OD_COMPACT_LINK_TAILS`): a shared cache's link leaves a spent budget through its slot's
///   own tail instead of a second copy of it.
///
/// MEASURED (`dynarmic-sys/tests/code_size.rs`, shared cache, 0042 on, bionic `libc.so`'s first
/// blocks: 4,855 blocks, 16,417 memory accesses): 414.3 bytes a block off, **327.4 with `1`**
/// (-21%; out-of-line code 149.6 -> 61.9 a block, 44 -> 18 a memory access), 394.7 with `2`
/// (terminals 82.9 -> 63.1), 307.8 with both (-26%). Speed (`codegen_bench.rs::the_cost_of_
/// compact_code`, 15 interleaved rounds, E-cores): `1` within noise everywhere (-4.4% .. +1.2%
/// shared, -3.1% .. +0.3% per-thread: the hot path's bytes do not change); `2` **+17.5%** on a
/// loop of eight linked two-instruction blocks (shared cache; the same hot bytes, so layout), flat
/// elsewhere -- so `1` is the one to try in a world. Off by default; `OMNI_JIT_COMPACT=<bits>`
/// (announced by [`DynarmicOptions::with_environment`]) or `omni-linux`'s `jit_compact=<bits>`
/// lever turns it on.
pub fn set_compact_code(bits: u32) -> u32 {
    // SAFETY: stores one process-wide atomic; no pointer crosses.
    unsafe { dynarmic_sys::od_set_compact_code(bits) }
}

/// **Whether hot `libc.so` functions run a native host implementation** (patch: HLE), process-wide.
/// With it on, a guest call to a registered [`DynarmicBackend::add_hle`] entry (`memcpy`,
/// `memmove`, `memset`) runs host code -- args `X0`-`X2`, result `X0`, honouring guest faults --
/// instead of the guest's own. Read at translation (whether to plant the entry) and at the call.
/// Off by default; `omni-linux`'s `jit_hle=1` lever (or `OMNI_JIT_HLE=1` from the start) turns it
/// on and drops translations so planted entries appear. Always `true`/`false` as asked; the native
/// code is x64 and arm64 alike (it is host Rust, not emitted).
///
/// **MEASURED not to win on this host, kept for the in-world A/B** (`tests/bench.rs::
/// the_cost_of_native_memcpy`, called in a guest loop, ns per call): 64 B 20.9 guest vs 24.7
/// native (the SVC round trip and two range checks cost more than the tiny copy), 4 KiB 1133 vs
/// 1153 (equal), 1 MiB 337k vs 323k (4%). The guest's own `__memcpy_aarch64_simd` already
/// saturates memory bandwidth for large copies, so native code cannot beat it, and the
/// interception makes small copies slower. So this stays off unless an in-world A/B shows a gain
/// the micro-benchmark misses.
pub fn set_hle(on: bool) -> bool {
    callbacks::HLE_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
    on
}

/// What [`set_hle`] last set.
#[must_use]
pub fn hle_enabled() -> bool {
    callbacks::HLE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Native HLE calls served across the process, and of those the ones that met an inaccessible byte
/// (a guest fault): `(calls, faults)`. A measurement reads them.
#[must_use]
pub fn hle_stats() -> (u64, u64) {
    (
        callbacks::HLE_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        callbacks::HLE_FAULTS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// **Guest instructions that learned Top Byte Ignore's mask** (patch 0041): with the mask off
/// ([`set_tbi_unmasked`]), each instruction that meets a tagged address once is noted and from
/// then on emitted masked; this counts them, process-wide (0 on arm64).
#[must_use]
pub fn tbi_sites_noted() -> u64 {
    // SAFETY: loads one process-wide atomic.
    unsafe { dynarmic_sys::od_tbi_sites_noted() }
}

/// **Data accesses through a tagged address** (bits 56-63 non-zero) that the slow path served, by
/// every context in the process since it started. With [`DynarmicOptions::tbi_direct_mask`] off
/// that is every tagged access -- each one a host fault -- so this is what says whether running
/// without the mask is paying off.
#[must_use]
pub fn tagged_accesses() -> u64 {
    callbacks::TAGGED_ACCESSES.load(std::sync::atomic::Ordering::Relaxed)
}

/// **Whether a scalar floating-point operand stays in an XMM register** (patch 0039, x64):
/// process-wide, for every block emitted from now on; returns what is in force (`false` on arm64,
/// where it does nothing). Blocks already emitted keep what they were emitted with --
/// [`DynarmicBackend::clear_code_cache`] has them emitted again.
///
/// The A64 frontend reads every scalar FP operand as element 0 of the vector register, which
/// upstream copies to a general register, and the SSE instruction that uses it copies straight
/// back: two cross-domain moves per operand (and per single-precision result). The switch keeps
/// the element in an XMM register, zeroed above it exactly as the round trip left it, so every
/// value is bit-identical. MEASURED (`dynarmic-sys/tests/codegen_bench.rs`, E-cores, ns per 4
/// dependent ops): `FADD D` 22.8 -> 11.5, `FADD S` 32.6 -> 14.3, `FMADD D` 27.4 -> 17.7; four
/// independent `FADD D` 8.3 -> 3.8, `FCVTZS` 7.6 -> 4.2. `dynarmic-sys/tests/scalar_fp_xmm.rs`
/// compares 3,000 random scalar-FP/vector blocks with it off and on (registers, flags, `FPSR`).
///
/// **Off by default** (a code-generation change, A/B first); `OMNI_JIT_SCALAR_FP_XMM=1` (announced
/// by [`DynarmicOptions::with_environment`]) or `omni-linux`'s `jit_fpxmm=1` lever turns it on.
pub fn set_scalar_fp_in_xmm(on: bool) -> bool {
    // SAFETY: stores one process-wide atomic; no pointer crosses.
    unsafe { dynarmic_sys::od_set_scalar_fp_in_xmm(u32::from(on)) != 0 }
}

/// What [`set_scalar_fp_in_xmm`] last set (`false` on arm64).
#[must_use]
pub fn scalar_fp_in_xmm() -> bool {
    // SAFETY: loads one process-wide atomic.
    unsafe { dynarmic_sys::od_scalar_fp_in_xmm() != 0 }
}

/// What [`set_precise_get_set`] last set (`false` on arm64).
#[must_use]
pub fn precise_get_set() -> bool {
    // SAFETY: loads one process-wide atomic.
    unsafe { dynarmic_sys::od_precise_get_set() != 0 }
}

impl DynarmicOptions {
    /// Whether the direct path masks a tag off (56 bits, mirrored): [`top_byte_ignore`]
    /// (Self::top_byte_ignore) with [`tbi_direct_mask`](Self::tbi_direct_mask), which only an x64
    /// host may turn off.
    #[must_use]
    pub const fn tbi_masks_direct_path(&self) -> bool {
        self.top_byte_ignore && (self.tbi_direct_mask || !cfg!(target_arch = "x86_64"))
    }

    /// Bytes of one guest thread's fast-dispatch table under these options (patch 0035): the
    /// asked-for entries where the shared cache honours them, else the pin's 64 KiB.
    #[must_use]
    pub const fn fast_dispatch_table_bytes(&self) -> usize {
        let n = self.fast_dispatch_entries as usize;
        if self.shared_code_cache && n >= 0x40 && n <= 0x1_0000 && n.is_power_of_two() {
            n * 16
        } else {
            OD_FIXED_PER_JIT_BYTES
        }
    }

    /// The `OptimizationFlag` bitmask these options select.
    #[must_use]
    pub const fn optimizations(&self) -> u32 {
        let safe = match self.optimizations_override {
            Some(mask) => mask & optimization::ALL_SAFE,
            None if self.interruptible => optimization::INTERRUPTIBLE,
            None => optimization::ALL_SAFE,
        };
        match self.exclusive_monitor {
            ExclusiveMonitor::Global => safe,
            ExclusiveMonitor::ValueCompare => safe | optimization::UNSAFE_IGNORE_GLOBAL_MONITOR,
        }
    }

    /// Whether dynarmic's `unsafe_optimizations` gate has to be open: only for the one unsafe flag
    /// these options can select, [`ExclusiveMonitor::ValueCompare`]'s.
    #[must_use]
    pub const fn unsafe_optimizations(&self) -> bool {
        matches!(self.exclusive_monitor, ExclusiveMonitor::ValueCompare)
    }

    /// Monitor slots between two processors: see [`VALUE_COMPARE_SLOT_STRIDE`].
    #[must_use]
    pub const fn monitor_slot_stride(&self) -> u32 {
        match self.exclusive_monitor {
            ExclusiveMonitor::Global => 1,
            ExclusiveMonitor::ValueCompare => VALUE_COMPARE_SLOT_STRIDE,
        }
    }

    /// Apply the process's measurement switches. **Each says so on stderr when it is set**,
    /// because a run measured under one is a run about the switch (`docs/VERIFICATION.md`
    /// entry 15):
    ///
    /// * `OMNI_JIT_EXCLUSIVE_MONITOR=global|value` -- [`ExclusiveMonitor`];
    /// * `OMNI_JIT_OPTIMIZATIONS=<hex mask>` -- [`optimizations_override`](Self::optimizations_override);
    /// * `OMNI_JIT_CHECK_HALT_ON_MEMORY=0|1` -- [`check_halt_on_memory_access`](Self::check_halt_on_memory_access);
    /// * `OMNI_JIT_PRECISE_GETSET=0|1` -- [`set_precise_get_set`] (process-wide);
    /// * `OMNI_JIT_SCALAR_FP_XMM=0|1` -- [`set_scalar_fp_in_xmm`] (process-wide);
    /// * `OMNI_JIT_TBI_AND=0|1` -- [`set_fastmem_mask_by_and`] (process-wide);
    /// * `OMNI_JIT_FASTDISP=0|1` -- [`set_fast_dispatch_inline`] (process-wide);
    /// * `OMNI_JIT_TABLE_SHRINK=0|1` -- [`set_shrink_tables`] (process-wide);
    /// * `OMNI_JIT_RETRANSLATION=1` -- [`crate::stats::track_retranslation`];
    /// * `OMNI_JIT_CODE_CACHE_MB=<MiB>` -- [`code_cache_size`](Self::code_cache_size), per thread
    ///   where each thread has its own cache (arm64).
    ///
    /// Called by [`DynarmicBackend::new`], so every backend in the process sees the same switches.
    ///
    /// # Panics
    ///
    /// On a value it cannot read, naming the switch: a typo must not silently measure the default.
    #[must_use]
    pub fn with_environment(mut self) -> Self {
        fn say(text: &str) {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "JIT SWITCH: {text}");
        }
        if let Ok(value) = std::env::var("OMNI_JIT_CODE_CACHE_MB") {
            let mib: u64 = value.parse().unwrap_or_else(|_| panic!("OMNI_JIT_CODE_CACHE_MB={value:?} is not a number of MiB"));
            self.code_cache_size = mib << 20;
            say(&format!("code cache {mib} MiB per thread (OMNI_JIT_CODE_CACHE_MB)"));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_EXCLUSIVE_MONITOR") {
            self.exclusive_monitor = match value.trim() {
                "global" => ExclusiveMonitor::Global,
                "value" => ExclusiveMonitor::ValueCompare,
                other => panic!("OMNI_JIT_EXCLUSIVE_MONITOR={other:?} is not `global` or `value`"),
            };
            say(&format!(
                "exclusive monitor {:?} (OMNI_JIT_EXCLUSIVE_MONITOR)",
                self.exclusive_monitor
            ));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_OPTIMIZATIONS") {
            let text = value.trim().trim_start_matches("0x");
            let mask = u32::from_str_radix(text, 16)
                .unwrap_or_else(|_| panic!("OMNI_JIT_OPTIMIZATIONS={value:?} is not a hex mask"));
            let before = self.optimizations();
            self.optimizations_override = Some(mask);
            say(&format!(
                "optimization mask {:#010x} instead of {before:#010x} (OMNI_JIT_OPTIMIZATIONS). A \
                 measurement: with ReturnStackBuffer or FastDispatch set, an indirect-branch loop \
                 checks no budget (D16)",
                self.optimizations()
            ));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_CHECK_HALT_ON_MEMORY") {
            self.check_halt_on_memory_access = match value.trim() {
                "0" => false,
                "1" => true,
                other => panic!("OMNI_JIT_CHECK_HALT_ON_MEMORY={other:?} is not 0 or 1"),
            };
            say(&format!(
                "check_halt_on_memory_access = {} (OMNI_JIT_CHECK_HALT_ON_MEMORY). A measurement: \
                 off, a guest fault no longer stops at the faulting instruction",
                self.check_halt_on_memory_access
            ));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_PRECISE_GETSET") {
            let on = match value.trim() {
                "0" => false,
                "1" => true,
                other => panic!("OMNI_JIT_PRECISE_GETSET={other:?} is not 0 or 1"),
            };
            let kept = set_precise_get_set(on);
            say(&format!(
                "precise GetSetElimination {} (OMNI_JIT_PRECISE_GETSET){}",
                if kept { "on" } else { "off" },
                if on && !kept { ": not on this host's backend" } else { "" }
            ));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_SCALAR_FP_XMM") {
            let on = match value.trim() {
                "0" => false,
                "1" => true,
                other => panic!("OMNI_JIT_SCALAR_FP_XMM={other:?} is not 0 or 1"),
            };
            let kept = set_scalar_fp_in_xmm(on);
            say(&format!(
                "scalar FP operands in XMM registers {} (OMNI_JIT_SCALAR_FP_XMM){}",
                if kept { "on" } else { "off" },
                if on && !kept { ": not on this host's backend" } else { "" }
            ));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_COMPACT") {
            let bits = match value.trim() {
                v @ ("0" | "1" | "2" | "3") => v.parse::<u32>().expect("a digit"),
                other => panic!("OMNI_JIT_COMPACT={other:?} is not 0, 1, 2 or 3"),
            };
            let kept = set_compact_code(bits);
            say(&format!("compact translated code {kept} (OMNI_JIT_COMPACT; 1 fault stubs, 2 link tails)"));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_HLE") {
            let on = match value.trim() {
                "0" => false,
                "1" => true,
                other => panic!("OMNI_JIT_HLE={other:?} is not 0 or 1"),
            };
            set_hle(on);
            say(&format!("native libc fast paths {} (OMNI_JIT_HLE)", if on { "on" } else { "off" }));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_FASTDISP") {
            let on = match value.trim() {
                "0" => false,
                "1" => true,
                other => panic!("OMNI_JIT_FASTDISP={other:?} is not 0 or 1"),
            };
            let kept = set_fast_dispatch_inline(on);
            say(&format!("dispatch hit paths inline {} (OMNI_JIT_FASTDISP)", if kept { "on" } else { "off" }));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_TABLE_SHRINK") {
            let on = match value.trim() {
                "0" => false,
                "1" => true,
                other => panic!("OMNI_JIT_TABLE_SHRINK={other:?} is not 0 or 1"),
            };
            let kept = set_shrink_tables(on);
            say(&format!("code caches' maps shrink after forgetting blocks {} (OMNI_JIT_TABLE_SHRINK)", if kept { "on" } else { "off" }));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_TBI_AND") {
            let on = match value.trim() {
                "0" => false,
                "1" => true,
                other => panic!("OMNI_JIT_TBI_AND={other:?} is not 0 or 1"),
            };
            let kept = set_fastmem_mask_by_and(on);
            say(&format!("Top Byte Ignore's mask as one `and` {} (OMNI_JIT_TBI_AND)", if kept { "on" } else { "off" }));
        }
        if std::env::var_os("OMNI_JIT_RETRANSLATION").is_some() {
            crate::stats::track_retranslation(true);
            say("counting retranslated block starts per context (OMNI_JIT_RETRANSLATION)");
        }
        if let Ok(value) = std::env::var("OMNI_JIT_SHARED_CACHE") {
            self.shared_code_cache = match value.trim() {
                "0" => false,
                "1" => true,
                other => panic!("OMNI_JIT_SHARED_CACHE={other:?} is not 0 or 1"),
            };
            say(&format!(
                "shared code cache {} (OMNI_JIT_SHARED_CACHE): {}",
                if self.shared_code_cache { "on" } else { "off" },
                if self.shared_code_cache {
                    "every guest thread runs one set of translations (D38)"
                } else {
                    "a code cache per guest thread"
                }
            ));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_SHARED_CACHE_MB") {
            let mb: u64 = value
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("OMNI_JIT_SHARED_CACHE_MB={value:?} is not a number of MiB"));
            let bytes = mb << 20;
            assert!(
                (SHARED_CODE_CACHE_MIN_BYTES..=SHARED_CODE_CACHE_MAX_BYTES).contains(&bytes),
                "OMNI_JIT_SHARED_CACHE_MB={mb} is outside {}..={} MiB",
                SHARED_CODE_CACHE_MIN_BYTES >> 20,
                SHARED_CODE_CACHE_MAX_BYTES >> 20
            );
            self.shared_code_cache_bytes = bytes;
            say(&format!("a {mb} MiB shared code cache (OMNI_JIT_SHARED_CACHE_MB)"));
        }
        let mib = |name: &str, value: &str| -> u64 {
            value
                .trim()
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("{name}={value:?} is not a number of MiB"))
                << 20
        };
        if let Ok(value) = std::env::var("OMNI_JIT_SHARED_CACHE_REGION_MB") {
            let bytes = mib("OMNI_JIT_SHARED_CACHE_REGION_MB", &value);
            assert!(
                bytes >= 8 << 20 && bytes.saturating_mul(3) <= self.shared_code_cache_bytes,
                "OMNI_JIT_SHARED_CACHE_REGION_MB={value} must be at least 8 and leave the cache room for three"
            );
            self.shared_code_region_bytes = bytes;
            say(&format!("{} MiB shared code cache regions (OMNI_JIT_SHARED_CACHE_REGION_MB)", bytes >> 20));
        }
        if let Ok(value) = std::env::var("OMNI_JIT_SHARED_CACHE_LIVE_MB") {
            let bytes = mib("OMNI_JIT_SHARED_CACHE_LIVE_MB", &value);
            assert!(bytes > 0, "OMNI_JIT_SHARED_CACHE_LIVE_MB={value} must be more than 0");
            self.shared_code_live_bytes = bytes;
            say(&format!(
                "{} MiB of live code in the shared cache, oldest region retired past it (OMNI_JIT_SHARED_CACHE_LIVE_MB)",
                bytes >> 20
            ));
        }
        self
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

impl Overrides {
    /// Whether these override nothing, i.e. build the configuration every context gets.
    fn is_conforming(&self) -> bool {
        self.address_space_bits.is_none()
            && self.direct_access.is_none()
            && self.mirrors_out_of_range.is_none()
            && !self.null_thread_pointer
    }
}

/// The `OdConfig` a context of a backend with `options` passes: the one place it is written, so
/// that a shared code cache's template (D38) is the same configuration its contexts attach with.
fn thread_config(
    options: &DynarmicOptions,
    monitor: *mut c_void,
    ctx: *mut c_void,
    tpidr_el0: *mut u64,
    tpidrro_el0: *const u64,
    processor_id: u32,
    overrides: Overrides,
    low_window_delta: Option<u64>,
) -> OdConfig {
    OdConfig {
        abi_version: OD_DYNARMIC_ABI_VERSION,
        callbacks: &callbacks::CALLBACKS,
        ctx,
        // D13. The shim accepts a null pointer here — its header says the guest's read then
        // faults into `exception_raised` — which is precisely the configuration the startup
        // assertion's `TPIDR_EL0 storage` check exists to refuse, so it has to be reachable for
        // that check to be provable. Only `create_misconfigured_thread` can ask for it.
        tpidr_el0: if overrides.null_thread_pointer { core::ptr::null_mut() } else { tpidr_el0 },
        tpidrro_el0: if overrides.null_thread_pointer { core::ptr::null() } else { tpidrro_el0 },
        // D4, all four fields together. `fastmem_pointer = 0` with 64 bits is the identity
        // mapping; mirroring is off so a guest address with nothing behind it faults instead of
        // aliasing a valid page; recompiling on a fastmem failure is what routes a declined
        // fault to the slow path, where it becomes a typed exit. (With a shared code cache the
        // shim turns the recompiling off: a declined fault still reaches the slow path, on every
        // occurrence, and the block is not recompiled for every thread from a fault handler.)
        //
        // **"Faults" means "is unmapped in the HOST process", and that is narrower than it
        // reads.** See this module's documentation, under "What fastmem does not check".
        fastmem_enabled: i32::from(overrides.direct_access.unwrap_or(true)),
        // D41: a space with a low window (macOS: nothing maps below 4 GiB) has its guest's low
        // 4 GiB at `delta + address`; dynarmic adds the base below 2^32 only (patch 0030), and
        // above it this is still the identity.
        fastmem_pointer: low_window_delta.unwrap_or(0),
        // Top Byte Ignore (`DynarmicOptions::top_byte_ignore`): 56 bits, mirrored, is dynarmic's
        // mask of the top byte on every direct access -- aliasing across the top byte is exactly
        // what the architecture specifies, and an address with nothing behind it still faults.
        fastmem_address_space_bits: overrides.address_space_bits.unwrap_or(if options.tbi_masks_direct_path() { 56 } else { 64 }),
        silently_mirror_fastmem: i32::from(overrides.mirrors_out_of_range.unwrap_or(options.tbi_masks_direct_path())),
        recompile_on_fastmem_failure: i32::from(options.recompile_on_declined_fault),
        // Task 2's review measured this: off gives 1 slow-path read plus 1 exclusive callback
        // per `LDXR`, on gives 0, which matters because D5 lists the global exclusive monitor's
        // 21x anti-scaling as a primary risk.
        fastmem_exclusive_access: 1,
        monitor,
        // Spaced by the stride the monitor was sized with; see `VALUE_COMPARE_SLOT_STRIDE`.
        processor_id: processor_id * options.monitor_slot_stride(),
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
        // Open only for the one unsafe flag `DynarmicOptions` can select, and that flag is in
        // `optimizations` exactly when this is 1 -- dynarmic requires both.
        unsafe_optimizations: i32::from(options.unsafe_optimizations()),
        optimizations: options.optimizations(),
        fastmem_low_window: i32::from(low_window_delta.is_some()),
        fast_dispatch_entries: options.fast_dispatch_entries,
    }
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

impl Monitor {
    /// Where the monitor keeps its lock and slots.
    fn layout(&self) -> OdMonitorLayout {
        let mut out = OdMonitorLayout::default();
        // SAFETY: `self.0` came from `od_monitor_new` and is freed only in `drop`; `out` is
        // writable.
        unsafe { od_monitor_layout_of(self.0, &mut out) };
        out
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        crate::stats::unregister_monitor(self.layout().lock as usize);
        // SAFETY: the handle came from `od_monitor_new` and every jit using it has been freed —
        // `DynarmicBackend` hands out contexts that hold an `Arc` of the shared state, so the
        // monitor outlives them all.
        unsafe { od_monitor_free(self.0) };
    }
}

/// The shared code cache (vendored patch 0022), freed after every jit on it -- it is a field of
/// [`Shared`], which every context holds an `Arc` of -- and before the monitor its code names.
struct CodeCache(*mut c_void);

// SAFETY: the cache is built to be used by the jits of several threads at once and does its own
// locking; the handle is passed to `od_jit_new_shared`, to the invalidation and statistics calls,
// and freed once, in `Drop`, after every jit attached to it.
unsafe impl Send for CodeCache {}
// SAFETY: as above.
unsafe impl Sync for CodeCache {}

impl Drop for CodeCache {
    fn drop(&mut self) {
        // SAFETY: from `od_code_cache_new`; every jit on it is freed (see the type's docs).
        unsafe { od_code_cache_free(self.0) };
    }
}

/// With a shared code cache, the addresses this backend plants into the instruction stream --
/// thunks, inline thunks, the return sentinel (`read_code`'s `STOP_SVC`) -- are translated once
/// for every context, so whether an address is planted is a property of the space: a count of the
/// contexts that planted it. What a context reaches at a planted address is still decided by its
/// own registrations (`cb_call_svc`).
#[derive(Default)]
pub(crate) struct Planted {
    counts: parking_lot::RwLock<std::collections::HashMap<GuestAddr, u32>>,
}

impl Planted {
    pub(crate) fn contains(&self, address: GuestAddr) -> bool {
        self.counts.read().contains_key(&address)
    }

    /// Count one more context in; `true` if the address was not planted before.
    fn add(&self, address: GuestAddr) -> bool {
        let mut counts = self.counts.write();
        let n = counts.entry(address).or_insert(0);
        *n += 1;
        *n == 1
    }

    /// Count one context out; `true` if no context plants the address any more.
    fn remove(&self, address: GuestAddr) -> bool {
        let mut counts = self.counts.write();
        match counts.get_mut(&address) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                counts.remove(&address);
                true
            }
            None => false,
        }
    }
}

/// Guest function entries a space serves with a native host implementation (patch: HLE), shared
/// by its contexts so a thread started after registration sees them. The address is the whole key
/// (`cb_read_code` plants it, `cb_call_svc` dispatches it); the value is which function.
#[derive(Default)]
pub(crate) struct HleSites {
    map: parking_lot::RwLock<BTreeMap<GuestAddr, callbacks::Hle>>,
}

impl HleSites {
    pub(crate) fn get(&self, address: GuestAddr) -> Option<callbacks::Hle> {
        self.map.read().get(&address).copied()
    }
    pub(crate) fn contains(&self, address: GuestAddr) -> bool {
        self.map.read().contains_key(&address)
    }
    fn insert(&self, address: GuestAddr, kind: callbacks::Hle) -> bool {
        self.map.write().insert(address, kind).is_none()
    }
    fn addresses(&self) -> Vec<GuestAddr> {
        self.map.read().keys().copied().collect()
    }
}

/// The jits of one guest address space's contexts, as addresses. A context is in it from the
/// moment its jit exists until just before the jit is freed, and every use holds the lock, so an
/// address read from it is a live jit for as long as the lock is held.
#[derive(Default)]
pub(crate) struct Peers(pub(crate) parking_lot::Mutex<Vec<usize>>);

/// What every context of one guest address space shares.
struct Shared {
    space: Arc<GuestSpace>,
    extent: GuestAddressSpace,
    /// The shared code cache, when [`DynarmicOptions::shared_code_cache`] asked for one and the
    /// host has it. Declared before `monitor`, so it is freed first.
    code_cache: Option<CodeCache>,
    /// The planted addresses, counted per space; used only with `code_cache`.
    planted: Arc<Planted>,
    /// Guest `libc.so` entries this space serves natively (patch: HLE), shared by its contexts.
    hle: Arc<HleSites>,
    monitor: Monitor,
    tls: TlsArena,
    options: DynarmicOptions,
    /// Installed once per backend. `None` on a target with no vectored-handler implementation, or
    /// when the caller installed one of its own; the difference is reported by
    /// [`DynarmicBackend::owns_guest_paging`].
    _pager: Option<DemandPager>,
    owns_guest_paging: bool,
    /// The jits of this space's contexts, when each has its own translation cache (no shared cache:
    /// arm64). A guest's `IC IVAU` is broadcast to all of them, as the architecture broadcasts it
    /// to every core of the inner-shareable domain; with a shared cache one invalidation of it is
    /// every context's already. See `callbacks::cb_icache_op`.
    peers: Arc<Peers>,
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
        // The process's measurement switches, each announced where it is set. Applied here rather
        // than by the embedding so that every backend -- a gate's, a test's -- honours the same
        // ones and none can forget to.
        let options = options.with_environment();
        let extent = GuestAddressSpace::of(&space)?;
        let tls = TlsArena::new(&space, options.max_threads.max(1) as usize)?;

        // One slot per processor under the global monitor; `monitor_slot_stride` apart under
        // value-compare, where the scan the slot count would cost is not emitted at all.
        let slots = u64::from(options.max_threads.max(1)) * u64::from(options.monitor_slot_stride());
        // SAFETY: freed exactly once, in `Monitor::drop`, after every jit that references it.
        let raw = unsafe { od_monitor_new(slots) };
        if raw.is_null() {
            return Err(CpuError::Backend {
                backend: BACKEND_NAME,
                operation: "allocate the shared exclusive monitor",
                detail: format!("od_monitor_new({slots}) returned null"),
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

        let monitor = Monitor(raw);

        // D38: one code cache for the space, when asked for. Its template is the configuration
        // every context of this backend passes (`thread_config`), with this backend's monitor.
        let code_cache = if options.shared_code_cache {
            let mut tpidr = 0u64;
            let tpidrro = 0u64;
            let template = thread_config(
                &options,
                raw,
                core::ptr::null_mut(),
                &mut tpidr,
                &tpidrro,
                0,
                Overrides::default(),
                extent.low_window_delta(),
            );
            // D38 amendment 3 (vendored patch 0028): regions are filled one at a time and a full
            // one stays live; past `shared_code_live_bytes` the oldest is retired, and only its
            // blocks are translated again. (Before it, a full region forgot every block, so regions
            // were a quarter of the cache, to hold the whole working set -- amendment 1.)
            let region = options.shared_code_region_bytes.min(options.shared_code_cache_bytes / 3).max(8 << 20);
            // SAFETY: `template` is a valid config; its pointers are not kept past the call. The
            // cache is freed in `CodeCache::drop`, after every jit on it.
            let cache = unsafe {
                od_code_cache_new(&template, options.shared_code_cache_bytes, region, options.shared_code_live_bytes)
            };
            if !cache.is_null() {
                let (mut address, mut value) = (0u32, 0u32);
                // SAFETY: two writable `u32`s.
                unsafe { dynarmic_sys::od_shared_monitor_slot_offsets(&mut address, &mut value) };
                crate::stats::register_jit_state_monitor_offsets(address, value);
            }
            if cache.is_null() {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "JIT SWITCH: shared code cache not available here (the arm64 backend has none, \
                     or {} bytes were refused): a code cache per guest thread",
                    options.shared_code_cache_bytes
                );
                None
            } else {
                Some(CodeCache(cache))
            }
        } else {
            None
        };

        let layout = monitor.layout();
        crate::stats::register_monitor(crate::stats::MonitorLayout {
            lock: layout.lock as usize,
            addresses: layout.addresses as usize,
            address_stride: layout.address_stride as usize,
            values: layout.values as usize,
            value_stride: layout.value_stride as usize,
            slots: layout.processor_count as usize,
            global: options.exclusive_monitor == ExclusiveMonitor::Global,
        });

        let has_code_cache = code_cache.is_some();
        let backend = Self {
            shared: Arc::new(Shared {
                space,
                extent,
                code_cache,
                planted: Arc::new(Planted::default()),
                hle: Arc::new(HleSites::default()),
                monitor,
                tls,
                options,
                _pager: pager,
                owns_guest_paging,
                peers: Arc::new(Peers::default()),
                free_processors: parking_lot::Mutex::new(Vec::new()),
                next_processor: AtomicU32::new(0),
                ids_released_early: AtomicU64::new(0),
            }),
        };
        if has_code_cache {
            // D38: what `OMNI_PERF` prints about the cache, for as long as the backend lives.
            let shared = Arc::downgrade(&backend.shared);
            crate::stats::register_code_cache(Box::new(move |with_tables| {
                let shared = shared.upgrade()?;
                let cache = shared.code_cache.as_ref()?;
                let mut s = OdCodeCacheStats::default();
                // SAFETY: the cache lives as long as `shared`, held here; `s` is writable.
                unsafe { od_code_cache_stats_of(cache.0, &mut s) };
                let mut tables = [crate::stats::CodeCacheTable::default(); 5];
                if with_tables {
                    let mut t = OdCodeCacheTables::default();
                    // SAFETY: as above; `t` is writable.
                    unsafe { od_code_cache_tables_of(cache.0, &mut t) };
                    for (out, (name, one)) in tables.iter_mut().zip(t.named()) {
                        *out = crate::stats::CodeCacheTable {
                            name,
                            entries: one.entries,
                            bytes: one.bytes,
                            largest_address: one.largest_address as usize,
                            largest_bytes: one.largest_bytes,
                        };
                    }
                }
                Some(crate::stats::CodeCacheCounters {
                    caches: 1,
                    blocks_emitted: s.blocks_emitted,
                    code_bytes_emitted: s.code_bytes_emitted,
                    translate_ns: s.translate_ns,
                    emit_ns: s.emit_ns,
                    invalidations: s.invalidations,
                    blocks_invalidated: s.blocks_invalidated,
                    regions_retired: s.regions_retired,
                    regions_evicted: s.regions_evicted,
                    blocks_evicted: s.blocks_evicted,
                    blocks_reemitted: s.blocks_reemitted,
                    evict_max_ns: s.evict_max_ns,
                    regions_live: s.regions_live,
                    regions_reclaimed: s.regions_reclaimed,
                    regions_pinned: s.regions_pinned,
                    parked_redirected: s.parked_redirected,
                    locked_lookups: s.locked_lookups,
                    committed_bytes: s.committed_bytes,
                    tables,
                })
            }));
            // Patch 0036: where in the guest a sampled host address in this cache is
            // (`omni-linux`'s `OMNI_GUEST_PROF`).
            let shared = Arc::downgrade(&backend.shared);
            crate::stats::register_guest_pc_resolver(Box::new(move |hosts, out| {
                let Some(shared) = shared.upgrade() else { return false };
                let Some(cache) = shared.code_cache.as_ref() else { return false };
                debug_assert_eq!(hosts.len(), out.len());
                let n = hosts.len().min(out.len());
                // SAFETY: the cache lives as long as `shared`, held here; `hosts` and `out` hold
                // `n` elements each, `hosts` ascending (the caller's contract).
                unsafe { od_code_cache_guest_pcs_of(cache.0, hosts.as_ptr(), n as u64, out.as_mut_ptr()) };
                true
            }));
        }
        Ok(backend)
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

    /// Whether this backend's contexts run from one shared code cache (D38).
    #[must_use]
    pub fn shares_translations(&self) -> bool {
        self.shared.code_cache.is_some()
    }

    /// Invalidate the translations of `range` that code written into the space **by the host** --
    /// a loader, a test harness -- has made stale.
    ///
    /// With a shared code cache (D38) a translation is the space's, not a context's: a context
    /// created after the write runs the space's translation of the old bytes until something
    /// invalidates it, as every context that ran them before does. So whoever writes code into the
    /// space outside a context invalidates it here. Without a shared cache this does nothing: a
    /// context's own cache is its own, and code the guest itself rewrites is invalidated through
    /// [`GuestCpu::invalidate_code`] on its context either way.
    pub fn invalidate_code(&self, range: GuestRange) {
        if let Some(cache) = &self.shared.code_cache {
            // SAFETY: the cache is live for `self.shared`'s life; this is not called from inside a
            // callback (the backend has none -- only its contexts do).
            unsafe { od_code_cache_invalidate_range(cache.0, range.start() as u64, range.len() as u64) };
        }
    }

    /// **Serve a guest `libc.so` function natively** (patch: HLE): a call to `address` runs
    /// [`Hle`](callbacks::Hle)'s host implementation instead of the guest code, while
    /// [`set_hle`] is on. Shared by every context of this space, so a thread started later has it
    /// too. `address` is resolved per process from the mapped `libc.so`'s symbol table (never a
    /// hardcoded address); the caller registers it when `libc.so` is mapped. The translation
    /// covering the entry is dropped so it is re-read as a planted site.
    pub fn add_hle(&self, address: GuestAddr, kind: callbacks::Hle) {
        if self.shared.hle.insert(address, kind) {
            self.invalidate_code_everywhere(GuestRange::new(address, 4).expect("a code range"));
        }
    }

    /// The guest `libc.so` entries this backend serves natively, for a caller that wants to drop
    /// their translations on a switch flip (the lever does, through [`DynarmicBackend::invalidate_code_everywhere`]).
    #[must_use]
    pub fn hle_addresses(&self) -> Vec<GuestAddr> {
        self.shared.hle.addresses()
    }

    /// Drop the translations of `range` on **every** context of this backend: the shared cache's,
    /// or each context's own (arm64). What a kernel owes the instruction cache when the guest
    /// changes code by other means than `IC IVAU` -- remapping it, or re-protecting it executable.
    /// Callable from inside a context's callback (a system call): `od_jit_invalidate_range` is
    /// queued for a jit that is executing.
    pub fn invalidate_code_everywhere(&self, range: GuestRange) {
        if self.shared.code_cache.is_some() {
            return self.invalidate_code(range);
        }
        for &jit in self.shared.peers.0.lock().iter() {
            // SAFETY: a peer is live while it is in the list (a context leaves it, under this
            // lock, before its jit is freed); the call is safe from any thread and callback.
            unsafe { od_jit_invalidate_range(jit as *mut core::ffi::c_void, range.start() as u64, range.len() as u64) };
        }
    }

    /// **Drop every translation of the shared code cache**, from a host thread (not a callback of
    /// one of its contexts): every region filled so far is retired and given back to the OS as
    /// soon as the threads that ran in it have left generated code, and what runs next is
    /// translated again. What a process that has gone idle keeps of the code it ran once. Nothing
    /// without a shared cache.
    pub fn clear_code_cache(&self) {
        if let Some(cache) = &self.shared.code_cache {
            // SAFETY: the cache is live for `self.shared`'s life; this is not called from inside a
            // callback (the backend has none -- only its contexts do).
            unsafe { od_code_cache_clear(cache.0) };
        }
    }

    /// Retire the shared cache's oldest full regions until at most `keep_bytes` of regions are
    /// live (vendored patch 0050; the region being filled always stays). Their blocks are
    /// translated again if they run again. How many regions were retired (0 without a cache).
    pub fn evict_code_to(&self, keep_bytes: u64) -> u64 {
        match &self.shared.code_cache {
            // SAFETY: as `clear_code_cache`.
            Some(cache) => unsafe { od_code_cache_evict_to(cache.0, keep_bytes) },
            None => 0,
        }
    }

    /// The shared cache's region size (0 without a cache): what `evict_code_to` retires at a time.
    #[must_use]
    pub fn code_region_bytes(&self) -> u64 {
        if self.shared.code_cache.is_some() {
            self.shared.options.shared_code_region_bytes.min(self.shared.options.shared_code_cache_bytes / 3).max(8 << 20)
        } else {
            0
        }
    }

    /// **Translation snapshots** (vendored patch 0070, x64): hash the guest code of every block
    /// the shared code cache emits from now on, so that [`save_translation_snapshot`] can write
    /// them. False without a shared cache.
    ///
    /// [`save_translation_snapshot`]: Self::save_translation_snapshot
    pub fn enable_translation_snapshots(&self) -> bool {
        let Some(cache) = self.shared.code_cache.as_ref() else { return false };
        // SAFETY: the cache is live for `self.shared`'s life.
        unsafe { dynarmic_sys::od_code_cache_enable_snapshots(cache.0) };
        true
    }

    /// Write the shared code cache's translations to `path`, tagged `key` (see
    /// `dynarmic_sys::od_code_cache_save_snapshot`): the blocks written, or a negative error.
    /// Takes the cache's lock: every guest thread waits while the regions are copied out.
    /// `entered_only` leaves out the blocks restored from a snapshot and not entered since (a
    /// snapshot of the working set of this run alone).
    pub fn save_translation_snapshot(&self, path: &std::path::Path, key: &str, max_bytes: u64, entered_only: bool) -> i64 {
        let Some(cache) = self.shared.code_cache.as_ref() else { return -1 };
        let (Some(path), Ok(key)) = (path.to_str().and_then(|p| std::ffi::CString::new(p).ok()), std::ffi::CString::new(key)) else { return -1 };
        // SAFETY: a live cache; NUL-terminated strings.
        unsafe {
            dynarmic_sys::od_code_cache_save_snapshot(cache.0, path.as_ptr(), key.as_ptr(), max_bytes, if entered_only { dynarmic_sys::OD_SNAPSHOT_ENTERED_ONLY } else { 0 })
        }
    }

    /// Install the translation snapshot at `path` (tagged `key`) into the shared code cache, which
    /// must not have translated anything yet: the blocks installed (each entered only once its guest
    /// code reads back unchanged), or a negative error and nothing changed.
    /// `lazily` (patch 0075): nothing of the code is read or committed at load; a page is read in
    /// from the file when a block on it is first entered, so restored code never entered costs no
    /// memory.
    pub fn load_translation_snapshot(&self, path: &std::path::Path, key: &str, lazily: bool) -> i64 {
        let Some(cache) = self.shared.code_cache.as_ref() else { return -1 };
        let (Some(path), Ok(key)) = (path.to_str().and_then(|p| std::ffi::CString::new(p).ok()), std::ffi::CString::new(key)) else { return -1 };
        // SAFETY: as `save_translation_snapshot`.
        unsafe { dynarmic_sys::od_code_cache_load_snapshot(cache.0, path.as_ptr(), key.as_ptr(), if lazily { dynarmic_sys::OD_SNAPSHOT_LOAD_LAZY } else { 0 }) }
    }

    /// Patch 0076: forget every block restored from a snapshot and not entered yet, and give
    /// their tables' memory back. How many were forgotten (0 without a shared cache).
    pub fn forget_unverified_translations(&self) -> i64 {
        let Some(cache) = self.shared.code_cache.as_ref() else { return 0 };
        // SAFETY: a live cache.
        unsafe { dynarmic_sys::od_code_cache_forget_unverified(cache.0) }
    }

    /// Patch 0076: the guest PCs of the restored blocks not entered yet (empty without a shared
    /// cache).
    #[must_use]
    pub fn unverified_translation_pcs(&self) -> Vec<u64> {
        let Some(cache) = self.shared.code_cache.as_ref() else { return Vec::new() };
        // SAFETY: a live cache; a null `out` with no capacity only counts.
        let n = unsafe { dynarmic_sys::od_code_cache_unverified_pcs(cache.0, std::ptr::null_mut(), 0) };
        let mut out = vec![0u64; n as usize + 1024];
        // SAFETY: `out` holds its length.
        let n = unsafe { dynarmic_sys::od_code_cache_unverified_pcs(cache.0, out.as_mut_ptr(), out.len() as u64) };
        out.truncate((n as usize).min(out.len()));
        out
    }

    /// The shared code cache's counters, or `None` without one.
    #[must_use]
    pub fn code_cache_stats(&self) -> Option<OdCodeCacheStats> {
        self.shared.code_cache.as_ref().map(|cache| {
            let mut out = OdCodeCacheStats::default();
            // SAFETY: the cache is live for `self.shared`'s life; `out` is writable.
            unsafe { od_code_cache_stats_of(cache.0, &mut out) };
            out
        })
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
        match require_memory_path(&cpu.memory_mapping(), self.shared.extent, self.shared.options.tbi_masks_direct_path()) {
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

    /// The inherent [`create_thread_with_tls`](DynarmicBackend::create_thread_with_tls), behind
    /// the trait — so that a runtime holding `&dyn GuestCpuBackend` can create a guest thread
    /// without knowing which backend it has, which is what `pthread_create` needs.
    ///
    /// The block comes from this backend's **one** arena, so every thread of this address space
    /// carries the same stack guard (D13).
    fn create_guest_thread(&self) -> CpuResult<Box<dyn GuestCpu>> {
        Ok(Box::new(self.create_thread_with_tls()?))
    }

    fn shared_cost(&self) -> ContextCost {
        let mut cost = self.shared.tls.cost();
        // D38: the shared code cache is what every context shares, as the arena is.
        if let Some(stats) = self.code_cache_stats() {
            cost.shared_committed = cost.shared_committed.saturating_add(stats.committed_bytes as usize);
        }
        cost
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
#[cfg(target_arch = "x86_64")]
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

/// The AArch64 host's floating-point control register, which is what plays `MXCSR`'s part on an
/// `aarch64` host, and the same guard over it.
///
/// # Why an arm64 host needs the same guard
///
/// dynarmic's arm64 prelude (`A64AddressSpace::EmitPrelude`, `a64_address_space.cpp`) saves the
/// host's `FPCR` into `StackLayout::save_host_fpcr` and writes the **guest's** `FPCR` into the host
/// register before it branches into translated code, and puts the host's back only in
/// `return_from_run_code`. Its call trampolines (`EmitCallTrampoline`, same file) -- the path
/// `CallSVC` takes, and therefore every inline thunk handler -- switch nothing. So a host callback
/// runs with the guest's rounding mode, flush-to-zero (`FZ`, bit 24), default-NaN (`DN`, bit 25) and
/// `FZ16` live in the real register, and Rust's `f32`/`f64` compile to the very instructions those
/// bits govern. Same defect class as `MXCSR` on x86-64, same fix, same single place.
///
/// Named `mxcsr` for the rest of this module (below) so the dispatcher and the thunk path are one
/// code path on both hosts; the value it carries is `FPCR` here, never an x86 word.
#[cfg(target_arch = "aarch64")]
pub(crate) mod fpcr {
    /// Read `FPCR`.
    #[must_use]
    pub(crate) fn read() -> u32 {
        let out: u64;
        // SAFETY: `FPCR` is readable at EL0 on every AArch64 implementation; `mrs` has no memory
        // operand and no side effect.
        unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) out, options(nomem, nostack, preserves_flags)) };
        // The architecturally defined bits are all in the low 32 (FEAT_AFP's `AH`/`FIZ`/`NEP` are
        // bits 0-2); the upper half is RES0.
        out as u32
    }

    /// Write `FPCR`.
    pub(crate) fn write(value: u32) {
        // SAFETY: as `read`. Every value written here was read out of `FPCR` in the first place, so
        // no RES0 bit is set.
        unsafe {
            core::arch::asm!("msr fpcr, {}", in(reg) u64::from(value), options(nomem, nostack, preserves_flags));
        }
    }

    /// Installs the host's `FPCR` for the body of a host callback and puts the guest's back.
    ///
    /// Nothing is switched when the two words are already equal, which is the common case.
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

/// On an `aarch64` host the control word the guard switches is `FPCR`; see [`fpcr`].
#[cfg(target_arch = "aarch64")]
pub(crate) use fpcr as mxcsr;

/// **A diagnostic, off by default**: start counting the guest instructions every context of
/// every backend fetches for translation, and return the count so far. Translation is the only
/// thing that fetches, so a count that keeps rising in a steady state is code being translated
/// again -- a code cache that is too small for what a thread runs.
pub fn count_code_fetches() -> u64 {
    callbacks::COUNTING_CODE_FETCHES.store(true, std::sync::atomic::Ordering::Relaxed);
    callbacks::CODE_FETCHES.load(std::sync::atomic::Ordering::Relaxed)
}

/// [`count_code_fetches`], by host thread: each thread's name and its count since counting began.
pub fn code_fetches_by_thread() -> Vec<(String, u64)> {
    callbacks::CODE_FETCHES_BY_THREAD.lock().map_or_else(
        |_| Vec::new(),
        |all| {
            all.iter()
                .map(|(name, count)| (name.clone(), count.load(std::sync::atomic::Ordering::Relaxed)))
                .collect()
        },
    )
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
    fn tpidr_el0(&self) -> Option<u64> {
        let mut out = OdEffectiveConfig::default();
        // SAFETY: the jit is live (this runs inside one of its callbacks) and `out` is writable.
        unsafe { od_jit_effective_config(self.jit, &mut out) };
        // SAFETY: the slot is the boxed `u64` the jit was configured with, alive as long as it is.
        (out.tpidr_el0_ptr != 0).then(|| unsafe { *(out.tpidr_el0_ptr as *const u64) })
    }

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
    /// Data accesses the slow path served through a split host page's alias (the 4 KiB overlay,
    /// `omni_mem::subpage`): allowed by the guest's view, refused by the host page. Not a degraded
    /// block -- the slice invariant subtracts them.
    pub(crate) split_served: u64,
    /// Data accesses through a tagged address the slow path served (`top_byte_ignore`): with
    /// `tbi_direct_mask` off, every one; the slice invariant subtracts them as it does
    /// `split_served`.
    pub(crate) tagged_served: u64,

    pub(crate) thunks: BTreeSet<GuestAddr>,
    /// Guest function entries served by a native host implementation (patch: HLE), shared by every
    /// context of the space (so a thread started after `libc.so` was mapped has them too). The
    /// entry is planted like a thunk while [`callbacks::HLE_ENABLED`] is on, and `cb_call_svc` runs
    /// the native code (`callbacks::Hle`) with this context in hand, so a fault becomes the same
    /// typed exit a guest access would. See [`DynarmicBackend::add_hle`].
    pub(crate) hle: Arc<HleSites>,
    /// Native HLE calls this context served. Those that found a byte inaccessible are
    /// [`hle_faults`](Self::hle_faults_field); both are read for a measurement.
    pub(crate) hle_calls: u64,
    pub(crate) hle_faults_field: u64,
    /// Thunks serviced **inside** the run loop rather than by exiting to the caller. See
    /// [`DynarmicCpu::add_inline_thunk`].
    pub(crate) inline_thunks: inline_table::InlineThunks,
    /// The guest-syscall handler (`GuestCpu::set_svc_handler`), if one is registered.
    pub(crate) svc_handler: Option<(ThunkFn, ThunkContext)>,
    /// `DynarmicOptions::top_byte_ignore`: the callback path clears bits 56-63 of a data address.
    pub(crate) top_byte_ignore: bool,
    /// How many inline thunks have been serviced, so a measurement can prove the path ran.
    pub(crate) inline_calls: u64,
    /// How many of those asked to be handed back to the caller. See
    /// [`ThunkCall::defer_to_caller`].
    pub(crate) inline_deferred: u64,
    /// Hint instructions (`YIELD`, `WFE`, `WFI`, `SEV`, `SEVL`) that arrived as raised exceptions
    /// and were continued through.
    ///
    /// A **watch, not a detector** (`VERIFICATION.md` entry 11): it rises when the guest spins and
    /// stays at zero when it does not, and neither says the handling is right. It exists because
    /// the hints only arrive at all because the x64 A64 backend does not forward
    /// `hook_hint_instructions` (`callbacks::is_hint` carries the file and line), and a pin that
    /// started forwarding it would take this to zero silently.
    pub(crate) hints: u64,
    /// The host thread's `MXCSR`, captured at the top of every `run`. See [`mxcsr`].
    pub(crate) host_mxcsr: u32,
    pub(crate) breakpoints: BTreeSet<GuestAddr>,
    /// The sentinel return address planted in `X30`, if one is armed.
    pub(crate) sentinel: Option<GuestAddr>,
    /// A breakpoint to ignore for exactly one fetch, so that resuming from a breakpoint executes the
    /// instruction under it rather than tripping over it again.
    pub(crate) suppressed_breakpoint: Option<GuestAddr>,

    pub(crate) pending: Option<PendingExit>,
    /// What this context's translator has done; see [`JitCounters`]. Plain integers: the context
    /// is owned by one thread, and a reader gets a copy through [`GuestCpu::jit_counters`].
    pub(crate) counters: JitCounters,
    /// The previous fetch's address, so a fetch that does not continue it counts a block start.
    pub(crate) last_fetch: u64,
    /// Block starts translated so far, kept only while
    /// [`crate::stats::tracking_retranslation`] is on.
    pub(crate) seen_blocks: Option<std::collections::HashSet<u64>>,
    /// With a shared code cache (D38): the space's planted addresses, which `read_code` consults
    /// instead of this context's own, and how many of this context's roles (thunk, inline thunk,
    /// sentinel) plant each address it counted in.
    pub(crate) shared_plants: Option<Arc<Planted>>,
    /// This space's other contexts' jits, for a guest `IC IVAU` to reach (none with a shared cache).
    pub(crate) peers: Option<Arc<Peers>>,
    pub(crate) plants_here: std::collections::HashMap<GuestAddr, u32>,
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
            // dynarmic asks for exactly one access per fault, so it never wants a W+X page as such;
            // a guest's own `PROT_READ|PROT_WRITE|PROT_EXEC` mapping reaches this only ever as a
            // read, write or code fetch, each of which a W+X region already permits. Execute is the
            // conservative reading of a W+X intent were it ever passed here.
            Protection::ReadExecute | Protection::ReadWriteExecute => FaultAccess::Execute,
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
        let top_byte_ignore = shared.options.tbi_masks_direct_path();
        let cpu =
            Self::build_unchecked(shared, config, tls, processor_id, Overrides::default())?;
        // **The startup assertion.** Read back from the live `UserConfig` rather than echoed from
        // what was asked for, and run before the context is handed to anyone, so a context that
        // exists is a context whose memory path is D4's.
        require_memory_path(&cpu.memory_mapping(), extent, top_byte_ignore)?;
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
            split_served: 0,
            tagged_served: 0,
            thunks: BTreeSet::new(),
            hle: Arc::clone(&shared.hle),
            hle_calls: 0,
            hle_faults_field: 0,
            inline_thunks: inline_table::InlineThunks::default(),
            svc_handler: None,
            top_byte_ignore: shared.options.top_byte_ignore,
            inline_calls: 0,
            inline_deferred: 0,
            hints: 0,
            host_mxcsr: mxcsr::read(),
            breakpoints: BTreeSet::new(),
            sentinel: None,
            suppressed_breakpoint: None,
            pending: None,
            counters: JitCounters::default(),
            last_fetch: 0,
            seen_blocks: None,
            shared_plants: None,
            peers: shared.code_cache.is_none().then(|| Arc::clone(&shared.peers)),
            plants_here: std::collections::HashMap::new(),
            ticks_remaining: 0,
            ticks_used: 0,
            panic_msg: None,
        }));

        // D13: the thread pointer is programmed *before* the jit exists, so there is no window in
        // which a context could be run with a zero one.
        let mut tpidr_el0 = Box::new(config.tpidr_el0() as u64);
        let tpidrro_el0 = Box::new(config.tpidr_el0() as u64);

        let options = shared.options;
        let cfg = thread_config(
            &options,
            shared.monitor.0,
            ctx.get().cast::<c_void>(),
            &mut *tpidr_el0,
            &*tpidrro_el0,
            processor_id,
            overrides,
            shared.extent.low_window_delta(),
        );

        // D38: a context of a space with a shared code cache runs from it -- unless it is one of
        // `create_misconfigured_thread`'s, whose configuration shapes code differently on purpose
        // and which the cache would refuse.
        let shared_cache = shared
            .code_cache
            .as_ref()
            .filter(|_| overrides.is_conforming())
            .map(|cache| cache.0);
        if shared_cache.is_some() {
            // SAFETY: nothing is executing yet.
            unsafe { (*ctx.get()).shared_plants = Some(Arc::clone(&shared.planted)) };
        }

        // SAFETY: `cfg` is fully initialised; `callbacks` is a `'static` constant; `ctx`, the two
        // register boxes and the monitor all outlive the jit, which is freed in `Drop` before any of
        // them -- and so does the shared cache, a field of the `Arc<Shared>` the context holds.
        // dynarmic copies `cfg` and keeps the pointers.
        let jit = unsafe {
            match shared_cache {
                Some(cache) => od_jit_new_shared(&cfg, cache),
                None => od_jit_new(&cfg),
            }
        };
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

        // D10: the pager must see a guest fault before dynarmic does. Building the first jit is
        // when dynarmic's POSIX build installs its own SIGSEGV handler, over the pager's, and that
        // handler takes a fault in its code cache itself -- onto the 30-49x callback path -- without
        // passing it on. So first place is re-asserted here, after every jit, while nothing runs.
        // A no-op on Windows, where a vectored handler is first by construction.
        if shared.owns_guest_paging {
            if let Err(error) = DemandPager::reassert_precedence() {
                // SAFETY: the jit was created above and nothing has run on it or holds it.
                unsafe { od_jit_free(jit) };
                return Err(CpuError::Backend {
                    backend: BACKEND_NAME,
                    operation: "keep the guest demand pager ahead of dynarmic's fault handler",
                    detail: format!(
                        "{error}. D10 requires Omnidroid to take guest faults ahead of dynarmic's \
                         own handler; without that every demand-paged access in translated code \
                         recompiles its block onto the callback path, measured 30-49x slower"
                    ),
                });
            }
        }

        let armed = options.assert_callback_free_slices && shared.owns_guest_paging;
        if shared.code_cache.is_none() {
            shared.peers.0.lock().push(jit as usize);
        }

        Ok(Self {
            jit,
            ctx,
            tpidr_el0,
            tpidrro_el0,
            shared,
            cost: ContextCost {
                // The guest's TLS block, plus the fast-dispatch table **when the optimization that
                // reads it is on** -- patch 0017 allocates it only then (D32), and on x64 it is on
                // under `INTERRUPTIBLE` since patch 0019, at 64 KiB (D35). Both are derived
                // rather than measured: the first is one page by construction, the second is
                // `sizeof(FastDispatchEntry) * fast_dispatch_table_size` from the pin, checked
                // against the vendored source by `dynarmic-sys`'s `pin_constants` test. The code
                // cache's committed high-water mark is still missing; see `cost`.
                private_committed: tls.as_ref().map_or(0, GuestTls::len).saturating_add(
                    if options.optimizations() & dynarmic_sys::optimization::FAST_DISPATCH != 0 {
                        options.fast_dispatch_table_bytes()
                    } else {
                        0
                    },
                ),
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
            low_window: observed.fastmem_low_window != 0,
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

    /// Data accesses this context's slow path served through a split host page's alias (the 4 KiB
    /// overlay). Zero wherever the host page is the guest's.
    #[must_use]
    pub fn split_served(&self) -> u64 {
        self.with_ctx(|ctx| ctx.split_served)
    }

    /// Data accesses through a tagged address this context's slow path served
    /// ([`DynarmicOptions::tbi_direct_mask`] off: all of them).
    #[must_use]
    pub fn tagged_served(&self) -> u64 {
        self.with_ctx(|ctx| ctx.tagged_served)
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

    /// Whether this context runs from the space's shared code cache (D38).
    fn shares_code(&self) -> bool {
        self.with_ctx(|ctx| ctx.shared_plants.is_some())
    }

    /// Shared code cache: one more of this context's roles plants `address` (`role_is_new` false
    /// when the role already did, e.g. a thunk registered twice). The translated word changes --
    /// and the translation is dropped, for every context -- only when the space's set changes.
    fn plant(&mut self, address: GuestAddr, role_is_new: bool) -> CpuResult<()> {
        if !role_is_new {
            return Ok(());
        }
        let newly_planted = self.with_ctx(|ctx| {
            let roles = ctx.plants_here.entry(address).or_insert(0);
            *roles += 1;
            *roles == 1 && ctx.shared_plants.as_ref().is_some_and(|p| p.add(address))
        });
        if newly_planted {
            self.invalidate_word(address)?;
        }
        Ok(())
    }

    /// Shared code cache: one fewer of this context's roles plants `address`.
    fn unplant(&mut self, address: GuestAddr, role_existed: bool) -> CpuResult<()> {
        if !role_existed {
            return Ok(());
        }
        let no_longer_planted = self.with_ctx(|ctx| {
            let Some(roles) = ctx.plants_here.get_mut(&address) else { return false };
            *roles -= 1;
            if *roles > 0 {
                return false;
            }
            ctx.plants_here.remove(&address);
            ctx.shared_plants.as_ref().is_some_and(|p| p.remove(address))
        });
        if no_longer_planted {
            self.invalidate_word(address)?;
        }
        Ok(())
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
        // D38: this context's plants leave the space. An address no context plants any more is
        // translated from guest memory again, for every context -- through the cache itself, since
        // this jit is about to go.
        let released: Vec<GuestAddr> = self.with_ctx(|ctx| {
            let Some(planted) = ctx.shared_plants.as_ref() else { return Vec::new() };
            ctx.plants_here.keys().copied().filter(|&address| planted.remove(address)).collect()
        });
        if let Some(cache) = &self.shared.code_cache {
            for address in released {
                // SAFETY: the cache outlives every context (`Arc<Shared>`); this thread is not
                // executing a jit of it (`&mut self`).
                unsafe { od_code_cache_invalidate_range(cache.0, address as u64, 4) };
            }
        }
        // Out of the peers first, under their lock: no other context's `IC IVAU` can then be
        // reaching this jit when it is freed.
        let jit = self.jit as usize;
        self.shared.peers.0.lock().retain(|&p| p != jit);
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
            // Refused on a shared code cache (D38): see `add_breakpoint`.
            breakpoints: !self.shares_code(),
            // See `add_inline_thunk`: the `SVC` terminal's `CheckHalt{PopRSBHint}` is what makes it
            // possible. `PopRSBHint` is keyed on the PC the handler wrote, so the resume is that
            // address either way; the flag gates it because the path is only tested under it.
            inline_thunks: self.shared.options.interruptible,
            // D38: this context runs the space's one set of translations.
            shared_translation: self.with_ctx(|ctx| ctx.shared_plants.is_some()),
        }
    }

    fn space(&self) -> GuestAddressSpace {
        self.shared.extent
    }

    fn run(&mut self, from: GuestAddr, limit: RunLimit) -> CpuResult<ExitReason> {
        // The thread about to take guest faults needs room to take them: the jit may have been
        // built on another thread, so this is checked where guest code runs, once per thread.
        if self.shared.owns_guest_paging {
            DemandPager::prepare_thread().map_err(|error| CpuError::Backend {
                backend: BACKEND_NAME,
                operation: "prepare this thread to take guest faults",
                detail: error.to_string(),
            })?;
        }
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
            let served_before = self.with_ctx(|ctx| ctx.split_served + ctx.tagged_served);

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
                // An access the 4 KiB overlay's slow path served through a split page's alias is
                // not a block that stopped reaching memory directly: the host page refuses what
                // the guest's 4 KiB page allows, so the callback is the only way there
                // (`omni_mem::subpage`). Those entries are not counted against the slice, and
                // neither are tagged accesses served there (`DynarmicOptions::tbi_direct_mask`).
                let served =self.with_ctx(|ctx| ctx.split_served + ctx.tagged_served).saturating_sub(served_before);
                let delta = self.slow_path_entries().saturating_sub(before).saturating_sub(served);
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
        self.with_ctx(|ctx| {
            ctx.executable_cache = None;
            ctx.counters.invalidations += 1;
        });
        // SAFETY: the jit is live; the shim clamps a zero or overflowing length, which matters
        // because this range comes from guest `mprotect`, guest `munmap` and the guest's own
        // `IC IVAU` (Global Constraint 11).
        unsafe { od_jit_invalidate_range(self.jit, range.start() as u64, range.len() as u64) };
        Ok(())
    }

    fn add_thunk(&mut self, address: GuestAddr) -> CpuResult<()> {
        let new = self.with_ctx(|ctx| ctx.thunks.insert(address));
        if self.shares_code() {
            return self.plant(address, new);
        }
        self.invalidate_word(address)
    }

    fn remove_thunk(&mut self, address: GuestAddr) -> CpuResult<bool> {
        let had = self.with_ctx(|ctx| ctx.thunks.remove(&address));
        if self.shares_code() {
            self.unplant(address, had)?;
            return Ok(had);
        }
        self.invalidate_word(address)?;
        Ok(had)
    }

    /// # Why this backend can service a thunk without leaving the run loop
    ///
    /// Because a thunk is a planted `SVC` (`STOP_SVC`, this module's own constant) and `SVC`'s
    /// terminal in dynarmic's A64
    /// frontend is `CheckHalt{PopRSBHint}`. A callback that does **not** raise a halt falls through
    /// `CheckHalt` into `PopRSBHint`, whose handler computes the location descriptor from the PC in
    /// `JitState` — the one the callback wrote — and compares it with the top return-stack-buffer
    /// entry. A hit jumps to that entry's block, which is therefore the written PC's block (the
    /// `SVC` pushed its own PC + 4, so a callback that leaves the PC alone hits; since patch 0018,
    /// D33, a hit also checks the halt flag and the budget). A miss, with `FastDispatch` on (x64
    /// under `optimization::INTERRUPTIBLE` since patch 0019, D35), continues into the fast-dispatch
    /// handler, which checks both, then probes its table with the same descriptor -- a hit is the
    /// written PC's block again, a miss is `LookupBlock` of the written PC. With `FastDispatch`
    /// cleared (arm64, which does not implement it) a miss emits `ReturnFromRunCode`. And
    /// `ReturnFromRunCode` is **not** a return to the caller: it is the top
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
        // **Refused rather than registered when the flag is clear.** This was first reasoned as "the
        // resume would be a return-stack-buffer prediction"; it is not -- both the RSB and the
        // fast-dispatch lookup are keyed on the PC the handler wrote (D33). What remains is that the
        // thunk path is only exercised under `INTERRUPTIBLE`, whose flag set is the one whose
        // indirect-branch handlers are measured to check the halt flag (D35), so the configuration
        // is refused, not assumed.
        if !self.shared.options.interruptible {
            return Err(CpuError::Unsupported {
                backend: BACKEND_NAME,
                operation: "dispatch a thunk inside the run loop",
                reason: "`DynarmicOptions::interruptible` is false: the in-loop thunk path is only \
                         tested under `optimization::INTERRUPTIBLE`, the flag set whose \
                         indirect-branch handlers are measured to check the halt flag",
            });
        }
        let new = self.with_ctx(|ctx| {
            let new = !ctx.inline_thunks.contains_key(&address);
            ctx.inline_thunks.insert(address, handler, context);
            new
        });
        if self.shares_code() {
            return self.plant(address, new);
        }
        self.invalidate_word(address)
    }

    fn remove_inline_thunk(&mut self, address: GuestAddr) -> CpuResult<bool> {
        let had = self.with_ctx(|ctx| ctx.inline_thunks.remove(&address));
        if self.shares_code() {
            self.unplant(address, had)?;
            return Ok(had);
        }
        self.invalidate_word(address)?;
        Ok(had)
    }

    fn set_svc_handler(&mut self, handler: ThunkFn, context: ThunkContext) -> CpuResult<()> {
        // The same condition as `add_inline_thunk`, for the same reason: a handler served inside
        // the loop is only tested under the flag set whose dispatch checks the halt flag (D35).
        if !self.shared.options.interruptible {
            return Err(CpuError::Unsupported {
                backend: BACKEND_NAME,
                operation: "serve guest syscalls inside the run loop",
                reason: "`DynarmicOptions::interruptible` is false",
            });
        }
        self.with_ctx(|ctx| ctx.svc_handler = Some((handler, context)));
        Ok(())
    }

    fn inline_thunk_calls(&self) -> InlineThunkCounts {
        self.with_ctx(|ctx| InlineThunkCounts {
            serviced: ctx.inline_calls,
            deferred: ctx.inline_deferred,
        })
    }

    fn set_return_sentinel(&mut self, address: GuestAddr) -> CpuResult<()> {
        let previous = self.with_ctx(|ctx| ctx.sentinel.replace(address));
        if self.shares_code() {
            // Re-arming the same sentinel -- every host-to-guest call does -- changes nothing a
            // translation depends on.
            if previous == Some(address) {
                return Ok(());
            }
            if let Some(previous) = previous {
                self.unplant(previous, true)?;
            }
            return self.plant(address, true);
        }
        self.invalidate_word(address)
    }

    fn return_sentinel(&self) -> Option<GuestAddr> {
        self.with_ctx(|ctx| ctx.sentinel)
    }

    fn add_breakpoint(&mut self, address: GuestAddr) -> CpuResult<()> {
        if self.shares_code() {
            return Err(CpuError::Unsupported {
                backend: BACKEND_NAME,
                operation: "set a breakpoint",
                reason: "this context runs from a code cache every guest thread of the space shares \
                         (OMNI_JIT_SHARED_CACHE, D38): a breakpoint for one thread cannot be planted \
                         in code every thread runs",
            });
        }
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
    /// * [`OD_FIXED_PER_JIT_BYTES`], the `FastDispatchEntry` table, **only when the
    ///   `FastDispatch` optimization is on** -- on x64 under `INTERRUPTIBLE` since patch 0019, at
    ///   64 KiB (D35); never on arm64, which has no table. Upstream holds 16 MiB of it by value and
    ///   writes it in every jit; patch 0017 allocates it only when it is read (D32).
    ///   `dynarmic-sys`'s `pin_constants` test reads both factors and the guard back out of the
    ///   vendored source, so a re-pin cannot move them silently.
    ///
    /// **The missing term is dynarmic's code cache**, which on Windows commits incrementally as code
    /// is emitted (`BlockOfCode::EnsureMemoryCommitted`: 2 MiB for the prelude since patch 0017,
    /// 16 MiB before it, then 1 MiB ahead of each block), so the figure that matters is a high-water
    /// mark. It is a private member of `BlockOfCode` that `A64::Jit` does not expose, and reading it
    /// would mean patching the vendored pin. It is **bounded above** by
    /// [`DynarmicOptions::code_cache_size`], and M2's gate asserts both ends: the measured
    /// per-thread charge is under a ceiling, and the gap between it and this figure is under
    /// `code_cache_size`. So the omission is bounded and asserted rather than merely admitted.
    ///
    /// Measured against this: **24.5 MiB** per guest thread at the 8 MiB default cache (n = 8
    /// threads, serialized), of which this reports 16.004 MiB. D5's 20-35 MiB band was measured
    /// against the 128 MiB default. Since patch 0017 it measures 4.47 MiB (D32); patch 0019 adds
    /// the 64 KiB table on x64 (D35).
    fn cost(&self) -> ContextCost {
        self.cost
    }

    fn jit_counters(&self) -> JitCounters {
        self.with_ctx(|ctx| ctx.counters)
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
