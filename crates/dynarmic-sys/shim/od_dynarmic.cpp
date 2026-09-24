/* Omnidroid: the C ABI over dynarmic's A64 JIT. See od_dynarmic.h for the
 * surface and the re-entrancy contract.
 *
 * Copyright (c) 2026 Omnidroid contributors. Licensed MIT OR Apache-2.0.
 */

#include "od_dynarmic.h"

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <limits>
#include <new>
#include <optional>

#include "dynarmic/interface/A64/a64.h"
#include "dynarmic/interface/A64/config.h"
#include "dynarmic/interface/exclusive_monitor.h"
#include "dynarmic/interface/halt_reason.h"
#include "dynarmic/interface/optimization_flags.h"
#if defined(__aarch64__)
#    include "dynarmic/backend/arm64/page_backed_allocator.h"
#endif

namespace {

using Dynarmic::ExclusiveMonitor;
using Dynarmic::HaltReason;
namespace A64 = Dynarmic::A64;

using u8 = std::uint8_t;
using u16 = std::uint16_t;
using u32 = std::uint32_t;
using u64 = std::uint64_t;

/* The `kind` values in od_dynarmic.h are a hand-written copy of
 * `Dynarmic::A64::Exception`. If a future pin reorders that enum, these fail to
 * compile rather than silently renaming every exception the guest can raise. */
static_assert(static_cast<u32>(A64::Exception::UnallocatedEncoding) == OD_EXCEPTION_UNALLOCATED_ENCODING, "");
static_assert(static_cast<u32>(A64::Exception::ReservedValue) == OD_EXCEPTION_RESERVED_VALUE, "");
static_assert(static_cast<u32>(A64::Exception::UnpredictableInstruction) == OD_EXCEPTION_UNPREDICTABLE_INSTRUCTION, "");
static_assert(static_cast<u32>(A64::Exception::WaitForInterrupt) == OD_EXCEPTION_WAIT_FOR_INTERRUPT, "");
static_assert(static_cast<u32>(A64::Exception::WaitForEvent) == OD_EXCEPTION_WAIT_FOR_EVENT, "");
static_assert(static_cast<u32>(A64::Exception::SendEvent) == OD_EXCEPTION_SEND_EVENT, "");
static_assert(static_cast<u32>(A64::Exception::SendEventLocal) == OD_EXCEPTION_SEND_EVENT_LOCAL, "");
static_assert(static_cast<u32>(A64::Exception::Yield) == OD_EXCEPTION_YIELD, "");
static_assert(static_cast<u32>(A64::Exception::Breakpoint) == OD_EXCEPTION_BREAKPOINT, "");
static_assert(static_cast<u32>(A64::Exception::NoExecuteFault) == OD_EXCEPTION_NO_EXECUTE_FAULT, "");

static_assert(static_cast<u32>(HaltReason::Step) == OD_HALT_STEP, "");
static_assert(static_cast<u32>(HaltReason::CacheInvalidation) == OD_HALT_CACHE_INVALIDATION, "");
static_assert(static_cast<u32>(HaltReason::MemoryAbort) == OD_HALT_MEMORY_ABORT, "");
static_assert(static_cast<u32>(HaltReason::UserDefined8) == OD_HALT_USER8, "");
/* The shim's own return values must not collide with any dynarmic halt bit. */
static_assert((OD_HALT_SHIM_THREW
               & (OD_HALT_STEP | OD_HALT_CACHE_INVALIDATION | OD_HALT_MEMORY_ABORT
                  | OD_HALT_USER1 | OD_HALT_USER2 | OD_HALT_USER3 | OD_HALT_USER4
                  | OD_HALT_USER5 | OD_HALT_USER6 | OD_HALT_USER7 | OD_HALT_USER8
                  | OD_HALT_SHIM_REENTERED))
                  == 0u,
              "OD_HALT_SHIM_THREW overlaps another halt bit");
static_assert((OD_HALT_SHIM_REENTERED
               & (OD_HALT_STEP | OD_HALT_CACHE_INVALIDATION | OD_HALT_MEMORY_ABORT
                  | OD_HALT_USER1 | OD_HALT_USER2 | OD_HALT_USER3 | OD_HALT_USER4
                  | OD_HALT_USER5 | OD_HALT_USER6 | OD_HALT_USER7 | OD_HALT_USER8))
                  == 0u,
              "OD_HALT_SHIM_REENTERED overlaps a real HaltReason bit");

static_assert(sizeof(A64::Vector) == 2 * sizeof(u64), "");

static_assert(static_cast<u32>(Dynarmic::OptimizationFlag::BlockLinking) == OD_OPT_BLOCK_LINKING, "");
static_assert(static_cast<u32>(Dynarmic::OptimizationFlag::ReturnStackBuffer) == OD_OPT_RETURN_STACK_BUFFER, "");
static_assert(static_cast<u32>(Dynarmic::OptimizationFlag::FastDispatch) == OD_OPT_FAST_DISPATCH, "");
static_assert(static_cast<u32>(Dynarmic::OptimizationFlag::GetSetElimination) == OD_OPT_GET_SET_ELIMINATION, "");
static_assert(static_cast<u32>(Dynarmic::OptimizationFlag::ConstProp) == OD_OPT_CONST_PROP, "");
static_assert(static_cast<u32>(Dynarmic::OptimizationFlag::MiscIROpt) == OD_OPT_MISC_IR_OPT, "");
static_assert(static_cast<u32>(Dynarmic::all_safe_optimizations) == OD_OPT_ALL_SAFE, "");
static_assert(static_cast<u32>(Dynarmic::no_optimizations) == OD_OPT_NONE, "");

class ShimCallbacks final : public A64::UserCallbacks {
public:
    /* Copied from `od_config` at construction and never written again, so a
     * callback fired from generated code reads immutable state. */
    od_callbacks cb{};
    void* ctx = nullptr;
    od_stats stats{};
#if defined(__aarch64__)
    u64 last_svc_return = 0;
#endif

    std::optional<u32> MemoryReadCode(u64 vaddr) override {
        stats.read_code++;
        u32 out = 0;
        if (cb.read_code(ctx, vaddr, &out) == 0) {
            /* Becomes Exception::NoExecuteFault: a guest branch into unmapped
             * memory leaves as a typed exit, not a host access violation. */
            return std::nullopt;
        }
        return out;
    }

    u8 MemoryRead8(u64 v) override {
        stats.slow_path_reads++;
        stats.slow_path_total++;
        return cb.read8(ctx, v);
    }
    u16 MemoryRead16(u64 v) override {
        stats.slow_path_reads++;
        stats.slow_path_total++;
        return cb.read16(ctx, v);
    }
    u32 MemoryRead32(u64 v) override {
        stats.slow_path_reads++;
        stats.slow_path_total++;
        return cb.read32(ctx, v);
    }
    u64 MemoryRead64(u64 v) override {
        stats.slow_path_reads++;
        stats.slow_path_total++;
        return cb.read64(ctx, v);
    }
    A64::Vector MemoryRead128(u64 v) override {
        stats.slow_path_reads++;
        stats.slow_path_total++;
        u64 out[2] = {0, 0};
        cb.read128(ctx, v, out);
        return A64::Vector{out[0], out[1]};
    }

    void MemoryWrite8(u64 v, u8 x) override {
        stats.slow_path_writes++;
        stats.slow_path_total++;
        cb.write8(ctx, v, x);
    }
    void MemoryWrite16(u64 v, u16 x) override {
        stats.slow_path_writes++;
        stats.slow_path_total++;
        cb.write16(ctx, v, x);
    }
    void MemoryWrite32(u64 v, u32 x) override {
        stats.slow_path_writes++;
        stats.slow_path_total++;
        cb.write32(ctx, v, x);
    }
    void MemoryWrite64(u64 v, u64 x) override {
        stats.slow_path_writes++;
        stats.slow_path_total++;
        cb.write64(ctx, v, x);
    }
    void MemoryWrite128(u64 v, A64::Vector x) override {
        stats.slow_path_writes++;
        stats.slow_path_total++;
        const u64 arr[2] = {x[0], x[1]};
        cb.write128(ctx, v, arr);
    }

    bool MemoryWriteExclusive8(u64 v, u8 x, u8 e) override {
        stats.slow_path_exclusive++;
        stats.slow_path_total++;
        return cb.write_exclusive8(ctx, v, x, e) != 0;
    }
    bool MemoryWriteExclusive16(u64 v, u16 x, u16 e) override {
        stats.slow_path_exclusive++;
        stats.slow_path_total++;
        return cb.write_exclusive16(ctx, v, x, e) != 0;
    }
    bool MemoryWriteExclusive32(u64 v, u32 x, u32 e) override {
        stats.slow_path_exclusive++;
        stats.slow_path_total++;
        return cb.write_exclusive32(ctx, v, x, e) != 0;
    }
    bool MemoryWriteExclusive64(u64 v, u64 x, u64 e) override {
        stats.slow_path_exclusive++;
        stats.slow_path_total++;
        return cb.write_exclusive64(ctx, v, x, e) != 0;
    }
    bool MemoryWriteExclusive128(u64 v, A64::Vector x, A64::Vector e) override {
        stats.slow_path_exclusive++;
        stats.slow_path_total++;
        const u64 xa[2] = {x[0], x[1]};
        const u64 ea[2] = {e[0], e[1]};
        return cb.write_exclusive128(ctx, v, xa, ea) != 0;
    }

    void InterpreterFallback(u64 pc, std::size_t n) override {
        stats.interpreter_fallbacks++;
        cb.interpreter_fallback(ctx, pc, static_cast<u64>(n));
    }
    void CallSVC(u32 swi) override {
#if defined(__aarch64__)
        /* The arm64 backend's call trampolines reach this override with a plain `BR`, so the
         * link register still holds the return address into the translated block: an address
         * inside the code cache, which is what the W^X measurement needs and nothing else
         * exposes. */
        last_svc_return = reinterpret_cast<u64>(__builtin_return_address(0));
#endif
        stats.svc_calls++;
        cb.call_svc(ctx, swi);
    }
    void ExceptionRaised(u64 pc, A64::Exception e) override {
        stats.exceptions++;
        cb.exception_raised(ctx, pc, static_cast<u32>(e));
    }
    void InstructionCacheOperationRaised(A64::InstructionCacheOperation op, u64 v) override {
        stats.icache_ops++;
        cb.instruction_cache_op(ctx, static_cast<u32>(op), v);
    }

    void AddTicks(u64 ticks) override { cb.add_ticks(ctx, ticks); }
    u64 GetTicksRemaining() override { return cb.get_ticks_remaining(ctx); }
    u64 GetCNTPCT() override { return cb.get_cntpct(ctx); }
};

struct OdJit {
    ShimCallbacks callbacks;
    /* Kept so `od_jit_effective_config` reports what dynarmic holds rather than
     * what the caller asked for. */
    A64::UserConfig conf{};
    A64::Jit* jit = nullptr;
};

inline OdJit* as_jit(void* p) { return static_cast<OdJit*>(p); }

#if defined(__aarch64__) || defined(_M_ARM64)
constexpr bool od_host_is_arm64 = true;
#else
constexpr bool od_host_is_arm64 = false;
#endif

/* Every function pointer in `od_callbacks` is dereferenced from generated code
 * with no null check, so one missing pointer is a jump to address 0 in the
 * middle of a translated block. Reject at construction instead. */
bool callbacks_complete(const od_callbacks* c) {
    return c->read_code && c->read8 && c->read16 && c->read32 && c->read64 && c->read128
        && c->write8 && c->write16 && c->write32 && c->write64 && c->write128
        && c->write_exclusive8 && c->write_exclusive16 && c->write_exclusive32
        && c->write_exclusive64 && c->write_exclusive128
        && c->interpreter_fallback && c->call_svc && c->exception_raised
        && c->instruction_cache_op && c->get_cntpct && c->add_ticks
        && c->get_ticks_remaining;
}

}  // namespace

extern "C" {

uint32_t od_dynarmic_abi_version(void) { return OD_DYNARMIC_ABI_VERSION; }

void od_dynarmic_abi_layout(od_abi_layout* out) {
    out->callbacks_size = static_cast<uint32_t>(sizeof(od_callbacks));
    out->callbacks_align = static_cast<uint32_t>(alignof(od_callbacks));
    out->config_size = static_cast<uint32_t>(sizeof(od_config));
    out->config_align = static_cast<uint32_t>(alignof(od_config));
    out->effective_config_size = static_cast<uint32_t>(sizeof(od_effective_config));
    out->effective_config_align = static_cast<uint32_t>(alignof(od_effective_config));
    out->stats_size = static_cast<uint32_t>(sizeof(od_stats));
    out->stats_align = static_cast<uint32_t>(alignof(od_stats));
}

void* od_monitor_new(uint64_t processor_count) {
    if (processor_count == 0 || processor_count > 4096) {
        return nullptr;
    }
    try {
        return new ExclusiveMonitor{static_cast<std::size_t>(processor_count)};
    } catch (...) {
        return nullptr;
    }
}

void od_monitor_free(void* monitor) {
    delete static_cast<ExclusiveMonitor*>(monitor);
}

void* od_jit_new(const od_config* config) {
    if (config == nullptr || config->abi_version != OD_DYNARMIC_ABI_VERSION) {
        return nullptr;
    }
    if (config->callbacks == nullptr || !callbacks_complete(config->callbacks)) {
        return nullptr;
    }
    /* dynarmic ASSERTs 12..64 inclusive; an assert is an abort, so check here. */
    if (config->fastmem_enabled
        && (config->fastmem_address_space_bits < 12 || config->fastmem_address_space_bits > 64)) {
        return nullptr;
    }
    /* dynarmic documents 8 MiB as the minimum and 2 GiB (x64) / 128 MiB
     * (arm64) as the maximum, both enforced by asserts inside the code-cache
     * allocator rather than by a return value. Refuse out-of-range sizes here
     * so a bad number is a null return rather than a terminate. Rejected, not
     * clamped: silently substituting a different value is the failure mode D4
     * warns about. */
    if (config->code_cache_size != 0) {
        const uint64_t min_cache = 8ull << 20;
        const uint64_t max_cache =
            sizeof(void*) == 8 && !od_host_is_arm64 ? (2ull << 30) : (128ull << 20);
        if (config->code_cache_size < min_cache || config->code_cache_size > max_cache) {
            return nullptr;
        }
    }
    if (config->monitor != nullptr) {
        const auto* mon = static_cast<const ExclusiveMonitor*>(config->monitor);
        if (static_cast<std::size_t>(config->processor_id) >= mon->GetProcessorCount()) {
            return nullptr;
        }
    }

    OdJit* self = nullptr;
    try {
        self = new OdJit{};
        self->callbacks.cb = *config->callbacks;
        self->callbacks.ctx = config->ctx;

        A64::UserConfig uc{};
        uc.callbacks = &self->callbacks;
        uc.processor_id = static_cast<std::size_t>(config->processor_id);
        uc.global_monitor = static_cast<ExclusiveMonitor*>(config->monitor);
        uc.optimizations = static_cast<Dynarmic::OptimizationFlag>(config->optimizations);
        uc.unsafe_optimizations = config->unsafe_optimizations != 0;
        uc.hook_data_cache_operations = false;
        uc.hook_isb = false;
        uc.hook_hint_instructions = config->hook_hint_instructions != 0;
        uc.cntfrq_el0 = config->cntfrq_el0 != 0 ? config->cntfrq_el0 : 600000000u;
        if (config->ctr_el0 != 0) {
            uc.ctr_el0 = config->ctr_el0;
        }
        uc.dczid_el0 = config->dczid_el0;
        uc.tpidr_el0 = config->tpidr_el0;
        uc.tpidrro_el0 = config->tpidrro_el0;
        uc.page_table = nullptr;
        if (config->fastmem_enabled) {
            uc.fastmem_pointer = static_cast<std::uintptr_t>(config->fastmem_pointer);
            uc.fastmem_address_space_bits = static_cast<std::size_t>(config->fastmem_address_space_bits);
            uc.silently_mirror_fastmem = config->silently_mirror_fastmem != 0;
            uc.recompile_on_fastmem_failure = config->recompile_on_fastmem_failure != 0;
            uc.fastmem_exclusive_access = config->fastmem_exclusive_access != 0;
        } else {
            uc.fastmem_pointer = std::nullopt;
        }
        uc.define_unpredictable_behaviour = config->define_unpredictable_behaviour != 0;
        uc.check_halt_on_memory_access = config->check_halt_on_memory_access != 0;
        uc.enable_cycle_counting = config->enable_cycle_counting != 0;
        uc.wall_clock_cntpct = config->wall_clock_cntpct != 0;
        if (config->code_cache_size != 0) {
            uc.code_cache_size = static_cast<std::size_t>(config->code_cache_size);
        }

        self->conf = uc;
        self->jit = new A64::Jit{uc};
        return self;
    } catch (...) {
        /* dynarmic allocates the code cache in its constructor; an over-large
         * `code_cache_size` throws out of `new A64::Jit`. Catching here is what
         * keeps that a null return instead of an unwind into Rust. */
        delete self;
        return nullptr;
    }
}

void od_jit_free(void* p) {
    if (p == nullptr) {
        return;
    }
    OdJit* self = as_jit(p);
    delete self->jit;
    delete self;
}

uint32_t od_jit_run(void* p) {
    OdJit* self = as_jit(p);
    /* dynarmic's `Run()` opens with `ASSERT(!is_executing)`, and its asserts
     * terminate the process. Guest code can reach a callback, and a callback
     * could call back in here. Refuse instead of aborting. */
    if (self->jit->IsExecuting()) {
        return OD_HALT_SHIM_REENTERED;
    }
    try {
        return static_cast<uint32_t>(self->jit->Run());
    } catch (...) {
        /* Low reachability, but not zero: xbyak throws `Xbyak::Error` from
         * `block_of_code.cpp` when the code cache runs out of room, and
         * translation happens inside `Run`. A C++ exception unwinding into
         * Rust is undefined behaviour, so it stops here and becomes a halt
         * reason the caller can see. */
        return OD_HALT_SHIM_THREW;
    }
}

uint32_t od_jit_step(void* p) {
    OdJit* self = as_jit(p);
    if (self->jit->IsExecuting()) {
        return OD_HALT_SHIM_REENTERED;
    }
    try {
        return static_cast<uint32_t>(self->jit->Step());
    } catch (...) {
        return OD_HALT_SHIM_THREW;
    }
}

void od_jit_halt(void* p, uint32_t reason) {
    as_jit(p)->jit->HaltExecution(static_cast<HaltReason>(reason));
}

void od_jit_clear_halt(void* p, uint32_t reason) {
    as_jit(p)->jit->ClearHalt(static_cast<HaltReason>(reason));
}

int od_jit_is_executing(void* p) {
    return as_jit(p)->jit->IsExecuting() ? 1 : 0;
}

uint64_t od_jit_get_reg(void* p, uint32_t index) {
    if (index > 30) {
        return 0;
    }
    return as_jit(p)->jit->GetRegister(index);
}

void od_jit_set_reg(void* p, uint32_t index, uint64_t value) {
    if (index > 30) {
        return;
    }
    as_jit(p)->jit->SetRegister(index, value);
}

uint64_t od_jit_get_sp(void* p) { return as_jit(p)->jit->GetSP(); }
void od_jit_set_sp(void* p, uint64_t v) { as_jit(p)->jit->SetSP(v); }
uint64_t od_jit_get_pc(void* p) { return as_jit(p)->jit->GetPC(); }
void od_jit_set_pc(void* p, uint64_t v) { as_jit(p)->jit->SetPC(v); }

void od_jit_get_vec(void* p, uint32_t index, uint64_t out[2]) {
    if (index > 31) {
        out[0] = 0;
        out[1] = 0;
        return;
    }
    const A64::Vector v = as_jit(p)->jit->GetVector(index);
    out[0] = v[0];
    out[1] = v[1];
}

void od_jit_set_vec(void* p, uint32_t index, const uint64_t value[2]) {
    if (index > 31) {
        return;
    }
    as_jit(p)->jit->SetVector(index, A64::Vector{value[0], value[1]});
}

uint32_t od_jit_get_pstate(void* p) { return as_jit(p)->jit->GetPstate(); }
void od_jit_set_pstate(void* p, uint32_t v) { as_jit(p)->jit->SetPstate(v); }
uint32_t od_jit_get_fpcr(void* p) { return as_jit(p)->jit->GetFpcr(); }
void od_jit_set_fpcr(void* p, uint32_t v) { as_jit(p)->jit->SetFpcr(v); }
uint32_t od_jit_get_fpsr(void* p) { return as_jit(p)->jit->GetFpsr(); }
void od_jit_set_fpsr(void* p, uint32_t v) { as_jit(p)->jit->SetFpsr(v); }

void od_jit_invalidate_range(void* p, uint64_t addr, uint64_t len) {
    /* dynarmic computes `addr + len - 1` and builds
     * `boost::icl::discrete_interval::closed()` from it, with no check that the
     * upper bound is above the lower. Measured on this pin:
     *
     *   len == 0   -> upper = addr - 1, an inverted (empty) interval. Nothing
     *                 is invalidated, but `HaltExecution(CacheInvalidation)` is
     *                 raised anyway, so a caller that asks for nothing has its
     *                 guest interrupted for nothing.
     *   overflow   -> the same inversion, and this time the range the caller
     *                 *did* ask for is silently not invalidated. The guest then
     *                 executes stale translations of code it just told us it
     *                 changed, which is a wrong answer rather than a slow one. */
    if (len == 0) {
        return;
    }
    /* Clamp so `addr + len - 1` cannot wrap. `room` is the number of bytes
     * above `addr`, so the largest valid length is `room + 1` -- which is
     * representable for every `addr` except 0, where it would be 2^64.
     *
     * Writing this as `max_len = UINT64_MAX - addr + 1` instead is wrong in a
     * way that is easy to miss and expensive to hit: at `addr == 0` it wraps to
     * 0, every length compares greater, and a four-byte invalidation at guest
     * address 0 clamps to `len = 0`, which dynarmic turns into
     * `closed(0, UINT64_MAX)` -- the entire code cache, thrown away by a guest
     * that asked to invalidate one instruction. Guest code chooses the address.
     *
     * Comparing `len - 1` against `room` never overflows: `len >= 1` here, and
     * `room <= UINT64_MAX`. */
    const uint64_t room = std::numeric_limits<uint64_t>::max() - addr;
    if (len - 1 > room) {
        len = room + 1;
    }
    as_jit(p)->jit->InvalidateCacheRange(addr, static_cast<std::size_t>(len));
}

void od_jit_clear_cache(void* p) { as_jit(p)->jit->ClearCache(); }
void od_jit_clear_exclusive(void* p) { as_jit(p)->jit->ClearExclusiveState(); }

void od_jit_effective_config(void* p, od_effective_config* out) {
    /* This reads the shim's saved `UserConfig`, not dynarmic's. They are the
     * same object by value and dynarmic's copy is `const` for its whole life on
     * this pin -- `A64::Jit::Impl` holds `const UserConfig conf` and nothing
     * assigns to it -- so reading ours cannot disagree with reading theirs.
     * dynarmic exposes no accessor, so this is the only way to answer the
     * question at all; if a future pin makes its copy mutable, this stops being
     * equivalent and the check Task 3 builds on it stops being worth anything. */
    const OdJit* self = as_jit(p);
    const A64::UserConfig& uc = self->conf;
    std::memset(out, 0, sizeof(*out));
    out->fastmem_enabled = uc.fastmem_pointer.has_value() ? 1 : 0;
    out->fastmem_pointer = uc.fastmem_pointer.has_value()
                               ? static_cast<uint64_t>(*uc.fastmem_pointer)
                               : 0u;
    out->fastmem_address_space_bits = static_cast<uint64_t>(uc.fastmem_address_space_bits);
    out->silently_mirror_fastmem = uc.silently_mirror_fastmem ? 1 : 0;
    out->recompile_on_fastmem_failure = uc.recompile_on_fastmem_failure ? 1 : 0;
    out->fastmem_exclusive_access = uc.fastmem_exclusive_access ? 1 : 0;
    out->page_table_present = uc.page_table != nullptr ? 1 : 0;
    out->code_cache_size = static_cast<uint64_t>(uc.code_cache_size);
    out->enable_cycle_counting = uc.enable_cycle_counting ? 1 : 0;
    out->hook_hint_instructions = uc.hook_hint_instructions ? 1 : 0;
    out->optimizations = static_cast<uint32_t>(uc.optimizations);
    out->unsafe_optimizations = uc.unsafe_optimizations ? 1u : 0u;
#if defined(OD_DYNARMIC_W_XOR_X) && OD_DYNARMIC_W_XOR_X
    out->code_cache_w_xor_x = OD_CODE_CACHE_W_XOR_X;
#elif defined(__APPLE__) && defined(__aarch64__)
    /* oaknut's CodeBlock maps the cache `MAP_JIT` (RWX in the VM map) and the arm64 backend
     * brackets every write with `pthread_jit_write_protect_np`, which switches the *calling
     * thread's* view between RW- and R-X in hardware. Not echoed from a flag: `tests/wx.rs`
     * measures it. */
    out->code_cache_w_xor_x = OD_CODE_CACHE_W_XOR_X_PER_THREAD;
#else
    out->code_cache_w_xor_x = OD_CODE_CACHE_W_AND_X;
#endif
    out->tpidr_el0_ptr = reinterpret_cast<uint64_t>(uc.tpidr_el0);
    out->tpidrro_el0_ptr = reinterpret_cast<uint64_t>(uc.tpidrro_el0);
}

void od_jit_stats(void* p, od_stats* out) {
    *out = as_jit(p)->callbacks.stats;
}

void od_jit_reset_stats(void* p) {
    as_jit(p)->callbacks.stats = od_stats{};
}

#if defined(__aarch64__)
uint64_t od_jit_last_svc_return_address(void* p) {
    return as_jit(p)->callbacks.last_svc_return;
}
#endif

uint64_t od_jit_slow_path_total(void* p) {
    return as_jit(p)->callbacks.stats.slow_path_total;
}

uint64_t od_invalidation_page_probes(void) {
#if defined(__aarch64__)
    return static_cast<uint64_t>(Dynarmic::Backend::Arm64::invalidation_page_probes.load(std::memory_order_relaxed));
#else
    return 0;
#endif
}

uint64_t od_invalidation_ranges_checked(void) {
#if defined(__aarch64__)
    return static_cast<uint64_t>(Dynarmic::Backend::Arm64::invalidation_ranges_checked.load(std::memory_order_relaxed));
#else
    return 0;
#endif
}

uint64_t od_page_backed_bytes(void) {
#if defined(__aarch64__)
    return static_cast<uint64_t>(Dynarmic::Backend::Arm64::page_backed_bytes.load(std::memory_order_relaxed));
#else
    return 0;
#endif
}

}  // extern "C"
