/* Omnidroid: the C ABI over dynarmic's A64 JIT.
 *
 * dynarmic is C++ with virtual-callback interfaces, which do not bind to Rust.
 * This header is the entire boundary: a flat `extern "C"` surface carrying only
 * what `GuestCpu` (omni-cpu) needs. Everything here is ABI-stable by hand --
 * plain integers, raw pointers and function pointers, no C++ types.
 *
 * Copyright (c) 2026 Omnidroid contributors. Licensed MIT OR Apache-2.0.
 * dynarmic itself is ISC/0BSD; see ../LICENSES.md.
 */

#ifndef OD_DYNARMIC_H
#define OD_DYNARMIC_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Bumped whenever anything below changes shape. `od_dynarmic_abi_version()` is
 * compiled into the C++ side; the Rust side compares against its own copy so a
 * stale object file is a clean error rather than silent memory corruption. */
#define OD_DYNARMIC_ABI_VERSION 1u

/* ---------------------------------------------------------------------------
 * Callbacks: the host side of the boundary.
 *
 * Every one of these can be entered from *generated guest code*. The
 * re-entrancy contract is stated once, here, and holds for all of them:
 *
 *   1. `ctx` is whatever was passed in `od_config::ctx`. It is never
 *      dereferenced by C++; it is passed through untouched.
 *   2. A callback is never invoked recursively by dynarmic: no callback runs
 *      guest code, so callback depth is always exactly 1.
 *   3. A callback MUST NOT unwind. C++ frames and JIT-generated frames sit
 *      between the callback and `od_jit_run`, and neither is unwind-safe.
 *      Rust implementations must catch panics and turn them into
 *      `od_jit_halt`.
 *   4. `od_jit_halt`, `od_jit_clear_cache` and `od_jit_invalidate_range` are
 *      the only entry points that may be called from inside a callback.
 *      Calling `od_jit_run` or `od_jit_step` re-entrantly is refused by the
 *      shim (see `OD_HALT_SHIM_REENTERED`) rather than tripping dynarmic's
 *      `ASSERT(!is_executing)`, which would abort the process.
 * ------------------------------------------------------------------------ */
typedef struct od_callbacks {
    /* Fetch the instruction word at `vaddr` (always 4-byte aligned).
     * Return 1 having stored the word, or 0 to make dynarmic raise
     * `OD_EXCEPTION_NO_EXECUTE_FAULT` -- which is how a guest jump into
     * unmapped memory becomes a typed exit instead of a host crash. */
    int (*read_code)(void* ctx, uint64_t vaddr, uint32_t* out);

    /* Data reads. Only reached when fastmem is off or has faulted; with
     * identity fastmem (D4) the hot path never enters these. */
    uint8_t (*read8)(void* ctx, uint64_t vaddr);
    uint16_t (*read16)(void* ctx, uint64_t vaddr);
    uint32_t (*read32)(void* ctx, uint64_t vaddr);
    uint64_t (*read64)(void* ctx, uint64_t vaddr);
    void (*read128)(void* ctx, uint64_t vaddr, uint64_t out[2]);

    void (*write8)(void* ctx, uint64_t vaddr, uint8_t value);
    void (*write16)(void* ctx, uint64_t vaddr, uint16_t value);
    void (*write32)(void* ctx, uint64_t vaddr, uint32_t value);
    void (*write64)(void* ctx, uint64_t vaddr, uint64_t value);
    void (*write128)(void* ctx, uint64_t vaddr, const uint64_t value[2]);

    /* Store-exclusive (`STXR`/`STLXR`) compare-and-swap. Return 1 on success,
     * 0 on failure. All five widths are present deliberately: dynarmic's
     * defaults return false, and a `STXR` that can never succeed turns the
     * guest's standard retry loop into an infinite loop. */
    int (*write_exclusive8)(void* ctx, uint64_t vaddr, uint8_t value, uint8_t expected);
    int (*write_exclusive16)(void* ctx, uint64_t vaddr, uint16_t value, uint16_t expected);
    int (*write_exclusive32)(void* ctx, uint64_t vaddr, uint32_t value, uint32_t expected);
    int (*write_exclusive64)(void* ctx, uint64_t vaddr, uint64_t value, uint64_t expected);
    int (*write_exclusive128)(void* ctx, uint64_t vaddr, const uint64_t value[2], const uint64_t expected[2]);

    /* 231 of dynarmic's 874 A64 decoder entries are unimplemented (D5) and
     * surface here. The host must execute exactly `num_insns` instructions
     * starting at `pc`, or halt. */
    void (*interpreter_fallback)(void* ctx, uint64_t pc, uint64_t num_insns);

    /* `SVC #imm`. This is the guest asking bionic/the kernel for something. */
    void (*call_svc)(void* ctx, uint32_t swi);

    /* Undefined encodings, reserved values, hint instructions and
     * `NoExecuteFault`. `kind` is an `OD_EXCEPTION_*` value. */
    void (*exception_raised)(void* ctx, uint64_t pc, uint32_t kind);

    /* `IC IVAU` / `IC IALLU` / `IC IALLUIS`: the guest telling us it wrote
     * code. This is how translated code gets invalidated correctly; without
     * it, a guest that patches itself executes stale translations. */
    void (*instruction_cache_op)(void* ctx, uint32_t op, uint64_t vaddr);

    /* `CNTPCT_EL0`. Only called when `wall_clock_cntpct` is 0. */
    uint64_t (*get_cntpct)(void* ctx);

    /* Cycle counting. Only called when `enable_cycle_counting` is 1; that flag
     * exists so a step budget ("exhausted a step budget" in `GuestCpu`) can be
     * enforced without `Step()`-ing one instruction at a time. */
    void (*add_ticks)(void* ctx, uint64_t ticks);
    uint64_t (*get_ticks_remaining)(void* ctx);
} od_callbacks;

/* `od_callbacks::exception_raised` `kind` values. These mirror
 * `Dynarmic::A64::Exception` and are checked against it by static_assert in
 * the shim, so a version bump that reorders them fails to compile. */
#define OD_EXCEPTION_UNALLOCATED_ENCODING 0u
#define OD_EXCEPTION_RESERVED_VALUE 1u
#define OD_EXCEPTION_UNPREDICTABLE_INSTRUCTION 2u
#define OD_EXCEPTION_WAIT_FOR_INTERRUPT 3u
#define OD_EXCEPTION_WAIT_FOR_EVENT 4u
#define OD_EXCEPTION_SEND_EVENT 5u
#define OD_EXCEPTION_SEND_EVENT_LOCAL 6u
#define OD_EXCEPTION_YIELD 7u
#define OD_EXCEPTION_BREAKPOINT 8u
#define OD_EXCEPTION_NO_EXECUTE_FAULT 9u

/* `HaltReason` bits returned by `od_jit_run`/`od_jit_step`. The low bits
 * mirror `Dynarmic::HaltReason`; the top one is ours. */
#define OD_HALT_STEP 0x00000001u
#define OD_HALT_CACHE_INVALIDATION 0x00000002u
#define OD_HALT_MEMORY_ABORT 0x00000004u
#define OD_HALT_USER1 0x01000000u
#define OD_HALT_USER2 0x02000000u
#define OD_HALT_USER3 0x04000000u
#define OD_HALT_USER4 0x08000000u
#define OD_HALT_USER5 0x10000000u
#define OD_HALT_USER6 0x20000000u
#define OD_HALT_USER7 0x40000000u
#define OD_HALT_USER8 0x80000000u

/* `od_config::optimizations`, mirroring `Dynarmic::OptimizationFlag`. */
#define OD_OPT_BLOCK_LINKING 0x00000001u
#define OD_OPT_RETURN_STACK_BUFFER 0x00000002u
#define OD_OPT_FAST_DISPATCH 0x00000004u
#define OD_OPT_GET_SET_ELIMINATION 0x00000008u
#define OD_OPT_CONST_PROP 0x00000010u
#define OD_OPT_MISC_IR_OPT 0x00000020u
#define OD_OPT_NONE 0x00000000u
#define OD_OPT_ALL_SAFE 0x0000FFFFu

/* Not a dynarmic halt reason. Returned by `od_jit_run`/`od_jit_step` when they
 * are called while this jit is already executing -- i.e. from inside a
 * callback. dynarmic would `ASSERT(!is_executing)` and abort the process;
 * Global Constraint 11 makes an abort reachable from guest code Critical, so
 * the shim refuses instead. Deliberately distinct from every HaltReason bit. */
#define OD_HALT_SHIM_REENTERED 0x00800000u

/* Also not a dynarmic halt reason. Returned when a C++ exception escaped
 * `Jit::Run`/`Jit::Step` and was caught here. xbyak throws `Xbyak::Error` when
 * the code cache runs out of room, and translation happens inside `Run`, so
 * this is reachable -- and an exception unwinding into Rust is undefined
 * behaviour, so it has to stop at the boundary. The jit is in an
 * uncharacterised state afterwards; free it. */
#define OD_HALT_SHIM_THREW 0x00400000u

/* Configuration handed to `od_jit_new`. Copied on entry; the struct itself
 * need not outlive the call. The pointers in it must outlive the jit. */
typedef struct od_config {
    /* Must be `OD_DYNARMIC_ABI_VERSION`. Rejected otherwise. */
    uint32_t abi_version;

    /* Borrowed for the jit's whole life. `callbacks` is copied; `ctx` is not
     * touched at all. */
    const od_callbacks* callbacks;
    void* ctx;

    /* D13: `TPIDR_EL0` must be a valid bionic TLS block before any guest code
     * runs. dynarmic bakes these pointers into generated code and reads
     * through them directly -- no callback -- so the pointee must stay put and
     * stay valid for the jit's whole life. Null means the guest read faults
     * into `exception_raised`. */
    uint64_t* tpidr_el0;
    const uint64_t* tpidrro_el0;

    /* D4: identity mapping. `fastmem_enabled` 0 routes every guest access
     * through the callbacks above -- measured 30-49x slower. When 1,
     * `fastmem_pointer` is the host base (0 for identity) and
     * `fastmem_address_space_bits` must be 64 for a full-width guest address
     * space. dynarmic's own default is 36, which silently degrades a high
     * guest VA onto the callback path while still producing correct results.
     * `od_jit_effective_config` exists so that can be asserted, not assumed. */
    int fastmem_enabled;
    uint64_t fastmem_pointer;
    uint32_t fastmem_address_space_bits;
    int silently_mirror_fastmem;
    int recompile_on_fastmem_failure;
    int fastmem_exclusive_access;

    /* Exclusive monitor shared between guest threads, from `od_monitor_new`,
     * or null for a jit that never executes `LDXR`/`STXR`. */
    void* monitor;
    uint32_t processor_id;

    /* 0 selects dynarmic's default (128 MiB). D5 measured 20-35 MiB committed
     * per thread, so this is a live memory-budget knob, not a formality.
     * A non-zero value outside dynarmic's documented 8 MiB..2 GiB range (8 MiB
     * ..128 MiB on an arm64 host) makes `od_jit_new` return null; dynarmic
     * itself would assert, and an assert terminates the process. */
    uint64_t code_cache_size;

    uint32_t cntfrq_el0;
    uint32_t ctr_el0;  /* 0 -> dynarmic default 0x8444c004 */
    uint32_t dczid_el0;

    int enable_cycle_counting;
    int wall_clock_cntpct;
    int hook_hint_instructions;
    int define_unpredictable_behaviour;
    int check_halt_on_memory_access;
    int unsafe_optimizations;
    /* dynarmic's `OptimizationFlag` bitmask, used verbatim. `OD_OPT_ALL_SAFE`
     * is the normal value; 0 turns everything off, which is how a
     * miscompilation gets bisected.
     *
     * This is exposed rather than hard-coded because two of these flags decide
     * whether guest code can wedge the host thread. `OD_OPT_RETURN_STACK_BUFFER`
     * and `OD_OPT_FAST_DISPATCH` emit terminal handlers that jump straight from
     * one translated block to the next, checking neither the cycle counter nor
     * the halt flag. A guest `BR`/`RET` loop that stays in either cache
     * therefore ignores a step budget *and* ignores `od_jit_halt` from another
     * thread; see the tests. Clearing both restores both escapes. */
    uint32_t optimizations;
} od_config;

/* What dynarmic actually ended up configured with. Read back from the live
 * `UserConfig` rather than echoed from `od_config`, so it catches dynarmic
 * silently substituting a default. */
typedef struct od_effective_config {
    int fastmem_enabled;
    uint64_t fastmem_pointer;
    uint64_t fastmem_address_space_bits;
    int silently_mirror_fastmem;
    int recompile_on_fastmem_failure;
    int fastmem_exclusive_access;
    int page_table_present;
    uint64_t code_cache_size;
    int enable_cycle_counting;
    int hook_hint_instructions;
    uint32_t optimizations;
    uint32_t unsafe_optimizations;
    /* 1 if dynarmic was *built* with `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT`.
     *
     * This echoes the build flag. It does **not** query the page protection:
     * doing that means `VirtualQuery`, and Global Constraint 4 keeps OS calls
     * in `omni-platform`. So it answers "was W^X asked for at compile time",
     * which is one inference away from "are these pages W^X" -- the inference
     * being that the flag does what it says.
     *
     * **0 on this pin.** Upstream's default commits the code cache
     * `PAGE_EXECUTE_READWRITE` (`block_of_code.cpp:280`), so the region holding
     * every byte of generated guest code is writable and executable at once,
     * which is what D12 says Omnidroid never does. The upstream switch for it
     * crashes; see `build.rs`. Reported rather than assumed so the
     * contradiction is a checked fact and flips loudly when it changes. */
    int code_cache_w_xor_x;
    uint64_t tpidr_el0_ptr;
    uint64_t tpidrro_el0_ptr;
} od_effective_config;

/* Callback-entry counters. Plain `uint64_t`, incremented on the thread that
 * owns the jit, so they cost one non-atomic increment. Task 3 asserts
 * `slow_path_total == 0` for a memory-heavy loop under identity fastmem; that
 * assertion is the only defence against a silent 30-49x regression, and it
 * cannot be written unless the count is reachable. */
typedef struct od_stats {
    uint64_t read_code;
    uint64_t slow_path_reads;
    uint64_t slow_path_writes;
    uint64_t slow_path_exclusive;
    uint64_t interpreter_fallbacks;
    uint64_t svc_calls;
    uint64_t exceptions;
    uint64_t icache_ops;
    /* reads + writes + exclusives: the single number Task 3's assertion wants. */
    uint64_t slow_path_total;
} od_stats;

/* ------------------------------------------------------------------ */

/* `sizeof`/`alignof` of every struct shared across the boundary, as C++ sees
 * them. The Rust side hand-writes its `#[repr(C)]` mirrors, and a mirror that
 * disagrees is silent memory corruption rather than a link error, so the sizes
 * are exported and compared by a test. */
typedef struct od_abi_layout {
    uint32_t callbacks_size;
    uint32_t callbacks_align;
    uint32_t config_size;
    uint32_t config_align;
    uint32_t effective_config_size;
    uint32_t effective_config_align;
    uint32_t stats_size;
    uint32_t stats_align;
} od_abi_layout;

uint32_t od_dynarmic_abi_version(void);
void od_dynarmic_abi_layout(od_abi_layout* out);

/* Shared exclusive monitor. One per address space, sized to the maximum
 * number of guest threads that will use it; `processor_id` in `od_config`
 * indexes into it and must be unique and less than `processor_count`. */
void* od_monitor_new(uint64_t processor_count);
void od_monitor_free(void* monitor);

/* Returns null if the config is rejected or dynarmic throws (it allocates the
 * code cache in the constructor, which is where an over-large
 * `code_cache_size` fails). Never throws, never aborts. */
void* od_jit_new(const od_config* config);
void od_jit_free(void* jit);

/* Run until halted. Returns a bitwise-or of `OD_HALT_*`. */
uint32_t od_jit_run(void* jit);
/* Execute a single guest instruction. */
uint32_t od_jit_step(void* jit);

/* Safe from any thread and from inside a callback. */
void od_jit_halt(void* jit, uint32_t reason);
void od_jit_clear_halt(void* jit, uint32_t reason);
/* 1 while `od_jit_run`/`od_jit_step` is on the stack for this jit. */
int od_jit_is_executing(void* jit);

/* Register access. `index` is bounds-checked here rather than in dynarmic,
 * which indexes an array unchecked. Out-of-range reads return 0 and writes are
 * dropped. X0-X30 are 0-30; X31 does not exist (use SP). */
uint64_t od_jit_get_reg(void* jit, uint32_t index);
void od_jit_set_reg(void* jit, uint32_t index, uint64_t value);
uint64_t od_jit_get_sp(void* jit);
void od_jit_set_sp(void* jit, uint64_t value);
uint64_t od_jit_get_pc(void* jit);
void od_jit_set_pc(void* jit, uint64_t value);
/* V0-V31, little-endian: out[0] is bits 63:0. */
void od_jit_get_vec(void* jit, uint32_t index, uint64_t out[2]);
void od_jit_set_vec(void* jit, uint32_t index, const uint64_t value[2]);
/* NZCV lives in bits 31:28. */
uint32_t od_jit_get_pstate(void* jit);
void od_jit_set_pstate(void* jit, uint32_t value);
uint32_t od_jit_get_fpcr(void* jit);
void od_jit_set_fpcr(void* jit, uint32_t value);
uint32_t od_jit_get_fpsr(void* jit);
void od_jit_set_fpsr(void* jit, uint32_t value);

/* Discard translations covering [addr, addr+len).
 *
 * `len == 0` is a no-op, and an `addr + len` overflow is clamped to the end of
 * the address space. dynarmic builds a closed interval from `addr + len - 1`
 * without checking that the upper bound is above the lower one: at `len == 0`
 * that halts the guest to invalidate an empty range, and on overflow it
 * silently invalidates nothing at all, leaving the guest running stale
 * translations of code it just told us it changed. */
void od_jit_invalidate_range(void* jit, uint64_t addr, uint64_t len);
void od_jit_clear_cache(void* jit);
void od_jit_clear_exclusive(void* jit);

void od_jit_effective_config(void* jit, od_effective_config* out);
void od_jit_stats(void* jit, od_stats* out);
void od_jit_reset_stats(void* jit);

#ifdef __cplusplus
}  /* extern "C" */
#endif

#endif /* OD_DYNARMIC_H */
