/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstring>
#include <limits>
#include <memory>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <optional>
#include <shared_mutex>
#include <stdexcept>
#include <thread>
#include <vector>

#include <boost/icl/interval_set.hpp>
#include <mcl/assert.hpp>
#include <mcl/bit_cast.hpp>
#include <mcl/scope_exit.hpp>
#include <tsl/robin_set.h>

#include "dynarmic/backend/x64/a64_emit_x64.h"
#include "dynarmic/backend/x64/a64_jitstate.h"
#include "dynarmic/backend/x64/block_of_code.h"
#include "dynarmic/backend/x64/devirtualize.h"
#include "dynarmic/backend/x64/exclusive_monitor_friend.h"
#include "dynarmic/backend/x64/jitstate_info.h"
#include "dynarmic/common/atomic.h"
#include "dynarmic/common/x64_disassemble.h"
#include "dynarmic/frontend/A64/translate/a64_translate.h"
#include "dynarmic/interface/A64/a64.h"
#include "dynarmic/ir/basic_block.h"
#include "dynarmic/ir/opt/passes.h"

#ifdef _WIN32
#    define WIN32_LEAN_AND_MEAN
#    include <windows.h>
#elif defined(__linux__)
#    include <linux/membarrier.h>
#    include <sys/syscall.h>
#    include <unistd.h>
#endif

namespace Dynarmic::A64 {

using namespace Backend::X64;

static RunCodeCallbacks GenRunCodeCallbacks(A64::UserCallbacks* cb, CodePtr (*LookupBlock)(void* lookup_block_arg), void* arg, const A64::UserConfig& conf) {
    return RunCodeCallbacks{
        std::make_unique<ArgCallback>(LookupBlock, reinterpret_cast<u64>(arg)),
        std::make_unique<ArgCallback>(Devirtualize<&A64::UserCallbacks::AddTicks>(cb)),
        std::make_unique<ArgCallback>(Devirtualize<&A64::UserCallbacks::GetTicksRemaining>(cb)),
        conf.enable_cycle_counting,
    };
}

static std::function<void(BlockOfCode&)> GenRCP(const A64::UserConfig& conf) {
    return [conf](BlockOfCode& code) {
        if (conf.page_table) {
            code.mov(code.r14, mcl::bit_cast<u64>(conf.page_table));
        }
        if (conf.fastmem_pointer) {
            code.mov(code.r13, *conf.fastmem_pointer);
        }
    };
}

static Optimization::PolyfillOptions GenPolyfillOptions(const BlockOfCode& code) {
    return Optimization::PolyfillOptions{
        .sha256 = !code.HasHostFeature(HostFeature::SHA),
        .vector_multiply_widen = true,
    };
}

/// The translator's pipeline, as upstream's GetBlock runs it, for one location.
static IR::Block TranslateBlock(IR::LocationDescriptor current_location, const UserConfig& conf, const Optimization::PolyfillOptions& polyfill_options) {
    const auto get_code = [&conf](u64 vaddr) { return conf.callbacks->MemoryReadCode(vaddr); };
    IR::Block ir_block = A64::Translate(A64::LocationDescriptor{current_location}, get_code,
                                        {conf.define_unpredictable_behaviour, conf.wall_clock_cntpct});
    Optimization::PolyfillPass(ir_block, polyfill_options);
    Optimization::A64CallbackConfigPass(ir_block, conf);
    Optimization::NamingPass(ir_block);
    if (conf.HasOptimization(OptimizationFlag::GetSetElimination) && !conf.check_halt_on_memory_access) {
        Optimization::A64GetSetElimination(ir_block);
        Optimization::DeadCodeElimination(ir_block);
    }
    if (conf.HasOptimization(OptimizationFlag::ConstProp)) {
        Optimization::ConstantPropagation(ir_block);
        Optimization::DeadCodeElimination(ir_block);
    }
    if (conf.HasOptimization(OptimizationFlag::MiscIROpt)) {
        Optimization::A64MergeInterpretBlocksPass(ir_block, conf.callbacks);
    }
    Optimization::VerificationPass(ir_block);
    return ir_block;
}

constexpr size_t MINIMUM_REMAINING_CODESIZE = 1 * 1024 * 1024;

// ------------------------------------------------------------------------------------------------
// Omnidroid patch 0022: a code cache shared by every Jit of one guest address space.
// docs/research/shared-jit-cache.md is the design; the comments here say what each piece holds.
// ------------------------------------------------------------------------------------------------

namespace {

constexpr u64 NOT_RUNNING = std::numeric_limits<u64>::max();
/// Commit and decommit granule inside a region.
constexpr size_t SHARED_GRANULE = 64 * 1024;
/// Room a region must have left, between code and slots, before a block is emitted into it.
constexpr size_t SHARED_HEADROOM = 2 * MINIMUM_REMAINING_CODESIZE;
/// A reclaimed region is reused only if its largest span free of parked threads' return sites is
/// at least this; a region is at least twice it.
constexpr size_t SHARED_MINIMUM_SPAN = 4 * MINIMUM_REMAINING_CODESIZE;
/// How often a thread leaving Run may try to give retired regions back.
constexpr u64 SHARED_RECLAIM_INTERVAL_NS = 1'000'000;
/// Bytes kept around a parked thread's return site: the call sequence before it and the block's
/// tail after it (clear the site, clear exclusive state, charge cycles, test the halt word, jump).
constexpr size_t SHARED_HOLE_BEFORE = 64;
constexpr size_t SHARED_HOLE_AFTER = 192;

u8* AlignUp(u8* p, size_t a) {
    return reinterpret_cast<u8*>((reinterpret_cast<uintptr_t>(p) + a - 1) & ~(uintptr_t(a) - 1));
}

u8* AlignDown(u8* p, size_t a) {
    return reinterpret_cast<u8*>(reinterpret_cast<uintptr_t>(p) & ~(uintptr_t(a) - 1));
}

/// What a shared cache holds of one attached Jit: the reclaimer reads `running_epoch` and the
/// thread's JitState; the thread itself does everything else.
struct SharedThreadState {
    A64JitState* jit_state = nullptr;
    /// The epoch this thread observed when it last became quiescent (entered Run, or re-synced in
    /// the dispatcher's lookup); NOT_RUNNING while it is outside RunCode.
    std::atomic<u64> running_epoch{NOT_RUNNING};
    /// The cache generation this thread's RSB and fast-dispatch table are consistent with.
    u64 seen_generation = 0;
    /// Clears this thread's RSB and fast-dispatch table. Called only on the thread itself.
    void (*flush_routing)(SharedThreadState*) = nullptr;
    /// The dispatcher's LookupBlock for this thread.
    CodePtr (*lookup)(SharedThreadState*) = nullptr;
    /// Lookups this thread's own fast-dispatch table could not answer (written by the thread
    /// alone; read for the statistics).
    std::atomic<u64> locked_lookups{0};
    void* owner = nullptr;
};

/// The dispatcher's LookupBlock in a shared cache: its argument is read from
/// JitState::od_lookup_arg, which points at the running thread's SharedThreadState.
CodePtr SharedLookupTrampoline(void* arg) {
    auto* thread = static_cast<SharedThreadState*>(arg);
    return thread->lookup(thread);
}

RunCodeCallbacks GenSharedRunCodeCallbacks(A64::UserCallbacks* template_cb, const A64::UserConfig& conf) {
    return RunCodeCallbacks{
        std::make_unique<ArgCallback>(&SharedLookupTrampoline, ArgCallback::FromJitState{offsetof(A64JitState, od_lookup_arg)}),
        std::make_unique<ArgCallback>(DevirtualizeFromJitState<&A64::UserCallbacks::AddTicks>(template_cb, offsetof(A64JitState, od_callbacks))),
        std::make_unique<ArgCallback>(DevirtualizeFromJitState<&A64::UserCallbacks::GetTicksRemaining>(template_cb, offsetof(A64JitState, od_callbacks))),
        conf.enable_cycle_counting,
    };
}

UserConfig SharedTemplate(const UserConfig& conf, size_t total_bytes) {
    UserConfig t = conf;
    // A shared block is never recompiled from inside a fault handler: that would rewrite code
    // every thread runs, and put every thread's accesses of that instruction on the callback path.
    // A declined fault still reaches the fallback callback, on every occurrence.
    t.recompile_on_fastmem_failure = false;
    t.recompile_on_exclusive_fastmem_failure = false;
    t.shared_code_cache = nullptr;
    t.code_cache_size = total_bytes;
    return t;
}

/// Whether a Jit configured as `c` may run code emitted for `t`: every field that shapes emitted
/// code is equal. `callbacks`, `processor_id` and the TPIDR pointers are what may differ -- they
/// are read from JitState -- but the callbacks must be of the same class and the TPIDR pointers
/// null exactly when the template's are.
bool EmitsTheSameCode(const UserConfig& t, const UserConfig& c) {
    if (c.callbacks == nullptr || t.callbacks == nullptr) {
        return false;
    }
    // The first word of a polymorphic object is its vtable pointer on both ABIs this backend
    // builds with: the same vtable is the same class, so a function resolved from the template's
    // object is the one each thread's object has.
    if (*reinterpret_cast<void* const*>(c.callbacks) != *reinterpret_cast<void* const*>(t.callbacks)) {
        return false;
    }
    return c.global_monitor == t.global_monitor
        && c.optimizations == t.optimizations
        && c.unsafe_optimizations == t.unsafe_optimizations
        && c.hook_data_cache_operations == t.hook_data_cache_operations
        && c.hook_isb == t.hook_isb
        && c.hook_hint_instructions == t.hook_hint_instructions
        && c.cntfrq_el0 == t.cntfrq_el0
        && c.ctr_el0 == t.ctr_el0
        && c.dczid_el0 == t.dczid_el0
        && (c.tpidrro_el0 == nullptr) == (t.tpidrro_el0 == nullptr)
        && (c.tpidr_el0 == nullptr) == (t.tpidr_el0 == nullptr)
        && c.page_table == nullptr && t.page_table == nullptr
        && c.fastmem_pointer == t.fastmem_pointer
        && !c.recompile_on_fastmem_failure
        && c.fastmem_address_space_bits == t.fastmem_address_space_bits
        && c.silently_mirror_fastmem == t.silently_mirror_fastmem
        && c.fastmem_exclusive_access == t.fastmem_exclusive_access
        && !c.recompile_on_exclusive_fastmem_failure
        && c.define_unpredictable_behaviour == t.define_unpredictable_behaviour
        && c.wall_clock_cntpct == t.wall_clock_cntpct
        && c.check_halt_on_memory_access == t.check_halt_on_memory_access
        && c.enable_cycle_counting == t.enable_cycle_counting
        && c.very_verbose_debugging_output == t.very_verbose_debugging_output
        && (c.global_monitor == nullptr || c.processor_id < GetExclusiveMonitorProcessorCount(c.global_monitor));
}

/// An asymmetric memory barrier: once it returns, every store any other thread of this process
/// made before it is visible here, and every load another thread makes after it sees what this
/// thread stored before it. Windows: FlushProcessWriteBuffers. Linux: membarrier's private
/// expedited command (registered when the cache is built). `false` where neither is available.
bool AsymmetricBarrier() {
#if defined(_WIN32)
    FlushProcessWriteBuffers();
    return true;
#elif defined(__linux__) && defined(__NR_membarrier)
    // (The commands are enumerators, not macros: `#if defined` cannot test them.)
    return syscall(__NR_membarrier, MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, 0) == 0;
#else
    return false;
#endif
}

bool RegisterAsymmetricBarrier() {
#if defined(_WIN32)
    return true;
#elif defined(__linux__) && defined(__NR_membarrier)
    return syscall(__NR_membarrier, MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED, 0, 0) == 0;
#else
    return false;
#endif
}

}  // namespace

struct SharedCodeCache::Impl final {
    Impl(const UserConfig& template_conf, size_t total_bytes, size_t region_bytes);
    ~Impl();

    /// A span of the buffer after the prelude. Code grows up from `use_begin`, link slots down
    /// from `use_end`; `holes` are parked threads' return sites kept from a previous use.
    struct Region {
        u8* begin = nullptr;
        u8* end = nullptr;
        u8* use_begin = nullptr;
        u8* use_end = nullptr;
        u8* code_committed_end = nullptr;
        u8* slot_top = nullptr;
        u8* slots_committed_begin = nullptr;
        std::vector<std::pair<u8*, u8*>> holes;
        enum class State { Free,
                           Current,
                           Retired } state = State::Free;
        u64 retired_epoch = 0;
    };

    const UserConfig conf;
    A64JitState layout;  // only its field offsets are used (JitStateInfo)
    BlockOfCode block_of_code;
    A64EmitX64 emitter;
    Optimization::PolyfillOptions polyfill_options;

    /// Translation and every map: exclusive. Lookups and the fault handler: shared.
    mutable SharedCodeLock lock;
    std::vector<Region> regions;
    static constexpr size_t NO_REGION = std::numeric_limits<size_t>::max();
    size_t current = NO_REGION;
    std::atomic<u64> generation{0};
    std::atomic<u64> epoch{0};
    bool barrier_available = false;
    /// Regions retired and not yet given back; when non-zero, a thread leaving Run tries.
    std::atomic<u64> retired_now{0};
    std::atomic<u64> last_reclaim_attempt_ns{0};

    mutable std::mutex attach_lock;
    std::vector<SharedThreadState*> attached;

    // Under `lock`, exclusive.
    u64 blocks_emitted = 0;
    u64 code_bytes_emitted = 0;
    u64 translations_raced = 0;
    u64 invalidations = 0;
    u64 blocks_invalidated = 0;
    u64 regions_retired = 0;
    u64 regions_reclaimed = 0;
    u64 translations_redone = 0;
    u64 emit_ns = 0;                      // under `lock`, exclusive
    std::atomic<u64> translate_ns{0};     // outside it, summed over threads
    /// Dispatcher lookups that took the lock, from threads that have detached (each attached
    /// thread counts its own, so the count costs no shared cache line). Under `attach_lock`.
    u64 locked_lookups_detached = 0;

    /// Every applied invalidation bumps this; a translation made outside the lock compares it.
    std::atomic<u64> invalidation_serial{0};
    /// Locations a thread is translating outside the lock (under `lock`, exclusive), and the
    /// condition a thread that needs one of them waits on.
    tsl::robin_set<IR::LocationDescriptor> in_flight;
    std::condition_variable_any in_flight_done;
    /// Bumped whenever a location stops being in flight, for a waiter that spins before sleeping.
    std::atomic<u64> in_flight_finished{0};
    struct RecentInvalidation {
        u64 serial;
        bool entire;
        boost::icl::interval_set<u64> ranges;
    };
    static constexpr size_t RECENT_INVALIDATIONS = 64;
    std::deque<RecentInvalidation> recent_invalidations;  // under `lock`, exclusive
    bool InvalidatedSince(u64 serial, u64 begin_pc, u64 end_pc) const;

    void Attach(SharedThreadState* thread);
    void Detach(SharedThreadState* thread);

    /// Publish this thread's epoch, then bring its RSB and fast-dispatch table up to the current
    /// generation. Called on the thread at Run entry and at every dispatcher lookup, where it holds
    /// no reference into the cache but the prelude's.
    void Sync(SharedThreadState& thread);

    std::optional<CodePtr> Lookup(IR::LocationDescriptor location) const;
    CodePtr Emit(IR::LocationDescriptor location, const UserConfig& translator_conf, SharedThreadState& thread);
    void Invalidate(bool entire, const boost::icl::interval_set<u64>& ranges);
    SharedCodeCache::Stats GetStats() const;
    /// Called by a thread that has just left RunCode: every attached thread was asked to halt when
    /// a region was retired, so this is when the last holder lets go. Throttled, and skipped if
    /// the lock is busy.
    void ReclaimSoon();

private:
    void EnsureRoom(SharedThreadState& thread, std::unique_lock<SharedCodeLock>& held);
    void StartRegion(size_t index);
    void RetireCurrentRegion(SharedThreadState& thread);
    void TryReclaimRetired();
    bool TryReclaim(Region& region, bool parked_sites_known);
};

// ------------------------------------------------------------------------------------------------

struct Jit::Impl final {
public:
    Impl(Jit* jit, UserConfig conf)
            : conf(conf)
            , shared(conf.shared_code_cache ? conf.shared_code_cache->impl.get() : nullptr)
            , owned(shared ? nullptr : std::make_unique<Owned>(this, jit, this->conf, jit_state))
            , block_of_code(shared ? shared->block_of_code : owned->block_of_code)
            , emitter(shared ? shared->emitter : owned->emitter)
            , polyfill_options(GenPolyfillOptions(block_of_code)) {
        ASSERT(conf.page_table_address_space_bits >= 12 && conf.page_table_address_space_bits <= 64);
        if (shared) {
            if (!EmitsTheSameCode(shared->conf, SharedTemplate(conf, shared->conf.code_cache_size))) {
                throw std::invalid_argument("dynarmic: this Jit's configuration shapes code differently from the shared code cache's");
            }
            if (conf.HasOptimization(OptimizationFlag::FastDispatch)) {
                fast_dispatch_table = std::make_unique<u8[]>(A64EmitX64::FastDispatchTableBytes());
                A64EmitX64::ResetFastDispatchTable(fast_dispatch_table.get());
            }
            thread.jit_state = &jit_state;
            thread.flush_routing = &FlushRoutingThunk;
            thread.lookup = &SharedLookupThunk;
            thread.owner = this;
            ProgramSharedJitState();
            shared->Attach(&thread);
        }
    }

    ~Impl() {
        if (shared) {
            shared->Detach(&thread);
        }
    }

    HaltReason Run() {
        ASSERT(!is_executing);
        PerformRequestedCacheInvalidation(static_cast<HaltReason>(Atomic::Load(&jit_state.halt_reason)));

        is_executing = true;
        SCOPE_EXIT {
            this->is_executing = false;
        };

        if (shared) {
            shared->Sync(thread);
        }
        SCOPE_EXIT {
            if (shared) {
                // Patch 0022: outside RunCode, this thread holds no region.
                thread.running_epoch.store(NOT_RUNNING, std::memory_order_seq_cst);
            }
        };

        // TODO: Check code alignment

        const CodePtr current_code_ptr = [this] {
            // RSB optimization
            const u32 new_rsb_ptr = (jit_state.rsb_ptr - 1) & A64JitState::RSBPtrMask;
            if (jit_state.GetUniqueHash() == jit_state.rsb_location_descriptors[new_rsb_ptr]) {
                jit_state.rsb_ptr = new_rsb_ptr;
                return reinterpret_cast<CodePtr>(jit_state.rsb_codeptrs[new_rsb_ptr]);
            }

            return GetCurrentBlock();
        }();

        const HaltReason hr = block_of_code.RunCode(&jit_state, current_code_ptr);

        if (shared) {
            thread.running_epoch.store(NOT_RUNNING, std::memory_order_seq_cst);
            if (shared->retired_now.load(std::memory_order_relaxed) != 0) {
                shared->ReclaimSoon();
            }
        }

        PerformRequestedCacheInvalidation(hr);

        return hr;
    }

    HaltReason Step() {
        ASSERT(!is_executing);
        PerformRequestedCacheInvalidation(static_cast<HaltReason>(Atomic::Load(&jit_state.halt_reason)));

        is_executing = true;
        SCOPE_EXIT {
            this->is_executing = false;
        };

        if (shared) {
            shared->Sync(thread);
        }
        SCOPE_EXIT {
            if (shared) {
                thread.running_epoch.store(NOT_RUNNING, std::memory_order_seq_cst);
            }
        };

        const HaltReason hr = block_of_code.StepCode(&jit_state, GetCurrentSingleStep());

        if (shared) {
            thread.running_epoch.store(NOT_RUNNING, std::memory_order_seq_cst);
        }

        PerformRequestedCacheInvalidation(hr);

        return hr;
    }

    void ClearCache() {
        if (shared && !is_executing) {
            // Omnidroid patch 0022: applied now, for every Jit of the cache. Deferring it to this
            // Jit's next Run would leave the translations to every other thread for as long as
            // this one does not run -- for ever, if it never runs again.
            shared->Invalidate(true, {});
            return;
        }
        std::unique_lock lock{invalidation_mutex};
        invalidate_entire_cache = true;
        HaltExecution(HaltReason::CacheInvalidation);
    }

    void InvalidateCacheRange(u64 start_address, size_t length) {
        const auto end_address = static_cast<u64>(start_address + length - 1);
        const auto range = boost::icl::discrete_interval<u64>::closed(start_address, end_address);
        if (shared && !is_executing) {
            // Omnidroid patch 0022: as ClearCache.
            boost::icl::interval_set<u64> ranges;
            ranges.add(range);
            shared->Invalidate(false, ranges);
            return;
        }
        std::unique_lock lock{invalidation_mutex};
        invalid_cache_ranges.add(range);
        HaltExecution(HaltReason::CacheInvalidation);
    }

    void Reset() {
        ASSERT(!is_executing);
        jit_state = {};
        if (shared) {
            ProgramSharedJitState();
        }
    }

    void HaltExecution(HaltReason hr) {
        Atomic::Or(&jit_state.halt_reason, static_cast<u32>(hr));
    }

    void ClearHalt(HaltReason hr) {
        Atomic::And(&jit_state.halt_reason, ~static_cast<u32>(hr));
    }

    u64 GetSP() const {
        return jit_state.sp;
    }

    void SetSP(u64 value) {
        jit_state.sp = value;
    }

    u64 GetPC() const {
        return jit_state.pc;
    }

    void SetPC(u64 value) {
        jit_state.pc = value;
    }

    u64 GetRegister(size_t index) const {
        if (index == 31)
            return GetSP();
        return jit_state.reg.at(index);
    }

    void SetRegister(size_t index, u64 value) {
        if (index == 31)
            return SetSP(value);
        jit_state.reg.at(index) = value;
    }

    std::array<u64, 31> GetRegisters() const {
        return jit_state.reg;
    }

    void SetRegisters(const std::array<u64, 31>& value) {
        jit_state.reg = value;
    }

    Vector GetVector(size_t index) const {
        return {jit_state.vec.at(index * 2), jit_state.vec.at(index * 2 + 1)};
    }

    void SetVector(size_t index, Vector value) {
        jit_state.vec.at(index * 2) = value[0];
        jit_state.vec.at(index * 2 + 1) = value[1];
    }

    std::array<Vector, 32> GetVectors() const {
        std::array<Vector, 32> ret;
        static_assert(sizeof(ret) == sizeof(jit_state.vec));
        std::memcpy(ret.data(), jit_state.vec.data(), sizeof(jit_state.vec));
        return ret;
    }

    void SetVectors(const std::array<Vector, 32>& value) {
        static_assert(sizeof(value) == sizeof(jit_state.vec));
        std::memcpy(jit_state.vec.data(), value.data(), sizeof(jit_state.vec));
    }

    u32 GetFpcr() const {
        return jit_state.GetFpcr();
    }

    void SetFpcr(u32 value) {
        jit_state.SetFpcr(value);
    }

    u32 GetFpsr() const {
        return jit_state.GetFpsr();
    }

    void SetFpsr(u32 value) {
        jit_state.SetFpsr(value);
    }

    u32 GetPstate() const {
        return jit_state.GetPstate();
    }

    void SetPstate(u32 value) {
        jit_state.SetPstate(value);
    }

    void ClearExclusiveState() {
        jit_state.exclusive_state = 0;
    }

    bool IsExecuting() const {
        return is_executing;
    }

    void DumpDisassembly() const {
        const size_t size = reinterpret_cast<const char*>(block_of_code.getCurr()) - reinterpret_cast<const char*>(block_of_code.GetCodeBegin());
        Common::DumpDisassembledX64(block_of_code.GetCodeBegin(), size);
    }

    std::vector<std::string> Disassemble() const {
        const size_t size = reinterpret_cast<const char*>(block_of_code.getCurr()) - reinterpret_cast<const char*>(block_of_code.GetCodeBegin());
        return Common::DisassembleX64(block_of_code.GetCodeBegin(), size);
    }

private:
    /// A Jit with its own code cache: upstream's two members, now behind a pointer so that a Jit
    /// on a shared cache (patch 0022) has neither.
    struct Owned {
        Owned(Impl* impl, Jit* jit, const UserConfig& conf, A64JitState& jit_state)
                : block_of_code(GenRunCodeCallbacks(conf.callbacks, &GetCurrentBlockThunk, impl, conf), JitStateInfo{jit_state}, conf.code_cache_size, GenRCP(conf))
                , emitter(block_of_code, conf, jit) {}
        BlockOfCode block_of_code;
        A64EmitX64 emitter;
    };

    static CodePtr GetCurrentBlockThunk(void* thisptr) {
        Jit::Impl* this_ = static_cast<Jit::Impl*>(thisptr);
        return this_->GetCurrentBlock();
    }

    static CodePtr SharedLookupThunk(SharedThreadState* thread) {
        return static_cast<Jit::Impl*>(thread->owner)->GetCurrentBlock();
    }

    static void FlushRoutingThunk(SharedThreadState* thread) {
        static_cast<Jit::Impl*>(thread->owner)->FlushRoutingState();
    }

    /// Patch 0022: this thread's RSB and fast-dispatch table, emptied because some translation
    /// they may name has been dropped from the shared cache.
    void FlushRoutingState() {
        jit_state.ResetRSB();
        if (fast_dispatch_table) {
            A64EmitX64::ResetFastDispatchTable(fast_dispatch_table.get());
        }
    }

    /// Patch 0022: what the shared cache's code reads from JitState instead of immediates.
    void ProgramSharedJitState() {
        jit_state.od_callbacks = reinterpret_cast<u64>(conf.callbacks);
        jit_state.od_conf = reinterpret_cast<u64>(&conf);
        jit_state.od_lookup_arg = reinterpret_cast<u64>(&thread);
        jit_state.od_exclusive_address = conf.global_monitor
                                           ? reinterpret_cast<u64>(GetExclusiveMonitorAddressPointer(conf.global_monitor, conf.processor_id))
                                           : 0;
        jit_state.od_exclusive_value = conf.global_monitor
                                         ? reinterpret_cast<u64>(GetExclusiveMonitorValuePointer(conf.global_monitor, conf.processor_id))
                                         : 0;
        jit_state.od_tpidr_el0 = reinterpret_cast<u64>(conf.tpidr_el0);
        jit_state.od_tpidrro_el0 = reinterpret_cast<u64>(conf.tpidrro_el0);
        jit_state.od_fast_dispatch_table = reinterpret_cast<u64>(fast_dispatch_table.get());
        jit_state.od_callback_return = 0;
    }

    IR::LocationDescriptor GetCurrentLocation() const {
        return IR::LocationDescriptor{jit_state.GetUniqueHash()};
    }

    CodePtr GetCurrentBlock() {
        return GetBlock(GetCurrentLocation());
    }

    CodePtr GetCurrentSingleStep() {
        return GetBlock(A64::LocationDescriptor{GetCurrentLocation()}.SetSingleStepping(true));
    }

    CodePtr GetBlock(IR::LocationDescriptor current_location) {
        if (shared) {
            // Patch 0022. The dispatcher (or Run's entry) is a quiescent point for this thread:
            // nothing on its stack but the prelude, so it re-publishes its epoch and catches its
            // RSB and fast-dispatch table up with invalidations other threads made.
            shared->Sync(thread);
            // What this thread has looked up since its tables were last emptied, from its own
            // fast-dispatch table, without the cache's lock -- which every thread's lookups would
            // otherwise contend on. The table is consistent with the generation just synced, as
            // the emitted handler's probes are.
            void* const table = fast_dispatch_table.get();
            if (table) {
                if (const CodePtr entrypoint = emitter.ProbeFastDispatchTable(table, current_location.Value())) {
                    return entrypoint;
                }
            }
            CodePtr entrypoint;
            thread.locked_lookups.store(thread.locked_lookups.load(std::memory_order_relaxed) + 1, std::memory_order_relaxed);
            if (const auto found = shared->Lookup(current_location)) {
                entrypoint = *found;
            } else {
                entrypoint = shared->Emit(current_location, conf, thread);
            }
            if (table) {
                emitter.FillFastDispatchTable(table, current_location.Value(), entrypoint);
            }
            return entrypoint;
        }

        if (auto block = emitter.GetBasicBlock(current_location))
            return block->entrypoint;

        if (block_of_code.SpaceRemaining() < MINIMUM_REMAINING_CODESIZE) {
            // Immediately evacuate cache
            invalidate_entire_cache = true;
            PerformRequestedCacheInvalidation(HaltReason::CacheInvalidation);
        }
        block_of_code.EnsureMemoryCommitted(MINIMUM_REMAINING_CODESIZE);

        // JIT Compile
        IR::Block ir_block = TranslateBlock(current_location, conf, polyfill_options);
        return emitter.Emit(ir_block).entrypoint;
    }

    void PerformRequestedCacheInvalidation(HaltReason hr) {
        if (Has(hr, HaltReason::CacheInvalidation)) {
            std::unique_lock lock{invalidation_mutex};

            ClearHalt(HaltReason::CacheInvalidation);

            if (!invalidate_entire_cache && invalid_cache_ranges.empty()) {
                return;
            }

            if (shared) {
                // Patch 0022: requested from inside a callback, applied now that this thread is
                // outside generated code. Its own RSB and table catch up at its next Sync.
                const bool entire = invalidate_entire_cache;
                boost::icl::interval_set<u64> ranges;
                ranges.swap(invalid_cache_ranges);
                invalidate_entire_cache = false;
                lock.unlock();
                shared->Invalidate(entire, ranges);
                return;
            }

            jit_state.ResetRSB();
            if (invalidate_entire_cache) {
                block_of_code.ClearCache();
                emitter.ClearCache();
            } else {
                emitter.InvalidateCacheRanges(invalid_cache_ranges);
            }
            invalid_cache_ranges.clear();
            invalidate_entire_cache = false;
        }
    }

    bool is_executing = false;

    const UserConfig conf;
    A64JitState jit_state;
    SharedCodeCache::Impl* const shared;
    std::unique_ptr<Owned> owned;
    BlockOfCode& block_of_code;
    A64EmitX64& emitter;
    Optimization::PolyfillOptions polyfill_options;

    // Patch 0022, shared cache only.
    SharedThreadState thread;
    std::unique_ptr<u8[]> fast_dispatch_table;

    bool invalidate_entire_cache = false;
    boost::icl::interval_set<u64> invalid_cache_ranges;
    std::mutex invalidation_mutex;
};

// ------------------------------------------------------------------------------------------------

SharedCodeCache::Impl::Impl(const UserConfig& template_conf, size_t total_bytes, size_t region_bytes)
        : conf(SharedTemplate(template_conf, total_bytes))
        , block_of_code(GenSharedRunCodeCallbacks(conf.callbacks, conf), JitStateInfo{layout}, total_bytes, GenRCP(conf))
        , emitter(block_of_code, conf, nullptr, true)
        , polyfill_options(GenPolyfillOptions(block_of_code)) {
    emitter.shared_lock = &lock;
    barrier_available = RegisterAsymmetricBarrier();

    region_bytes &= ~(SHARED_GRANULE - 1);
    u8* const first = AlignUp(block_of_code.getCurr<u8*>(), SHARED_GRANULE);
    u8* const last = AlignDown(const_cast<u8*>(block_of_code.GetCodeEnd()), SHARED_GRANULE);
    if (region_bytes < 2 * SHARED_MINIMUM_SPAN || first >= last || static_cast<size_t>(last - first) / region_bytes < 2) {
        throw std::invalid_argument("dynarmic: a shared code cache needs room for two regions of at least 8 MiB after its prelude");
    }
    for (u8* at = first; static_cast<size_t>(last - at) >= region_bytes; at += region_bytes) {
        Region region;
        region.begin = at;
        region.end = at + region_bytes;
        regions.push_back(region);
    }
    StartRegion(0);
}

SharedCodeCache::Impl::~Impl() = default;

void SharedCodeCache::Impl::Attach(SharedThreadState* thread) {
    std::lock_guard guard{attach_lock};
    thread->seen_generation = generation.load(std::memory_order_seq_cst);
    attached.push_back(thread);
}

void SharedCodeCache::Impl::Detach(SharedThreadState* thread) {
    std::lock_guard guard{attach_lock};
    locked_lookups_detached += thread->locked_lookups.load(std::memory_order_relaxed);
    attached.erase(std::remove(attached.begin(), attached.end(), thread), attached.end());
}

void SharedCodeCache::Impl::Sync(SharedThreadState& thread) {
    // The order is the argument (design section 6): publish the epoch first, seq-cst, then read
    // the generation. A retirement bumps the generation before the epoch, so a thread that
    // published the new epoch cannot then read the old generation and keep a stale table.
    for (;;) {
        const u64 e = epoch.load(std::memory_order_seq_cst);
        if (thread.running_epoch.load(std::memory_order_relaxed) == e) {
            break;  // already published (seq-cst) when it was stored, which precedes this read
        }
        thread.running_epoch.store(e, std::memory_order_seq_cst);
        if (epoch.load(std::memory_order_seq_cst) == e) {
            break;
        }
    }
    const u64 g = generation.load(std::memory_order_seq_cst);
    if (g != thread.seen_generation) {
        thread.flush_routing(&thread);
        thread.seen_generation = g;
    }
}

std::optional<CodePtr> SharedCodeCache::Impl::Lookup(IR::LocationDescriptor location) const {
    std::shared_lock guard{lock};
    if (const auto block = emitter.GetBasicBlock(location)) {
        return block->entrypoint;
    }
    return std::nullopt;
}

CodePtr SharedCodeCache::Impl::Emit(IR::LocationDescriptor location, const UserConfig& translator_conf, SharedThreadState& thread) {
    // Translate -- the frontend and the IR passes, most of the work, reading guest code through
    // this thread's callbacks -- without the lock, so other threads' lookups are not held up by
    // it. Only emission, which writes the shared buffer and maps, takes the lock. A location
    // another thread is already translating is waited for, not translated twice: when a burst of
    // new code reaches several threads at once, one translates it and the rest wait for it.
    std::unique_lock held{lock};
    for (;;) {
        if (const auto block = emitter.GetBasicBlock(location)) {
            translations_raced++;
            return block->entrypoint;
        }
        if (in_flight.count(location) == 0) {
            break;
        }
        // Another thread is translating it: a block takes microseconds, a sleep and a wake-up
        // take about as long again, so wait by spinning first -- without the lock -- and sleep
        // on the condition only if it takes longer.
        const u64 seen = in_flight_finished.load(std::memory_order_acquire);
        held.unlock();
        bool moved = false;
        for (int spin = 0; spin < 4096 && !moved; spin++) {
            std::this_thread::yield();
            moved = in_flight_finished.load(std::memory_order_acquire) != seen;
        }
        held.lock();
        if (!moved && in_flight.count(location) != 0) {
            in_flight_done.wait(held);
        }
    }
    in_flight.insert(location);
    const u64 serial_before = invalidation_serial.load(std::memory_order_relaxed);
    held.unlock();

    std::optional<IR::Block> ir_block;
    {
        // Whatever happens, the location stops being in flight and its waiters wake.
        SCOPE_EXIT {
            if (!held.owns_lock()) {
                held.lock();
            }
            in_flight.erase(location);
            in_flight_finished.fetch_add(1, std::memory_order_release);
            in_flight_done.notify_all();
        };
        const auto t0 = std::chrono::steady_clock::now();
        ir_block.emplace(TranslateBlock(location, translator_conf, polyfill_options));
        translate_ns.fetch_add(static_cast<u64>(std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now() - t0).count()), std::memory_order_relaxed);
        held.lock();
    }

    for (;;) {
        if (const auto block = emitter.GetBasicBlock(location)) {
            translations_raced++;
            return block->entrypoint;
        }
        // Guest code the block was translated from may have changed since: an invalidation
        // applied meanwhile that touches its range means translating it again, now, under the
        // lock (which invalidations also take).
        const A64::LocationDescriptor begin{ir_block->Location()};
        const A64::LocationDescriptor end{ir_block->EndLocation()};
        if (InvalidatedSince(serial_before, begin.PC(), end.PC())) {
            translations_redone++;
            ir_block.emplace(TranslateBlock(location, translator_conf, polyfill_options));
        }
        const u64 before = epoch.load(std::memory_order_relaxed);
        EnsureRoom(thread, held);
        // EnsureRoom may have let go of the lock to wait for a region; if the cache moved on
        // meanwhile, look again (another thread may have emitted the block, or the code changed).
        if (epoch.load(std::memory_order_relaxed) == before) {
            break;
        }
    }

    const auto t0 = std::chrono::steady_clock::now();
    const auto descriptor = emitter.Emit(*ir_block);
    emit_ns += static_cast<u64>(std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now() - t0).count());
    blocks_emitted++;
    code_bytes_emitted += descriptor.size;
    return descriptor.entrypoint;
}

bool SharedCodeCache::Impl::InvalidatedSince(u64 serial, u64 begin_pc, u64 end_pc) const {
    const u64 now = invalidation_serial.load(std::memory_order_relaxed);
    if (now == serial) {
        return false;
    }
    if (recent_invalidations.empty() || now - serial > recent_invalidations.size()) {
        return true;  // more happened than is remembered: assume one touched it
    }
    // The block covers [begin_pc, end_pc); an empty block (never emitted) still covers begin_pc.
    const auto block = boost::icl::discrete_interval<u64>::closed(begin_pc, std::max(begin_pc, end_pc - 1));
    for (const RecentInvalidation& r : recent_invalidations) {
        if (r.serial > serial && (r.entire || boost::icl::intersects(r.ranges, block))) {
            return true;
        }
    }
    return false;
}

void SharedCodeCache::Impl::Invalidate(bool entire, const boost::icl::interval_set<u64>& ranges) {
    std::unique_lock held{lock};
    invalidations++;
    // Remembered for translations running outside the lock (Emit), which must not publish a
    // block translated from code this changed.
    const u64 serial = invalidation_serial.load(std::memory_order_relaxed) + 1;
    if (recent_invalidations.size() == RECENT_INVALIDATIONS) {
        recent_invalidations.pop_front();
    }
    recent_invalidations.push_back(RecentInvalidation{serial, entire, entire ? boost::icl::interval_set<u64>{} : ranges});
    invalidation_serial.store(serial, std::memory_order_release);
    const size_t dropped = entire ? emitter.ForgetAllBlocks() : emitter.InvalidateCacheRangesCounted(ranges);
    if (dropped != 0) {
        blocks_invalidated += dropped;
        generation.fetch_add(1, std::memory_order_seq_cst);
    }
}

void SharedCodeCache::Impl::StartRegion(size_t index) {
    Region& r = regions[index];
    // The largest span free of holes; a fresh region has none.
    u8* best_begin = r.begin;
    u8* best_end = r.begin;
    u8* cursor = r.begin;
    auto holes = r.holes;
    std::sort(holes.begin(), holes.end());
    for (const auto& [hole_begin, hole_end] : holes) {
        if (hole_begin - cursor > best_end - best_begin) {
            best_begin = cursor;
            best_end = hole_begin;
        }
        cursor = std::max(cursor, hole_end);
    }
    if (r.end - cursor > best_end - best_begin) {
        best_begin = cursor;
        best_end = r.end;
    }
    r.use_begin = best_begin;
    r.use_end = best_end;
    r.code_committed_end = r.use_begin;
    r.slot_top = r.use_end;
    r.slots_committed_begin = r.use_end;
    r.state = Region::State::Current;
    current = index;
    block_of_code.SetCodePtr(r.use_begin);
}

void SharedCodeCache::Impl::EnsureRoom(SharedThreadState& thread, std::unique_lock<SharedCodeLock>& held) {
    for (;;) {
        if (current != NO_REGION) {
            Region& r = regions[current];
            u8* const cur = block_of_code.getCurr<u8*>();
            if (r.slot_top - cur >= static_cast<ptrdiff_t>(SHARED_HEADROOM)) {
                u8* const want = std::min(AlignUp(cur + MINIMUM_REMAINING_CODESIZE, SHARED_GRANULE), r.slots_committed_begin);
                if (want > r.code_committed_end) {
                    block_of_code.CommitRange(r.code_committed_end, static_cast<size_t>(want - r.code_committed_end));
                    r.code_committed_end = want;
                }
                return;
            }
            RetireCurrentRegion(thread);
        }
        TryReclaimRetired();
        for (size_t i = 0; i < regions.size(); i++) {
            if (regions[i].state == Region::State::Free) {
                StartRegion(i);
                break;
            }
        }
        if (current != NO_REGION) {
            continue;
        }
        // Every region is retired and something still holds each one: a thread executing code
        // it entered before the retirement. Every attached thread has been asked to halt, and a
        // thread outside generated code does not hold a region, so this ends -- but not with the
        // lock held, which a running thread may need to reach its halt check.
        held.unlock();
        std::this_thread::yield();
        held.lock();
    }
}

void SharedCodeCache::Impl::RetireCurrentRegion(SharedThreadState& thread) {
    Region& r = regions[current];
    const size_t dropped = emitter.ForgetAllBlocks();
    blocks_invalidated += dropped;
    generation.fetch_add(1, std::memory_order_seq_cst);
    r.retired_epoch = epoch.fetch_add(1, std::memory_order_seq_cst) + 1;
    r.state = Region::State::Retired;
    regions_retired++;
    retired_now.fetch_add(1, std::memory_order_relaxed);
    current = NO_REGION;

    // Ask every thread to leave generated code, so the region is released sooner -- and so that a
    // thread parked in a callback leaves through its block's halt test as soon as it returns,
    // touching nothing but the few bytes after its return site (see TryReclaim).
    {
        std::lock_guard guard{attach_lock};
        for (SharedThreadState* other : attached) {
            if (other != &thread) {
                Atomic::Or(&other->jit_state->halt_reason, static_cast<u32>(HaltReason::CacheInvalidation));
            }
        }
    }

    // This thread is in the dispatcher's lookup or at Run's entry: its stack holds nothing of the
    // region but its RSB and table might, so it catches up now and holds no region any more.
    Sync(thread);
}

void SharedCodeCache::Impl::TryReclaimRetired() {
    bool any_retired = false;
    for (const Region& r : regions) {
        any_retired |= r.state == Region::State::Retired;
    }
    if (!any_retired) {
        return;
    }

    // Where each thread parked inside a callback will return to. Read only after the asymmetric
    // barrier: a thread's plain store clearing its site (it returned) must be visible before the
    // site is believed, and a thread that stores its site after the barrier reads the halt word
    // after it too, so it leaves through its halt test (design section 6).
    const bool known = barrier_available && AsymmetricBarrier();
    for (Region& r : regions) {
        if (r.state == Region::State::Retired) {
            TryReclaim(r, known);
        }
    }
}

bool SharedCodeCache::Impl::TryReclaim(Region& r, bool parked_sites_known) {
    std::vector<std::pair<u8*, u8*>> holes;
    {
        std::lock_guard guard{attach_lock};
        for (SharedThreadState* t : attached) {
            const u64 e = t->running_epoch.load(std::memory_order_seq_cst);
            if (e == NOT_RUNNING || e >= r.retired_epoch) {
                continue;  // outside RunCode, or entered (and synced) since the retirement
            }
            if (parked_sites_known) {
                const u64 site = std::atomic_ref<u64>{t->jit_state->od_callback_return}.load(std::memory_order_seq_cst);
                if (site != 0) {
                    u8* const at = reinterpret_cast<u8*>(site);
                    if (at >= r.begin && at < r.end) {
                        holes.emplace_back(AlignDown(at - SHARED_HOLE_BEFORE, 4096), AlignUp(at + SHARED_HOLE_AFTER, 4096));
                    }
                    continue;  // parked in a callback, halted: it will leave through its block's tail
                }
            }
            return false;  // may still be executing code in this region
        }
    }

    // Keep the holes committed and untouched; give the rest back.
    std::sort(holes.begin(), holes.end());
    u8* cursor = r.begin;
    for (const auto& [hole_begin, hole_end] : holes) {
        const u8* const b = std::max(hole_begin, r.begin);
        if (b > cursor) {
            block_of_code.DecommitRange(cursor, static_cast<size_t>(b - cursor));
            emitter.PurgeFastmemPatchInfo(cursor, b);
        }
        cursor = std::max(cursor, std::min(hole_end, r.end));
    }
    if (r.end > cursor) {
        block_of_code.DecommitRange(cursor, static_cast<size_t>(r.end - cursor));
        emitter.PurgeFastmemPatchInfo(cursor, r.end);
    }
    r.holes = std::move(holes);

    // Usable again only if a large enough span is free of holes.
    u8* best = r.begin;
    size_t best_len = 0;
    u8* c = r.begin;
    for (const auto& [hole_begin, hole_end] : r.holes) {
        if (hole_begin > c && static_cast<size_t>(hole_begin - c) > best_len) {
            best_len = static_cast<size_t>(hole_begin - c);
            best = c;
        }
        c = std::max(c, hole_end);
    }
    if (r.end > c && static_cast<size_t>(r.end - c) > best_len) {
        best_len = static_cast<size_t>(r.end - c);
        best = c;
    }
    (void)best;
    if (best_len < SHARED_MINIMUM_SPAN) {
        return false;
    }
    r.state = Region::State::Free;
    regions_reclaimed++;
    retired_now.fetch_sub(1, std::memory_order_relaxed);
    return true;
}

void SharedCodeCache::Impl::ReclaimSoon() {
    const u64 now = static_cast<u64>(std::chrono::duration_cast<std::chrono::nanoseconds>(
                                         std::chrono::steady_clock::now().time_since_epoch())
                                         .count());
    u64 last = last_reclaim_attempt_ns.load(std::memory_order_relaxed);
    if (now - last < SHARED_RECLAIM_INTERVAL_NS || !last_reclaim_attempt_ns.compare_exchange_strong(last, now)) {
        return;
    }
    std::unique_lock held{lock, std::try_to_lock};
    if (held) {
        TryReclaimRetired();
    }
}

SharedCodeCache::Stats SharedCodeCache::Impl::GetStats() const {
    std::shared_lock guard{lock};
    SharedCodeCache::Stats s;
    s.blocks_emitted = blocks_emitted;
    s.code_bytes_emitted = code_bytes_emitted;
    s.translations_raced = translations_raced;
    s.translations_redone = translations_redone;
    s.translate_ns = translate_ns.load(std::memory_order_relaxed);
    s.emit_ns = emit_ns;
    {
        std::lock_guard attach_guard{attach_lock};
        s.locked_lookups = locked_lookups_detached;
        for (const SharedThreadState* t : attached) {
            s.locked_lookups += t->locked_lookups.load(std::memory_order_relaxed);
        }
    }
    s.invalidations = invalidations;
    s.blocks_invalidated = blocks_invalidated;
    s.generation = generation.load(std::memory_order_relaxed);
    s.regions_total = regions.size();
    s.regions_retired = regions_retired;
    s.regions_reclaimed = regions_reclaimed;
    // The prelude's commit, up to where the regions start (Windows; 0 where pages come on first
    // touch), then each region's: its code and slot spans unless it has been given back, and the
    // holes kept for parked threads.
    u64 committed = std::min<u64>(block_of_code.PreludeCommittedBytes(),
                                  static_cast<u64>(regions.front().begin - block_of_code.getCode()));
    for (const Region& r : regions) {
        if (r.state == Region::State::Retired) {
            s.regions_pinned++;
        }
        if (r.state != Region::State::Free) {
            committed += static_cast<u64>(r.code_committed_end - r.use_begin) + static_cast<u64>(r.use_end - r.slots_committed_begin);
        }
        for (const auto& [hole_begin, hole_end] : r.holes) {
            committed += static_cast<u64>(hole_end - hole_begin);
        }
    }
    s.committed_bytes = committed;
    {
        std::lock_guard attach_guard{attach_lock};
        s.attached = attached.size();
    }
    return s;
}

SharedCodeCache::SharedCodeCache(const UserConfig& template_config, std::size_t total_bytes, std::size_t region_bytes)
        : impl(std::make_unique<Impl>(template_config, total_bytes, region_bytes)) {}

SharedCodeCache::~SharedCodeCache() = default;

SharedCodeCache::Stats SharedCodeCache::GetStats() const {
    return impl->GetStats();
}

void SharedCodeCache::InvalidateCacheRange(std::uint64_t start_address, std::size_t length) {
    if (length == 0) {
        return;
    }
    boost::icl::interval_set<u64> ranges;
    ranges.add(boost::icl::discrete_interval<u64>::closed(start_address, static_cast<u64>(start_address + length - 1)));
    impl->Invalidate(false, ranges);
}

void SharedCodeCache::ClearCache() {
    impl->Invalidate(true, {});
}

// ------------------------------------------------------------------------------------------------

Jit::Jit(UserConfig conf)
        : impl(std::make_unique<Jit::Impl>(this, conf)) {}

Jit::~Jit() = default;

HaltReason Jit::Run() {
    return impl->Run();
}

HaltReason Jit::Step() {
    return impl->Step();
}

void Jit::ClearCache() {
    impl->ClearCache();
}

void Jit::InvalidateCacheRange(u64 start_address, size_t length) {
    impl->InvalidateCacheRange(start_address, length);
}

void Jit::Reset() {
    impl->Reset();
}

void Jit::HaltExecution(HaltReason hr) {
    impl->HaltExecution(hr);
}

void Jit::ClearHalt(HaltReason hr) {
    impl->ClearHalt(hr);
}

u64 Jit::GetSP() const {
    return impl->GetSP();
}

void Jit::SetSP(u64 value) {
    impl->SetSP(value);
}

u64 Jit::GetPC() const {
    return impl->GetPC();
}

void Jit::SetPC(u64 value) {
    impl->SetPC(value);
}

u64 Jit::GetRegister(size_t index) const {
    return impl->GetRegister(index);
}

void Jit::SetRegister(size_t index, u64 value) {
    impl->SetRegister(index, value);
}

std::array<u64, 31> Jit::GetRegisters() const {
    return impl->GetRegisters();
}

void Jit::SetRegisters(const std::array<u64, 31>& value) {
    impl->SetRegisters(value);
}

Vector Jit::GetVector(size_t index) const {
    return impl->GetVector(index);
}

void Jit::SetVector(size_t index, Vector value) {
    impl->SetVector(index, value);
}

std::array<Vector, 32> Jit::GetVectors() const {
    return impl->GetVectors();
}

void Jit::SetVectors(const std::array<Vector, 32>& value) {
    impl->SetVectors(value);
}

u32 Jit::GetFpcr() const {
    return impl->GetFpcr();
}

void Jit::SetFpcr(u32 value) {
    impl->SetFpcr(value);
}

u32 Jit::GetFpsr() const {
    return impl->GetFpsr();
}

void Jit::SetFpsr(u32 value) {
    impl->SetFpsr(value);
}

u32 Jit::GetPstate() const {
    return impl->GetPstate();
}

void Jit::SetPstate(u32 value) {
    impl->SetPstate(value);
}

void Jit::ClearExclusiveState() {
    impl->ClearExclusiveState();
}

bool Jit::IsExecuting() const {
    return impl->IsExecuting();
}

void Jit::DumpDisassembly() const {
    return impl->DumpDisassembly();
}

std::vector<std::string> Jit::Disassemble() const {
    return impl->Disassemble();
}

}  // namespace Dynarmic::A64
