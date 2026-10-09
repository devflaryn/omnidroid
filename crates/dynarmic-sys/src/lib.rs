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
//! path (`ARCHITECTURE.md` §6) targets. `omni-cpu`'s `dynarmic` module maps this
//! crate onto that trait.
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
//! the callback path — 30-49x. dynarmic's default
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
pub const OD_DYNARMIC_ABI_VERSION: u32 = 9;

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

/// Also not a dynarmic halt reason. Returned when a C++ exception escaped
/// `Jit::Run`/`Jit::Step` and the shim caught it. xbyak throws `Xbyak::Error`
/// when the code cache runs out of room, and translation happens inside `Run`,
/// so this is reachable — and an exception unwinding into Rust is undefined
/// behaviour, so it stops at the boundary. The jit is in an uncharacterised
/// state afterwards; free it rather than using it.
pub const OD_HALT_SHIM_THREW: u32 = 0x0040_0000;

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
    /// 30-49x slower (D4).
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
    /// Exposed rather than hard-coded because two of these flags decided whether
    /// guest code can wedge the host thread:
    /// [`optimization::RETURN_STACK_BUFFER`] and [`optimization::FAST_DISPATCH`]
    /// emit terminal handlers that jump from one translated block straight to
    /// the next, and upstream's check **neither** the cycle counter nor the halt
    /// flag. Patches 0018-0020 add both checks to every such handler this pin
    /// emits (D33, D35); `tests/hostile.rs`'s `the_stoppability_matrix` is what
    /// says they are still there.
    pub optimizations: u32,
    /// D41 (patch 0030): [`fastmem_pointer`](Self::fastmem_pointer) is added only to a guest
    /// address below 2^32; at and above it the host address is the guest address. arm64 hosts
    /// only (`od_jit_new` refuses it elsewhere), with fastmem on and a window wider than 32 bits.
    pub fastmem_low_window: i32,
    /// Patch 0035 (x64, shared cache): entries in each thread's fast-dispatch table, a power of
    /// two from 0x40 to 0x10000; 0 (or anything else) is the pin's 0x1000 -- 64 KiB a thread. The
    /// cache's value is every thread's.
    pub fast_dispatch_entries: u32,
}

/// `OdConfig::optimizations` bits, mirroring `Dynarmic::OptimizationFlag`.
pub mod optimization {
    /// Emitted blocks jump directly to a successor whose PC is known at
    /// translation time. The cycle counter *is* checked on this path.
    pub const BLOCK_LINKING: u32 = 0x0000_0001;
    /// Return-address prediction. Upstream's terminal handler checks nothing; **patch 0018**
    /// (x64) and **patch 0020** (arm64) make a hit check the cycle budget and the halt flag as the
    /// dispatcher does, which is what lets [`INTERRUPTIBLE`] keep it on both (D33, D35).
    pub const RETURN_STACK_BUFFER: u32 = 0x0000_0002;
    /// A per-thread table from a guest location to its translation, probed in emitted code by
    /// every `BR`/`BLR` and every `RET` that misses the return-stack buffer, instead of a return to
    /// the dispatcher. x64 only (the arm64 backend's `FastDispatchHint` is a dispatcher return).
    /// Upstream's handler checks nothing; **patch 0019** makes it check the cycle budget and the
    /// halt flag before the probe, and shrinks the table to 4,096 entries (64 KiB) (D35).
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
    /// `ALL_SAFE` without the flags whose terminal handlers skip the cycle and
    /// halt checks. The configuration in which a runaway guest can still be
    /// stopped. On x64 that is now **none**: `0x0000_FFF9` (D16) cleared
    /// [`RETURN_STACK_BUFFER`] and [`FAST_DISPATCH`]; patch 0018 gave the first
    /// handler the checks (`0x0000_FFFB`, D33) and patch 0019 the second, so
    /// `INTERRUPTIBLE` is `ALL_SAFE` (D35). `tests/hostile.rs`'s
    /// `the_stoppability_matrix` is the detector for both handlers.
    #[cfg(not(target_arch = "aarch64"))]
    pub const INTERRUPTIBLE: u32 = ALL_SAFE;
    /// `ALL_SAFE` without the flags whose terminal handlers skip the cycle and
    /// halt checks. On arm64 the return-stack buffer's handler checks both since
    /// patch 0020 (`0x0000_FFF9` before: a `RET` loop was MEASURED unstoppable
    /// under `0xFFFF` on M1, `return-ring`). [`FAST_DISPATCH`] stays clear: the
    /// arm64 backend does not implement it (its `FastDispatchHint` is a
    /// dispatcher return), so the flag buys nothing there, and a re-pin that
    /// implements it must not turn an unchecked handler on unseen (D35).
    #[cfg(target_arch = "aarch64")]
    pub const INTERRUPTIBLE: u32 = ALL_SAFE & !FAST_DISPATCH;
    /// dynarmic's `Unsafe_IgnoreGlobalMonitor`: exclusive loads and stores no longer take the
    /// monitor's process-wide spin lock, and an exclusive store no longer clears every other
    /// processor's matching reservation. What remains is a per-processor reservation (address and
    /// value) and a compare-and-swap of the reserved value on the store (a host `lock cmpxchg` on
    /// x64, an `LDAXR`/`STLXR` loop on arm64). **Honoured only when
    /// [`super::OdConfig::unsafe_optimizations`] is non-zero**, which is dynarmic's own guard. The
    /// x64 backend always honoured it; the arm64 backend's inline exclusives (patch 0007) ignored it
    /// -- lock and scan regardless -- until **patch 0021** (D31 amendment 1).
    pub const UNSAFE_IGNORE_GLOBAL_MONITOR: u32 = 0x0010_0000;
    /// dynarmic's `Unsafe_UnfuseFMA`: a fused multiply-add as a multiply then an add.
    pub const UNSAFE_UNFUSE_FMA: u32 = 0x0001_0000;
    /// dynarmic's `Unsafe_ReducedErrorFP`: the reciprocal (square root) estimates and steps with the
    /// host's own approximations (`rcpss`/`rsqrtss`), not a call into a bit-exact software model.
    pub const UNSAFE_REDUCED_ERROR_FP: u32 = 0x0002_0000;
    /// dynarmic's `Unsafe_InaccurateNaN`: no fix-up of a NaN result to the one the architecture names.
    pub const UNSAFE_INACCURATE_NAN: u32 = 0x0004_0000;
    /// dynarmic's `Unsafe_IgnoreStandardFPCRValue`: vector floating point under the guest's FPCR, not
    /// the standard value (no MXCSR switch around each such instruction).
    pub const UNSAFE_IGNORE_STANDARD_FPCR: u32 = 0x0008_0000;
    /// The four unsafe floating-point flags together (what `od_set_live_fp_optimizations` takes).
    pub const UNSAFE_FP: u32 = UNSAFE_UNFUSE_FMA | UNSAFE_REDUCED_ERROR_FP | UNSAFE_INACCURATE_NAN | UNSAFE_IGNORE_STANDARD_FPCR;
}

/// Where an exclusive monitor keeps its state, from [`od_monitor_layout_of`].
///
/// A diagnostic's view: the emitted exclusive-access code carries these addresses as 64-bit
/// immediates, so a sampled host instruction pointer near such an immediate is monitor work.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OdMonitorLayout {
    /// The spin lock word every exclusive access takes under the global monitor.
    pub lock: u64,
    /// Processor 0's reservation-address slot.
    pub addresses: u64,
    /// Bytes between two processors' address slots.
    pub address_stride: u64,
    /// Processor 0's reserved-value slot.
    pub values: u64,
    /// Bytes between two processors' value slots.
    pub value_stride: u64,
    /// How many processors the monitor was sized for.
    pub processor_count: u64,
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
    /// 1 if dynarmic was *built* with `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT`.
    ///
    /// This echoes the build flag; it does **not** query the page protection.
    /// Querying means `VirtualQuery`, and Global Constraint 4 keeps OS calls in
    /// `omni-platform`. So it answers "was W^X asked for at compile time",
    /// which is one inference away from "are these pages W^X".
    ///
    /// **0 on this pin.** Upstream commits the code cache
    /// `PAGE_EXECUTE_READWRITE`, so the region holding every byte of generated
    /// guest code is writable and executable at once — which D12 says Omnidroid
    /// never does. The upstream switch for it crashes on this pin (see
    /// `build.rs`), so the contradiction stands and is reported here rather
    /// than left silent.
    ///
    /// **[`code_cache::W_XOR_X_PER_THREAD`] on an Apple arm64 host**: the cache is `MAP_JIT` and
    /// every write is bracketed by `pthread_jit_write_protect_np`, so each thread sees it either
    /// writable or executable, never both. That one is measured by `tests/wx.rs`, not echoed.
    pub code_cache_w_xor_x: i32,
    /// Address of the `TPIDR_EL0` slot baked into generated code; 0 means the
    /// guest cannot read it, which D13 says breaks every stack-protected
    /// function in `libroblox.so`.
    pub tpidr_el0_ptr: u64,
    /// Address of the `TPIDRRO_EL0` slot.
    pub tpidrro_el0_ptr: u64,
    /// 1 if `fastmem_pointer` applies below 2^32 only (D41, patch 0030).
    pub fastmem_low_window: i32,
}

/// Bytes of per-jit state this pin allocates **when the `FastDispatch` optimization is enabled**,
/// whatever the code cache size and whatever the guest does -- and nothing when it is not.
///
/// `A64EmitX64`'s fast-dispatch table is `sizeof(FastDispatchEntry) == 0x10` times
/// `fast_dispatch_table_size`, written in full on construction (the entries carry a non-zero
/// initialiser). Upstream's is 0x100000 entries, a flat **16 MiB**, held by value, so every jit
/// paid it whether or not the optimization was on; **patch 0017** (`patches/`, D32) allocates it
/// only when it is (MEASURED on the landing, 45 guest threads: 704 MiB of commit and working set,
/// gone), and **patch 0019** (D35) shrinks it to 0x1000 entries, **64 KiB**, because
/// `optimization::INTERRUPTIBLE` now turns the optimization on in every guest thread.
///
/// It is a constant rather than a call because it is a property of the *pin*, not of a live jit:
/// there is no accessor for it and adding one would mean patching the vendored tree. What keeps it
/// honest is `tests/pin_constants.rs`, which reads the two declarations out of the vendored header
/// and fails if a re-pin moves either.
///
/// This is **not** the whole per-thread cost. The code cache commits incrementally as code is
/// emitted, and its high-water mark is a private member of `BlockOfCode` that `A64::Jit` does not
/// expose; that term is bounded above by [`OdConfig::code_cache_size`] and measured from the
/// outside in `omni-cpu`'s M2 gate.
///
/// **Per host architecture**, because the two backends are different code: the table above is a
/// member of the *x64* emitter. The arm64 backend has no fast-dispatch table at all -- its
/// `FastDispatchHint` terminal is a plain return to the dispatcher with a `TODO` in its place
/// (`emit_arm64_a64.cpp`) -- and nothing else of a fixed large size, so on `aarch64` this is **0**
/// and the per-jit cost is the code cache plus small objects. MEASURED on Apple M1 (n = 8 threads,
/// one measurement, `omni-cpu`'s M2 gate): 8.010 MiB of `phys_footprint` per jit at creation with an
/// 8 MiB code cache. `tests/pin_constants.rs` checks both halves against the vendored source.
#[cfg(target_arch = "x86_64")]
pub const OD_FIXED_PER_JIT_BYTES: usize = 0x10 * 0x1000;

/// See the `x86_64` definition: the arm64 backend holds no fixed-size per-jit table.
#[cfg(target_arch = "aarch64")]
pub const OD_FIXED_PER_JIT_BYTES: usize = 0;

/// [`OdEffectiveConfig::code_cache_w_xor_x`] values, mirroring `OD_CODE_CACHE_*` in the header.
pub mod code_cache {
    /// Writable and executable at once, for every thread: x64's `PAGE_EXECUTE_READWRITE` cache.
    pub const W_AND_X: i32 = 0;
    /// Built with `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT`: pages flipped between RW and RX.
    pub const W_XOR_X: i32 = 1;
    /// Apple arm64: `MAP_JIT` pages, RWX in the VM map, each thread seeing them either RW- or R-X
    /// as `pthread_jit_write_protect_np` last set *for that thread*. W^X holds for any one thread;
    /// a thread with its write window open can write every `MAP_JIT` page while another thread
    /// executes them. Measured, not echoed: `tests/wx.rs`.
    pub const W_XOR_X_PER_THREAD: i32 = 2;
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
    /// identity fastmem this must stay 0; anything else is the silent 30-49x
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
    /// `sizeof(od_code_cache_stats)`.
    pub code_cache_stats_size: u32,
    /// `alignof(od_code_cache_stats)`.
    pub code_cache_stats_align: u32,
    /// `sizeof(od_code_cache_tables)`.
    pub code_cache_tables_size: u32,
    /// `alignof(od_code_cache_tables)`.
    pub code_cache_tables_align: u32,
}

/// What a shared code cache (vendored patch 0022, [`od_code_cache_new`]) has done, from
/// [`od_code_cache_stats_of`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OdCodeCacheStats {
    /// Blocks translated and emitted into the cache, by any jit.
    pub blocks_emitted: u64,
    /// Host code bytes of those blocks.
    pub code_bytes_emitted: u64,
    /// Misses another jit had translated by the time this one held the lock.
    pub translations_raced: u64,
    /// Translations made again under the lock, because an invalidation touched their code while
    /// they were being made outside it.
    pub translations_redone: u64,
    /// Nanoseconds spent translating (the frontend and the IR passes), outside the lock, summed
    /// over the threads that did it.
    pub translate_ns: u64,
    /// Nanoseconds spent emitting host code, holding the lock.
    pub emit_ns: u64,
    /// Dispatcher lookups a jit's own fast-dispatch table could not answer, which took the
    /// cache's lock.
    pub locked_lookups: u64,
    /// Invalidation requests applied.
    pub invalidations: u64,
    /// Blocks those requests dropped.
    pub blocks_invalidated: u64,
    /// Bumped by each request that dropped a block; a jit catches its return-stack buffer and
    /// fast-dispatch table up with it at its next run or dispatcher entry.
    pub generation: u64,
    /// Regions the buffer is cut into after the prelude.
    pub regions_total: u64,
    /// Region retirements so far: the oldest live region evicted to make room, and full regions a
    /// clear emptied.
    pub regions_retired: u64,
    /// Retired regions given back to the OS so far.
    pub regions_reclaimed: u64,
    /// Regions retired and not given back yet.
    pub regions_pinned: u64,
    /// Threads parked in an `SVC` callback whose resume address was moved out of a retiring
    /// region (to a stub that leaves the run), so the region could be given back.
    pub parked_redirected: u64,
    /// Passes over the retired regions looking for ones to give back.
    pub reclaim_attempts: u64,
    /// Bytes committed now (Windows); where pages come on first touch, what was made available.
    pub committed_bytes: u64,
    /// Jits attached now.
    pub attached: u64,
    /// Patch 0028 (ABI 4): the oldest live region retired to make room for another.
    pub regions_evicted: u64,
    /// Blocks those evictions forgot.
    pub blocks_evicted: u64,
    /// Blocks emitted again at a location the latest eviction forgot: how much of what it forgot
    /// was still in use.
    pub blocks_reemitted: u64,
    /// Nanoseconds spent evicting, holding the cache's lock.
    pub evict_ns: u64,
    /// The longest single eviction, in nanoseconds.
    pub evict_max_ns: u64,
    /// Regions whose blocks are live now, the one being filled included.
    pub regions_live: u64,
    /// How many may be (`live_bytes` in regions).
    pub regions_live_max: u64,
    /// Patch 0070 (ABI 7): blocks installed from a translation snapshot, unverified.
    pub snapshot_blocks_restored: u64,
    /// Of those, found unchanged at their first lookup, and entered.
    pub snapshot_blocks_verified: u64,
    /// Of those, whose guest code had changed: dropped and translated again.
    pub snapshot_blocks_rejected: u64,
    /// ABI 8: the latest save's time holding the cache's lock, copying the snapshot out (guest
    /// threads that need the cache wait for it); the file is written after.
    pub snapshot_save_lock_ns: u64,
    /// ABI 9 (patch 0075): pages of a lazily loaded snapshot read in, as blocks on them were entered.
    pub snapshot_pages_read: u64,
}

/// What one of a shared code cache's per-block tables holds on the C heap (vendored patch 0024),
/// from [`od_code_cache_tables_of`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OdCodeCacheTable {
    /// What it holds.
    pub entries: u64,
    /// Its arrays at their capacity, and its entries' own allocations.
    pub bytes: u64,
    /// An address inside its largest single allocation, or 0 when it names none -- so a report can
    /// say whose a large heap allocation is.
    pub largest_address: u64,
    /// That allocation's size.
    pub largest_bytes: u64,
}

/// A shared code cache's per-block tables (vendored patch 0024), from [`od_code_cache_tables_of`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OdCodeCacheTables {
    /// Location -> translated block: the dispatcher's map.
    pub blocks: OdCodeCacheTable,
    /// Link target -> the link slots that jump to it.
    pub link_targets: OdCodeCacheTable,
    /// Each block's own link slots.
    pub links: OdCodeCacheTable,
    /// Host fault site -> fallback, one per fastmem access.
    pub fastmem_sites: OdCodeCacheTable,
    /// The guest bytes each block was translated from, for invalidation.
    pub guest_ranges: OdCodeCacheTable,
}

impl OdCodeCacheTables {
    /// Each table with its name, in the order the header declares them.
    #[must_use]
    pub fn named(&self) -> [(&'static str, OdCodeCacheTable); 5] {
        [
            ("block map", self.blocks),
            ("link targets", self.link_targets),
            ("block links", self.links),
            ("fastmem sites", self.fastmem_sites),
            ("guest ranges", self.guest_ranges),
        ]
    }
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

    /// Fill `out` with where `monitor` keeps its lock and slots. A null `monitor` leaves `out`
    /// zeroed.
    ///
    /// # Safety
    /// `monitor` must be null or come from [`od_monitor_new`] and not have been freed; `out` must
    /// be writable.
    pub fn od_monitor_layout_of(monitor: *mut c_void, out: *mut OdMonitorLayout);

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
    /// `jit` must come from [`od_jit_new`] or [`od_jit_new_shared`], must not be executing, and
    /// must not be freed twice.
    pub fn od_jit_free(jit: *mut c_void);

    /// Create a code cache every jit of one guest address space can share (vendored patch 0022,
    /// `docs/research/shared-jit-cache.md`). `template_config` is a config as a jit of the space
    /// would pass it: every field that shapes emitted code is taken from it. `total_bytes` is
    /// address space (8 MiB..2 GiB) committed as code is emitted, cut into regions of
    /// `region_bytes` (at least 8 MiB, at least two) filled one at a time. At most `live_bytes` of
    /// regions keep their blocks (0: all but one; at least one region): past that the oldest region
    /// is retired, its blocks alone forgotten, and given back (vendored patch 0028). Null on an
    /// arm64 host, where the backend has no shared cache, and for a refused configuration.
    ///
    /// # Safety
    /// `template_config` must be a valid `OdConfig`; its `callbacks` pointer is read during the
    /// call. The cache must be freed with [`od_code_cache_free`] after every jit attached to it.
    pub fn od_code_cache_new(template_config: *const OdConfig, total_bytes: u64, region_bytes: u64, live_bytes: u64) -> *mut c_void;

    /// Free a cache from [`od_code_cache_new`]. Null is accepted.
    ///
    /// # Safety
    /// No jit attached to it may still exist.
    pub fn od_code_cache_free(cache: *mut c_void);

    /// As [`od_jit_new`], but translating into and running from `cache`. Null if `config` shapes
    /// code differently from the cache's template, or for anything `od_jit_new` refuses.
    /// `config.code_cache_size` is ignored, and recompiling on a fastmem failure is off.
    ///
    /// # Safety
    /// As [`od_jit_new`]; `cache` must come from [`od_code_cache_new`] and outlive the jit.
    pub fn od_jit_new_shared(config: *const OdConfig, cache: *mut c_void) -> *mut c_void;

    /// The shared cache `jit` runs from, or null for a jit with its own.
    ///
    /// # Safety
    /// `jit` must be live.
    pub fn od_jit_code_cache(jit: *mut c_void) -> *mut c_void;

    /// Read a shared cache's counters.
    ///
    /// # Safety
    /// `cache` must be live (or null, which zeroes `out`); `out` must be writable.
    pub fn od_code_cache_stats_of(cache: *mut c_void, out: *mut OdCodeCacheStats);

    /// Patch 0070: hash the guest code of every block emitted from now on, for a snapshot.
    ///
    /// # Safety
    /// `cache` is null or a live cache.
    pub fn od_code_cache_enable_snapshots(cache: *mut c_void);

    /// Patch 0070: write a translation snapshot of `cache` to `path` (UTF-8, NUL-terminated),
    /// tagged `key`. The blocks written, or negative (see the header).
    ///
    /// # Safety
    /// `cache` is null or a live cache; `path` and `key` are NUL-terminated. Not from inside a
    /// callback of a jit on the cache.
    pub fn od_code_cache_save_snapshot(cache: *mut c_void, path: *const core::ffi::c_char, key: *const core::ffi::c_char, max_bytes: u64, flags: u32) -> i64;

    /// Patch 0070: install the translation snapshot at `path` into `cache`, which must have emitted
    /// nothing. The blocks installed, or negative (see the header).
    ///
    /// # Safety
    /// As [`od_code_cache_save_snapshot`].
    pub fn od_code_cache_load_snapshot(cache: *mut c_void, path: *const core::ffi::c_char, key: *const core::ffi::c_char, flags: u32) -> i64;

    /// What the cache's per-block tables hold (a census: takes the cache's lock, shared, and walks
    /// what cannot be sized in O(1) -- for a report made every few minutes, not a hot path). All
    /// zero on an arm64 host.
    ///
    /// # Safety
    /// `cache` must be live (or null, which zeroes `out`); `out` must be writable.
    pub fn od_code_cache_tables_of(cache: *mut c_void, out: *mut OdCodeCacheTables);

    /// Patch 0036: for each of `count` ascending host code addresses at `hosts`, the guest PC of
    /// the cache's block whose translated code holds it, or `u64::MAX` where none does (prelude,
    /// far code, link slot, forgotten block). Takes the cache's lock, shared, and walks every block
    /// once: for a profiler's report thread only. All `u64::MAX` on an arm64 host.
    ///
    /// # Safety
    /// `cache` must be live (or null); `hosts` must hold `count` readable `u64`s, ascending, and
    /// `guest_pcs` `count` writable ones.
    pub fn od_code_cache_guest_pcs_of(cache: *mut c_void, hosts: *const u64, count: u64, guest_pcs: *mut u64);

    /// Invalidate `[addr, addr + len)` for every jit of the cache, now. Clamped as
    /// [`od_jit_invalidate_range`] is.
    ///
    /// # Safety
    /// `cache` must be live, and this must not be called from inside a callback of a jit attached
    /// to it (use [`od_jit_invalidate_range`] there).
    pub fn od_code_cache_invalidate_range(cache: *mut c_void, addr: u64, len: u64);

    /// Drop every translation of the cache, now. Same restriction.
    ///
    /// # Safety
    /// As [`od_code_cache_invalidate_range`].
    pub fn od_code_cache_clear(cache: *mut c_void);

    /// Vendored patch 0050: retire the oldest full regions until at most `keep_bytes` of regions
    /// are live (at least the one being filled stays); how many were retired. Their blocks are
    /// translated again if they run again.
    ///
    /// # Safety
    /// As [`od_code_cache_invalidate_range`].
    pub fn od_code_cache_evict_to(cache: *mut c_void, keep_bytes: u64) -> u64;

    /// Where a jit's `JitState` keeps the two exclusive-monitor slot pointers that code in a shared
    /// cache loads (`mov r64, [r15 + offset]`) where a jit with its own cache has the slot
    /// addresses as immediates -- for a sampler that recognises monitor code by what it reads.
    /// `(0, 0)` on an arm64 host.
    ///
    /// # Safety
    /// Both pointers must be writable.
    pub fn od_shared_monitor_slot_offsets(address_offset: *mut u32, value_offset: *mut u32);

    /// Run guest code until halted. Returns a bitwise-or of `OD_HALT_*`,
    /// [`OD_HALT_SHIM_REENTERED`] if called from inside a callback, or
    /// [`OD_HALT_SHIM_THREW`] if dynarmic threw.
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

    /// Just [`OdStats::slow_path_total`], as one load rather than a 72-byte struct copy.
    ///
    /// Omnidroid reads this around **every** run slice, not only at the end of a benchmark:
    /// the startup assertion defends the *configuration*, and Task 3 found two ways the memory
    /// path degrades at **runtime**, after that assertion has passed, leaving the runtime 30-49x
    /// slower with correct results and no functional symptom at all. Checking the delta per slice
    /// is the class-level answer, and it is only affordable if reading the counter is a load.
    ///
    /// # Safety
    /// `jit` must be live, and this must be called from the thread that owns it — the counter is
    /// non-atomic and that thread is the only writer.
    pub fn od_jit_slow_path_total(jit: *mut c_void) -> u64;

    /// arm64 hosts only: the host return address the most recent `SVC` callback was entered with
    /// -- an address inside this jit's code cache, 0 before the first `SVC`. It exists so the code
    /// cache's page protection can be measured (`tests/wx.rs`); nothing else locates the cache.
    ///
    /// # Safety
    /// `jit` must be live; read on the jit's own thread.
    #[cfg(target_arch = "aarch64")]
    pub fn od_jit_last_svc_return_address(jit: *mut c_void) -> u64;

    /// Patch 0034 (x64 only): the unsafe floating-point flags ([`optimization::UNSAFE_FP`]'s bits;
    /// others are dropped) every jit of this process emits with from now on, where its config's
    /// unsafe gate is open. Blocks already emitted keep theirs -- clear the cache to have them
    /// again. Returns the mask in force; 0 and a no-op on arm64.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it stores one process-wide atomic.
    pub fn od_set_live_fp_optimizations(mask: u32) -> u32;

    /// Patch 0037 (x64 only): non-zero runs `GetSetElimination`, in a form precise at every guest
    /// data access, on every block translated from now on by a jit whose config sets
    /// `check_halt_on_memory_access` -- which otherwise skips the pass, so that every guest
    /// register read is a load from `JitState` and every write a store. Blocks already translated
    /// keep what they were translated with -- clear the cache to have them again. Returns the value
    /// in force (1 or 0); 0 and a no-op on arm64.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it stores one process-wide atomic.
    pub fn od_set_precise_get_set(on: u32) -> u32;

    /// Patch 0037: the switch [`od_set_precise_get_set`] sets (1 or 0; 0 on arm64).
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it loads one process-wide atomic.
    pub fn od_precise_get_set() -> u32;

    /// Patch 0039 (x64 only): non-zero keeps element 0 of a vector -- every scalar floating-point
    /// operand of the A64 frontend -- in an XMM register (zeroed above it: the same value) in
    /// blocks emitted from now on, instead of a `movq gpr, xmm` that the SSE consumer moves
    /// straight back. Clear the cache to have every block again. Returns the value in force (1 or
    /// 0); 0 and a no-op on arm64.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it stores one process-wide atomic.
    pub fn od_set_scalar_fp_in_xmm(on: u32) -> u32;

    /// Patch 0039: the switch [`od_set_scalar_fp_in_xmm`] sets (1 or 0; 0 on arm64).
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it loads one process-wide atomic.
    pub fn od_scalar_fp_in_xmm() -> u32;

    /// Patch 0040 (x64 only): non-zero masks a mirrored fastmem address wider than 32 bits (Top
    /// Byte Ignore's 56) with one `and` against a pool constant instead of a `shl`/`shr` pair, in
    /// blocks emitted from now on: the same address, one cycle less on its path. Returns the value
    /// in force (1 or 0); 0 and a no-op on arm64.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it stores one process-wide atomic.
    pub fn od_set_fastmem_mask_by_and(on: u32) -> u32;

    /// Patch 0040 (x64 only): non-zero leaves Top Byte Ignore's mask (56-bit mirrored fastmem)
    /// off in blocks emitted from now on, as a 64-bit configuration has it: a tagged address is
    /// non-canonical, faults, and the fastmem handler sends it to the callback path, which clears
    /// the tag. Returns the value in force (1 or 0); 0 and a no-op on arm64.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it stores one process-wide atomic.
    pub fn od_set_tbi_unmasked(on: u32) -> u32;

    /// Patch 0041 (x64 only): guest instructions (by location) that met a tagged address while
    /// Top Byte Ignore's mask was off, and are emitted masked since, in this process. 0 on arm64.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it loads one process-wide atomic.
    pub fn od_tbi_sites_noted() -> u64;

    /// Patch 0060 (x64 only): a census of the emitted code, process-wide -- up to `n` counters into
    /// `out`, in [`CODEGEN_PARTS`] order. Returns how many counters there are (0 on arm64).
    ///
    /// # Safety
    /// `out` is valid for `n` writes.
    pub fn od_codegen_census(out: *mut u64, n: u32) -> u32;

    /// Patch 0061 (x64 only): bits that emit smaller code in blocks emitted from now on:
    /// [`OD_COMPACT_FAULT_STUBS`], each fastmem site's slow path calls one shared memory-abort check
    /// (its PC as data) instead of carrying the check inline; [`OD_COMPACT_LINK_TAILS`], a shared
    /// cache's link leaves through its slot's own tail. The same behaviour. Returns the bits in
    /// force; 0 and a no-op on arm64.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it stores one process-wide atomic.
    pub fn od_set_compact_code(on: u32) -> u32;

    /// Patch 0060: zero the census.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call.
    pub fn od_codegen_census_reset();

    /// Patch 0062 (x64 only): `observer(ctx, guest_pc, host, code_bytes, total_bytes)` for every
    /// block emitted from now on, on the emitting thread; null stops it. `host` is the block's
    /// entry, `code_bytes` its code up to its link slots, `total_bytes` with them. For
    /// differential tests of the emitter. A no-op on arm64.
    ///
    /// # Safety
    /// `observer` must be safe to call with `ctx` from any thread that emits, until it is replaced.
    pub fn od_set_emit_observer(observer: Option<OdEmitObserver>, ctx: *mut c_void);

    /// Patch 0042 (x64 only): non-zero emits the return-stack buffer's and the fast-dispatch
    /// table's hit paths inside each `RET`/`BR`/`BLR` block -- from the target PC still in a
    /// register, with an indirect jump of the site's own -- instead of a jump to one shared handler
    /// that reloads the PC from `JitState`, in blocks emitted from now on. Returns the value in
    /// force (1 or 0); 0 and a no-op on arm64.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it stores one process-wide atomic.
    pub fn od_set_fast_dispatch_inline(on: u32) -> u32;

    /// Vendored patch 0066 (x64): non-zero rehashes a shared cache's block map, link heads and
    /// guest-range page index down to what they hold once an eviction or an invalidation has
    /// forgotten blocks, when that is at most half their bucket arrays (a robin_map never shrinks by
    /// itself). Process-wide, read at each eviction or invalidation. Returns the value in force (1
    /// or 0); 0 and a no-op on arm64.
    ///
    /// # Safety
    /// None beyond an ordinary FFI call: it stores one process-wide atomic.
    pub fn od_set_shrink_tables(on: u32) -> u32;
}

/// Patch 0075: [`od_code_cache_load_snapshot`]'s flag reading the code a page at a time, as entered.
pub const OD_SNAPSHOT_LOAD_LAZY: u32 = 1;

/// Patch 0070: [`od_code_cache_save_snapshot`]'s flag leaving out the blocks restored and never
/// entered since.
pub const OD_SNAPSHOT_ENTERED_ONLY: u32 = 1;

/// Patch 0062: see [`od_set_emit_observer`].
pub type OdEmitObserver = unsafe extern "C" fn(ctx: *mut c_void, guest_pc: u64, host: *const c_void, code_bytes: usize, total_bytes: usize);

/// Patch 0061: [`od_set_compact_code`]'s bit for the shared memory-abort check behind each fastmem
/// site (the bulk of the saving: out-of-line code 44 -> 18 bytes a memory access on `libc.so`).
pub const OD_COMPACT_FAULT_STUBS: u32 = 1;
/// Patch 0061: [`od_set_compact_code`]'s bit for shared-cache links that leave through their slot's
/// tail (~20 bytes a block; measured 9-18% slower on tight loops of tiny linked blocks).
pub const OD_COMPACT_LINK_TAILS: u32 = 2;

/// Patch 0060: the names of [`od_codegen_census`]'s counters, in order. The first ten are bytes,
/// the last five counts.
pub const CODEGEN_PARTS: [&str; 15] = [
    "align", "memory", "getset", "flags", "setpc", "other", "cycles", "terminal", "far", "slots",
    "blocks", "ir_insts", "guest_insts", "memory_ops", "deferred",
];
