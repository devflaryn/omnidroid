/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <atomic>
#include <map>
#include <optional>
#include <vector>

#include <mcl/stdint.hpp>
#include <oaknut/code_block.hpp>
#include <oaknut/oaknut.hpp>
#include <tsl/robin_map.h>
#include <tsl/robin_set.h>

#include "dynarmic/backend/arm64/emit_arm64.h"
#include "dynarmic/backend/arm64/fastmem.h"
#include "dynarmic/backend/arm64/page_backed_allocator.h"
#include "dynarmic/interface/halt_reason.h"
#include "dynarmic/ir/basic_block.h"
#include "dynarmic/ir/location_descriptor.h"

namespace Dynarmic::Backend::Arm64 {

class AddressSpace {
public:
    explicit AddressSpace(size_t code_cache_size);
    virtual ~AddressSpace();

    virtual IR::Block GenerateIR(IR::LocationDescriptor) const = 0;

    CodePtr Get(IR::LocationDescriptor descriptor);

    // Returns "most likely" LocationDescriptor assocated with the emitted code at that location
    std::optional<IR::LocationDescriptor> ReverseGetLocation(CodePtr host_pc);

    // Returns "most likely" entry_point associated with the emitted code at that location
    CodePtr ReverseGetEntryPoint(CodePtr host_pc);

    CodePtr GetOrEmit(IR::LocationDescriptor descriptor);

    void InvalidateBasicBlocks(const tsl::robin_set<IR::LocationDescriptor>& descriptors);

    // Omnidroid patch 0011: virtual, so that `Emit`'s own clear (when the cache is full) also clears
    // what a derived address space keeps per block.
    virtual void ClearCache();

    // Omnidroid patch 0031: how many times the cache has been cleared, so that the dispatcher can
    // tell a clear made while it emitted a block -- inside a run -- from none.
    u64 CacheClears() const { return cache_clears; }

    // Omnidroid patch 0102: the bytes of translated code the cache holds now (above its prelude)
    // and has emitted in all, read from any thread.
    std::size_t OdCodeBytesInUse() const { return od_in_use.load(std::memory_order_relaxed); }
    u64 OdCodeBytesEmitted() const { return od_emitted.load(std::memory_order_relaxed); }

    // Omnidroid patch 0107: the bytes the per-block tables hold (`A64AddressSpace::OdNoteTableBytes`),
    // read from any thread; and whether a parked thread's tables have been given back and its clear
    // is still to come (`A64AddressSpace::OdGiveBackTables`).
    std::size_t OdTableBytes() const { return od_table_bytes.load(std::memory_order_relaxed); }
    bool OdTablesGivenBack() const { return od_tables_given_back.load(std::memory_order_relaxed); }

    void DumpDisassembly() const;

protected:
    virtual EmitConfig GetEmitConfig() = 0;
    virtual void RegisterNewBasicBlock(const IR::Block& block, const EmittedBlockInfo& block_info) = 0;

    void ProtectCodeMemory() {
#if defined(DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT) || defined(__APPLE__) || defined(__OpenBSD__)
        mem.protect();
#endif
    }

    void UnprotectCodeMemory() {
#if defined(DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT) || defined(__APPLE__) || defined(__OpenBSD__)
        mem.unprotect();
#endif
    }

    size_t GetRemainingSize();
    EmittedBlockInfo Emit(IR::Block ir_block);
    void Link(const EmittedBlockInfo& block, u32 block_index);
    void LinkBlockLinks(const CodePtr entry_point, const CodePtr target_ptr, const std::vector<BlockRelocation>& block_relocations_list);
    void LinkBlockLink(const CodePtr entry_point, const CodePtr target_ptr, BlockRelocation block_relocation);
    void RelinkForDescriptor(IR::LocationDescriptor target_descriptor, CodePtr target_ptr);

    FakeCall FastmemCallback(u64 host_pc);

    const size_t code_cache_size;
    u64 cache_clears = 0;

    // Omnidroid patch 0102: a clear gives the pages its blocks used back to the OS (`OdDiscard`);
    // on macOS they are marked reusable and accounted for again, a megabyte ahead of emission, as
    // they are written once more (`OdReuseUpTo`).
    std::atomic<std::size_t> od_in_use{0};
    std::atomic<u64> od_emitted{0};
    std::size_t od_reuse_mark = 0;
    std::size_t od_reusable_end = 0;
    void OdDiscard(std::size_t from, std::size_t to);
    void OdReuseUpTo(std::size_t to);

    // Omnidroid patch 0107: the per-block tables given back (what `ClearCache` does to them), their
    // bytes as the derived space last counted them, and the parked-thread state (set from another
    // thread, under the jit's invalidation lock, while this one is parked).
    void OdDropTables();
    std::size_t OdBaseTableBytes() const;
    // A robin_map's or robin_set's bucket array (tsl keeps each value after a distance and a flag,
    // aligned to the value), and a vector's capacity.
    template<typename Map>
    static std::size_t OdBucketBytes(const Map& map) {
        using V = typename Map::value_type;
        return map.bucket_count() * (sizeof(V) + alignof(V));
    }
    template<typename Vector>
    static std::size_t OdVectorBytes(const Vector& vector) {
        return vector.capacity() * sizeof(typename Vector::value_type);
    }

    // Omnidroid patch 0109: the block map and the link heads at load factor 0.75, not tsl's 0.5:
    // their bucket arrays are half the size whenever the entries are between a half and three
    // quarters of a power of two, for a little longer robin-hood probes. Set on an empty map. tsl
    // computes each size limit as `size_t(float(bucket_count) * load_factor)` on the emitting
    // thread -- inside a run, under the guest's FPCR -- so the factor must give exact products:
    // 0.75 x 2^k is an integer from 4 buckets up, the same in every rounding mode and raising no
    // flag; tsl's first growth from none would be to 2 (1.5), so the map starts at 4.
    template<typename Map>
    static void OdUseLoadFactor(Map& map) {
        map.max_load_factor(0.75f);
        map.rehash(4);
    }
    std::atomic<std::size_t> od_table_bytes{0};
    std::atomic<bool> od_tables_given_back{false};

public:
    // Omnidroid patch 0104: every page of translated code given back but the prelude and the two
    // pages from `keep` (host code a parked thread returns to); its owner is parked and holds no
    // pointer into the rest. The bytes given back.
    std::size_t OdDiscardExcept(std::uint64_t keep);

    // Omnidroid patch 0106: the census (`Jit::OdForEachBlock`, `Jit::OdSetEmitHook`).
    using OdBlockVisitor = void (*)(void* ctx, u64 location, u32 host_bytes);
    void OdForEachBlock(OdBlockVisitor fn, void* ctx) const;
    void OdSetEmitHook(OdBlockVisitor fn, void* ctx) {
        od_emit_hook_ctx = ctx;
        od_emit_hook = fn;
    }

    // Omnidroid patch 0114, a test hook (`Jit::OdBlockLinks`): the block held for `location` (null
    // if none) and its size, and each of its link sites, `fn(ctx, offset, kind)` -- kind 0 a branch
    // word, 1 a return-stack slot; what an unlinked slot holds; and the bytes of code relinks have
    // handed to instruction-cache maintenance. On the owning thread, outside a run.
    using OdLinkVisitor = void (*)(void* ctx, std::uint32_t offset, std::uint32_t kind);
    CodePtr OdBlockLinks(IR::LocationDescriptor location, u32& host_bytes, OdLinkVisitor fn, void* ctx) const;
    void* OdUnlinkedSlot() const { return prelude_info.return_to_dispatcher; }
    u64 OdRelinkMaintained() const { return od_relink_maintained; }

protected:
    OdBlockVisitor od_emit_hook = nullptr;
    void* od_emit_hook_ctx = nullptr;
    u64 od_relink_maintained = 0;  // patch 0114

    oaknut::CodeBlock mem;
    oaknut::CodeGenerator code;

    // A IR::LocationDescriptor will have one current CodePtr.
    // However, there can be multiple other CodePtrs which are older, previously invalidated blocks.
    // Omnidroid patch 0013: this and the records below allocate through `PageBackedAllocator`.
    template<typename K, typename V>
    using PageBackedMap = tsl::robin_map<K, V, std::hash<K>, std::equal_to<K>, PageBackedAllocator<std::pair<K, V>>>;
    template<typename T>
    using PageBackedVector = std::vector<T, PageBackedAllocator<T>>;

    PageBackedMap<IR::LocationDescriptor, CodePtr> block_entries;

    // Omnidroid patch 0010: what is kept of each emitted block, compactly.
    //
    // The pin kept every block's whole `EmittedBlockInfo` -- a vector and two robin_maps, 200 bytes
    // -- inline in the buckets of a robin_map keyed by entry point, next to a std::map for the
    // reverse lookup and a robin_map of robin_sets for the references between blocks: about
    // 2.1 KB per block, measured, in every jit (one per guest thread). What is read after a block
    // is emitted is only this: its entry point, location and size (reverse lookup, relinking),
    // its fastmem patch sites (`FastmemCallback`) and, per link target, where it links to that
    // target (`RelinkForDescriptor`). `relocations` is consumed by `Link` at emission and never
    // read again.
    //
    // Blocks are emitted at an offset that only grows until `ClearCache`, so the records are
    // appended in ascending `entry_point` order and a binary search replaces the maps keyed by
    // it. Nothing is removed before `ClearCache`: an invalidated block keeps its records, exactly
    // as the pin kept its `block_infos` entry and its `block_references` entries.
    struct BlockRecord {
        CodePtr entry_point;
        IR::LocationDescriptor location;
        u32 size;
        u32 fastmem_begin;  ///< First of this block's `fastmem_records`; they end where the next block's begin.
    };
    struct FastmemRecord {
        IR::LocationDescriptor marker_location;  ///< `std::get<0>(FastmemPatchInfo::marker)`
        u32 offset;                              ///< The patched access, from the block's entry point.
        u32 fc_offset;                           ///< `FakeCall::call_pc`, from the block's entry point.
        u32 marker_index;                        ///< `std::get<1>(FastmemPatchInfo::marker)`
        bool recompile;
    };
    struct LinkRecord {
        IR::LocationDescriptor target;
        u32 block;   ///< Index into `block_records`.
        u32 offset;  ///< `BlockRelocation::code_offset`
        u32 next;    ///< The previous record linking to the same target, or `no_link`.
        BlockRelocationType type;
    };
    static constexpr u32 no_link = ~u32{0};

    PageBackedVector<BlockRecord> block_records;
    PageBackedVector<FastmemRecord> fastmem_records;  ///< Grouped by block, ascending `offset` within a block.
    PageBackedVector<LinkRecord> link_records;
    /// The newest `LinkRecord` for each link target. A block's records for one target are adjacent
    /// in the chain.
    PageBackedMap<IR::LocationDescriptor, u32> link_heads;

    u32 RecordBlock(IR::LocationDescriptor location, const EmittedBlockInfo& block_info);
    const BlockRecord* FindBlockRecord(CodePtr host_pc) const;

    ExceptionHandler exception_handler;
    FastmemManager fastmem_manager;

    struct PreludeInfo {
        std::ptrdiff_t end_of_prelude;

        using RunCodeFuncType = HaltReason (*)(CodePtr entry_point, void* jit_state, volatile u32* halt_reason);
        RunCodeFuncType run_code;
        RunCodeFuncType step_code;
        void* return_to_dispatcher;
        void* return_from_run_code;

        void* read_memory_8;
        void* read_memory_16;
        void* read_memory_32;
        void* read_memory_64;
        void* read_memory_128;
        void* wrapped_read_memory_8;
        void* wrapped_read_memory_16;
        void* wrapped_read_memory_32;
        void* wrapped_read_memory_64;
        void* wrapped_read_memory_128;
        void* exclusive_read_memory_8;
        void* exclusive_read_memory_16;
        void* exclusive_read_memory_32;
        void* exclusive_read_memory_64;
        void* exclusive_read_memory_128;
        void* write_memory_8;
        void* write_memory_16;
        void* write_memory_32;
        void* write_memory_64;
        void* write_memory_128;
        void* wrapped_write_memory_8;
        void* wrapped_write_memory_16;
        void* wrapped_write_memory_32;
        void* wrapped_write_memory_64;
        void* wrapped_write_memory_128;
        void* exclusive_write_memory_8;
        void* exclusive_write_memory_16;
        void* exclusive_write_memory_32;
        void* exclusive_write_memory_64;
        void* exclusive_write_memory_128;

        void* call_svc;
        void* exception_raised;
        void* dc_raised;
        void* ic_raised;
        void* isb_raised;

        void* get_cntpct;
        void* add_ticks;
        void* get_ticks_remaining;

        // Omnidroid patch 0002.
        void* interpreter_fallback;

        // Omnidroid patch 0007.
        void* wrapped_exclusive_read_memory_8;
        void* wrapped_exclusive_read_memory_16;
        void* wrapped_exclusive_read_memory_32;
        void* wrapped_exclusive_read_memory_64;
        void* wrapped_exclusive_read_memory_128;
        void* wrapped_exclusive_write_memory_8;
        void* wrapped_exclusive_write_memory_16;
        void* wrapped_exclusive_write_memory_32;
        void* wrapped_exclusive_write_memory_64;
        void* wrapped_exclusive_write_memory_128;

        // Omnidroid patch 0020: the `PopRSBHint` terminal's handler, once, in the prelude.
        void* pop_rsb_hint;
    } prelude_info;
};

}  // namespace Dynarmic::Backend::Arm64
