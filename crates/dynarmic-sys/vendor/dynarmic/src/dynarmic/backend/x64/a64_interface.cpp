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
/// The smallest region: a block never needs more than MINIMUM_REMAINING_CODESIZE.
constexpr size_t SHARED_MINIMUM_REGION = 8 * MINIMUM_REMAINING_CODESIZE;
/// How often a thread leaving Run may try to give retired regions back.
constexpr u64 SHARED_RECLAIM_INTERVAL_NS = 10'000'000;

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

}  // namespace

struct SharedCodeCache::Impl final {
    Impl(const UserConfig& template_conf, size_t total_bytes, size_t region_bytes, size_t live_bytes);
    ~Impl();

    /// A span of the buffer after the prelude, filled from `begin` (blocks and their link slots).
    /// Patch 0028: Current (being filled) -> Full (still live) -> Retired (its blocks forgotten,
    /// waiting for no thread to hold it) -> Free.
    struct Region {
        u8* begin = nullptr;
        u8* end = nullptr;
        u8* code_committed_end = nullptr;
        enum class State { Free,
                           Current,
                           Full,
                           Retired } state = State::Free;
        u64 retired_epoch = 0;
        /// Patch 0028: when it started being filled (live regions are retired in this order), and
        /// the emitter's link-record and guest-range serials then: the records and ranges of the
        /// blocks emitted into it run from these to the next live region's.
        u64 sequence = 0;
        u32 first_link = 0;
        u32 first_range = 0;
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
    /// Patch 0028: how many regions may be live (Current or Full) at once.
    size_t live_limit = 0;
    u64 next_sequence = 0;
    std::atomic<u64> generation{0};
    std::atomic<u64> epoch{0};
    /// Regions retired and not yet given back; when non-zero, a thread leaving Run tries.
    std::atomic<u64> retired_now{0};
    std::atomic<u64> last_reclaim_attempt_ns{0};

    mutable std::mutex attach_lock;
    std::vector<SharedThreadState*> attached;

    // Under `lock`, exclusive.
    u64 blocks_emitted = 0;
    u64 code_bytes_emitted = 0;
    std::atomic<u64> translations_raced{0};
    u64 invalidations = 0;
    u64 blocks_invalidated = 0;
    u64 regions_retired = 0;
    u64 regions_reclaimed = 0;
    // Patch 0028.
    u64 regions_evicted = 0;
    u64 blocks_evicted = 0;
    u64 blocks_reemitted = 0;
    u64 evict_ns = 0;
    u64 evict_max_ns = 0;
    /// The locations the latest eviction forgot, for `blocks_reemitted`: how much of a retired
    /// region was still in use shows as its blocks being translated again.
    tsl::robin_set<u64> last_evicted;
    std::vector<u64> evicted_scratch;
    u64 translations_redone = 0;
    u64 parked_redirected = 0;  // parked threads sent to svc_resume_retired
    u64 reclaim_attempts = 0;
    u64 emit_ns = 0;                      // under `lock`, exclusive
    std::atomic<u64> translate_ns{0};     // outside it, summed over threads
    /// Dispatcher lookups that took the lock, from threads that have detached (each attached
    /// thread counts its own, so the count costs no shared cache line). Under `attach_lock`.
    u64 locked_lookups_detached = 0;

    /// Every applied invalidation bumps this; a translation made outside the lock compares it.
    std::atomic<u64> invalidation_serial{0};
    /// Locations a thread is translating outside the cache's lock, under a lock of their own, and
    /// the condition a thread that needs one of them waits on.
    std::mutex in_flight_lock;
    tsl::robin_set<IR::LocationDescriptor> in_flight;
    std::condition_variable in_flight_done;
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
    SharedCodeCache::Tables GetTables() const;
    /// Called by a thread that has just left RunCode: every attached thread was asked to halt when
    /// a region was retired, so this is when the last holder lets go. Throttled, and skipped if
    /// the lock is busy.
    void ReclaimSoon();

private:
    void EnsureRoom(SharedThreadState& thread, std::unique_lock<SharedCodeLock>& held);
    void StartRegion(size_t index);
    /// Patch 0028: the current region is full; it stays live, and the oldest live regions are
    /// retired until starting another keeps within `live_limit`.
    void FillCurrentRegion(SharedThreadState* thread);
    /// Patch 0028: forget the blocks of the oldest Full region and retire it. False if none.
    bool EvictOldest(SharedThreadState* thread);
    /// Mark `region` retired at a new epoch, and ask every other attached thread to leave
    /// generated code; `thread`, if given, is the caller's, in the dispatcher.
    void Retire(Region& region, SharedThreadState* thread);
    /// Forget every block and retire every Full region (they hold none now): ClearCache, and the
    /// emitter's serials running out.
    void ForgetEverything(SharedThreadState* thread);
    size_t LiveRegions() const;
    void TryReclaimRetired();
    bool TryReclaim(Region& region);
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

SharedCodeCache::Impl::Impl(const UserConfig& template_conf, size_t total_bytes, size_t region_bytes, size_t live_bytes)
        : conf(SharedTemplate(template_conf, total_bytes))
        , block_of_code(GenSharedRunCodeCallbacks(conf.callbacks, conf), JitStateInfo{layout}, total_bytes, GenRCP(conf))
        , emitter(block_of_code, conf, nullptr, true)
        , polyfill_options(GenPolyfillOptions(block_of_code)) {
    emitter.shared_lock = &lock;

    region_bytes &= ~(SHARED_GRANULE - 1);
    u8* const first = AlignUp(block_of_code.getCurr<u8*>(), SHARED_GRANULE);
    u8* const last = AlignDown(const_cast<u8*>(block_of_code.GetCodeEnd()), SHARED_GRANULE);
    if (region_bytes < SHARED_MINIMUM_REGION || first >= last || static_cast<size_t>(last - first) / region_bytes < 2) {
        throw std::invalid_argument("dynarmic: a shared code cache needs room for two regions of at least 8 MiB after its prelude");
    }
    for (u8* at = first; static_cast<size_t>(last - at) >= region_bytes; at += region_bytes) {
        Region region;
        region.begin = at;
        region.end = at + region_bytes;
        regions.push_back(region);
    }
    // Patch 0028: all but one region may be live by default -- the spare is what the next region
    // is started in while the one just retired waits for its last holder.
    const size_t most = regions.size() - 1;
    live_limit = live_bytes == 0 ? most : std::clamp<size_t>(live_bytes / region_bytes, 1, most);
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
    // this thread's callbacks -- without the cache's lock, so other threads' lookups are not held
    // up by it. Only emission, which writes the shared buffer and maps, takes the lock. A location
    // another thread is already translating is waited for, not translated twice: when a burst of
    // new code reaches several threads at once, one translates it and the rest wait for it -- on a
    // lock of its own, not the cache's, so that waiting does not queue behind emission (MEASURED on
    // the 4-core Linux host: waiters retaking the cache's writer-preferring lock doubled the cold
    // pass of eight threads).
    bool waited = false;
    for (;;) {
        if (waited) {
            // Still in the dispatcher, holding nothing of any region: say so, so that waiting here
            // does not keep a retired region from being given back.
            Sync(thread);
        }
        if (const auto found = Lookup(location)) {
            if (waited) {
                translations_raced.fetch_add(1, std::memory_order_relaxed);
            }
            return *found;
        }
        std::unique_lock flight{in_flight_lock};
        if (in_flight.count(location) == 0) {
            in_flight.insert(location);
            break;
        }
        // A block takes microseconds to translate, and a sleep and a wake-up about as long again:
        // spin a little first, then sleep until this location is no longer in flight.
        waited = true;
        const u64 seen = in_flight_finished.load(std::memory_order_acquire);
        flight.unlock();
        bool moved = false;
        for (int spin = 0; spin < 64 && !moved; spin++) {
            std::this_thread::yield();
            moved = in_flight_finished.load(std::memory_order_acquire) != seen;
        }
        flight.lock();
        in_flight_done.wait(flight, [&] { return in_flight.count(location) == 0; });
    }
    // Whatever happens, the location stops being in flight -- after the block is registered, as
    // `held` below is released first -- and its waiters wake.
    SCOPE_EXIT {
        {
            std::lock_guard flight{in_flight_lock};
            in_flight.erase(location);
            in_flight_finished.fetch_add(1, std::memory_order_release);
        }
        in_flight_done.notify_all();
    };

    const u64 serial_before = invalidation_serial.load(std::memory_order_acquire);
    std::optional<IR::Block> ir_block;
    {
        const auto t0 = std::chrono::steady_clock::now();
        ir_block.emplace(TranslateBlock(location, translator_conf, polyfill_options));
        translate_ns.fetch_add(static_cast<u64>(std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now() - t0).count()), std::memory_order_relaxed);
    }

    std::unique_lock held{lock};
    for (;;) {
        if (const auto block = emitter.GetBasicBlock(location)) {
            translations_raced.fetch_add(1, std::memory_order_relaxed);
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
    if (!last_evicted.empty() && last_evicted.erase(location.Value()) != 0) {
        blocks_reemitted++;
    }
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
    if (entire) {
        ForgetEverything(nullptr);
        return;
    }
    const size_t dropped = emitter.InvalidateCacheRangesCounted(ranges);
    if (dropped != 0) {
        blocks_invalidated += dropped;
        generation.fetch_add(1, std::memory_order_seq_cst);
    }
}

void SharedCodeCache::Impl::ForgetEverything(SharedThreadState* thread) {
    blocks_invalidated += emitter.ForgetAllBlocks();
    generation.fetch_add(1, std::memory_order_seq_cst);
    last_evicted = {};
    // Patch 0028: the emitter's serials start again from 0; the region being filled goes on from
    // there, and the full ones, whose blocks are all forgotten, are given back.
    if (current != NO_REGION) {
        regions[current].first_link = 0;
        regions[current].first_range = 0;
    }
    for (Region& r : regions) {
        if (r.state == Region::State::Full) {
            Retire(r, thread);
        }
    }
    TryReclaimRetired();
}

void SharedCodeCache::Impl::StartRegion(size_t index) {
    Region& r = regions[index];
    r.code_committed_end = r.begin;
    r.state = Region::State::Current;
    r.sequence = next_sequence++;
    r.first_link = emitter.NextLinkSerial();    // patch 0028
    r.first_range = emitter.NextRangeSerial();  // patch 0028
    current = index;
    block_of_code.SetCodePtr(r.begin);
    emitter.BeginFastmemSites(r.begin, r.end);  // patch 0025
}

size_t SharedCodeCache::Impl::LiveRegions() const {
    size_t live = 0;
    for (const Region& r : regions) {
        live += r.state == Region::State::Current || r.state == Region::State::Full;
    }
    return live;
}

void SharedCodeCache::Impl::EnsureRoom(SharedThreadState& thread, std::unique_lock<SharedCodeLock>& held) {
    for (;;) {
        if (current != NO_REGION) {
            Region& r = regions[current];
            u8* const cur = block_of_code.getCurr<u8*>();
            if (r.end - cur >= static_cast<ptrdiff_t>(SHARED_HEADROOM)) {
                u8* const want = std::min(AlignUp(cur + MINIMUM_REMAINING_CODESIZE, SHARED_GRANULE), r.end);
                if (want > r.code_committed_end) {
                    block_of_code.CommitRange(r.code_committed_end, static_cast<size_t>(want - r.code_committed_end));
                    r.code_committed_end = want;
                }
                return;
            }
            FillCurrentRegion(&thread);
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
        // No region is free: the rest are live, or retired and still held. With nothing retired,
        // make room by retiring the oldest live one; with something retired, wait for it rather
        // than retire more.
        bool any_retired = false;
        for (const Region& r : regions) {
            any_retired |= r.state == Region::State::Retired;
        }
        if (!any_retired && EvictOldest(&thread)) {
            continue;
        }
        // Every region still retired is held by a thread executing code it entered before the
        // retirement. Every attached thread has been asked to halt, and a thread outside
        // generated code does not hold a region, so this ends -- but not with the lock held,
        // which a running thread may need to reach its halt check.
        held.unlock();
        Sync(thread);  // in the dispatcher: holds no region, so it must not pin one while it waits
        std::this_thread::yield();
        held.lock();
    }
}

void SharedCodeCache::Impl::FillCurrentRegion(SharedThreadState* thread) {
    regions[current].state = Region::State::Full;
    current = NO_REGION;
    // The emitter's link and range serials are 32 bits and, with blocks forgotten a region at a
    // time, never start again by themselves: long before they would run out (some billions of
    // blocks), they are started again by forgetting every block.
    constexpr u32 SERIALS_RUNNING_OUT = 0xC000'0000;
    if (emitter.NextLinkSerial() >= SERIALS_RUNNING_OUT || emitter.NextRangeSerial() >= SERIALS_RUNNING_OUT) {
        ForgetEverything(thread);
        return;
    }
    // The region about to be started counts as live.
    while (LiveRegions() + 1 > live_limit && EvictOldest(thread)) {
    }
}

bool SharedCodeCache::Impl::EvictOldest(SharedThreadState* thread) {
    Region* oldest = nullptr;
    for (Region& r : regions) {
        if (r.state == Region::State::Full && (oldest == nullptr || r.sequence < oldest->sequence)) {
            oldest = &r;
        }
    }
    if (oldest == nullptr) {
        return false;
    }
    // Its blocks were emitted -- their records and ranges made -- from its first serials up to the
    // next live region's (the next oldest, or the current one), or up to now if there is none.
    const Region* next = nullptr;
    for (const Region& r : regions) {
        if ((r.state == Region::State::Full || r.state == Region::State::Current) && r.sequence > oldest->sequence
            && (next == nullptr || r.sequence < next->sequence)) {
            next = &r;
        }
    }
    const u32 end_link = next ? next->first_link : emitter.NextLinkSerial();
    const u32 end_range = next ? next->first_range : emitter.NextRangeSerial();

    const auto t0 = std::chrono::steady_clock::now();
    evicted_scratch.clear();
    const size_t dropped = emitter.ForgetRegionBlocks(oldest->begin, oldest->end, oldest->first_range, end_range, evicted_scratch);
    // Every record and range below the next live region's serials is dead now: the blocks they
    // named were this region's, an older region's, or invalidated.
    emitter.TrimLinkRecords(end_link);
    emitter.TrimGuestRanges(end_range);
    last_evicted = {};
    last_evicted.insert(evicted_scratch.begin(), evicted_scratch.end());
    if (evicted_scratch.capacity() > 2 * evicted_scratch.size() + 4096) {
        std::vector<u64>{}.swap(evicted_scratch);
    }
    const u64 ns = static_cast<u64>(std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now() - t0).count());
    evict_ns += ns;
    evict_max_ns = std::max(evict_max_ns, ns);
    blocks_evicted += dropped;
    regions_evicted++;
    generation.fetch_add(1, std::memory_order_seq_cst);
    Retire(*oldest, thread);
    return true;
}

void SharedCodeCache::Impl::Retire(Region& r, SharedThreadState* thread) {
    // Its blocks are forgotten and the generation bumped (by the caller) before the epoch, so a
    // thread that publishes the new epoch then reads a generation that empties its tables.
    r.retired_epoch = epoch.fetch_add(1, std::memory_order_seq_cst) + 1;
    r.state = Region::State::Retired;
    regions_retired++;
    retired_now.fetch_add(1, std::memory_order_relaxed);

    // Ask every thread to leave generated code, so the region is released sooner -- and so that a
    // thread parked in a callback leaves through its block's halt test as soon as it returns,
    // touching nothing but the few bytes after its return site (see TryReclaim).
    {
        std::lock_guard guard{attach_lock};
        for (SharedThreadState* other : attached) {
            if (other != thread) {
                Atomic::Or(&other->jit_state->halt_reason, static_cast<u32>(HaltReason::CacheInvalidation));
            }
        }
    }

    // A thread in the dispatcher's lookup or at Run's entry: its stack holds nothing of the region
    // but its RSB and table might, so it catches up now and holds no region any more.
    if (thread) {
        Sync(*thread);
    }
}

void SharedCodeCache::Impl::TryReclaimRetired() {
    bool any_retired = false;
    for (const Region& r : regions) {
        any_retired |= r.state == Region::State::Retired;
    }
    if (!any_retired) {
        return;
    }

    reclaim_attempts++;
    for (Region& r : regions) {
        if (r.state == Region::State::Retired) {
            TryReclaim(r);
        }
    }
}

bool SharedCodeCache::Impl::TryReclaim(Region& r) {
    // Every attached thread must be unable to execute this region again: outside RunCode, or
    // entered (and synced) since the retirement, or parked in an SVC callback with its resume
    // address -- the only address into generated code it holds -- outside the region, or swapped
    // (compare-exchange against the thread's own take) for the prelude's `svc_resume_retired`.
    u64 redirected_now = 0;
    {
        std::lock_guard guard{attach_lock};
        for (SharedThreadState* t : attached) {
            const u64 e = t->running_epoch.load(std::memory_order_seq_cst);
            if (e == NOT_RUNNING || e >= r.retired_epoch) {
                continue;
            }
            std::atomic_ref<u64> site_ref{t->jit_state->od_callback_return};
            u64 site = site_ref.load(std::memory_order_seq_cst);
            if (site == 0) {
                return false;  // may be executing this region's code
            }
            u8* const at = reinterpret_cast<u8*>(site);
            if (at < r.begin || at >= r.end) {
                // Parked, and its block is elsewhere: on waking it finishes that block's tail,
                // whose halt test -- the retirement raised the halt -- leaves the run.
                continue;
            }
            const u64 resume = reinterpret_cast<u64>(emitter.SvcResumeRetired());
            if (!site_ref.compare_exchange_strong(site, resume, std::memory_order_seq_cst)) {
                return false;  // it woke and took its resume address: running this region
            }
            redirected_now++;
        }
    }
    parked_redirected += redirected_now;

    block_of_code.DecommitRange(r.begin, static_cast<size_t>(r.end - r.begin));
    emitter.PurgeFastmemPatchInfo(r.begin, r.end);
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
    s.translations_raced = translations_raced.load(std::memory_order_relaxed);
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
    s.regions_evicted = regions_evicted;
    s.blocks_evicted = blocks_evicted;
    s.blocks_reemitted = blocks_reemitted;
    s.evict_ns = evict_ns;
    s.evict_max_ns = evict_max_ns;
    s.regions_live = LiveRegions();
    s.regions_live_max = live_limit;
    // The prelude's commit, up to where the regions start (Windows; 0 where pages come on first
    // touch), then each region's code, unless it has been given back.
    u64 committed = std::min<u64>(block_of_code.PreludeCommittedBytes(),
                                  static_cast<u64>(regions.front().begin - block_of_code.getCode()));
    for (const Region& r : regions) {
        if (r.state == Region::State::Retired) {
            s.regions_pinned++;
        }
        if (r.state != Region::State::Free) {
            committed += static_cast<u64>(r.code_committed_end - r.begin);
        }
    }
    s.parked_redirected = parked_redirected;
    s.reclaim_attempts = reclaim_attempts;
    s.committed_bytes = committed;
    {
        std::lock_guard attach_guard{attach_lock};
        s.attached = attached.size();
    }
    return s;
}

SharedCodeCache::Tables SharedCodeCache::Impl::GetTables() const {
    std::shared_lock guard{lock};
    return emitter.Census();
}

SharedCodeCache::SharedCodeCache(const UserConfig& template_config, std::size_t total_bytes, std::size_t region_bytes, std::size_t live_bytes)
        : impl(std::make_unique<Impl>(template_config, total_bytes, region_bytes, live_bytes)) {}

SharedCodeCache::~SharedCodeCache() = default;

SharedCodeCache::Stats SharedCodeCache::GetStats() const {
    return impl->GetStats();
}

SharedCodeCache::Tables SharedCodeCache::GetTables() const {
    return impl->GetTables();
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
