/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

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

    void ClearCache();

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
    oaknut::CodeBlock mem;
    oaknut::CodeGenerator code;

    // A IR::LocationDescriptor will have one current CodePtr.
    // However, there can be multiple other CodePtrs which are older, previously invalidated blocks.
    tsl::robin_map<IR::LocationDescriptor, CodePtr> block_entries;

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

    std::vector<BlockRecord> block_records;
    std::vector<FastmemRecord> fastmem_records;  ///< Grouped by block, ascending `offset` within a block.
    std::vector<LinkRecord> link_records;
    /// The newest `LinkRecord` for each link target. A block's records for one target are adjacent
    /// in the chain.
    tsl::robin_map<IR::LocationDescriptor, u32> link_heads;

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
    } prelude_info;
};

}  // namespace Dynarmic::Backend::Arm64
