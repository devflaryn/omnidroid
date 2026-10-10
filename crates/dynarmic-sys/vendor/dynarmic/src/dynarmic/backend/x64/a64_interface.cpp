/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstring>
#include <limits>
#include <memory>
#include <condition_variable>
#include <deque>
#include <cstdio>
#include <filesystem>
#include <mutex>
#include <optional>
#include <shared_mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <tuple>
#include <type_traits>
#include <vector>

#include <boost/icl/interval_set.hpp>
#include <mcl/assert.hpp>
#include <mcl/bit_cast.hpp>
#include <mcl/scope_exit.hpp>
#include <tsl/robin_map.h>
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
    // Patch 0037: under `check_halt_on_memory_access` upstream skips the pass altogether (every
    // guest register read a load from `JitState`, every write a store); the precise variant keeps
    // the state exact wherever the block can leave early, so it runs there too, while switched on.
    // And there a load is never dead code: it may fault, which is the point of the check.
    const bool precise = conf.check_halt_on_memory_access && live_precise_get_set.load(std::memory_order_relaxed) != 0;
    const Optimization::DeadCodeEliminationOptions dce{.keep_memory_reads = conf.check_halt_on_memory_access};
    if (conf.HasOptimization(OptimizationFlag::GetSetElimination) && (!conf.check_halt_on_memory_access || precise)) {
        // Patch 0081: `OMNI_JIT_GETSET_WIDTH=0` turns the width forwarding off.
        static const bool width = [] {
            const char* v = std::getenv("OMNI_JIT_GETSET_WIDTH");
            return v == nullptr || v[0] != '0';
        }();
        Optimization::A64GetSetElimination(ir_block, {.precise_at_memory_aborts = precise, .forward_width_changes = width});
        Optimization::DeadCodeElimination(ir_block, dce);
    }
    if (conf.HasOptimization(OptimizationFlag::ConstProp)) {
        Optimization::ConstantPropagation(ir_block);
        Optimization::DeadCodeElimination(ir_block, dce);
    }
    if (conf.HasOptimization(OptimizationFlag::MiscIROpt)) {
        Optimization::A64MergeInterpretBlocksPass(ir_block, conf.callbacks);
    }
    // Omnidroid patch 0077: the IR's consistency check (argument types, use counts) only when
    // asked for, `OMNI_JIT_VERIFY=1`: it asserts and changes nothing, and cost ~1.5% of first
    // translation on every block of every process.
    static const bool verify = [] {
        const char* v = std::getenv("OMNI_JIT_VERIFY");
        return v != nullptr && v[0] == '1';
    }();
    if (verify) {
        Optimization::VerificationPass(ir_block);
    }
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
    void GuestPcsOf(const u64* hosts, size_t count, u64* guest_pcs) const;
    /// Called by a thread that has just left RunCode: every attached thread was asked to halt when
    /// a region was retired, so this is when the last holder lets go. Throttled, and skipped if
    /// the lock is busy.
    void ReclaimSoon();
    /// Omnidroid patch 0050: retire the oldest full regions until at most `keep_bytes` of regions
    /// are live (the one being filled counts). How many were retired.
    size_t EvictTo(size_t keep_bytes);

    // Omnidroid patch 0070: translation snapshots (see SharedCodeCache::SaveSnapshot).
    void EnableSnapshots();
    s64 SaveSnapshot(const char* path, const char* key, u64 max_bytes, bool include_unverified);
    s64 LoadSnapshot(const char* path, const char* key, bool lazily);
    /// Patch 0076: SharedCodeCache::ForgetUnverified.
    s64 ForgetUnverified();
    u64 snapshot_forgotten = 0;
    size_t UnverifiedPcs(u64* out, size_t capacity) const {
        std::shared_lock guard{lock};
        size_t i = 0;
        for (const auto& [location, pending] : unverified) {
            if (i < capacity) {
                out[i] = A64::LocationDescriptor{IR::LocationDescriptor{location}}.PC();
            }
            i++;
        }
        return unverified.size();
    }
    /// The guest-range serials the last load registered (its restored blocks'), `[first, end)`.
    u32 restored_ranges_first = 0;
    u32 restored_ranges_end = 0;
    // Patch 0075: a snapshot restored lazily -- its code read in a page at a time, as blocks on it
    // are first entered. The file stays open for that.
    struct LazyRegion {
        size_t index;
        u64 file_offset;
        u64 used;
        std::vector<std::pair<u32, u32>> slots;  // (slot offset in the buffer, link serial), ascending
    };
    EmitX64::LazyPages lazy_pages;
    std::vector<LazyRegion> lazy_regions;
    std::FILE* lazy_file = nullptr;
    u64 snapshot_pages_read = 0;
    /// Read in the lazily restored pages of `[begin, end)` not read in yet, and set their slots.
    /// False if one could not be read.
    bool Materialize(const u8* begin, const u8* end);
    /// Forget the lazy state of `region` (it is being evicted, or every block forgotten).
    void DropLazy(size_t region);
    /// A restored block at `location`, unverified: its guest code read through `translator_conf`'s
    /// callbacks and compared. Its entry point if it is the same (and now entered), nothing if
    /// there is none or it was dropped.
    std::optional<CodePtr> VerifyRestored(IR::LocationDescriptor location, const UserConfig& translator_conf);
    /// What shapes emitted code besides the guest's: one hash.
    u64 CodeShape() const;
    /// Remember emitted blocks' guest-code hashes (under `lock`).
    bool snapshot_hashing = false;
    tsl::robin_map<u64, u64> block_hashes;  // location -> hash of the guest code emitted from
    struct Unverified {
        u64 first;
        u32 span;
        u64 hash;
    };
    tsl::robin_map<u64, Unverified> unverified;  // location -> what verifying needs (under `lock`)
    std::atomic<u64> unverified_count{0};        // non-zero while any may remain: Emit checks
    u64 snapshot_restored = 0;
    u64 snapshot_verified = 0;
    u64 snapshot_rejected = 0;
    u64 snapshot_save_lock_ns = 0;  // the latest save's time holding the lock (copying out)

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
                // Omnidroid patch 0035: the size the cache's emitter masks with.
                fast_dispatch_entries = A64EmitX64::FastDispatchEntries(shared->conf);
                fast_dispatch_table = std::make_unique<u8[]>(A64EmitX64::FastDispatchTableBytes(fast_dispatch_entries));
                A64EmitX64::ResetFastDispatchTable(fast_dispatch_table.get(), fast_dispatch_entries);
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
            A64EmitX64::ResetFastDispatchTable(fast_dispatch_table.get(), fast_dispatch_entries);
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
    size_t fast_dispatch_entries = 0;  ///< Patch 0035: its entries.

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

SharedCodeCache::Impl::~Impl() {
    if (lazy_file != nullptr) {
        std::fclose(lazy_file);
    }
}

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

// ------------------------------------------------------------------------------------------------
// Omnidroid patch 0070: translation snapshots.
//
// A shared cache's emitted code does not depend on where the cache is: a block reaches the prelude,
// the constant pool and its own link slots `rip`-relatively, and everything per-thread or
// per-process through JitState (patch 0022). MEASURED (`tests/code_size.rs`, OMNI_EMIT_DUMP2):
// 11,005 blocks emitted into two caches at different addresses, with different monitors, have the
// same bytes up to their link slots. What it does depend on is the prelude (whose thunks it calls
// by offset, and which holds the host's function addresses) and everything that shapes code --
// both hashed into the snapshot and compared before anything is installed. A block is restored at
// the same offset in the same region, its slots rewritten for the new buffer and unlinked, and
// entered only once its guest code reads back the same.
// ------------------------------------------------------------------------------------------------

namespace {

constexpr u64 SNAPSHOT_MAGIC = 0x3150414E534A444FULL;  // "ODJSNAP1"
constexpr u32 SNAPSHOT_VERSION = 1;

struct Fnv {
    u64 h = 0xCBF29CE484222325ULL;
    void Byte(u8 b) {
        h ^= b;
        h *= 0x100000001B3ULL;
    }
    void Bytes(const void* p, size_t n) {
        const u8* b = static_cast<const u8*>(p);
        for (size_t i = 0; i < n; i++) {
            Byte(b[i]);
        }
    }
    template<typename T>
    void Value(const T& v) {
        static_assert(std::is_trivially_copyable_v<T>);
        Bytes(&v, sizeof(v));
    }
};

/// The words of guest code `[first, end)` read through `conf`'s callbacks, hashed; nothing if one
/// cannot be read.
std::optional<u64> GuestCodeHash(const UserConfig& conf, u64 first, u64 end) {
    if (end < first || end - first > (1u << 24)) {
        return std::nullopt;
    }
    Fnv f;
    f.Value(first);
    f.Value(end - first);
    for (u64 at = first; at < end; at += 4) {
        const auto word = conf.callbacks->MemoryReadCode(at);
        if (!word) {
            return std::nullopt;
        }
        f.Value(*word);
    }
    return f.h;
}

/// Builds a snapshot in memory: under the cache's lock only for the copy, written to the file
/// after the lock is let go (writing a game's ~230 MiB under it held every guest thread up for as
/// long as the disk took).
class Writer {
public:
    explicit Writer(std::vector<u8>& out)
            : out(out) {}
    template<typename T>
    void Value(const T& v) {
        static_assert(std::is_trivially_copyable_v<T>);
        Bytes(&v, sizeof(v));
    }
    void Bytes(const void* p, size_t n) {
        const u8* b = static_cast<const u8*>(p);
        out.insert(out.end(), b, b + n);
    }

private:
    std::vector<u8>& out;
};

/// Reads a snapshot from its file as it parses -- the region bytes are skipped (their offsets
/// noted) and read straight into the code buffer, or (patch 0075) a page at a time when entered,
/// rather than the whole file first into memory.
class Reader {
public:
    explicit Reader(std::FILE* f)
            : f(f) {
#ifdef _WIN32
        _fseeki64(f, 0, SEEK_END);
        size = static_cast<u64>(_ftelli64(f));
        _fseeki64(f, 0, SEEK_SET);
#else
        fseeko(f, 0, SEEK_END);
        size = static_cast<u64>(ftello(f));
        fseeko(f, 0, SEEK_SET);
#endif
    }
    template<typename T>
    bool Value(T& v) {
        static_assert(std::is_trivially_copyable_v<T>);
        return Bytes(&v, sizeof(T));
    }
    // Omnidroid patch 0095: read through a buffer of its own, a large `fread` at a time -- the
    // records are a few bytes each, and an `fread` per field (with the stream's lock) was a fifth
    // of a snapshot's install.
    bool Bytes(void* out, size_t n) {
        if (size - at < n) {
            return false;
        }
        u8* to = static_cast<u8*>(out);
        while (n != 0) {
            if (pos == len) {
                if (n >= buffer.size()) {
                    if (std::fread(to, 1, n, f) != n) {
                        return false;
                    }
                    at += n;
                    return true;
                }
                pos = 0;
                len = std::fread(buffer.data(), 1, static_cast<size_t>(std::min<u64>(buffer.size(), size - at)), f);
                if (len == 0) {
                    return false;
                }
            }
            const size_t k = std::min(n, len - pos);
            std::memcpy(to, buffer.data() + pos, k);
            pos += k;
            to += k;
            n -= k;
            at += k;
        }
        return true;
    }
    bool Skip(u64 n) {
        if (size - at < n) {
            return false;
        }
        at += n;
        if (n <= len - pos) {
            pos += static_cast<size_t>(n);
            return true;
        }
        pos = len = 0;
#ifdef _WIN32
        return _fseeki64(f, static_cast<s64>(at), SEEK_SET) == 0;
#else
        return fseeko(f, static_cast<off_t>(at), SEEK_SET) == 0;
#endif
    }
    u64 Offset() const { return at; }
    bool AtEnd() const { return at == size; }

private:
    std::FILE* f;
    u64 size = 0;
    u64 at = 0;
    std::vector<u8> buffer = std::vector<u8>(1 << 20);  // patch 0095
    size_t pos = 0, len = 0;
};

/// Read `n` bytes at `offset` of `f` into `out`.
bool ReadAt(std::FILE* f, u64 offset, void* out, size_t n) {
#ifdef _WIN32
    if (_fseeki64(f, static_cast<s64>(offset), SEEK_SET) != 0) {
        return false;
    }
#else
    if (fseeko(f, static_cast<off_t>(offset), SEEK_SET) != 0) {
        return false;
    }
#endif
    return n == 0 || std::fread(out, 1, n, f) == n;
}

std::FILE* OpenFile(const char* path, bool write) {
    const std::filesystem::path p{std::u8string{reinterpret_cast<const char8_t*>(path)}};
#ifdef _WIN32
    return _wfopen(p.c_str(), write ? L"wb" : L"rb");
#else
    return std::fopen(p.c_str(), write ? "wb" : "rb");
#endif
}

struct SavedBlock {
    u64 location;
    u32 entry;
    u32 size;
    u64 first;
    u32 span;
    u64 hash;
    std::vector<EmitX64::SnapshotSlot> slots;
    std::vector<EmitX64::SnapshotSite> sites;
};

/// Omnidroid patch 0095: a block as a load reads it -- its slots and sites in its region's flat
/// arrays (two allocations a region, not two a block: the game's snapshot has 1.4 M blocks).
struct LoadedBlock {
    u64 location;
    u32 entry;
    u32 size;
    u64 first;
    u32 span;
    u64 hash;
    u32 slot_begin, slot_count;
    u32 site_begin, site_count;
};

struct LoadedRegion {
    u32 index;
    u64 used;
    u64 file_offset;
    std::vector<LoadedBlock> blocks;
    std::vector<EmitX64::SnapshotSlot> slots;
    std::vector<EmitX64::SnapshotSite> sites;
};

}  // namespace

u64 SharedCodeCache::Impl::CodeShape() const {
    Fnv f;
    f.Value(SNAPSHOT_VERSION);
    // The prelude's code (after the constant pool), which every block calls into by offset and
    // which holds the host's addresses: the same build, configuration and host, at the same
    // module addresses, generate the same bytes.
    const ConstantPool& pool = block_of_code.GetConstantPool();
    const u8* const buffer = block_of_code.getCode();
    const u8* const pool_end = static_cast<const u8*>(pool.Begin()) + pool.CapacityBytes();
    const u8* const prelude_end = static_cast<const u8*>(block_of_code.GetCodeBegin());
    f.Value(static_cast<u64>(static_cast<const u8*>(pool.Begin()) - buffer));
    f.Value(static_cast<u64>(pool_end - buffer));
    f.Value(static_cast<u64>(prelude_end - buffer));
    // The prelude's bytes, with every 64-bit immediate (`mov r64, imm64`) and every word that is
    // an address inside the buffer left out: those are its own addresses, its exception handler's
    // object and the host's function addresses, which differ from cache to cache and which no block
    // reads. What is hashed is its code -- every thunk at the same offset, doing the same thing.
    {
        const u64 lo = reinterpret_cast<u64>(buffer);
        const u64 hi = lo + block_of_code.GetTotalCodeSize();
        const size_t n = static_cast<size_t>(prelude_end - pool_end);
        size_t i = 0;
        while (i < n) {
            const u8 b = pool_end[i];
            if (i + 10 <= n && (b == 0x48 || b == 0x49) && pool_end[i + 1] >= 0xB8 && pool_end[i + 1] <= 0xBF) {
                f.Byte(b);
                f.Byte(pool_end[i + 1]);
                f.Byte(0x5A);
                i += 10;
                continue;
            }
            if (i + 8 <= n) {
                u64 word = 0;
                std::memcpy(&word, pool_end + i, 8);
                if ((word >= lo && word < hi) || (u64{0} - word >= lo && u64{0} - word < hi)) {
                    f.Byte(0xA5);
                    i += 8;
                    continue;
                }
            }
            f.Byte(b);
            i++;
        }
    }
    // What a block's own absolute addresses are: the host functions it calls (an `mov rax, imm64;
    // call rax` each), in this executable and the C runtime -- the same addresses only where these
    // are -- and only while the buffer is out of `call rel32` reach of them, or the instruction
    // chosen, and its bytes, would depend on where the buffer is.
    {
        const u64 functions[] = {
            reinterpret_cast<u64>(&GuestCodeHash),
            reinterpret_cast<u64>(&Backend::X64::NoteTbiTaggedSite),
            reinterpret_cast<u64>(static_cast<void* (*)(void*, const void*, size_t)>(&std::memcpy)),
            reinterpret_cast<u64>(static_cast<void* (*)(void*, const void*, size_t)>(&std::memmove)),
            reinterpret_cast<u64>(static_cast<double (*)(double, double, double)>(&std::fma)),
            reinterpret_cast<u64>(static_cast<double (*)(double)>(&std::sqrt)),
        };
        const u64 lo = reinterpret_cast<u64>(buffer);
        const u64 hi = lo + block_of_code.GetTotalCodeSize();
        bool close_by = false;
        for (const u64 fn : functions) {
            f.Value(fn);
            const u64 distance = fn < lo ? hi - fn : fn - lo;
            close_by |= distance < (u64{1} << 32);
        }
        if (close_by) {
            f.Value(lo);  // then only a cache at this very address has this shape
        }
    }
    f.Value(static_cast<u64>(block_of_code.HostFeatureBits()));
    f.Value(block_of_code.GetTotalCodeSize());
    f.Value(static_cast<u64>(regions.size()));
    f.Value(static_cast<u64>(regions.front().begin - buffer));
    f.Value(static_cast<u64>(regions.front().end - regions.front().begin));
    f.Value(static_cast<u64>(live_limit));
    // Everything a block's code depends on besides the guest's: the configuration that shapes code
    // (as EmitsTheSameCode compares it) and the switches read at emit time.
    f.Value(static_cast<u32>(conf.optimizations));
    f.Value(conf.unsafe_optimizations);
    f.Value(conf.hook_data_cache_operations);
    f.Value(conf.hook_isb);
    f.Value(conf.hook_hint_instructions);
    f.Value(conf.cntfrq_el0);
    f.Value(conf.ctr_el0);
    f.Value(conf.dczid_el0);
    f.Value(conf.tpidrro_el0 == nullptr);
    f.Value(conf.tpidr_el0 == nullptr);
    f.Value(conf.fastmem_pointer.has_value());
    f.Value(static_cast<u64>(conf.fastmem_pointer.value_or(0)));
    f.Value(conf.fastmem_address_space_bits);
    f.Value(conf.silently_mirror_fastmem);
    f.Value(conf.fastmem_exclusive_access);
    f.Value(conf.define_unpredictable_behaviour);
    f.Value(conf.wall_clock_cntpct);
    f.Value(conf.check_halt_on_memory_access);
    f.Value(conf.enable_cycle_counting);
    f.Value(conf.global_monitor == nullptr);
    f.Value(live_fp_optimizations.load(std::memory_order_relaxed));
    f.Value(live_precise_get_set.load(std::memory_order_relaxed));
    f.Value(live_fast_dispatch_inline.load(std::memory_order_relaxed));
    f.Value(live_scalar_fp_in_xmm.load(std::memory_order_relaxed));
    f.Value(live_fastmem_mask_by_and.load(std::memory_order_relaxed));
    f.Value(live_fastmem_tbi_unmasked.load(std::memory_order_relaxed));
    f.Value(live_compact_code.load(std::memory_order_relaxed));
    return f.h;
}

void SharedCodeCache::Impl::EnableSnapshots() {
    std::unique_lock held{lock};
    snapshot_hashing = true;
}

s64 SharedCodeCache::Impl::SaveSnapshot(const char* path, const char* key, u64 max_bytes, bool include_unverified) {
    std::unique_lock held{lock};
    if (!snapshot_hashing) {
        return -2;
    }
    u8* const buffer = const_cast<u8*>(block_of_code.getCode());

    // Every live block, in emission order: the newest range of each location names it.
    std::vector<std::tuple<u64, u64, u32>> ranges;
    emitter.SnapshotRanges(ranges);
    tsl::robin_map<u64, size_t> newest;
    for (size_t i = 0; i < ranges.size(); i++) {
        newest[std::get<0>(ranges[i])] = i;
    }
    std::vector<size_t> live;
    for (size_t i = 0; i < regions.size(); i++) {
        if (regions[i].state == Region::State::Current || regions[i].state == Region::State::Full) {
            live.push_back(i);
        }
    }
    std::sort(live.begin(), live.end(), [this](size_t a, size_t b) { return regions[a].sequence < regions[b].sequence; });
    std::vector<std::vector<SavedBlock>> per_region(regions.size());
    std::vector<u64> used(regions.size(), 0);
    for (size_t i = 0; i < ranges.size(); i++) {
        const auto [location, first, span] = ranges[i];
        if (newest[location] != i) {
            continue;
        }
        const auto any = emitter.GetAnyBlock(IR::LocationDescriptor{location});
        if (!any) {
            continue;
        }
        const auto& [block, verified] = *any;
        u64 hash = 0;
        if (verified) {
            const auto h = block_hashes.find(location);
            if (h == block_hashes.end()) {
                continue;  // emitted before hashing was on, or its code could not be read back
            }
            hash = h->second;
        } else {
            const auto u = unverified.find(location);
            if (!include_unverified || u == unverified.end()) {
                continue;
            }
            hash = u->second.hash;
        }
        const u8* const entry = static_cast<const u8*>(block.entrypoint);
        size_t region = regions.size();
        for (size_t r : live) {
            if (entry >= regions[r].begin && entry + block.size <= regions[r].end) {
                region = r;
            }
        }
        if (region == regions.size()) {
            continue;
        }
        SavedBlock saved{location, static_cast<u32>(entry - buffer), block.size, first, span, hash, {}, {}};
        emitter.SnapshotSlotsOf(block.first_link, saved.slots);
        emitter.SnapshotSitesIn(entry, entry + block.size, saved.sites);
        used[region] = std::max<u64>(used[region], static_cast<u64>(entry + block.size - regions[region].begin));
        per_region[region].push_back(std::move(saved));
    }

    u64 total = 0;
    for (size_t r : live) {
        total += used[r];
    }
    if (total > max_bytes) {
        return -3;
    }

    const auto lock_start = std::chrono::steady_clock::now();
    std::vector<u8> image;
    image.reserve(static_cast<size_t>(total) + (1 << 20));
    Writer w{image};
    const size_t key_len = std::strlen(key);
    w.Value(SNAPSHOT_MAGIC);
    w.Value(SNAPSHOT_VERSION);
    w.Value(static_cast<u32>(key_len));
    w.Bytes(key, key_len);
    w.Value(CodeShape());
    const auto placed = block_of_code.GetConstantPool().Placed();
    w.Value(static_cast<u64>(placed.size()));
    w.Bytes(placed.data(), placed.size_bytes());
    u64 blocks = 0;
    u32 region_count = 0;
    for (size_t r : live) {
        region_count += per_region[r].empty() ? 0 : 1;
    }
    w.Value(region_count);
    for (size_t r : live) {
        if (per_region[r].empty()) {
            continue;
        }
        w.Value(static_cast<u32>(r));
        w.Value(used[r]);
        // Patch 0075: a page restored lazily and never read in holds nothing -- written as zeros
        // (no block saved lies on it: each saved one was entered, so its pages were read in).
        for (u64 at = 0; at < used[r]; at += 4096) {
            const u64 n = std::min<u64>(4096, used[r] - at);
            if (lazy_pages.Pending(regions[r].begin + at)) {
                static const u8 zeros[4096] = {};
                w.Bytes(zeros, n);
            } else {
                w.Bytes(regions[r].begin + at, n);
            }
        }
        w.Value(static_cast<u64>(per_region[r].size()));
        for (const SavedBlock& b : per_region[r]) {
            w.Value(b.location);
            w.Value(b.entry);
            w.Value(b.size);
            w.Value(b.first);
            w.Value(b.span);
            w.Value(b.hash);
            w.Value(static_cast<u32>(b.slots.size()));
            w.Bytes(b.slots.data(), b.slots.size() * sizeof(EmitX64::SnapshotSlot));
            w.Value(static_cast<u32>(b.sites.size()));
            w.Bytes(b.sites.data(), b.sites.size() * sizeof(EmitX64::SnapshotSite));
            blocks++;
        }
    }
    w.Value(SNAPSHOT_MAGIC);
    snapshot_save_lock_ns = static_cast<u64>(std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now() - lock_start).count());
    held.unlock();

    const std::string temporary = std::string{path} + ".partial";
    std::FILE* const file = OpenFile(temporary.c_str(), true);
    if (file == nullptr) {
        return -4;
    }
    const bool written = image.empty() || std::fwrite(image.data(), 1, image.size(), file) == image.size();
    const bool closed = std::fclose(file) == 0;
    if (!written || !closed) {
        std::filesystem::remove(std::filesystem::path{std::u8string{reinterpret_cast<const char8_t*>(temporary.c_str())}});
        return -5;
    }
    std::error_code error;
    const std::filesystem::path from{std::u8string{reinterpret_cast<const char8_t*>(temporary.c_str())}};
    const std::filesystem::path to{std::u8string{reinterpret_cast<const char8_t*>(path)}};
    std::filesystem::rename(from, to, error);
    if (error) {
        // Patch 0075: the file in the way may be open (a lazily loaded snapshot, shared for
        // deletion): deleted first -- the open copy reads on -- then renamed into its place.
        std::error_code ignored;
        std::filesystem::remove(to, ignored);
        error.clear();
        std::filesystem::rename(from, to, error);
    }
    if (error) {
        return -6;
    }
    return static_cast<s64>(blocks);
}

s64 SharedCodeCache::Impl::LoadSnapshot(const char* path, const char* key, bool lazily) {
    std::FILE* file = OpenSnapshotFileForRead(path);
    if (file == nullptr) {
        return -1;
    }
    // Closed on every way out but a lazy load's success, which keeps it to read pages from.
    SCOPE_EXIT {
        if (file != nullptr) {
            std::fclose(file);
        }
    };
    Reader in{file};

    std::unique_lock held{lock};
    // Only into a cache that has emitted nothing: the restored blocks' records and ranges are made
    // in emission order, region by region, as their emission made them.
    if (blocks_emitted != 0 || snapshot_restored != 0 || current != 0 || block_of_code.getCurr<u8*>() != regions[0].begin) {
        return -7;
    }

    // Parse and check everything before changing anything.
    u64 magic = 0;
    u32 version = 0, key_len = 0;
    if (!in.Value(magic) || magic != SNAPSHOT_MAGIC || !in.Value(version) || version != SNAPSHOT_VERSION || !in.Value(key_len)) {
        return -8;
    }
    if (key_len != std::strlen(key) || key_len > (1u << 20)) {
        return -9;
    }
    std::vector<char> saved_key(key_len);
    if (!in.Bytes(saved_key.data(), key_len)) {
        return -8;
    }
    if (std::memcmp(saved_key.data(), key, key_len) != 0) {
        return -9;
    }
    u64 shape = 0;
    if (!in.Value(shape) || shape != CodeShape()) {
        return -10;
    }
    u64 constant_count = 0;
    if (!in.Value(constant_count)) {
        return -8;
    }
    const ConstantPool& pool = block_of_code.GetConstantPool();
    if (constant_count > pool.CapacityBytes() / 16) {
        return -8;
    }
    std::vector<u8> constant_bytes(constant_count * 16);
    if (!in.Bytes(constant_bytes.data(), constant_bytes.size())) {
        return -8;
    }
    const u8* const constants = constant_bytes.data();
    const auto placed = pool.Placed();
    if (placed.size() > constant_count || std::memcmp(placed.data(), constants, placed.size_bytes()) != 0) {
        return -11;  // the prelude placed other constants
    }
    u32 region_count = 0;
    if (!in.Value(region_count) || region_count > live_limit) {
        return -8;
    }
    u8* const buffer = const_cast<u8*>(block_of_code.getCode());
    std::vector<LoadedRegion> saved_regions(region_count);
    std::vector<bool> seen(regions.size(), false);
    u64 block_total = 0;
    u64 slot_total = 0;
    for (LoadedRegion& r : saved_regions) {
        if (!in.Value(r.index) || r.index >= regions.size() || seen[r.index] || !in.Value(r.used)) {
            return -8;
        }
        seen[r.index] = true;
        const Region& region = regions[r.index];
        if (r.used > static_cast<u64>(region.end - region.begin)) {
            return -8;
        }
        r.file_offset = in.Offset();
        u64 count = 0;
        if (!in.Skip(r.used) || !in.Value(count) || count > r.used) {
            return -8;
        }
        const u64 lo = static_cast<u64>(region.begin - buffer);
        const u64 hi = lo + r.used;
        r.blocks.resize(count);
        const u64 prelude_end = static_cast<u64>(static_cast<const u8*>(block_of_code.GetCodeBegin()) - buffer);
        for (LoadedBlock& b : r.blocks) {
            u32 slots = 0, sites = 0;
            if (!in.Value(b.location) || !in.Value(b.entry) || !in.Value(b.size) || !in.Value(b.first) || !in.Value(b.span) || !in.Value(b.hash)) {
                return -8;
            }
            if (b.entry < lo || static_cast<u64>(b.entry) + b.size > hi || (b.size & EmitX64::UNVERIFIED_BLOCK) != 0) {
                return -8;
            }
            if (!in.Value(slots) || slots > b.size / 8) {
                return -8;
            }
            b.slot_begin = static_cast<u32>(r.slots.size());
            b.slot_count = slots;
            r.slots.resize(r.slots.size() + slots);
            if (!in.Bytes(r.slots.data() + b.slot_begin, slots * sizeof(EmitX64::SnapshotSlot))) {
                return -8;
            }
            for (u32 i = 0; i < slots; i++) {
                const auto& s = r.slots[b.slot_begin + i];
                if (s.slot < b.entry || static_cast<u64>(s.slot) + 8 > static_cast<u64>(b.entry) + b.size || s.slot % 8 != 0 || s.unlinked >= hi) {
                    return -8;
                }
            }
            if (!in.Value(sites) || sites > b.size) {
                return -8;
            }
            b.site_begin = static_cast<u32>(r.sites.size());
            b.site_count = sites;
            r.sites.resize(r.sites.size() + sites);
            if (!in.Bytes(r.sites.data() + b.site_begin, sites * sizeof(EmitX64::SnapshotSite))) {
                return -8;
            }
            for (u32 i = 0; i < sites; i++) {
                const auto& s = r.sites[b.site_begin + i];
                if (s.site < b.entry || s.site >= b.entry + b.size || s.resume < lo || s.resume >= hi || s.callback >= prelude_end) {
                    return -8;
                }
            }
        }
        block_total += count;
        slot_total += r.slots.size();
    }
    u64 trailer = 0;
    if (!in.Value(trailer) || trailer != SNAPSHOT_MAGIC || !in.AtEnd()) {
        return -8;
    }

    // Install. The constant pool first: the blocks read their constants at fixed offsets.
    block_of_code.EnableWriting();
    SCOPE_EXIT {
        block_of_code.DisableWriting();
    };
    ConstantPool& writable_pool = block_of_code.GetConstantPool();
    for (u64 i = placed.size(); i < constant_count; i++) {
        u64 pair[2];
        std::memcpy(pair, constants + i * 16, 16);
        const void* const at = writable_pool.Place(pair[0], pair[1]);
        // A constant appears once in a pool; a snapshot whose pool repeats one is not ours.
        ASSERT(at == static_cast<const u8*>(writable_pool.Begin()) + i * 16);
    }
    lazy_pages.base = block_of_code.getCode();  // patch 0075
    restored_ranges_first = emitter.NextRangeSerial();  // patch 0076
    if (lazily) {
        emitter.lazy_pages = &lazy_pages;
    }
    // Patch 0095: the tables sized for what is restored, once, rather than grown through it.
    emitter.ReserveForRestore(static_cast<size_t>(block_total), static_cast<size_t>(slot_total));
    unverified.reserve(unverified.size() + static_cast<size_t>(block_total));
    // The constructor started region 0; the snapshot's regions are started in its order instead.
    regions[0].state = Region::State::Free;
    current = NO_REGION;
    for (LoadedRegion& r : saved_regions) {
        if (current != NO_REGION) {
            regions[current].state = Region::State::Full;
            current = NO_REGION;
        }
        StartRegion(r.index);
        Region& region = regions[r.index];
        u8* const committed = std::min(AlignUp(region.begin + r.used, SHARED_GRANULE), region.end);
        if (!lazily) {
            if (committed > region.begin) {
                block_of_code.CommitRange(region.begin, static_cast<size_t>(committed - region.begin));
            }
            if (!ReadAt(file, r.file_offset, region.begin, r.used)) {
                // Records not made yet: the region is simply forgotten (started again below).
                block_of_code.DecommitRange(region.begin, static_cast<size_t>(region.end - region.begin));
                return -8;
            }
        } else {
            // Patch 0075: nothing read or committed; every page of it pending, read in when a block
            // on it is first entered. The code after it, where emission goes on, is committed.
            lazy_pages.Mark(region.begin, region.begin + r.used, true);
            const u8* const tail = AlignDown(region.begin + r.used, 4096);
            if (committed > tail) {
                block_of_code.CommitRange(tail, static_cast<size_t>(committed - tail));
            }
            lazy_regions.push_back(LazyRegion{r.index, r.file_offset, r.used, {}});
        }
        region.code_committed_end = std::max(region.code_committed_end, committed);
        if (lazily) {
            lazy_regions.back().slots.reserve(r.slots.size());
        }
        for (const LoadedBlock& b : r.blocks) {
            const IR::LocationDescriptor location{b.location};
            const EmitX64::SnapshotSlot* const slots = r.slots.data() + b.slot_begin;
            if (lazily) {
                const u32 first_serial = emitter.NextLinkSerial();
                for (size_t i = 0; i < b.slot_count; i++) {
                    lazy_regions.back().slots.emplace_back(slots[i].slot, static_cast<u32>(first_serial + i));
                }
            }
            // Patch 0095: the sites committed once for the region, below (its blocks are in the
            // order they were emitted, ascending, so the records are the same).
            emitter.RestoreBlock(location, b.entry, b.size, slots, b.slot_count, r.sites.data() + b.site_begin, b.site_count, lazily, false);
            emitter.RestoreGuestRange(location, b.first, b.span);
            unverified[b.location] = Unverified{b.first, b.span, b.hash};
        }
        emitter.CommitRestoredSites();
        if (lazily) {
            std::sort(lazy_regions.back().slots.begin(), lazy_regions.back().slots.end());
        }
        block_of_code.SetCodePtr(region.begin + r.used);
    }
    if (lazily) {
        lazy_file = file;
        file = nullptr;  // kept open, to read pages from
        // The page the next block is emitted on holds restored code below it: read in now. If it
        // cannot be, nothing more is emitted into that region (EnsureRoom starts another).
        if (current != NO_REGION) {
            Region& region = regions[current];
            const u8* const at = block_of_code.getCurr<const u8*>();
            const u8* const tail = AlignDown(const_cast<u8*>(at), 4096);
            if (tail < at && tail >= region.begin && !Materialize(tail, at)) {
                region.state = Region::State::Full;
                current = NO_REGION;
            }
        }
    }
    if (current == NO_REGION) {
        // A free region to go on in (there is one: at most `live_limit` were restored).
        for (size_t i = 0; i < regions.size(); i++) {
            if (regions[i].state == Region::State::Free) {
                StartRegion(i);
                break;
            }
        }
    }
    snapshot_hashing = true;
    restored_ranges_end = emitter.NextRangeSerial();  // patch 0076
    snapshot_restored += block_total;
    unverified_count.store(unverified.size(), std::memory_order_relaxed);
    return static_cast<s64>(block_total);
}

std::optional<CodePtr> SharedCodeCache::Impl::VerifyRestored(IR::LocationDescriptor location, const UserConfig& translator_conf) {
    Unverified pending;
    {
        std::shared_lock guard{lock};
        const auto it = unverified.find(location.Value());
        if (it == unverified.end()) {
            return std::nullopt;
        }
        pending = it->second;
    }
    // Read back outside the lock, as the frontend reads.
    const auto hash = GuestCodeHash(translator_conf, pending.first, pending.first + pending.span);

    std::unique_lock held{lock};
    const auto it = unverified.find(location.Value());
    if (it == unverified.end()) {
        // Another thread verified (or dropped) it meanwhile.
        if (const auto block = emitter.GetBasicBlock(location)) {
            return block->entrypoint;
        }
        return std::nullopt;
    }
    unverified.erase(it);
    unverified_count.store(unverified.size(), std::memory_order_relaxed);
    const auto any = emitter.GetAnyBlock(location);
    if (!any) {
        return std::nullopt;  // invalidated or evicted before it was ever entered
    }
    if (any->second) {
        return any->first.entrypoint;  // a newer translation
    }
    block_of_code.EnableWriting();
    SCOPE_EXIT {
        block_of_code.DisableWriting();
    };
    if (hash && *hash == pending.hash) {
        // Patch 0075: its code read in first, if it was restored lazily.
        const u8* const entry = static_cast<const u8*>(any->first.entrypoint);
        if (!Materialize(entry, entry + any->first.size)) {
            emitter.InvalidateBasicBlocks({location});
            snapshot_rejected++;
            return std::nullopt;
        }
        emitter.MarkVerified(location);
        block_hashes[location.Value()] = pending.hash;
        snapshot_verified++;
        return any->first.entrypoint;
    }
    emitter.InvalidateBasicBlocks({location});
    snapshot_rejected++;
    return std::nullopt;
}

s64 SharedCodeCache::Impl::ForgetUnverified() {
    std::unique_lock held{lock};
    if (unverified.empty()) {
        return 0;
    }
    std::vector<u64> locations;
    locations.reserve(unverified.size());
    for (const auto& [location, pending] : unverified) {
        locations.push_back(location);
    }
    std::vector<std::pair<u32, u32>> dead_links;
    std::vector<std::pair<const u8*, const u8*>> dead_code;
    const size_t forgotten = emitter.ForgetUnverifiedBlocks(locations, dead_links, dead_code);
    // What verifying them needed: emptied, and its array given back.
    tsl::robin_map<u64, Unverified>{}.swap(unverified);
    unverified_count.store(0, std::memory_order_relaxed);
    // A lazily restored page read in later sets only the slots of blocks still known.
    std::sort(dead_links.begin(), dead_links.end());
    const auto dead_serial = [&dead_links](u32 serial) {
        auto it = std::upper_bound(dead_links.begin(), dead_links.end(), serial, [](u32 s, const auto& span) { return s < span.first; });
        return it != dead_links.begin() && serial <= std::prev(it)->second;
    };
    for (LazyRegion& l : lazy_regions) {
        const size_t before = l.slots.size();
        std::erase_if(l.slots, [&](const std::pair<u32, u32>& s) { return dead_serial(s.second); });
        if (l.slots.size() != before) {
            l.slots.shrink_to_fit();
        }
    }
    std::sort(dead_code.begin(), dead_code.end());
    emitter.ForgetFastmemSitesIn(dead_code);
    emitter.PruneGuestRangeIndex(restored_ranges_first, restored_ranges_end);
    emitter.ShrinkTables();  // patch 0066's, whatever its switch
    snapshot_forgotten += forgotten;
    return static_cast<s64>(forgotten);
}

bool SharedCodeCache::Impl::Materialize(const u8* begin, const u8* end) {
    if (lazy_regions.empty()) {
        return true;
    }
    for (const u8* page = AlignDown(const_cast<u8*>(begin), 4096); page < end; page += 4096) {
        if (!lazy_pages.Pending(page)) {
            continue;
        }
        LazyRegion* lazy = nullptr;
        for (LazyRegion& l : lazy_regions) {
            const Region& r = regions[l.index];
            if (page >= r.begin && page < r.begin + l.used) {
                lazy = &l;
            }
        }
        if (lazy == nullptr || lazy_file == nullptr) {
            return false;
        }
        const Region& region = regions[lazy->index];
        const u64 offset = static_cast<u64>(page - region.begin);
        const size_t n = static_cast<size_t>(std::min<u64>(4096, lazy->used - offset));
        u8* const writable = const_cast<u8*>(page);
        block_of_code.CommitRange(writable, 4096);
        if (!ReadAt(lazy_file, lazy->file_offset + offset, writable, n)) {
            return false;
        }
        lazy_pages.Mark(page, page + 4096, false);
        snapshot_pages_read++;
        // Its slots, as they would be had they been written all along.
        const u32 lo = static_cast<u32>(page - block_of_code.getCode());
        auto it = std::lower_bound(lazy->slots.begin(), lazy->slots.end(), std::make_pair(lo, u32{0}));
        for (; it != lazy->slots.end() && it->first < lo + 4096; ++it) {
            emitter.RefreshSlot(it->second);
        }
    }
    return true;
}

void SharedCodeCache::Impl::DropLazy(size_t index) {
    for (auto it = lazy_regions.begin(); it != lazy_regions.end(); ++it) {
        if (it->index == index) {
            const Region& r = regions[index];
            lazy_pages.Mark(r.begin, r.begin + it->used, false);
            lazy_regions.erase(it);
            return;
        }
    }
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
    // Patch 0070: a block restored from a snapshot is entered once its guest code is found the same.
    if (unverified_count.load(std::memory_order_relaxed) != 0) {
        if (const auto restored = VerifyRestored(location, translator_conf)) {
            return *restored;
        }
    }
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

    // Patch 0070: the guest code's hash, read back as the frontend read it, for a snapshot.
    std::optional<u64> code_hash;
    if (snapshot_hashing) {
        code_hash = GuestCodeHash(translator_conf, A64::LocationDescriptor{ir_block->Location()}.PC(), A64::LocationDescriptor{ir_block->EndLocation()}.PC());
    }

    std::unique_lock held{lock};
    for (;;) {
        if (const auto block = emitter.GetBasicBlock(location)) {
            translations_raced.fetch_add(1, std::memory_order_relaxed);
            return block->entrypoint;
        }
        // Patch 0070: a restored block still unverified here (its verification raced this
        // translation) gives way to the new one.
        if (const auto any = emitter.GetAnyBlock(location); any && !any->second) {
            block_of_code.EnableWriting();
            emitter.InvalidateBasicBlocks({location});
            block_of_code.DisableWriting();
        }
        // Guest code the block was translated from may have changed since: an invalidation
        // applied meanwhile that touches its range means translating it again, now, under the
        // lock (which invalidations also take).
        const A64::LocationDescriptor begin{ir_block->Location()};
        const A64::LocationDescriptor end{ir_block->EndLocation()};
        if (InvalidatedSince(serial_before, begin.PC(), end.PC())) {
            translations_redone++;
            ir_block.emplace(TranslateBlock(location, translator_conf, polyfill_options));
            if (snapshot_hashing) {
                code_hash = GuestCodeHash(translator_conf, A64::LocationDescriptor{ir_block->Location()}.PC(), A64::LocationDescriptor{ir_block->EndLocation()}.PC());
            }
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
    if (snapshot_hashing) {  // patch 0070
        if (code_hash) {
            block_hashes[location.Value()] = *code_hash;
        } else {
            block_hashes.erase(location.Value());
        }
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
        if (live_shrink_tables.load(std::memory_order_relaxed) != 0) {
            emitter.ShrinkTables();  // patch 0066
        }
    }
}

void SharedCodeCache::Impl::ForgetEverything(SharedThreadState* thread) {
    blocks_invalidated += emitter.ForgetAllBlocks();
    for (size_t i = 0; i < regions.size(); i++) {  // patch 0075: after the slots are unlinked
        DropLazy(i);
    }
    block_hashes = {};  // patch 0070: every block is forgotten
    unverified = {};
    unverified_count.store(0, std::memory_order_relaxed);
    generation.fetch_add(1, std::memory_order_seq_cst);
    last_evicted = {};
    // Patch 0028: the emitter's serials start again from 0, and the full regions, whose blocks are
    // all forgotten, are given back. Patch 0030: so is the region being filled -- its blocks are
    // forgotten too, and a cache whose code fits in one region (a service that translated its
    // start and then waits) would otherwise keep all of it committed through a clear. The next
    // block starts a fresh region, committed as it fills.
    if (current != NO_REGION) {
        regions[current].state = Region::State::Full;
        current = NO_REGION;
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
    DropLazy(static_cast<size_t>(oldest - regions.data()));  // patch 0075: before its records go
    // Every record and range below the next live region's serials is dead now: the blocks they
    // named were this region's, an older region's, or invalidated.
    emitter.TrimLinkRecords(end_link);
    emitter.TrimGuestRanges(end_range);
    if (live_shrink_tables.load(std::memory_order_relaxed) != 0) {
        emitter.ShrinkTables();  // patch 0066
    }
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

size_t SharedCodeCache::Impl::EvictTo(size_t keep_bytes) {
    std::unique_lock held{lock};
    if (regions.empty()) {
        return 0;
    }
    // Patch 0050: what a full region's eviction does when the live limit is reached (patch 0028),
    // on demand: the oldest live region's blocks are forgotten, every thread asked to leave
    // generated code, and the region given back once none holds it. Hot code in it is translated
    // again, into the region being filled, the next time it runs.
    const size_t region_bytes = static_cast<size_t>(regions.front().end - regions.front().begin);
    const size_t keep = std::max<size_t>(1, keep_bytes / region_bytes);
    size_t evicted = 0;
    while (LiveRegions() > keep && EvictOldest(nullptr)) {
        evicted++;
    }
    if (evicted != 0) {
        TryReclaimRetired();
    }
    return evicted;
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
    s.snapshot_blocks_restored = snapshot_restored;  // patch 0070
    s.snapshot_blocks_verified = snapshot_verified;
    s.snapshot_blocks_rejected = snapshot_rejected;
    s.snapshot_save_lock_ns = snapshot_save_lock_ns;
    s.snapshot_pages_read = snapshot_pages_read;
    s.snapshot_blocks_forgotten = snapshot_forgotten;  // patch 0076
    return s;
}

SharedCodeCache::Tables SharedCodeCache::Impl::GetTables() const {
    std::shared_lock guard{lock};
    return emitter.Census();
}

void SharedCodeCache::Impl::GuestPcsOf(const u64* hosts, size_t count, u64* guest_pcs) const {
    std::shared_lock guard{lock};
    emitter.GuestPcsOf(hosts, count, guest_pcs);
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

void SharedCodeCache::GuestPcsOf(const std::uint64_t* hosts, std::size_t count, std::uint64_t* guest_pcs) const {
    impl->GuestPcsOf(hosts, count, guest_pcs);
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

std::size_t SharedCodeCache::EvictTo(std::size_t keep_bytes) {
    return impl->EvictTo(keep_bytes);
}

void SharedCodeCache::EnableSnapshots() {
    impl->EnableSnapshots();
}

std::int64_t SharedCodeCache::SaveSnapshot(const char* path, const char* key, std::uint64_t max_bytes, bool include_unverified) {
    return impl->SaveSnapshot(path, key, max_bytes, include_unverified);
}

std::size_t SharedCodeCache::UnverifiedPcs(std::uint64_t* out, std::size_t capacity) const {
    return impl->UnverifiedPcs(out, capacity);
}

std::int64_t SharedCodeCache::ForgetUnverified() {
    return impl->ForgetUnverified();
}

std::int64_t SharedCodeCache::LoadSnapshot(const char* path, const char* key, bool lazily) {
    return impl->LoadSnapshot(path, key, lazily);
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
