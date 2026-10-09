/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include "dynarmic/backend/x64/a64_emit_x64.h"

#include <algorithm>
#include <limits>

#include <xmmintrin.h>

#include <fmt/format.h>
#include <fmt/ostream.h>
#include <mcl/assert.hpp>
#include <mcl/scope_exit.hpp>
#include <mcl/stdint.hpp>
#include <mcl/type_traits/integer_of_size.hpp>

#include "dynarmic/backend/x64/a64_jitstate.h"
#include "dynarmic/backend/x64/abi.h"
#include "dynarmic/backend/x64/block_of_code.h"
#include "dynarmic/backend/x64/devirtualize.h"
#include "dynarmic/backend/x64/emit_x64.h"
#include "dynarmic/backend/x64/nzcv_util.h"
#include "dynarmic/backend/x64/perf_map.h"
#include "dynarmic/backend/x64/stack_layout.h"
#include "dynarmic/frontend/A64/a64_location_descriptor.h"
#include "dynarmic/frontend/A64/a64_types.h"
#include "dynarmic/ir/basic_block.h"
#include "dynarmic/ir/cond.h"
#include "dynarmic/ir/microinstruction.h"
#include "dynarmic/ir/opcodes.h"

// TODO: Have ARM flags in host flags and not have them use up GPR registers unless necessary.
// TODO: Actually implement that proper instruction selector you've always wanted to sweetheart.

namespace Dynarmic::Backend::X64 {

using namespace Xbyak::util;

std::atomic<std::uint32_t> live_fp_optimizations{0};
std::atomic<std::uint32_t> live_precise_get_set{1};
std::atomic<std::uint32_t> live_fast_dispatch_inline{0};
std::atomic<std::uint32_t> live_shrink_tables{0};
std::array<std::atomic<std::uint64_t>, static_cast<std::size_t>(CodegenPart::Count)> codegen_census{};

std::size_t ReadCodegenCensus(std::uint64_t* out, std::size_t n) {
    const std::size_t count = std::min(n, codegen_census.size());
    for (std::size_t i = 0; i < count; ++i) {
        out[i] = codegen_census[i].load(std::memory_order_relaxed);
    }
    return codegen_census.size();
}

void ResetCodegenCensus() {
    for (auto& c : codegen_census) {
        c.store(0, std::memory_order_relaxed);
    }
}

namespace {
std::atomic<EmitObserver> emit_observer{nullptr};
std::atomic<void*> emit_observer_ctx{nullptr};
}  // namespace

void SetEmitObserver(EmitObserver observer, void* ctx) {
    emit_observer_ctx.store(ctx, std::memory_order_relaxed);
    emit_observer.store(observer, std::memory_order_release);
}

namespace {
void CodegenCount(CodegenPart part, std::uint64_t n) {
    codegen_census[static_cast<std::size_t>(part)].fetch_add(n, std::memory_order_relaxed);
}

CodegenPart PartOf(const IR::Inst& inst) {
    switch (inst.GetOpcode()) {
    case IR::Opcode::A64GetW:
    case IR::Opcode::A64GetX:
    case IR::Opcode::A64GetS:
    case IR::Opcode::A64GetD:
    case IR::Opcode::A64GetQ:
    case IR::Opcode::A64GetSP:
    case IR::Opcode::A64SetW:
    case IR::Opcode::A64SetX:
    case IR::Opcode::A64SetS:
    case IR::Opcode::A64SetD:
    case IR::Opcode::A64SetQ:
    case IR::Opcode::A64SetSP:
        return CodegenPart::GetSet;
    case IR::Opcode::A64GetNZCVRaw:
    case IR::Opcode::A64SetNZCVRaw:
    case IR::Opcode::A64SetNZCV:
    case IR::Opcode::A64GetCFlag:
    case IR::Opcode::GetCarryFromOp:
    case IR::Opcode::GetOverflowFromOp:
    case IR::Opcode::GetNZCVFromOp:
    case IR::Opcode::GetNZFromOp:
        return CodegenPart::Flags;
    case IR::Opcode::A64SetPC:
        return CodegenPart::SetPc;
    default:
        return inst.IsMemoryReadOrWrite() ? CodegenPart::Memory : CodegenPart::Other;
    }
}
}  // namespace

A64EmitContext::A64EmitContext(const A64::UserConfig& conf, RegAlloc& reg_alloc, IR::Block& block)
        : EmitContext(reg_alloc, block), conf(conf) {}

A64::LocationDescriptor A64EmitContext::Location() const {
    return A64::LocationDescriptor{block.Location()};
}

bool A64EmitContext::IsSingleStep() const {
    return Location().SingleStepping();
}

FP::FPCR A64EmitContext::FPCR(bool fpcr_controlled) const {
    return fpcr_controlled ? Location().FPCR() : Location().FPCR().ASIMDStandardValue();
}

A64EmitX64::A64EmitX64(BlockOfCode& code, A64::UserConfig conf, A64::Jit* jit_interface, bool shared)
        : EmitX64(code), conf(conf), jit_interface{jit_interface} {
    // Omnidroid patch 0022: before the prelude, which is shared too when this is.
    shared_code = shared;
    if (shared_code) {
        // Omnidroid patch 0027: the block map of a shared cache holds every block of the process
        // (~700,000 in a game world); at 0.75 rather than 0.5 its bucket array is half the size.
        UseLoadFactor(block_descriptors, SHARED_BLOCK_MAP_LOAD_FACTOR);
        // Omnidroid patch 0035: every thread's table has this cache's size.
        fast_dispatch_mask = (FastDispatchEntries(conf) - 1) * sizeof(FastDispatchEntry);
    }
    // In a shared cache each thread owns its table (JitState::od_fast_dispatch_table).
    if (conf.HasOptimization(OptimizationFlag::FastDispatch) && !shared_code) {
        fast_dispatch_table = std::make_unique<std::array<FastDispatchEntry, fast_dispatch_table_size>>();
    }
    GenMemory128Accessors();
    GenFastmemFallbacks();
    GenTerminalHandlers();
    if (shared_code) {
        GenSharedSvcTrampolines();  // Omnidroid patch 0022
    }
    code.PreludeComplete();
    ClearFastDispatchTable();

    exception_handler.SetFastmemCallback([this](u64 rip_) {
        return FastmemCallback(rip_);
    });
}

A64EmitX64::~A64EmitX64() = default;

A64EmitX64::BlockDescriptor A64EmitX64::Emit(IR::Block& block) {
    if (conf.very_verbose_debugging_output) {
        std::puts(IR::DumpBlock(block).c_str());
    }

    code.EnableWriting();
    SCOPE_EXIT {
        code.DisableWriting();
    };
    // Omnidroid patch 0022: nothing is left over from an emission that threw.
    pending_slots.clear();
    pending_fastmem_sites.clear();  // patch 0025
    od_pc_in_rbp = false;           // patch 0042
    // Patch 0025: records made for a block whose emission then threw name no block; out of their
    // targets' lists with them (their slots, in code nothing reaches, are unlinked on the way).
    if (pending_first_link != NO_LINK) {
        ForgetOutgoingSlots(std::exchange(pending_first_link, NO_LINK));
    }

    if (gpr_order_cache.empty()) {  // patch 0063: once, from the configuration, which is fixed
        gpr_order_cache = [this] {
            std::vector<HostLoc> gprs{any_gpr};
            if (conf.page_table) {
                gprs.erase(std::find(gprs.begin(), gprs.end(), HostLoc::R14));
            }
            if (conf.fastmem_pointer) {
                gprs.erase(std::find(gprs.begin(), gprs.end(), HostLoc::R13));
            }
            return gprs;
        }();
    }

    RegAlloc reg_alloc{code, gpr_order_cache, std::span<const HostLoc>{any_xmm.begin(), any_xmm.size()}};
    A64EmitContext ctx{conf, reg_alloc, block};

    // Start emitting.
    const u8* const before_align = code.getCurr();
    code.align();
    const u8* const entrypoint = code.getCurr();
    CodegenCount(CodegenPart::Align, static_cast<u64>(entrypoint - before_align));  // patch 0060

    ASSERT(block.GetCondition() == IR::Cond::AL);

    for (auto iter = block.begin(); iter != block.end(); ++iter) {
        IR::Inst* inst = &*iter;
        const u8* const inst_start = code.getCurr();  // patch 0060

        // Call the relevant Emit* member function.
        switch (inst->GetOpcode()) {
#define OPCODE(name, type, ...)            \
    case IR::Opcode::name:                 \
        A64EmitX64::Emit##name(ctx, inst); \
        break;
#define A32OPC(...)
#define A64OPC(name, type, ...)               \
    case IR::Opcode::A64##name:               \
        A64EmitX64::EmitA64##name(ctx, inst); \
        break;
#include "dynarmic/ir/opcodes.inc"
#undef OPCODE
#undef A32OPC
#undef A64OPC

        default:
            ASSERT_MSG(false, "Invalid opcode: {}", inst->GetOpcode());
            break;
        }

        ctx.reg_alloc.EndOfAllocScope();

        if (conf.very_verbose_debugging_output) {
            EmitVerboseDebuggingOutput(reg_alloc);
        }
        // Patch 0060.
        CodegenCount(PartOf(*inst), static_cast<u64>(code.getCurr() - inst_start));
        CodegenCount(CodegenPart::IrInsts, 1);
        if (inst->IsMemoryReadOrWrite()) {
            CodegenCount(CodegenPart::MemoryOps, 1);
        }
    }

    reg_alloc.AssertNoMoreUses();

    const u8* const cycles_start = code.getCurr();  // patch 0060
    if (conf.enable_cycle_counting) {
        EmitAddCycles(block.CycleCount());
    }
    const u8* const terminal_start = code.getCurr();
    EmitX64::EmitTerminal(block.GetTerminal(), ctx.Location().SetSingleStepping(false), ctx.IsSingleStep());
    code.int3();
    const u8* const far_start = code.getCurr();

    for (auto& deferred_emit : ctx.deferred_emits) {
        deferred_emit();
    }
    code.int3();
    const u8* const slots_start = code.getCurr();
    if (shared_code) {
        // Omnidroid patch 0022: the block's link slots, right after its code.
        EmitPendingSlots(block.Location());
        // Patch 0025: the block's fastmem sites, now that its code is complete.
        CommitSharedFastmemSites();
    }
    // Patch 0060.
    CodegenCount(CodegenPart::Cycles, static_cast<u64>(terminal_start - cycles_start));
    CodegenCount(CodegenPart::Terminal, static_cast<u64>(far_start - terminal_start));
    CodegenCount(CodegenPart::Far, static_cast<u64>(slots_start - far_start));
    CodegenCount(CodegenPart::Slots, static_cast<u64>(code.getCurr() - slots_start));
    CodegenCount(CodegenPart::Deferred, ctx.deferred_emits.size());
    CodegenCount(CodegenPart::Blocks, 1);
    CodegenCount(CodegenPart::GuestInsts, (A64::LocationDescriptor{block.EndLocation()}.PC() - A64::LocationDescriptor{block.Location()}.PC()) / 4);

    const size_t size = static_cast<size_t>(code.getCurr() - entrypoint);

    const A64::LocationDescriptor descriptor{block.Location()};
    const A64::LocationDescriptor end_location{block.EndLocation()};

    // Omnidroid patch 0026: the pin's `closed(descriptor.PC(), end_location.PC() - 1)`, which is
    // empty -- and was never returned -- when the block covers no bytes.
    AddGuestRange(descriptor, descriptor.PC(), end_location.PC() - 1);

    const BlockDescriptor registered = RegisterBlock(descriptor, entrypoint, size);
    if (const EmitObserver observer = emit_observer.load(std::memory_order_acquire)) {  // patch 0062
        observer(emit_observer_ctx.load(std::memory_order_relaxed), descriptor.PC(), entrypoint,
                 static_cast<std::size_t>(slots_start - entrypoint), size);
    }
    return registered;
}

void A64EmitX64::ClearCache() {
    EmitX64::ClearCache();
    ClearGuestRanges();
    ClearFastDispatchTable();
    fastmem_patch_info.clear();
}

void A64EmitX64::InvalidateCacheRanges(const boost::icl::interval_set<u64>& ranges) {
    InvalidateBasicBlocks(GuestRangeLocations(ranges));
}

void A64EmitX64::ClearFastDispatchTable() {
    if (conf.HasOptimization(OptimizationFlag::FastDispatch) && !shared_code) {
        fast_dispatch_table->fill({});
    }
}

size_t A64EmitX64::InvalidateCacheRangesCounted(const boost::icl::interval_set<u64>& ranges) {
    const auto locations = GuestRangeLocations(ranges);
    size_t dropped = 0;
    for (const auto& location : locations) {
        dropped += block_descriptors.count(Key64{location});
    }
    InvalidateBasicBlocks(locations);
    return dropped;
}

size_t A64EmitX64::ForgetAllBlocks() {
    ASSERT(shared_code);
    const size_t dropped = block_descriptors.size();
    UnlinkAllSlots();
    EmitX64::ClearCache();
    ClearGuestRanges();
    // Omnidroid patch 0031: the block map given back, not kept at the most blocks the cache ever
    // held. `clear()` keeps a robin_map's bucket array, so a quiet process whose translations a
    // trim dropped kept the map of its busiest moment (the system's host process: 86 MiB of block
    // maps for 562k live blocks, run 2026-09-29) -- and the next block fills it from 64 buckets
    // again, as a new cache does.
    decltype(block_descriptors){}.swap(block_descriptors);
    UseLoadFactor(block_descriptors, SHARED_BLOCK_MAP_LOAD_FACTOR);
    decltype(patch_information){}.swap(patch_information);
    return dropped;
}

void A64EmitX64::AddGuestRange(IR::LocationDescriptor location, u64 first, u64 last) {
    // Patch 0028: recorded even when empty (a region's blocks are found through their ranges),
    // but only a range covering bytes is indexed -- and so ever returned, as the pin's were.
    ASSERT(NextRangeSerial() < std::numeric_limits<u32>::max());
    const u32 index = NextRangeSerial();
    // Patch 0052: a block's guest bytes are far fewer than 4 GiB.
    ASSERT(last < first || last - first < std::numeric_limits<u32>::max());
    const u32 span = last < first ? 0 : static_cast<u32>(last - first + 1);
    // Patch 0065: the first byte is the location's PC, read back from it.
    ASSERT(first == A64::LocationDescriptor{location}.PC());
    guest_ranges.push_back(GuestRange{Key64{location}, span});
    if (last < first) {
        return;
    }

    const u64 first_page = first >> guest_page_bits;
    const u64 last_page = last >> guest_page_bits;
    if (last_page - first_page >= max_indexed_pages) {
        wide_guest_ranges.push_back(index);
        return;
    }
    for (u64 page = first_page;; ++page) {
        guest_range_pages[page].push_back(index);
        if (page == last_page) {
            break;
        }
    }
}

void A64EmitX64::SnapshotRanges(std::vector<std::tuple<u64, u64, u32>>& out) const {
    out.reserve(out.size() + guest_ranges.size());
    for (const GuestRange& r : guest_ranges) {
        out.emplace_back(r.location.Value(), r.First(), r.span);
    }
}

tsl::robin_set<IR::LocationDescriptor> A64EmitX64::GuestRangeLocations(const boost::icl::interval_set<u64>& ranges) const {
    tsl::robin_set<IR::LocationDescriptor> locations;
    for (const auto& interval : ranges) {
        const u64 first = boost::icl::first(interval);
        const u64 last = boost::icl::last(interval);
        const auto consider = [&](u32 index) {
            const GuestRange& range = RangeAt(index);
            if (range.First() <= last && first <= range.Last()) {
                locations.insert(range.location.Location());
            }
        };

        for (const u32 index : wide_guest_ranges) {
            consider(index);
        }

        const u64 first_page = first >> guest_page_bits;
        const u64 last_page = last >> guest_page_bits;
        if (last_page - first_page >= guest_range_pages.size()) {
            // More pages asked about than have anything on them: walk what there is.
            for (const auto& [page, indices] : guest_range_pages) {
                if (page >= first_page && page <= last_page) {
                    for (const u32 index : indices) {
                        consider(index);
                    }
                }
            }
        } else {
            for (u64 page = first_page;; ++page) {
                if (const auto iter = guest_range_pages.find(page); iter != guest_range_pages.end()) {
                    for (const u32 index : iter->second) {
                        consider(index);
                    }
                }
                if (page == last_page) {
                    break;
                }
            }
        }
    }
    return locations;
}

void A64EmitX64::ClearGuestRanges() {
    std::vector<GuestRange>{}.swap(guest_ranges);
    range_base = 0;  // patch 0028
    guest_range_pages = {};
    std::vector<u32>{}.swap(wide_guest_ranges);
}

size_t A64EmitX64::ForgetRegionBlocks(const void* begin, const void* end, u32 first_range, u32 end_range, std::vector<u64>& forgotten) {
    ASSERT(shared_code);
    ASSERT(first_range >= range_base && first_range <= end_range && end_range <= NextRangeSerial());
    const u8* const b = static_cast<const u8*>(begin);
    const u8* const e = static_cast<const u8*>(end);
    code.EnableWriting();
    SCOPE_EXIT {
        code.DisableWriting();
    };
    size_t dropped = 0;
    for (u32 serial = first_range; serial != end_range; serial++) {
        const IR::LocationDescriptor location = RangeAt(serial).location.Location();
        const auto it = block_descriptors.find(Key64{location});
        if (it == block_descriptors.end()) {
            continue;  // invalidated since, or listed twice (invalidated and emitted again here)
        }
        const u8* const entry = reinterpret_cast<const u8*>(Load(it->second).entrypoint);
        if (entry < b || entry >= e) {
            continue;  // invalidated and emitted again into a newer region: that block stays
        }
        Unpatch(location);
        ForgetOutgoingSlots(it->second.first_link);
        KeepHeadOf(it->first, it->second);  // patch 0064
        block_descriptors.erase(it);
        forgotten.push_back(location.Value());
        dropped++;
    }
    return dropped;
}

void A64EmitX64::TrimGuestRanges(u32 base) {
    ASSERT(base >= range_base && base <= NextRangeSerial());
    if (base == range_base) {
        return;
    }
    // Each page's list and the wide list are ascending (appended in emission order since the
    // ranges were last cleared): what goes is a prefix of each.
    const auto drop_below = [base](std::vector<u32>& indices) {
        indices.erase(indices.begin(), std::lower_bound(indices.begin(), indices.end(), base));
    };
    for (auto it = guest_range_pages.begin(); it != guest_range_pages.end();) {
        std::vector<u32>& indices = it.value();
        if (!indices.empty() && indices.front() < base) {
            drop_below(indices);
            if (indices.empty()) {
                it = guest_range_pages.erase(it);
                continue;
            }
            if (indices.capacity() > 2 * indices.size() + 16) {
                indices.shrink_to_fit();
            }
        }
        ++it;
    }
    drop_below(wide_guest_ranges);
    guest_ranges.erase(guest_ranges.begin(), guest_ranges.begin() + (base - range_base));
    range_base = base;
    if (guest_ranges.capacity() > 2 * guest_ranges.size() + 4096) {
        guest_ranges.shrink_to_fit();
    }
}

void A64EmitX64::PurgeFastmemPatchInfo(const void* begin, const void* end) {
    // Patch 0025: a shared cache's sites are records of the region, given back with it.
    PurgeFastmemSites(begin, end);
}

CodePtr A64EmitX64::ProbeFastDispatchTable(void* table, u64 descriptor) const {
    const FastDispatchEntry& entry = fast_dispatch_table_lookup_in(descriptor, table);
    return entry.location_descriptor == descriptor ? entry.code_ptr : nullptr;
}

void A64EmitX64::FillFastDispatchTable(void* table, u64 descriptor, CodePtr code_ptr) const {
    FastDispatchEntry& entry = fast_dispatch_table_lookup_in(descriptor, table);
    entry.location_descriptor = descriptor;
    entry.code_ptr = code_ptr;
}

namespace {

/// A robin_map's bucket array: `bucket_count()` buckets, each the value inline beside its probe
/// distance (tsl's `bucket_entry`; these maps do not store hashes). One allocation.
template<typename Map>
A64::SharedCodeCache::Table RobinMapFigure(const Map& map) {
    using Bucket = tsl::detail_robin_hash::bucket_entry<std::pair<typename Map::key_type, typename Map::mapped_type>, false>;
    A64::SharedCodeCache::Table t;
    t.entries = map.size();
    t.largest_bytes = static_cast<u64>(map.bucket_count()) * sizeof(Bucket);
    t.bytes = t.largest_bytes;
    // Any element lies inside the bucket array; an empty map names no address.
    t.largest_address = map.empty() ? 0 : reinterpret_cast<std::uintptr_t>(&*map.begin());
    return t;
}

template<typename T>
u64 VectorBytes(const std::vector<T>& v) {
    return static_cast<u64>(v.capacity()) * sizeof(T);
}

}  // namespace

namespace {

/// Patch 0066: rehash `map` to the smallest power-of-two bucket array that holds its entries at its
/// load factor, when that is at most half of the array it has (a map's arrays never go below 64
/// buckets, UseLoadFactor's start). Returns the bytes given back.
template<typename Map>
size_t ShrinkMap(Map& map) {
    using Bucket = tsl::detail_robin_hash::bucket_entry<std::pair<typename Map::key_type, typename Map::mapped_type>, false>;
    const size_t have = map.bucket_count();
    // Every product here is a power of two times 0.5 or 0.75: exact, so no MXCSR flag is set by it.
    size_t need = 64;
    while (static_cast<size_t>(static_cast<float>(need) * map.max_load_factor()) < map.size()) {
        need *= 2;
    }
    if (need * 2 > have) {
        return 0;
    }
    map.rehash(need);
    return (have - map.bucket_count()) * sizeof(Bucket);
}

}  // namespace

size_t A64EmitX64::ShrinkTables() {
    // tsl's rehash divides the size by the load factor in floats, inexactly: the host's MXCSR, the
    // word the dispatcher installs for a handler (see UseLoadFactor), keeps no flag from it.
    const unsigned int mxcsr = _mm_getcsr();
    size_t freed = ShrinkMap(block_descriptors);
    freed += ShrinkMap(link_heads);
    freed += ShrinkMap(guest_range_pages);
    _mm_setcsr(mxcsr);
    return freed;
}

A64::SharedCodeCache::Tables A64EmitX64::Census() const {
    A64::SharedCodeCache::Tables t;
    t.blocks = RobinMapFigure(block_descriptors);

    // A Jit's own cache links through `patch_information`; a shared one through the link records
    // and their targets' heads (patch 0025).
    if (shared_code) {
        t.link_targets = RobinMapFigure(link_heads);
    } else {
        t.link_targets = RobinMapFigure(patch_information);
        for (const auto& [target, info] : patch_information) {
            t.link_targets.bytes += VectorBytes(info.jg) + VectorBytes(info.jz) + VectorBytes(info.jmp)
                                  + VectorBytes(info.mov_rcx);
        }
    }

    t.links.entries = link_records.size();
    t.links.bytes = VectorBytes(link_records);
    t.links.largest_bytes = t.links.bytes;
    t.links.largest_address = reinterpret_cast<std::uintptr_t>(link_records.data());

    t.fastmem_sites = RobinMapFigure(fastmem_patch_info);
    // Patch 0025: a shared cache's sites, one record each, per region.
    t.fastmem_sites.bytes += VectorBytes(fastmem_site_runs) + VectorBytes(pending_fastmem_sites);
    t.fastmem_sites.bytes += VectorBytes(fastmem_callbacks) + RobinMapFigure(fastmem_callback_index).bytes;  // patch 0051
    for (const FastmemSiteRun& run : fastmem_site_runs) {
        t.fastmem_sites.entries += run.sites.size() + run.wide.size();
        t.fastmem_sites.bytes += VectorBytes(run.sites) + VectorBytes(run.wide);
        if (VectorBytes(run.sites) > t.fastmem_sites.largest_bytes) {
            t.fastmem_sites.largest_bytes = VectorBytes(run.sites);
            t.fastmem_sites.largest_address = reinterpret_cast<std::uintptr_t>(run.sites.data());
        }
    }

    // Patch 0026: one record per block, and the page index over them.
    t.guest_ranges = RobinMapFigure(guest_range_pages);
    t.guest_ranges.entries = guest_ranges.size();
    t.guest_ranges.bytes += VectorBytes(guest_ranges) + VectorBytes(wide_guest_ranges);
    for (const auto& [page, indices] : guest_range_pages) {
        t.guest_ranges.bytes += VectorBytes(indices);
    }
    if (VectorBytes(guest_ranges) > t.guest_ranges.largest_bytes) {
        t.guest_ranges.largest_bytes = VectorBytes(guest_ranges);
        t.guest_ranges.largest_address = reinterpret_cast<std::uintptr_t>(guest_ranges.data());
    }
    return t;
}

void A64EmitX64::GuestPcsOf(const u64* hosts, size_t count, u64* guest_pcs) const {
    std::fill(guest_pcs, guest_pcs + count, ~u64{0});
    if (count == 0) {
        return;
    }
    const u64 lowest = hosts[0];
    const u64 highest = hosts[count - 1];
    // One pass over the block map (no index from host address to block is kept: the dispatcher
    // never needs one), each block's code range searched for in the sorted addresses.
    for (const auto& [key, stored] : block_descriptors) {
        const BlockDescriptor block = Load(stored);
        const IR::LocationDescriptor location = key.Location();
        const u64 begin = reinterpret_cast<u64>(block.entrypoint);
        const u64 end = begin + block.size;
        if (end <= lowest || begin > highest) {
            continue;
        }
        const u64* at = std::lower_bound(hosts, hosts + count, begin);
        for (; at != hosts + count && *at < end; ++at) {
            guest_pcs[at - hosts] = A64::LocationDescriptor{location}.PC();
        }
    }
}

size_t A64EmitX64::FastDispatchTableBytes() {
    return sizeof(FastDispatchEntry) * fast_dispatch_table_size;
}

void A64EmitX64::ResetFastDispatchTable(void* table) {
    ResetFastDispatchTable(table, fast_dispatch_table_size);
}

size_t A64EmitX64::FastDispatchEntries(const A64::UserConfig& conf) {
    const size_t n = conf.od_fast_dispatch_entries;
    const bool valid = n >= 0x40 && n <= 0x10000 && (n & (n - 1)) == 0;
    return valid ? n : fast_dispatch_table_size;
}

size_t A64EmitX64::FastDispatchTableBytes(size_t entries) {
    return sizeof(FastDispatchEntry) * entries;
}

void A64EmitX64::ResetFastDispatchTable(void* table, size_t entries) {
    auto* e = static_cast<FastDispatchEntry*>(table);
    for (size_t i = 0; i < entries; i++) {
        e[i] = FastDispatchEntry{};
    }
}

void A64EmitX64::EmitLoadConfPointer(Xbyak::Reg64 reg) {
    if (shared_code) {
        code.mov(reg, qword[r15 + offsetof(A64JitState, od_conf)]);
    } else {
        code.mov(reg, reinterpret_cast<u64>(&conf));
    }
}

void A64EmitX64::GenTerminalHandlers() {
    // PC ends up in rbp, location_descriptor ends up in rbx
    const auto calculate_location_descriptor = [this] {
        // This calculation has to match up with A64::LocationDescriptor::UniqueHash
        // TODO: Optimization is available here based on known state of fpcr.
        code.mov(rbp, qword[r15 + offsetof(A64JitState, pc)]);
        code.mov(rcx, A64::LocationDescriptor::pc_mask);
        code.and_(rcx, rbp);
        code.mov(ebx, dword[r15 + offsetof(A64JitState, fpcr)]);
        code.and_(ebx, A64::LocationDescriptor::fpcr_mask);
        code.shl(rbx, A64::LocationDescriptor::fpcr_shift);
        code.or_(rbx, rcx);
    };

    Xbyak::Label fast_dispatch_cache_miss, rsb_cache_miss;

    code.align();
    terminal_handler_pop_rsb_hint = code.getCurr<const void*>();
    calculate_location_descriptor();
    code.mov(eax, dword[r15 + offsetof(A64JitState, rsb_ptr)]);
    code.sub(eax, 1);
    code.and_(eax, u32(A64JitState::RSBPtrMask));
    code.mov(dword[r15 + offsetof(A64JitState, rsb_ptr)], eax);
    code.cmp(rbx, qword[r15 + offsetof(A64JitState, rsb_location_descriptors) + rax * sizeof(u64)]);
    if (conf.HasOptimization(OptimizationFlag::FastDispatch)) {
        code.jne(rsb_cache_miss);
    } else {
        code.jne(code.GetReturnFromRunCodeAddress());
    }
    code.mov(rax, qword[r15 + offsetof(A64JitState, rsb_codeptrs) + rax * sizeof(u64)]);
    // Omnidroid patch 0018: a return-stack-buffer hit checks what `ReturnFromRunCode` checks --
    // the cycle budget when cycle counting is on, and the halt flag -- and on either leaves
    // through it (the guest PC is already stored), so a guest loop through `RET` can be stopped
    // and `ReturnStackBuffer` can stay on.
    if (conf.enable_cycle_counting) {
        code.cmp(qword[rsp + ABI_SHADOW_SPACE + offsetof(StackLayout, cycles_remaining)], 0);
        code.jng(code.GetReturnFromRunCodeAddress());
    }
    code.cmp(dword[r15 + offsetof(A64JitState, halt_reason)], 0);
    code.jne(code.GetReturnFromRunCodeAddress());
    code.jmp(rax);
    PerfMapRegister(terminal_handler_pop_rsb_hint, code.getCurr(), "a64_terminal_handler_pop_rsb_hint");

    if (conf.HasOptimization(OptimizationFlag::FastDispatch)) {
        code.align();
        terminal_handler_fast_dispatch_hint = code.getCurr<const void*>();
        calculate_location_descriptor();
        code.L(rsb_cache_miss);
        terminal_handler_fast_dispatch_probe = code.getCurr<const void*>();  // patch 0042
        // Omnidroid patch 0019: what patch 0018 does for a return-stack-buffer hit, for every
        // transfer this handler serves -- a `BR`/`BLR`, and a `RET` that missed the buffer. Before
        // the table is probed, compare the cycle budget (when cycle counting is on) and the halt
        // flag with 0, and on either leave through `ReturnFromRunCode` (the guest PC is already
        // stored). Both the hit and the miss (`LookupBlock`) paths start here, and neither changes
        // the budget, so a guest loop through this handler can be stopped and FastDispatch can
        // stay on.
        if (conf.enable_cycle_counting) {
            code.cmp(qword[rsp + ABI_SHADOW_SPACE + offsetof(StackLayout, cycles_remaining)], 0);
            code.jng(code.GetReturnFromRunCodeAddress());
        }
        code.cmp(dword[r15 + offsetof(A64JitState, halt_reason)], 0);
        code.jne(code.GetReturnFromRunCodeAddress());
        if (shared_code) {
            // Omnidroid patch 0022: the running thread's own table.
            code.mov(r12, qword[r15 + offsetof(A64JitState, od_fast_dispatch_table)]);
        } else {
            code.mov(r12, reinterpret_cast<u64>(fast_dispatch_table->data()));
        }
        code.mov(rbp, rbx);
        if (code.HasHostFeature(HostFeature::SSE42)) {
            code.crc32(rbp, r12);
        }
        code.and_(ebp, static_cast<u32>(fast_dispatch_mask));
        code.lea(rbp, ptr[r12 + rbp]);
        code.cmp(rbx, qword[rbp + offsetof(FastDispatchEntry, location_descriptor)]);
        code.jne(fast_dispatch_cache_miss);
        code.jmp(ptr[rbp + offsetof(FastDispatchEntry, code_ptr)]);
        code.L(fast_dispatch_cache_miss);
        terminal_handler_fast_dispatch_miss = code.getCurr<const void*>();  // patch 0042
        if (shared_code) {
            // Omnidroid patch 0022: the lookup consults this same table (the dispatcher probes the
            // running thread's own before taking the cache's lock), so the entry must not name this
            // location while it still holds another location's code pointer: it is written whole
            // after the lookup. rbx and rbp are callee-saved, so they survive the call.
            code.LookupBlock();
            code.mov(ptr[rbp + offsetof(FastDispatchEntry, code_ptr)], rax);
            code.mov(qword[rbp + offsetof(FastDispatchEntry, location_descriptor)], rbx);
        } else {
            code.mov(qword[rbp + offsetof(FastDispatchEntry, location_descriptor)], rbx);
            code.LookupBlock();
            code.mov(ptr[rbp + offsetof(FastDispatchEntry, code_ptr)], rax);
        }
        code.jmp(rax);
        PerfMapRegister(terminal_handler_fast_dispatch_hint, code.getCurr(), "a64_terminal_handler_fast_dispatch_hint");

        if (shared_code) {
            // Omnidroid patch 0022: no C++-callable lookup into "the" table -- there is one per
            // thread, each reset by its owner (Unpatch does not reach into them) -- but one into a
            // table the caller names, for the dispatcher's lookup to consult the running thread's.
            code.align();
            fast_dispatch_table_lookup_in = code.getCurr<FastDispatchEntry& (*)(u64, void*)>();
            if (code.HasHostFeature(HostFeature::SSE42)) {
                code.crc32(code.ABI_PARAM1, code.ABI_PARAM2);
            }
            code.and_(code.ABI_PARAM1.cvt32(), static_cast<u32>(fast_dispatch_mask));
            code.lea(code.ABI_RETURN, code.ptr[code.ABI_PARAM2 + code.ABI_PARAM1]);
            code.ret();
            PerfMapRegister(fast_dispatch_table_lookup_in, code.getCurr(), "a64_fast_dispatch_table_lookup_in");
            return;
        }

        code.align();
        fast_dispatch_table_lookup = code.getCurr<FastDispatchEntry& (*)(u64)>();
        code.mov(code.ABI_PARAM2, reinterpret_cast<u64>(fast_dispatch_table->data()));
        if (code.HasHostFeature(HostFeature::SSE42)) {
            code.crc32(code.ABI_PARAM1, code.ABI_PARAM2);
        }
        code.and_(code.ABI_PARAM1.cvt32(), static_cast<u32>(fast_dispatch_mask));
        code.lea(code.ABI_RETURN, code.ptr[code.ABI_PARAM2 + code.ABI_PARAM1]);
        code.ret();
        PerfMapRegister(fast_dispatch_table_lookup, code.getCurr(), "a64_fast_dispatch_table_lookup");
    }
}

void A64EmitX64::EmitPushRSB(EmitContext& ctx, IR::Inst* inst) {
    if (!conf.HasOptimization(OptimizationFlag::ReturnStackBuffer)) {
        return;
    }

    EmitX64::EmitPushRSB(ctx, inst);
}

void A64EmitX64::EmitA64SetCheckBit(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const Xbyak::Reg8 to_store = ctx.reg_alloc.UseGpr(args[0]).cvt8();
    code.mov(code.byte[rsp + ABI_SHADOW_SPACE + offsetof(StackLayout, check_bit)], to_store);
}

void A64EmitX64::EmitA64GetCFlag(A64EmitContext& ctx, IR::Inst* inst) {
    const Xbyak::Reg32 result = ctx.reg_alloc.ScratchGpr().cvt32();
    code.mov(result, dword[r15 + offsetof(A64JitState, cpsr_nzcv)]);
    code.shr(result, NZCV::x64_c_flag_bit);
    code.and_(result, 1);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetNZCVRaw(A64EmitContext& ctx, IR::Inst* inst) {
    const Xbyak::Reg32 nzcv_raw = ctx.reg_alloc.ScratchGpr().cvt32();

    code.mov(nzcv_raw, dword[r15 + offsetof(A64JitState, cpsr_nzcv)]);

    if (code.HasHostFeature(HostFeature::FastBMI2)) {
        const Xbyak::Reg32 tmp = ctx.reg_alloc.ScratchGpr().cvt32();
        code.mov(tmp, NZCV::x64_mask);
        code.pext(nzcv_raw, nzcv_raw, tmp);
        code.shl(nzcv_raw, 28);
    } else {
        code.and_(nzcv_raw, NZCV::x64_mask);
        code.imul(nzcv_raw, nzcv_raw, NZCV::from_x64_multiplier);
        code.and_(nzcv_raw, NZCV::arm_mask);
    }

    ctx.reg_alloc.DefineValue(inst, nzcv_raw);
}

void A64EmitX64::EmitA64SetNZCVRaw(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const Xbyak::Reg32 nzcv_raw = ctx.reg_alloc.UseScratchGpr(args[0]).cvt32();

    code.shr(nzcv_raw, 28);
    if (code.HasHostFeature(HostFeature::FastBMI2)) {
        const Xbyak::Reg32 tmp = ctx.reg_alloc.ScratchGpr().cvt32();
        code.mov(tmp, NZCV::x64_mask);
        code.pdep(nzcv_raw, nzcv_raw, tmp);
    } else {
        code.imul(nzcv_raw, nzcv_raw, NZCV::to_x64_multiplier);
        code.and_(nzcv_raw, NZCV::x64_mask);
    }
    code.mov(dword[r15 + offsetof(A64JitState, cpsr_nzcv)], nzcv_raw);
}

void A64EmitX64::EmitA64SetNZCV(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const Xbyak::Reg32 to_store = ctx.reg_alloc.UseScratchGpr(args[0]).cvt32();
    code.mov(dword[r15 + offsetof(A64JitState, cpsr_nzcv)], to_store);
}

void A64EmitX64::EmitA64GetW(A64EmitContext& ctx, IR::Inst* inst) {
    const A64::Reg reg = inst->GetArg(0).GetA64RegRef();
    const Xbyak::Reg32 result = ctx.reg_alloc.ScratchGpr().cvt32();

    code.mov(result, dword[r15 + offsetof(A64JitState, reg) + sizeof(u64) * static_cast<size_t>(reg)]);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetX(A64EmitContext& ctx, IR::Inst* inst) {
    const A64::Reg reg = inst->GetArg(0).GetA64RegRef();
    const Xbyak::Reg64 result = ctx.reg_alloc.ScratchGpr();

    code.mov(result, qword[r15 + offsetof(A64JitState, reg) + sizeof(u64) * static_cast<size_t>(reg)]);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetS(A64EmitContext& ctx, IR::Inst* inst) {
    const A64::Vec vec = inst->GetArg(0).GetA64VecRef();
    const auto addr = qword[r15 + offsetof(A64JitState, vec) + sizeof(u64) * 2 * static_cast<size_t>(vec)];

    const Xbyak::Xmm result = ctx.reg_alloc.ScratchXmm();
    code.movd(result, addr);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetD(A64EmitContext& ctx, IR::Inst* inst) {
    const A64::Vec vec = inst->GetArg(0).GetA64VecRef();
    const auto addr = qword[r15 + offsetof(A64JitState, vec) + sizeof(u64) * 2 * static_cast<size_t>(vec)];

    const Xbyak::Xmm result = ctx.reg_alloc.ScratchXmm();
    code.movq(result, addr);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetQ(A64EmitContext& ctx, IR::Inst* inst) {
    const A64::Vec vec = inst->GetArg(0).GetA64VecRef();
    const auto addr = xword[r15 + offsetof(A64JitState, vec) + sizeof(u64) * 2 * static_cast<size_t>(vec)];

    const Xbyak::Xmm result = ctx.reg_alloc.ScratchXmm();
    code.movaps(result, addr);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetSP(A64EmitContext& ctx, IR::Inst* inst) {
    const Xbyak::Reg64 result = ctx.reg_alloc.ScratchGpr();
    code.mov(result, qword[r15 + offsetof(A64JitState, sp)]);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetFPCR(A64EmitContext& ctx, IR::Inst* inst) {
    const Xbyak::Reg32 result = ctx.reg_alloc.ScratchGpr().cvt32();
    code.mov(result, dword[r15 + offsetof(A64JitState, fpcr)]);
    ctx.reg_alloc.DefineValue(inst, result);
}

static u32 GetFPSRImpl(A64JitState* jit_state) {
    return jit_state->GetFpsr();
}

void A64EmitX64::EmitA64GetFPSR(A64EmitContext& ctx, IR::Inst* inst) {
    ctx.reg_alloc.HostCall(inst);
    code.mov(code.ABI_PARAM1, code.r15);
    code.stmxcsr(code.dword[code.r15 + offsetof(A64JitState, guest_MXCSR)]);
    code.CallFunction(GetFPSRImpl);
}

void A64EmitX64::EmitA64SetW(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const A64::Reg reg = inst->GetArg(0).GetA64RegRef();
    const auto addr = qword[r15 + offsetof(A64JitState, reg) + sizeof(u64) * static_cast<size_t>(reg)];
    if (args[1].FitsInImmediateS32()) {
        code.mov(addr, args[1].GetImmediateS32());
    } else {
        // TODO: zext tracking, xmm variant
        const Xbyak::Reg64 to_store = ctx.reg_alloc.UseScratchGpr(args[1]);
        code.mov(to_store.cvt32(), to_store.cvt32());
        code.mov(addr, to_store);
    }
}

void A64EmitX64::EmitA64SetX(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const A64::Reg reg = inst->GetArg(0).GetA64RegRef();
    const auto addr = qword[r15 + offsetof(A64JitState, reg) + sizeof(u64) * static_cast<size_t>(reg)];
    if (args[1].FitsInImmediateS32()) {
        code.mov(addr, args[1].GetImmediateS32());
    } else if (args[1].IsInXmm()) {
        const Xbyak::Xmm to_store = ctx.reg_alloc.UseXmm(args[1]);
        code.movq(addr, to_store);
    } else {
        const Xbyak::Reg64 to_store = ctx.reg_alloc.UseGpr(args[1]);
        code.mov(addr, to_store);
    }
}

void A64EmitX64::EmitA64SetS(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const A64::Vec vec = inst->GetArg(0).GetA64VecRef();
    const auto addr = xword[r15 + offsetof(A64JitState, vec) + sizeof(u64) * 2 * static_cast<size_t>(vec)];

    const Xbyak::Xmm to_store = ctx.reg_alloc.UseXmm(args[1]);
    const Xbyak::Xmm tmp = ctx.reg_alloc.ScratchXmm();
    // TODO: Optimize
    code.pxor(tmp, tmp);
    code.movss(tmp, to_store);
    code.movaps(addr, tmp);
}

void A64EmitX64::EmitA64SetD(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const A64::Vec vec = inst->GetArg(0).GetA64VecRef();
    const auto addr = xword[r15 + offsetof(A64JitState, vec) + sizeof(u64) * 2 * static_cast<size_t>(vec)];

    const Xbyak::Xmm to_store = ctx.reg_alloc.UseScratchXmm(args[1]);
    code.movq(to_store, to_store);  // TODO: Remove when able
    code.movaps(addr, to_store);
}

void A64EmitX64::EmitA64SetQ(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const A64::Vec vec = inst->GetArg(0).GetA64VecRef();
    const auto addr = xword[r15 + offsetof(A64JitState, vec) + sizeof(u64) * 2 * static_cast<size_t>(vec)];

    const Xbyak::Xmm to_store = ctx.reg_alloc.UseXmm(args[1]);
    code.movaps(addr, to_store);
}

void A64EmitX64::EmitA64SetSP(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const auto addr = qword[r15 + offsetof(A64JitState, sp)];
    if (args[0].FitsInImmediateS32()) {
        code.mov(addr, args[0].GetImmediateS32());
    } else if (args[0].IsInXmm()) {
        const Xbyak::Xmm to_store = ctx.reg_alloc.UseXmm(args[0]);
        code.movq(addr, to_store);
    } else {
        const Xbyak::Reg64 to_store = ctx.reg_alloc.UseGpr(args[0]);
        code.mov(addr, to_store);
    }
}

static void SetFPCRImpl(A64JitState* jit_state, u32 value) {
    jit_state->SetFpcr(value);
}

void A64EmitX64::EmitA64SetFPCR(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ctx.reg_alloc.HostCall(nullptr, {}, args[0]);
    code.mov(code.ABI_PARAM1, code.r15);
    code.CallFunction(SetFPCRImpl);
    code.ldmxcsr(code.dword[code.r15 + offsetof(A64JitState, guest_MXCSR)]);
}

static void SetFPSRImpl(A64JitState* jit_state, u32 value) {
    jit_state->SetFpsr(value);
}

void A64EmitX64::EmitA64SetFPSR(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ctx.reg_alloc.HostCall(nullptr, {}, args[0]);
    code.mov(code.ABI_PARAM1, code.r15);
    code.CallFunction(SetFPSRImpl);
    code.ldmxcsr(code.dword[code.r15 + offsetof(A64JitState, guest_MXCSR)]);
}

void A64EmitX64::EmitA64SetPC(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const auto addr = qword[r15 + offsetof(A64JitState, pc)];

    // Omnidroid patch 0042: the last instruction of a block that ends in a dispatch hint also
    // leaves the target in rbp for the inline hit path (nothing after it needs a register). Not
    // where the block changes FPCR (`MSR FPCR`): the inline path takes FPCR's part of the
    // location from the block's own.
    const auto& terminal = ctx.block.GetTerminal();
    const bool hint = boost::get<IR::Term::FastDispatchHint>(&terminal) != nullptr
                   || boost::get<IR::Term::PopRSBHint>(&terminal) != nullptr;
    const bool keep = hint && &ctx.block.back() == inst && !ctx.IsSingleStep()
                   && live_fast_dispatch_inline.load(std::memory_order_relaxed) != 0
                   && std::none_of(ctx.block.begin(), ctx.block.end(), [](const IR::Inst& i) { return i.GetOpcode() == IR::Opcode::A64SetFPCR; });

    if (args[0].FitsInImmediateS32()) {
        code.mov(addr, args[0].GetImmediateS32());
        if (keep) {
            code.mov(rbp, args[0].GetImmediateS32());
        }
    } else if (args[0].IsInXmm()) {
        const Xbyak::Xmm to_store = ctx.reg_alloc.UseXmm(args[0]);
        code.movq(addr, to_store);
        if (keep) {
            code.movq(rbp, to_store);
        }
    } else {
        const Xbyak::Reg64 to_store = ctx.reg_alloc.UseGpr(args[0]);
        code.mov(addr, to_store);
        if (keep) {
            code.mov(rbp, to_store);
        }
    }
    od_pc_in_rbp = keep;
}

void A64EmitX64::EmitA64CallSupervisor(A64EmitContext& ctx, IR::Inst* inst) {
    ctx.reg_alloc.HostCall(nullptr);
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ASSERT(args[0].IsImmediate());
    const u32 imm = args[0].GetImmediateU32();
    if (shared_code) {
        // Omnidroid patch 0022: through the prelude's trampoline (see `svc_trampoline`), so that a
        // thread parked in the callback -- an import that waits -- holds nothing of this block.
        Xbyak::Label returned;
        code.mov(code.ABI_PARAM2.cvt32(), imm);
        code.lea(rax, ptr[rip + returned]);
        code.jmp(svc_trampoline);
        code.L(returned);
    } else {
        UserCallback<&A64::UserCallbacks::CallSVC>().EmitCall(code, [&](RegList param) {
            code.mov(param[0], imm);
        });
    }
    // The kernel would have to execute ERET to get here, which would clear exclusive state.
    code.mov(code.byte[r15 + offsetof(A64JitState, exclusive_state)], u8(0));
}

void A64EmitX64::GenSharedSvcTrampolines() {
    code.align();
    svc_trampoline = code.getCurr<const void*>();
    // rax: where the block resumes; the callback's second argument: the immediate. Published
    // before the call, from here -- by the time a reclaimer can see it, this thread is out of the
    // block -- and taken back (and cleared) atomically after it, so that a reclaimer's swap either
    // lands before the take, and the thread resumes where it was sent, or fails.
    code.mov(qword[r15 + offsetof(A64JitState, od_callback_return)], rax);
    UserCallback<&A64::UserCallbacks::CallSVC>().EmitCall(code, [](RegList) {});
    code.xor_(eax, eax);
    code.xchg(qword[r15 + offsetof(A64JitState, od_callback_return)], rax);
    code.jmp(rax);
    PerfMapRegister(svc_trampoline, code.getCurr(), "a64_svc_trampoline");

    code.align();
    svc_resume_retired = code.getCurr<const void*>();
    // A block's tail after its SVC, for a block in a retired region: clear the exclusive state, as
    // the tail does, and leave the run -- the retirement raised a halt on this thread, so the
    // tail's halt test would have left it here too. The PC is already the SVC's successor (or
    // what the callback wrote). The block's cycle charge, emitted after the call, is not made.
    code.mov(code.byte[r15 + offsetof(A64JitState, exclusive_state)], u8(0));
    code.jmp(code.GetForceReturnFromRunCodeAddress());
    PerfMapRegister(svc_resume_retired, code.getCurr(), "a64_svc_resume_retired");
}

void A64EmitX64::EmitA64ExceptionRaised(A64EmitContext& ctx, IR::Inst* inst) {
    ctx.reg_alloc.HostCall(nullptr);
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ASSERT(args[0].IsImmediate() && args[1].IsImmediate());
    const u64 pc = args[0].GetImmediateU64();
    const u64 exception = args[1].GetImmediateU64();
    UserCallback<&A64::UserCallbacks::ExceptionRaised>().EmitCall(code, [&](RegList param) {
        code.mov(param[0], pc);
        code.mov(param[1], exception);
    });
}

void A64EmitX64::EmitA64DataCacheOperationRaised(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ctx.reg_alloc.HostCall(nullptr, {}, args[1], args[2]);
    UserCallback<&A64::UserCallbacks::DataCacheOperationRaised>().EmitCall(code);
}

void A64EmitX64::EmitA64InstructionCacheOperationRaised(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ctx.reg_alloc.HostCall(nullptr, {}, args[0], args[1]);
    UserCallback<&A64::UserCallbacks::InstructionCacheOperationRaised>().EmitCall(code);
}

void A64EmitX64::EmitA64DataSynchronizationBarrier(A64EmitContext&, IR::Inst*) {
    code.mfence();
    code.lfence();
}

void A64EmitX64::EmitA64DataMemoryBarrier(A64EmitContext&, IR::Inst*) {
    code.mfence();
}

void A64EmitX64::EmitA64InstructionSynchronizationBarrier(A64EmitContext& ctx, IR::Inst*) {
    if (!conf.hook_isb) {
        return;
    }

    ctx.reg_alloc.HostCall(nullptr);
    UserCallback<&A64::UserCallbacks::InstructionSynchronizationBarrierRaised>().EmitCall(code);
}

void A64EmitX64::EmitA64GetCNTFRQ(A64EmitContext& ctx, IR::Inst* inst) {
    const Xbyak::Reg32 result = ctx.reg_alloc.ScratchGpr().cvt32();
    code.mov(result, conf.cntfrq_el0);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetCNTPCT(A64EmitContext& ctx, IR::Inst* inst) {
    ctx.reg_alloc.HostCall(inst);
    if (!conf.wall_clock_cntpct) {
        code.UpdateTicks();
    }
    UserCallback<&A64::UserCallbacks::GetCNTPCT>().EmitCall(code);
}

void A64EmitX64::EmitA64GetCTR(A64EmitContext& ctx, IR::Inst* inst) {
    const Xbyak::Reg32 result = ctx.reg_alloc.ScratchGpr().cvt32();
    code.mov(result, conf.ctr_el0);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetDCZID(A64EmitContext& ctx, IR::Inst* inst) {
    const Xbyak::Reg32 result = ctx.reg_alloc.ScratchGpr().cvt32();
    code.mov(result, conf.dczid_el0);
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetTPIDR(A64EmitContext& ctx, IR::Inst* inst) {
    const Xbyak::Reg64 result = ctx.reg_alloc.ScratchGpr();
    if (shared_code) {
        // Omnidroid patch 0022: the running thread's box.
        code.mov(result, qword[r15 + offsetof(A64JitState, od_tpidr_el0)]);
        code.mov(result, qword[result]);
    } else if (conf.tpidr_el0) {
        code.mov(result, u64(conf.tpidr_el0));
        code.mov(result, qword[result]);
    } else {
        code.xor_(result.cvt32(), result.cvt32());
    }
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64GetTPIDRRO(A64EmitContext& ctx, IR::Inst* inst) {
    const Xbyak::Reg64 result = ctx.reg_alloc.ScratchGpr();
    if (shared_code) {
        code.mov(result, qword[r15 + offsetof(A64JitState, od_tpidrro_el0)]);
        code.mov(result, qword[result]);
    } else if (conf.tpidrro_el0) {
        code.mov(result, u64(conf.tpidrro_el0));
        code.mov(result, qword[result]);
    } else {
        code.xor_(result.cvt32(), result.cvt32());
    }
    ctx.reg_alloc.DefineValue(inst, result);
}

void A64EmitX64::EmitA64SetTPIDR(A64EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    const Xbyak::Reg64 value = ctx.reg_alloc.UseGpr(args[0]);
    const Xbyak::Reg64 addr = ctx.reg_alloc.ScratchGpr();
    if (shared_code) {
        code.mov(addr, qword[r15 + offsetof(A64JitState, od_tpidr_el0)]);
        code.mov(qword[addr], value);
    } else if (conf.tpidr_el0) {
        code.mov(addr, u64(conf.tpidr_el0));
        code.mov(qword[addr], value);
    }
}

std::string A64EmitX64::LocationDescriptorToFriendlyName(const IR::LocationDescriptor& ir_descriptor) const {
    const A64::LocationDescriptor descriptor{ir_descriptor};
    return fmt::format("a64_{:016X}_fpcr{:08X}",
                       descriptor.PC(),
                       descriptor.FPCR().Value());
}

void A64EmitX64::EmitTerminalImpl(IR::Term::Interpret terminal, IR::LocationDescriptor, bool) {
    code.SwitchMxcsrOnExit();
    UserCallback<&A64::UserCallbacks::InterpreterFallback>().EmitCall(code, [&](RegList param) {
        code.mov(param[0], A64::LocationDescriptor{terminal.next}.PC());
        code.mov(qword[r15 + offsetof(A64JitState, pc)], param[0]);
        code.mov(param[1].cvt32(), terminal.num_instructions);
    });
    code.ReturnFromRunCode(true);  // TODO: Check cycles
}

void A64EmitX64::EmitTerminalImpl(IR::Term::ReturnToDispatch, IR::LocationDescriptor, bool) {
    code.ReturnFromRunCode();
}

void A64EmitX64::EmitTerminalImpl(IR::Term::LinkBlock terminal, IR::LocationDescriptor, bool is_single_step) {
    if (!conf.HasOptimization(OptimizationFlag::BlockLinking) || is_single_step) {
        code.mov(rax, A64::LocationDescriptor{terminal.next}.PC());
        code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
        code.ReturnFromRunCode();
        return;
    }

    if (shared_code) {
        // Omnidroid patch 0022: upstream's check, then a jump through the target's slot rather
        // than a `jg` that is rewritten when the target appears or goes. Same outcomes: linked,
        // straight to the target; unlinked, the dispatcher; budget spent (or halt raised, without
        // cycle counting), leave Run with the PC stored.
        if ((live_compact_code.load(std::memory_order_relaxed) & kCompactLinkTails) != 0) {
            // Omnidroid patch 0061: a spent budget (or a halt) leaves through the slot's own tail --
            // store the PC, enter the dispatcher, whose loop top checks the same two things and
            // returns -- instead of a second copy of that tail with a forced return: 22 bytes less a
            // link, the same outcome. (Measured 9-18% slower on a loop of eight linked two-
            // instruction blocks, though the hot path's bytes are the same: layout. Its own bit.)
            auto tail = std::make_shared<Xbyak::Label>();
            Xbyak::Label& slot = NewLinkSlot(terminal.next, 0, tail);
            if (conf.enable_cycle_counting) {
                code.cmp(qword[rsp + ABI_SHADOW_SPACE + offsetof(StackLayout, cycles_remaining)], 0);
                code.jng(*tail, code.T_NEAR);
            } else {
                code.cmp(dword[r15 + offsetof(A64JitState, halt_reason)], 0);
                code.jne(*tail, code.T_NEAR);
            }
            code.jmp(qword[rip + slot]);
            code.L(*tail);
            code.mov(rax, A64::LocationDescriptor{terminal.next}.PC());
            code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
            code.jmp(code.GetReturnFromRunCodeAddress());
            return;
        }
        Xbyak::Label exit;
        if (conf.enable_cycle_counting) {
            code.cmp(qword[rsp + ABI_SHADOW_SPACE + offsetof(StackLayout, cycles_remaining)], 0);
            code.jng(exit, code.T_NEAR);
        } else {
            code.cmp(dword[r15 + offsetof(A64JitState, halt_reason)], 0);
            code.jne(exit, code.T_NEAR);
        }
        EmitSlotJump(terminal.next);
        code.L(exit);
        code.mov(rax, A64::LocationDescriptor{terminal.next}.PC());
        code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
        code.ForceReturnFromRunCode();
        return;
    }

    if (conf.enable_cycle_counting) {
        code.cmp(qword[rsp + ABI_SHADOW_SPACE + offsetof(StackLayout, cycles_remaining)], 0);

        patch_information[terminal.next].jg.push_back(code.getCurr());
        if (const auto next_bb = GetBasicBlock(terminal.next)) {
            EmitPatchJg(terminal.next, next_bb->entrypoint);
        } else {
            EmitPatchJg(terminal.next);
        }
    } else {
        code.cmp(dword[r15 + offsetof(A64JitState, halt_reason)], 0);

        patch_information[terminal.next].jz.push_back(code.getCurr());
        if (const auto next_bb = GetBasicBlock(terminal.next)) {
            EmitPatchJz(terminal.next, next_bb->entrypoint);
        } else {
            EmitPatchJz(terminal.next);
        }
    }

    code.mov(rax, A64::LocationDescriptor{terminal.next}.PC());
    code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
    code.ForceReturnFromRunCode();
}

void A64EmitX64::EmitTerminalImpl(IR::Term::LinkBlockFast terminal, IR::LocationDescriptor, bool is_single_step) {
    if (!conf.HasOptimization(OptimizationFlag::BlockLinking) || is_single_step) {
        code.mov(rax, A64::LocationDescriptor{terminal.next}.PC());
        code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
        code.ReturnFromRunCode();
        return;
    }

    if (shared_code) {
        EmitSlotJump(terminal.next);  // Omnidroid patch 0022
        return;
    }

    patch_information[terminal.next].jmp.push_back(code.getCurr());
    if (auto next_bb = GetBasicBlock(terminal.next)) {
        EmitPatchJmp(terminal.next, next_bb->entrypoint);
    } else {
        EmitPatchJmp(terminal.next);
    }
}

void A64EmitX64::EmitTerminalImpl(IR::Term::PopRSBHint, IR::LocationDescriptor initial_location, bool is_single_step) {
    if (!conf.HasOptimization(OptimizationFlag::ReturnStackBuffer) || is_single_step) {
        code.ReturnFromRunCode();
        return;
    }

    if (od_pc_in_rbp) {
        // Omnidroid patch 0042: the shared handler's hit path, here: one host indirect jump per
        // `RET` site (the host predicts it per site), from the target already in rbp (no reload of
        // the PC just stored). A miss falls into the handler's table probe (rbx = the location).
        EmitInlineLocation(initial_location);
        code.mov(eax, dword[r15 + offsetof(A64JitState, rsb_ptr)]);
        code.sub(eax, 1);
        code.and_(eax, u32(A64JitState::RSBPtrMask));
        code.mov(dword[r15 + offsetof(A64JitState, rsb_ptr)], eax);
        code.cmp(rbx, qword[r15 + offsetof(A64JitState, rsb_location_descriptors) + rax * sizeof(u64)]);
        if (conf.HasOptimization(OptimizationFlag::FastDispatch)) {
            code.jne(terminal_handler_fast_dispatch_probe);
        } else {
            code.jne(code.GetReturnFromRunCodeAddress());
        }
        code.mov(rax, qword[r15 + offsetof(A64JitState, rsb_codeptrs) + rax * sizeof(u64)]);
        EmitInlineBudgetAndHaltChecks();
        code.jmp(rax);
        return;
    }

    code.jmp(terminal_handler_pop_rsb_hint);
}

/// Patch 0042: rbx = the location descriptor of the target in rbp, FPCR's part from this block's
/// (unchanged by it: `EmitA64SetPC` does not keep the PC where the block sets FPCR).
void A64EmitX64::EmitInlineLocation(const IR::LocationDescriptor& initial_location) {
    const u64 fpcr_part = A64::LocationDescriptor{initial_location}.SetPC(0).UniqueHash();
    code.mov(rbx, A64::LocationDescriptor::pc_mask);
    code.and_(rbx, rbp);
    if (fpcr_part != 0) {
        code.mov(rcx, fpcr_part);
        code.or_(rbx, rcx);
    }
}

/// Patch 0042: what patches 0018/0019 check before a dispatch-hint jump, inline (the PC is stored).
void A64EmitX64::EmitInlineBudgetAndHaltChecks() {
    if (conf.enable_cycle_counting) {
        code.cmp(qword[rsp + ABI_SHADOW_SPACE + offsetof(StackLayout, cycles_remaining)], 0);
        code.jng(code.GetReturnFromRunCodeAddress());
    }
    code.cmp(dword[r15 + offsetof(A64JitState, halt_reason)], 0);
    code.jne(code.GetReturnFromRunCodeAddress());
}

void A64EmitX64::EmitTerminalImpl(IR::Term::FastDispatchHint, IR::LocationDescriptor initial_location, bool is_single_step) {
    if (!conf.HasOptimization(OptimizationFlag::FastDispatch) || is_single_step) {
        code.ReturnFromRunCode();
        return;
    }

    if (od_pc_in_rbp) {
        // Omnidroid patch 0042: the shared handler's hit path, here (see PopRSBHint above): the
        // location from rbp, the checks, the hash and probe of the running thread's table, and an
        // indirect jump of this `BR`/`BLR` site's own. A miss falls into the handler's lookup
        // (rbx = the location, rbp = the table entry).
        EmitInlineLocation(initial_location);
        EmitInlineBudgetAndHaltChecks();
        if (shared_code) {
            code.mov(r12, qword[r15 + offsetof(A64JitState, od_fast_dispatch_table)]);
        } else {
            code.mov(r12, reinterpret_cast<u64>(fast_dispatch_table->data()));
        }
        code.mov(rbp, rbx);
        if (code.HasHostFeature(HostFeature::SSE42)) {
            code.crc32(rbp, r12);
        }
        code.and_(ebp, static_cast<u32>(fast_dispatch_mask));
        code.lea(rbp, ptr[r12 + rbp]);
        code.cmp(rbx, qword[rbp + offsetof(FastDispatchEntry, location_descriptor)]);
        code.jne(terminal_handler_fast_dispatch_miss);
        code.jmp(ptr[rbp + offsetof(FastDispatchEntry, code_ptr)]);
        return;
    }

    code.jmp(terminal_handler_fast_dispatch_hint);
}

void A64EmitX64::EmitTerminalImpl(IR::Term::If terminal, IR::LocationDescriptor initial_location, bool is_single_step) {
    switch (terminal.if_) {
    case IR::Cond::AL:
    case IR::Cond::NV:
        EmitTerminal(terminal.then_, initial_location, is_single_step);
        break;
    default:
        Xbyak::Label pass = EmitCond(terminal.if_);
        EmitTerminal(terminal.else_, initial_location, is_single_step);
        code.L(pass);
        EmitTerminal(terminal.then_, initial_location, is_single_step);
        break;
    }
}

void A64EmitX64::EmitTerminalImpl(IR::Term::CheckBit terminal, IR::LocationDescriptor initial_location, bool is_single_step) {
    Xbyak::Label fail;
    code.cmp(code.byte[rsp + ABI_SHADOW_SPACE + offsetof(StackLayout, check_bit)], u8(0));
    code.jz(fail);
    EmitTerminal(terminal.then_, initial_location, is_single_step);
    code.L(fail);
    EmitTerminal(terminal.else_, initial_location, is_single_step);
}

void A64EmitX64::EmitTerminalImpl(IR::Term::CheckHalt terminal, IR::LocationDescriptor initial_location, bool is_single_step) {
    code.cmp(dword[r15 + offsetof(A64JitState, halt_reason)], 0);
    code.jne(code.GetForceReturnFromRunCodeAddress());
    EmitTerminal(terminal.else_, initial_location, is_single_step);
}

void A64EmitX64::EmitPatchJg(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr) {
    const CodePtr patch_location = code.getCurr();
    if (target_code_ptr) {
        code.jg(target_code_ptr);
    } else {
        code.mov(rax, A64::LocationDescriptor{target_desc}.PC());
        code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
        code.jg(code.GetReturnFromRunCodeAddress());
    }
    code.EnsurePatchLocationSize(patch_location, 23);
}

void A64EmitX64::EmitPatchJz(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr) {
    const CodePtr patch_location = code.getCurr();
    if (target_code_ptr) {
        code.jz(target_code_ptr);
    } else {
        code.mov(rax, A64::LocationDescriptor{target_desc}.PC());
        code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
        code.jz(code.GetReturnFromRunCodeAddress());
    }
    code.EnsurePatchLocationSize(patch_location, 23);
}

void A64EmitX64::EmitPatchJmp(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr) {
    const CodePtr patch_location = code.getCurr();
    if (target_code_ptr) {
        code.jmp(target_code_ptr);
    } else {
        code.mov(rax, A64::LocationDescriptor{target_desc}.PC());
        code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
        code.jmp(code.GetReturnFromRunCodeAddress());
    }
    code.EnsurePatchLocationSize(patch_location, 22);
}

void A64EmitX64::EmitPatchMovRcx(CodePtr target_code_ptr) {
    if (!target_code_ptr) {
        target_code_ptr = code.GetReturnFromRunCodeAddress();
    }
    const CodePtr patch_location = code.getCurr();
    code.mov(code.rcx, reinterpret_cast<u64>(target_code_ptr));
    code.EnsurePatchLocationSize(patch_location, 10);
}

void A64EmitX64::EmitSlotJump(const IR::LocationDescriptor& target) {
    ASSERT(shared_code);
    // Unlinked, the slot holds the code right after the jump: set the PC, enter the dispatcher.
    auto tail = std::make_shared<Xbyak::Label>();
    Xbyak::Label& slot = NewLinkSlot(target, 0, tail);
    code.jmp(qword[rip + slot]);
    code.L(*tail);
    code.mov(rax, A64::LocationDescriptor{target}.PC());
    code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
    code.jmp(code.GetReturnFromRunCodeAddress());
}

void A64EmitX64::Unpatch(const IR::LocationDescriptor& location) {
    EmitX64::Unpatch(location);
    if (conf.HasOptimization(OptimizationFlag::FastDispatch) && !shared_code) {
        code.DisableWriting();
        (*fast_dispatch_table_lookup)(location.Value()) = {};
        code.EnableWriting();
    }
}

}  // namespace Dynarmic::Backend::X64
