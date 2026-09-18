//! Raw FFI bindings to the pinned dynarmic A64 JIT.
//!
//! This crate is deliberately the whole of the C++ problem and none of the
//! runtime's. It owns the vendored `yuzu-mirror/dynarmic@9d45823` tree
//! (D5), the build script that compiles it, the `extern "C"` shim over its
//! virtual-callback interfaces, and the declarations below. It owns no policy:
//! there is no `GuestCpu` implementation here, and nothing here decides what a
//! guest address means.
//!
//! The split is load-bearing. `omni-cpu` holds the `GuestCpu` trait and has no
//! build script and no C++ dependency, so the trait still compiles on a host
//! with no C++ toolchain at all — which is exactly the host the ARM64-native
//! path (`ARCHITECTURE.md` §6) targets. Mapping this crate onto that trait is a
//! later task.
//!
//! # The three things that make this sound
//!
//! Every function here is `unsafe`, and every one of them can end up running
//! *generated machine code* that calls back into Rust. Three properties carry
//! the weight; a caller that breaks any of them has undefined behaviour, and
//! the foreign side will not catch it.
//!
//! ## 1. Re-entrancy: the callback context is never the object you are calling
//!
//! [`OdConfig::ctx`] is handed back to every callback verbatim. Generated guest
//! code can enter a callback at any instruction boundary, so a callback can run
//! at any point inside [`od_jit_run`]. If `od_jit_run` were reached through a
//! `&mut T` and a callback then formed a second `&mut T` from `ctx`, that is two
//! live unique references to one object: undefined behaviour, not a race you
//! can test for.
//!
//! What the foreign side guarantees, which is what makes a discipline possible
//! at all:
//!
//! * **Callbacks are never nested.** No dynarmic callback executes guest code,
//!   so callback depth is always exactly one. (Verified in
//!   `tests/reentrancy.rs`, which counts depth from inside a callback.)
//! * **Callbacks run on the thread that called `od_jit_run`.** dynarmic does not
//!   move execution to another thread, so a callback cannot run concurrently
//!   with its own `od_jit_run`.
//! * **`od_jit_run` refuses to re-enter.** dynarmic's own `Jit::Run` opens with
//!   `ASSERT(!is_executing)`, and its asserts call `std::terminate`. Guest code
//!   is untrusted input, and Global Constraint 11 makes a reachable abort
//!   Critical, so the shim checks `IsExecuting()` first and returns
//!   [`OD_HALT_SHIM_REENTERED`] instead.
//!
//! The discipline that follows, and that a wrapper must implement:
//!
//! * `ctx` points at a heap allocation that is **not** the object holding the
//!   `*mut OdJit`, so no `&mut` to it can exist at the call site.
//! * The wrapper's `run` takes `&self`, never `&mut self`.
//! * A callback forms its `&mut` from `ctx` for the body of that callback only,
//!   and never stores it.
//!
//! ## 2. Callbacks must not unwind
//!
//! Between a callback and [`od_jit_run`] sit JIT-generated frames and C++
//! frames. Neither has unwind tables Rust can use. A Rust callback declared
//! `extern "C"` that panics aborts the process, which Global Constraint 11 also
//! rates Critical, since guest code chooses when callbacks fire.
//!
//! Callbacks must therefore wrap their body in
//! [`std::panic::catch_unwind`], record the payload, call [`od_jit_halt`] with
//! a user-defined bit, and return something benign. `od_jit_halt` is documented
//! by dynarmic as callable from inside a callback; it sets an atomic flag the
//! dispatcher checks between blocks.
//!
//! ## 3. Pointers baked into generated code must outlive the jit
//!
//! [`OdConfig::tpidr_el0`], [`OdConfig::tpidrro_el0`] and the fastmem base are
//! not read through callbacks — dynarmic **inlines them into emitted
//! instructions**. Moving or freeing what they point at leaves generated code
//! dereferencing a dangling pointer with no diagnostic. They must be pinned for
//! the jit's entire life, and the same goes for [`OdConfig::monitor`], which is
//! shared between the jits of every guest thread.
//!
//! # Configuration that is not optional
//!
//! D4 measured identity mapping (`fastmem_pointer = 0`,
//! `fastmem_address_space_bits = 64`) at 5,207 Mguest-insn/s against 396 for
//! the callback path — 13.2x. dynarmic's default
//! `fastmem_address_space_bits` is **36**, and a guest address above that
//! silently falls back to callbacks *while still producing correct results*.
//! No functional test can see that. [`od_jit_effective_config`] and
//! [`od_jit_stats`] exist so a startup assertion can, by reading what dynarmic
//! actually holds and by counting callback entries that should be zero.

#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

/// ABI version of the C shim. Compared against the C++ side's own copy by
/// [`od_dynarmic_abi_version`]; a mismatch means a stale object file, which
/// would otherwise be silent memory corruption.
pub const OD_DYNARMIC_ABI_VERSION: u32 = 1;

/// `kind` values passed to [`OdCallbacks::exception_raised`]. These mirror
/// `Dynarmic::A64::Exception`, which the shim checks with `static_assert`.
pub mod exception {
    /// Unallocated instruction encoding. The usual result of executing data.
    pub const UNALLOCATED_ENCODING: u32 = 0;
    /// An instruction containing a reserved field value.
    pub const RESERVED_VALUE: u32 = 1;
    /// An instruction whose behaviour the architecture leaves unpredictable.
    pub const UNPREDICTABLE_INSTRUCTION: u32 = 2;
    /// `WFI`.
    pub const WAIT_FOR_INTERRUPT: u32 = 3;
    /// `WFE`.
    pub const WAIT_FOR_EVENT: u32 = 4;
    /// `SEV`.
    pub const SEND_EVENT: u32 = 5;
    /// `SEVL`.
    pub const SEND_EVENT_LOCAL: u32 = 6;
    /// `YIELD`.
    pub const YIELD: u32 = 7;
    /// `BRK`.
    pub const BREAKPOINT: u32 = 8;
    /// The guest branched somewhere [`super::OdCallbacks::read_code`] refused.
    /// This is how a jump into unmapped memory becomes a typed exit.
    pub const NO_EXECUTE_FAULT: u32 = 9;
}

/// Single-step completed.
pub const OD_HALT_STEP: u32 = 0x0000_0001;
/// Execution stopped to service a code-cache invalidation.
pub const OD_HALT_CACHE_INVALIDATION: u32 = 0x0000_0002;
/// A memory abort was signalled.
pub const OD_HALT_MEMORY_ABORT: u32 = 0x0000_0004;
/// User-defined halt bit 1.
pub const OD_HALT_USER1: u32 = 0x0100_0000;
/// User-defined halt bit 2.
pub const OD_HALT_USER2: u32 = 0x0200_0000;
/// User-defined halt bit 3.
pub const OD_HALT_USER3: u32 = 0x0400_0000;
/// User-defined halt bit 4.
pub const OD_HALT_USER4: u32 = 0x0800_0000;
/// User-defined halt bit 5.
pub const OD_HALT_USER5: u32 = 0x1000_0000;
/// User-defined halt bit 6.
pub const OD_HALT_USER6: u32 = 0x2000_0000;
/// User-defined halt bit 7.
pub const OD_HALT_USER7: u32 = 0x4000_0000;
/// User-defined halt bit 8.
pub const OD_HALT_USER8: u32 = 0x8000_0000;

/// Not a dynarmic halt reason. Returned by [`od_jit_run`] and [`od_jit_step`]
/// when they are called on a jit that is already executing — that is, from
/// inside a callback. dynarmic would abort; the shim refuses.
pub const OD_HALT_SHIM_REENTERED: u32 = 0x0080_0000;

/// The host side of the boundary: one function pointer per thing dynarmic can
/// ask of us. All of them are required; [`od_jit_new`] rejects a struct with a
/// null member rather than letting generated code call address zero.
///
/// The first argument of every callback is [`OdConfig::ctx`], untouched.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OdCallbacks {
    /// Fetch the 4-byte-aligned instruction word at `vaddr`. Return 1 having
    /// written `out`, or 0 to raise [`exception::NO_EXECUTE_FAULT`].
    pub read_code: Option<unsafe extern "C" fn(*mut c_void, u64, *mut u32) -> i32>,
    /// 8-bit data read. Slow path: unreached while fastmem is working.
    pub read8: Option<unsafe extern "C" fn(*mut c_void, u64) -> u8>,
    /// 16-bit data read.
    pub read16: Option<unsafe extern "C" fn(*mut c_void, u64) -> u16>,
    /// 32-bit data read.
    pub read32: Option<unsafe extern "C" fn(*mut c_void, u64) -> u32>,
    /// 64-bit data read.
    pub read64: Option<unsafe extern "C" fn(*mut c_void, u64) -> u64>,
    /// 128-bit data read; `out` is little-endian, `out[0]` is bits 63:0.
    pub read128: Option<unsafe extern "C" fn(*mut c_void, u64, *mut u64)>,
    /// 8-bit data write.
    pub write8: Option<unsafe extern "C" fn(*mut c_void, u64, u8)>,
    /// 16-bit data write.
    pub write16: Option<unsafe extern "C" fn(*mut c_void, u64, u16)>,
    /// 32-bit data write.
    pub write32: Option<unsafe extern "C" fn(*mut c_void, u64, u32)>,
    /// 64-bit data write.
    pub write64: Option<unsafe extern "C" fn(*mut c_void, u64, u64)>,
    /// 128-bit data write.
    pub write128: Option<unsafe extern "C" fn(*mut c_void, u64, *const u64)>,
    /// `STXRB` compare-and-swap; return 1 on success.
    pub write_exclusive8: Option<unsafe extern "C" fn(*mut c_void, u64, u8, u8) -> i32>,
    /// `STXRH` compare-and-swap.
    pub write_exclusive16: Option<unsafe extern "C" fn(*mut c_void, u64, u16, u16) -> i32>,
    /// `STXR` (32-bit) compare-and-swap.
    pub write_exclusive32: Option<unsafe extern "C" fn(*mut c_void, u64, u32, u32) -> i32>,
    /// `STXR` (64-bit) compare-and-swap.
    pub write_exclusive64: Option<unsafe extern "C" fn(*mut c_void, u64, u64, u64) -> i32>,
    /// `STXP` compare-and-swap.
    pub write_exclusive128:
        Option<unsafe extern "C" fn(*mut c_void, u64, *const u64, *const u64) -> i32>,
    /// Execute exactly `num_insns` instructions at `pc`. 231 of dynarmic's 874
    /// A64 decoder entries are unimplemented (D5) and arrive here.
    pub interpreter_fallback: Option<unsafe extern "C" fn(*mut c_void, u64, u64)>,
    /// `SVC #imm`.
    pub call_svc: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    /// An `exception::*` kind was raised at `pc`.
    pub exception_raised: Option<unsafe extern "C" fn(*mut c_void, u64, u32)>,
    /// `IC IVAU` / `IC IALLU` / `IC IALLUIS`: the guest wrote code.
    pub instruction_cache_op: Option<unsafe extern "C" fn(*mut c_void, u32, u64)>,
    /// `CNTPCT_EL0`. Only called when `wall_clock_cntpct` is 0.
    pub get_cntpct: Option<unsafe extern "C" fn(*mut c_void) -> u64>,
    /// Cycle accounting. Only called when `enable_cycle_counting` is 1.
    pub add_ticks: Option<unsafe extern "C" fn(*mut c_void, u64)>,
    /// Remaining cycle budget; returning 0 ends the current run.
    pub get_ticks_remaining: Option<unsafe extern "C" fn(*mut c_void) -> u64>,
}

/// Configuration for [`od_jit_new`]. Copied on entry, so the struct itself need
/// not outlive the call — but every pointer in it must outlive the jit.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OdConfig {
    /// Must be [`OD_DYNARMIC_ABI_VERSION`].
    pub abi_version: u32,
    /// Borrowed for the call; the contents are copied.
    pub callbacks: *const OdCallbacks,
    /// Opaque host pointer handed to every callback. See the crate docs on
    /// re-entrancy before deciding what this points at.
    pub ctx: *mut c_void,
    /// D13: where `TPIDR_EL0` lives. dynarmic inlines this pointer into
    /// generated code, so the pointee must be pinned for the jit's life.
    pub tpidr_el0: *mut u64,
    /// Where `TPIDRRO_EL0` lives; same pinning requirement.
    pub tpidrro_el0: *const u64,
    /// 0 routes every guest memory access through the callbacks: correct, and
    /// 13.2x slower (D4).
    pub fastmem_enabled: i32,
    /// Host base address for guest address 0. 0 means identity mapping.
    pub fastmem_pointer: u64,
    /// Width of the guest address space. **64** for identity mapping;
    /// dynarmic's own default is 36 and degrades silently.
    pub fastmem_address_space_bits: u32,
    /// Whether accesses past the fastmem arena wrap instead of faulting.
    pub silently_mirror_fastmem: i32,
    /// Recompile a block with fastmem disabled after it page-faults.
    pub recompile_on_fastmem_failure: i32,
    /// Use fastmem for `LDXR`/`STXR` rather than the global monitor.
    pub fastmem_exclusive_access: i32,
    /// Shared [`od_monitor_new`] handle, or null.
    pub monitor: *mut c_void,
    /// Index into the monitor; must be unique per guest thread.
    pub processor_id: u32,
    /// Bytes of code cache; 0 selects dynarmic's 128 MiB default. D5 measured
    /// 20-35 MiB committed per thread, so this is a real memory knob. A
    /// non-zero value outside 8 MiB..=2 GiB (8 MiB..=128 MiB on an `aarch64`
    /// host) is rejected by [`od_jit_new`], because dynarmic would assert and
    /// an assert terminates the process.
    pub code_cache_size: u64,
    /// `CNTFRQ_EL0`; 0 selects dynarmic's default.
    pub cntfrq_el0: u32,
    /// `CTR_EL0`; 0 selects dynarmic's default of `0x8444c004`.
    pub ctr_el0: u32,
    /// `DCZID_EL0`: log2 of the `DC ZVA` block size in words.
    pub dczid_el0: u32,
    /// Enable `add_ticks`/`get_ticks_remaining`, which is how a step budget is
    /// enforced without single-stepping.
    pub enable_cycle_counting: i32,
    /// Treat `CNTPCT_EL0` as a wall clock, which lets dynarmic omit code.
    pub wall_clock_cntpct: i32,
    /// Raise `exception_raised` for hint instructions.
    pub hook_hint_instructions: i32,
    /// Give some unpredictable instructions defined behaviour instead of
    /// raising an exception.
    pub define_unpredictable_behaviour: i32,
    /// Check the halt flag after every guest memory access.
    pub check_halt_on_memory_access: i32,
    /// Permit dynarmic's accuracy-reducing optimizations. Leave 0.
    pub unsafe_optimizations: i32,
    /// dynarmic's `OptimizationFlag` bitmask, used verbatim.
    /// [`optimization::ALL_SAFE`] is the normal value.
    ///
    /// Exposed rather than hard-coded because two of these flags decide whether
    /// guest code can wedge the host thread:
    /// [`optimization::RETURN_STACK_BUFFER`] and [`optimization::FAST_DISPATCH`]
    /// emit terminal handlers that jump from one translated block straight to
    /// the next, checking **neither** the cycle counter nor the halt flag. A
    /// guest `BR`/`RET` loop that stays inside either cache therefore ignores a
    /// step budget *and* ignores [`od_jit_halt`] from another thread. Clearing
    /// both flags restores both escapes, at a cost in throughput.
    pub optimizations: u32,
}

/// `OdConfig::optimizations` bits, mirroring `Dynarmic::OptimizationFlag`.
pub mod optimization {
    /// Emitted blocks jump directly to a successor whose PC is known at
    /// translation time. The cycle counter *is* checked on this path.
    pub const BLOCK_LINKING: u32 = 0x0000_0001;
    /// Return-address prediction. Its terminal handler checks nothing; see
    /// [`super::OdConfig::optimizations`].
    pub const RETURN_STACK_BUFFER: u32 = 0x0000_0002;
    /// Two-tier dispatch with an MRU cache. Its terminal handler checks
    /// nothing either.
    pub const FAST_DISPATCH: u32 = 0x0000_0004;
    /// IR optimization: drop redundant guest-register reads and writes.
    pub const GET_SET_ELIMINATION: u32 = 0x0000_0008;
    /// IR optimization: constant propagation.
    pub const CONST_PROP: u32 = 0x0000_0010;
    /// Assorted safe IR optimizations.
    pub const MISC_IR_OPT: u32 = 0x0000_0020;
    /// Everything off. For bisecting a miscompilation.
    pub const NONE: u32 = 0;
    /// Every safe optimization; dynarmic's own default.
    pub const ALL_SAFE: u32 = 0x0000_FFFF;
    /// `ALL_SAFE` without the two flags whose terminal handlers skip the cycle
    /// and halt checks. This is the configuration in which a runaway guest can
    /// still be stopped.
    pub const INTERRUPTIBLE: u32 = ALL_SAFE & !(RETURN_STACK_BUFFER | FAST_DISPATCH);
}

/// What dynarmic is actually configured with, read back from its live
/// `UserConfig` rather than echoed from [`OdConfig`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OdEffectiveConfig {
    /// 1 if a fastmem pointer is set.
    pub fastmem_enabled: i32,
    /// The host base in effect.
    pub fastmem_pointer: u64,
    /// The width in effect. Assert this is 64 for identity mapping.
    pub fastmem_address_space_bits: u64,
    /// Whether out-of-arena accesses wrap.
    pub silently_mirror_fastmem: i32,
    /// Whether faulting blocks are recompiled without fastmem.
    pub recompile_on_fastmem_failure: i32,
    /// Whether exclusives use fastmem.
    pub fastmem_exclusive_access: i32,
    /// 1 if a page table is in use instead of / as well as fastmem.
    pub page_table_present: i32,
    /// Code cache size in bytes.
    pub code_cache_size: u64,
    /// Whether tick callbacks are live.
    pub enable_cycle_counting: i32,
    /// Whether hint instructions raise exceptions.
    pub hook_hint_instructions: i32,
    /// The `OptimizationFlag` bitmask in effect.
    pub optimizations: u32,
    /// 1 if accuracy-reducing optimizations were permitted.
    pub unsafe_optimizations: u32,
    /// Address of the `TPIDR_EL0` slot baked into generated code; 0 means the
    /// guest cannot read it, which D13 says breaks every stack-protected
    /// function in `libroblox.so`.
    pub tpidr_el0_ptr: u64,
    /// Address of the `TPIDRRO_EL0` slot.
    pub tpidrro_el0_ptr: u64,
}

/// Callback-entry counters, maintained by the shim on the jit's own thread.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OdStats {
    /// Instruction fetches. These happen during translation, not execution.
    pub read_code: u64,
    /// Data reads that missed fastmem.
    pub slow_path_reads: u64,
    /// Data writes that missed fastmem.
    pub slow_path_writes: u64,
    /// Exclusive accesses that missed fastmem.
    pub slow_path_exclusive: u64,
    /// Entries to the interpreter fallback.
    pub interpreter_fallbacks: u64,
    /// `SVC` instructions executed.
    pub svc_calls: u64,
    /// Exceptions raised.
    pub exceptions: u64,
    /// Instruction-cache maintenance operations.
    pub icache_ops: u64,
    /// `slow_path_reads + slow_path_writes + slow_path_exclusive`. Under
    /// identity fastmem this must stay 0; anything else is the silent 13.2x
    /// regression D4 warns about.
    pub slow_path_total: u64,
}

/// `sizeof`/`alignof` of each shared struct as the C++ side sees them. A
/// hand-written `#[repr(C)]` binding that disagrees with the header is silent
/// memory corruption, so `tests/abi.rs` compares these against Rust's.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OdAbiLayout {
    /// `sizeof(od_callbacks)`.
    pub callbacks_size: u32,
    /// `alignof(od_callbacks)`.
    pub callbacks_align: u32,
    /// `sizeof(od_config)`.
    pub config_size: u32,
    /// `alignof(od_config)`.
    pub config_align: u32,
    /// `sizeof(od_effective_config)`.
    pub effective_config_size: u32,
    /// `alignof(od_effective_config)`.
    pub effective_config_align: u32,
    /// `sizeof(od_stats)`.
    pub stats_size: u32,
    /// `alignof(od_stats)`.
    pub stats_align: u32,
}

extern "C" {
    /// The C++ side's [`OD_DYNARMIC_ABI_VERSION`].
    pub fn od_dynarmic_abi_version() -> u32;

    /// Struct sizes and alignments as the C++ side sees them.
    ///
    /// # Safety
    /// `out` must be a valid, writable `OdAbiLayout`.
    pub fn od_dynarmic_abi_layout(out: *mut OdAbiLayout);

    /// Allocate an exclusive monitor for `processor_count` guest threads.
    /// Returns null if the count is 0 or absurd, or on allocation failure.
    ///
    /// # Safety
    /// The returned pointer must be released exactly once with
    /// [`od_monitor_free`], and only after every jit using it is freed.
    pub fn od_monitor_new(processor_count: u64) -> *mut c_void;

    /// Release a monitor from [`od_monitor_new`].
    ///
    /// # Safety
    /// `monitor` must come from [`od_monitor_new`] and not be in use by any
    /// live jit. Null is accepted.
    pub fn od_monitor_free(monitor: *mut c_void);

    /// Create a jit. Returns null if the configuration is rejected or dynarmic
    /// throws; it never unwinds and never aborts.
    ///
    /// # Safety
    /// `config` must be a valid `OdConfig` whose `callbacks` pointer is valid
    /// for the call and whose other pointers stay valid, and pinned, until the
    /// returned jit is freed. Read the crate docs on re-entrancy before
    /// choosing `ctx`.
    pub fn od_jit_new(config: *const OdConfig) -> *mut c_void;

    /// Destroy a jit. Null is accepted.
    ///
    /// # Safety
    /// `jit` must come from [`od_jit_new`], must not be executing, and must not
    /// be freed twice.
    pub fn od_jit_free(jit: *mut c_void);

    /// Run guest code until halted. Returns a bitwise-or of `OD_HALT_*`, or
    /// [`OD_HALT_SHIM_REENTERED`] if called from inside a callback.
    ///
    /// # Safety
    /// `jit` must be live. This executes attacker-controlled guest code: the
    /// callbacks must not unwind, and everything the guest can reach through
    /// fastmem must be mapped or covered by a fault handler.
    pub fn od_jit_run(jit: *mut c_void) -> u32;

    /// Execute one guest instruction.
    ///
    /// # Safety
    /// As [`od_jit_run`].
    pub fn od_jit_step(jit: *mut c_void) -> u32;

    /// Set halt bits. Safe from another thread and from inside a callback.
    ///
    /// # Safety
    /// `jit` must be live for the duration of the call.
    pub fn od_jit_halt(jit: *mut c_void, reason: u32);

    /// Clear halt bits.
    ///
    /// # Safety
    /// `jit` must be live. Clearing a bit another thread is about to observe is
    /// a race; dynarmic says so too.
    pub fn od_jit_clear_halt(jit: *mut c_void, reason: u32);

    /// 1 while [`od_jit_run`] or [`od_jit_step`] is on the stack for this jit.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_is_executing(jit: *mut c_void) -> i32;

    /// Read `X0`-`X30`. Out-of-range indices read 0 rather than running off the
    /// end of dynarmic's register array.
    ///
    /// # Safety
    /// `jit` must be live. Values read while executing are in flight.
    pub fn od_jit_get_reg(jit: *mut c_void, index: u32) -> u64;

    /// Write `X0`-`X30`. Out-of-range indices are dropped.
    ///
    /// # Safety
    /// `jit` must be live and should not be executing.
    pub fn od_jit_set_reg(jit: *mut c_void, index: u32, value: u64);

    /// Read `SP`.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_get_sp(jit: *mut c_void) -> u64;

    /// Write `SP`.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_set_sp(jit: *mut c_void, value: u64);

    /// Read `PC`. dynarmic truncates it to a sign-extended 56 bits (D4).
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_get_pc(jit: *mut c_void) -> u64;

    /// Write `PC`.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_set_pc(jit: *mut c_void, value: u64);

    /// Read `V0`-`V31` into `out[0..2]`, little-endian.
    ///
    /// # Safety
    /// `jit` must be live and `out` must be writable for two `u64`.
    pub fn od_jit_get_vec(jit: *mut c_void, index: u32, out: *mut u64);

    /// Write `V0`-`V31`.
    ///
    /// # Safety
    /// `jit` must be live and `value` readable for two `u64`.
    pub fn od_jit_set_vec(jit: *mut c_void, index: u32, value: *const u64);

    /// Read `PSTATE`; `NZCV` is bits 31:28.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_get_pstate(jit: *mut c_void) -> u32;

    /// Write `PSTATE`.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_set_pstate(jit: *mut c_void, value: u32);

    /// Read `FPCR`.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_get_fpcr(jit: *mut c_void) -> u32;

    /// Write `FPCR`.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_set_fpcr(jit: *mut c_void, value: u32);

    /// Read `FPSR`.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_get_fpsr(jit: *mut c_void) -> u32;

    /// Write `FPSR`.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_set_fpsr(jit: *mut c_void, value: u32);

    /// Discard translations covering `[addr, addr + len)`. Safe from inside a
    /// callback.
    ///
    /// `len == 0` is a no-op and an overflowing length is clamped. dynarmic
    /// builds a closed interval from `addr + len - 1` without checking that
    /// the upper bound is above the lower: at `len == 0` it halts the guest to
    /// invalidate an empty range, and on overflow it silently invalidates
    /// nothing, leaving the guest running stale translations of code it just
    /// told us it changed.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_invalidate_range(jit: *mut c_void, addr: u64, len: u64);

    /// Discard every translation. Safe from inside a callback, where it halts
    /// execution to do the work.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_clear_cache(jit: *mut c_void);

    /// Drop this processor's exclusive reservation.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_clear_exclusive(jit: *mut c_void);

    /// Read back what dynarmic is configured with.
    ///
    /// # Safety
    /// `jit` must be live and `out` a valid, writable `OdEffectiveConfig`.
    pub fn od_jit_effective_config(jit: *mut c_void, out: *mut OdEffectiveConfig);

    /// Read the callback-entry counters.
    ///
    /// # Safety
    /// `jit` must be live and `out` a valid, writable `OdStats`.
    pub fn od_jit_stats(jit: *mut c_void, out: *mut OdStats);

    /// Zero the callback-entry counters, so a specific loop can be measured.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_reset_stats(jit: *mut c_void);
}
