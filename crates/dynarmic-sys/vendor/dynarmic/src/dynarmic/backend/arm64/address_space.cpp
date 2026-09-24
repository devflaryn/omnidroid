/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include <algorithm>
#include <cstdio>
#include <limits>

#include <mcl/bit_cast.hpp>

#include "dynarmic/backend/arm64/a64_address_space.h"
#include "dynarmic/backend/arm64/a64_jitstate.h"
#include "dynarmic/backend/arm64/abi.h"
#include "dynarmic/backend/arm64/devirtualize.h"
#include "dynarmic/backend/arm64/emit_arm64.h"
#include "dynarmic/backend/arm64/stack_layout.h"
#include "dynarmic/common/cast_util.h"
#include "dynarmic/common/fp/fpcr.h"
#include "dynarmic/common/llvm_disassemble.h"
#include "dynarmic/interface/exclusive_monitor.h"

namespace Dynarmic::Backend::Arm64 {

AddressSpace::AddressSpace(size_t code_cache_size)
        : code_cache_size(code_cache_size)
        , mem(code_cache_size)
        , code(mem.ptr(), mem.ptr())
        , fastmem_manager(exception_handler) {
    ASSERT_MSG(code_cache_size <= 128 * 1024 * 1024, "code_cache_size > 128 MiB not currently supported");

    exception_handler.Register(mem, code_cache_size);
    exception_handler.SetFastmemCallback([this](u64 host_pc) {
        return FastmemCallback(host_pc);
    });
}

AddressSpace::~AddressSpace() = default;

CodePtr AddressSpace::Get(IR::LocationDescriptor descriptor) {
    if (const auto iter = block_entries.find(descriptor); iter != block_entries.end()) {
        return iter->second;
    }
    return nullptr;
}

// Omnidroid patch 0010: the last block whose entry point is at or below `host_pc` -- what
// `reverse_block_entries.upper_bound(host_pc)` followed by a decrement found.
const AddressSpace::BlockRecord* AddressSpace::FindBlockRecord(CodePtr host_pc) const {
    const auto iter = std::upper_bound(block_records.begin(), block_records.end(), host_pc,
                                       [](CodePtr pc, const BlockRecord& record) { return pc < record.entry_point; });
    if (iter == block_records.begin()) {
        return nullptr;
    }
    return &*std::prev(iter);
}

std::optional<IR::LocationDescriptor> AddressSpace::ReverseGetLocation(CodePtr host_pc) {
    if (const BlockRecord* record = FindBlockRecord(host_pc)) {
        return record->location;
    }
    return std::nullopt;
}

CodePtr AddressSpace::ReverseGetEntryPoint(CodePtr host_pc) {
    if (const BlockRecord* record = FindBlockRecord(host_pc)) {
        return record->entry_point;
    }
    return nullptr;
}

CodePtr AddressSpace::GetOrEmit(IR::LocationDescriptor descriptor) {
    if (CodePtr block_entry = Get(descriptor)) {
        return block_entry;
    }

    IR::Block ir_block = GenerateIR(descriptor);
    const EmittedBlockInfo block_info = Emit(std::move(ir_block));
    return block_info.entry_point;
}

void AddressSpace::InvalidateBasicBlocks(const tsl::robin_set<IR::LocationDescriptor>& descriptors) {
    UnprotectCodeMemory();

    for (const auto& descriptor : descriptors) {
        const auto iter = block_entries.find(descriptor);
        if (iter == block_entries.end()) {
            continue;
        }

        // Unlink before removal because InvalidateBasicBlocks can be called within a fastmem callback,
        // and the currently executing block may have references to itself which need to be unlinked.
        RelinkForDescriptor(descriptor, nullptr);

        block_entries.erase(iter);
    }

    ProtectCodeMemory();
}

void AddressSpace::ClearCache() {
    // Omnidroid patch 0010: given back rather than cleared (`clear()` keeps a robin_map's buckets
    // and a vector's capacity), so a cleared cache holds no memory for the blocks it no longer has.
    block_entries = {};
    decltype(block_records){}.swap(block_records);
    decltype(fastmem_records){}.swap(fastmem_records);
    decltype(link_records){}.swap(link_records);
    link_heads = {};
    code.set_offset(prelude_info.end_of_prelude);
}

void AddressSpace::DumpDisassembly() const {
    for (u32* ptr = mem.ptr(); ptr < code.xptr<u32*>(); ptr++) {
        std::printf("%s", Common::DisassembleAArch64(*ptr, mcl::bit_cast<u64>(ptr)).c_str());
    }
}

size_t AddressSpace::GetRemainingSize() {
    return code_cache_size - static_cast<size_t>(code.offset());
}

EmittedBlockInfo AddressSpace::Emit(IR::Block block) {
    if (GetRemainingSize() < 1024 * 1024) {
        ClearCache();
    }

    UnprotectCodeMemory();

    EmittedBlockInfo block_info = EmitArm64(code, std::move(block), GetEmitConfig(), fastmem_manager);

    ASSERT(block_entries.insert({block.Location(), block_info.entry_point}).second);
    const u32 block_index = RecordBlock(block.Location(), block_info);

    Link(block_info, block_index);
    RelinkForDescriptor(block.Location(), block_info.entry_point);

    mem.invalidate(reinterpret_cast<u32*>(block_info.entry_point), block_info.size);
    ProtectCodeMemory();

    RegisterNewBasicBlock(block, block_info);

    return block_info;
}

// Omnidroid patch 0010: keep what is read after emission, and only that. See `BlockRecord`.
u32 AddressSpace::RecordBlock(IR::LocationDescriptor location, const EmittedBlockInfo& block_info) {
    // The pin asserted that the entry point was new to `reverse_block_entries` and `block_infos`.
    // Emission only moves forward until `ClearCache`, so it is also above every recorded one,
    // which is what the binary searches rely on.
    ASSERT(block_records.empty() || block_records.back().entry_point < block_info.entry_point);
    ASSERT(block_records.size() < no_link && block_info.size <= std::numeric_limits<u32>::max());
    ASSERT(fastmem_records.size() + block_info.fastmem_patch_info.size() <= std::numeric_limits<u32>::max());

    const u32 block_index = static_cast<u32>(block_records.size());
    block_records.push_back(BlockRecord{
        .entry_point = block_info.entry_point,
        .location = location,
        .size = static_cast<u32>(block_info.size),
        .fastmem_begin = static_cast<u32>(fastmem_records.size()),
    });

    const size_t first = fastmem_records.size();
    for (const auto& [offset, info] : block_info.fastmem_patch_info) {
        const std::ptrdiff_t fc_offset = mcl::bit_cast<CodePtr>(info.fc.call_pc) - block_info.entry_point;
        ASSERT(offset >= 0 && offset <= std::numeric_limits<u32>::max());
        ASSERT(fc_offset >= 0 && fc_offset <= std::numeric_limits<u32>::max());
        fastmem_records.push_back(FastmemRecord{
            .marker_location = std::get<0>(info.marker),
            .offset = static_cast<u32>(offset),
            .fc_offset = static_cast<u32>(fc_offset),
            .marker_index = std::get<1>(info.marker),
            .recompile = info.recompile,
        });
    }
    // `fastmem_patch_info` is keyed by offset, so the offsets are distinct.
    std::sort(fastmem_records.begin() + first, fastmem_records.end(),
              [](const FastmemRecord& a, const FastmemRecord& b) { return a.offset < b.offset; });

    return block_index;
}

void AddressSpace::Link(const EmittedBlockInfo& block_info, u32 block_index) {
    using namespace oaknut;
    using namespace oaknut::util;

    for (auto [ptr_offset, target] : block_info.relocations) {
        CodeGenerator c{mem.ptr(), mem.ptr()};
        c.set_xptr(reinterpret_cast<u32*>(block_info.entry_point + ptr_offset));

        switch (target) {
        case LinkTarget::ReturnToDispatcher:
            c.B(prelude_info.return_to_dispatcher);
            break;
        case LinkTarget::ReturnFromRunCode:
            c.B(prelude_info.return_from_run_code);
            break;
        case LinkTarget::ReadMemory8:
            c.BL(prelude_info.read_memory_8);
            break;
        case LinkTarget::ReadMemory16:
            c.BL(prelude_info.read_memory_16);
            break;
        case LinkTarget::ReadMemory32:
            c.BL(prelude_info.read_memory_32);
            break;
        case LinkTarget::ReadMemory64:
            c.BL(prelude_info.read_memory_64);
            break;
        case LinkTarget::ReadMemory128:
            c.BL(prelude_info.read_memory_128);
            break;
        case LinkTarget::WrappedReadMemory8:
            c.BL(prelude_info.wrapped_read_memory_8);
            break;
        case LinkTarget::WrappedReadMemory16:
            c.BL(prelude_info.wrapped_read_memory_16);
            break;
        case LinkTarget::WrappedReadMemory32:
            c.BL(prelude_info.wrapped_read_memory_32);
            break;
        case LinkTarget::WrappedReadMemory64:
            c.BL(prelude_info.wrapped_read_memory_64);
            break;
        case LinkTarget::WrappedReadMemory128:
            c.BL(prelude_info.wrapped_read_memory_128);
            break;
        case LinkTarget::ExclusiveReadMemory8:
            c.BL(prelude_info.exclusive_read_memory_8);
            break;
        case LinkTarget::ExclusiveReadMemory16:
            c.BL(prelude_info.exclusive_read_memory_16);
            break;
        case LinkTarget::ExclusiveReadMemory32:
            c.BL(prelude_info.exclusive_read_memory_32);
            break;
        case LinkTarget::ExclusiveReadMemory64:
            c.BL(prelude_info.exclusive_read_memory_64);
            break;
        case LinkTarget::ExclusiveReadMemory128:
            c.BL(prelude_info.exclusive_read_memory_128);
            break;
        case LinkTarget::WriteMemory8:
            c.BL(prelude_info.write_memory_8);
            break;
        case LinkTarget::WriteMemory16:
            c.BL(prelude_info.write_memory_16);
            break;
        case LinkTarget::WriteMemory32:
            c.BL(prelude_info.write_memory_32);
            break;
        case LinkTarget::WriteMemory64:
            c.BL(prelude_info.write_memory_64);
            break;
        case LinkTarget::WriteMemory128:
            c.BL(prelude_info.write_memory_128);
            break;
        case LinkTarget::WrappedWriteMemory8:
            c.BL(prelude_info.wrapped_write_memory_8);
            break;
        case LinkTarget::WrappedWriteMemory16:
            c.BL(prelude_info.wrapped_write_memory_16);
            break;
        case LinkTarget::WrappedWriteMemory32:
            c.BL(prelude_info.wrapped_write_memory_32);
            break;
        case LinkTarget::WrappedWriteMemory64:
            c.BL(prelude_info.wrapped_write_memory_64);
            break;
        case LinkTarget::WrappedWriteMemory128:
            c.BL(prelude_info.wrapped_write_memory_128);
            break;
        case LinkTarget::ExclusiveWriteMemory8:
            c.BL(prelude_info.exclusive_write_memory_8);
            break;
        case LinkTarget::ExclusiveWriteMemory16:
            c.BL(prelude_info.exclusive_write_memory_16);
            break;
        case LinkTarget::ExclusiveWriteMemory32:
            c.BL(prelude_info.exclusive_write_memory_32);
            break;
        case LinkTarget::ExclusiveWriteMemory64:
            c.BL(prelude_info.exclusive_write_memory_64);
            break;
        case LinkTarget::ExclusiveWriteMemory128:
            c.BL(prelude_info.exclusive_write_memory_128);
            break;
        case LinkTarget::CallSVC:
            c.BL(prelude_info.call_svc);
            break;
        case LinkTarget::ExceptionRaised:
            c.BL(prelude_info.exception_raised);
            break;
        case LinkTarget::InstructionSynchronizationBarrierRaised:
            c.BL(prelude_info.isb_raised);
            break;
        case LinkTarget::InstructionCacheOperationRaised:
            c.BL(prelude_info.ic_raised);
            break;
        case LinkTarget::DataCacheOperationRaised:
            c.BL(prelude_info.dc_raised);
            break;
        case LinkTarget::GetCNTPCT:
            c.BL(prelude_info.get_cntpct);
            break;
        case LinkTarget::AddTicks:
            c.BL(prelude_info.add_ticks);
            break;
        case LinkTarget::GetTicksRemaining:
            c.BL(prelude_info.get_ticks_remaining);
            break;
        case LinkTarget::InterpreterFallback:
            c.BL(prelude_info.interpreter_fallback);
            break;
        case LinkTarget::WrappedExclusiveReadMemory8:
            c.BL(prelude_info.wrapped_exclusive_read_memory_8);
            break;
        case LinkTarget::WrappedExclusiveReadMemory16:
            c.BL(prelude_info.wrapped_exclusive_read_memory_16);
            break;
        case LinkTarget::WrappedExclusiveReadMemory32:
            c.BL(prelude_info.wrapped_exclusive_read_memory_32);
            break;
        case LinkTarget::WrappedExclusiveReadMemory64:
            c.BL(prelude_info.wrapped_exclusive_read_memory_64);
            break;
        case LinkTarget::WrappedExclusiveReadMemory128:
            c.BL(prelude_info.wrapped_exclusive_read_memory_128);
            break;
        case LinkTarget::WrappedExclusiveWriteMemory8:
            c.BL(prelude_info.wrapped_exclusive_write_memory_8);
            break;
        case LinkTarget::WrappedExclusiveWriteMemory16:
            c.BL(prelude_info.wrapped_exclusive_write_memory_16);
            break;
        case LinkTarget::WrappedExclusiveWriteMemory32:
            c.BL(prelude_info.wrapped_exclusive_write_memory_32);
            break;
        case LinkTarget::WrappedExclusiveWriteMemory64:
            c.BL(prelude_info.wrapped_exclusive_write_memory_64);
            break;
        case LinkTarget::WrappedExclusiveWriteMemory128:
            c.BL(prelude_info.wrapped_exclusive_write_memory_128);
            break;
        default:
            ASSERT_FALSE("Invalid relocation target");
        }
    }

    for (const auto& [target_descriptor, list] : block_info.block_relocations) {
        // Omnidroid patch 0010: `block_references[target_descriptor].insert(entry_point)`, as a
        // chain through `link_records` from `link_heads`. This block's records for this target are
        // pushed together, so they stay adjacent in the chain.
        auto head = link_heads.find(target_descriptor);
        u32 next = head == link_heads.end() ? no_link : head->second;
        for (const BlockRelocation& relocation : list) {
            ASSERT(link_records.size() < no_link);
            ASSERT(relocation.code_offset >= 0 && relocation.code_offset <= std::numeric_limits<u32>::max());
            const u32 index = static_cast<u32>(link_records.size());
            link_records.push_back(LinkRecord{
                .target = target_descriptor,
                .block = block_index,
                .offset = static_cast<u32>(relocation.code_offset),
                .next = next,
                .type = relocation.type,
            });
            next = index;
        }
        if (!list.empty()) {
            link_heads.insert_or_assign(target_descriptor, next);
        }
        LinkBlockLinks(block_info.entry_point, Get(target_descriptor), list);
    }
}

void AddressSpace::LinkBlockLinks(const CodePtr entry_point, const CodePtr target_ptr, const std::vector<BlockRelocation>& block_relocations_list) {
    for (const BlockRelocation& block_relocation : block_relocations_list) {
        LinkBlockLink(entry_point, target_ptr, block_relocation);
    }
}

void AddressSpace::LinkBlockLink(const CodePtr entry_point, const CodePtr target_ptr, BlockRelocation block_relocation) {
    using namespace oaknut;
    using namespace oaknut::util;

    const auto [ptr_offset, type] = block_relocation;
    CodeGenerator c{mem.ptr(), mem.ptr()};
    c.set_xptr(reinterpret_cast<u32*>(entry_point + ptr_offset));

    switch (type) {
    case BlockRelocationType::Branch:
        if (target_ptr) {
            c.B((void*)target_ptr);
        } else {
            c.NOP();
        }
        break;
    case BlockRelocationType::MoveToScratch1:
        if (target_ptr) {
            c.ADRL(Xscratch1, (void*)target_ptr);
        } else {
            c.ADRL(Xscratch1, prelude_info.return_to_dispatcher);
        }
        break;
    default:
        ASSERT_FALSE("Invalid BlockRelocationType");
    }
}

void AddressSpace::RelinkForDescriptor(IR::LocationDescriptor target_descriptor, CodePtr target_ptr) {
    // Omnidroid patch 0010: every block that links to `target_descriptor` -- the pin's
    // `block_references[target_descriptor]` -- relinks its links to it and is invalidated once.
    const auto head = link_heads.find(target_descriptor);
    if (head == link_heads.end()) {
        return;
    }
    u32 index = head->second;
    while (index != no_link) {
        const u32 block_index = link_records[index].block;
        const BlockRecord& block = block_records[block_index];
        do {
            const LinkRecord& link = link_records[index];
            ASSERT(link.target == target_descriptor);
            LinkBlockLink(block.entry_point, target_ptr, BlockRelocation{link.offset, link.type});
            index = link.next;
        } while (index != no_link && link_records[index].block == block_index);

        mem.invalidate(reinterpret_cast<u32*>(block.entry_point), block.size);
    }
}

FakeCall AddressSpace::FastmemCallback(u64 host_pc) {
    {
        const auto host_ptr = mcl::bit_cast<CodePtr>(host_pc);

        // Omnidroid patch 0010: the block record at or below `host_pc`, then its patch sites by
        // binary search on the offset -- the pin's reverse map, `block_infos` and the block's
        // `fastmem_patch_info`.
        const BlockRecord* block = FindBlockRecord(host_ptr);
        if (!block) {
            goto fail;
        }

        const std::ptrdiff_t offset = host_ptr - block->entry_point;
        if (offset > std::numeric_limits<u32>::max()) {
            goto fail;
        }

        const auto first = fastmem_records.begin() + block->fastmem_begin;
        const auto last = block == &block_records.back()
                            ? fastmem_records.end()
                            : fastmem_records.begin() + (block + 1)->fastmem_begin;
        const auto patch_entry = std::lower_bound(first, last, static_cast<u32>(offset),
                                                  [](const FastmemRecord& record, u32 value) { return record.offset < value; });
        if (patch_entry == last || patch_entry->offset != static_cast<u32>(offset)) {
            goto fail;
        }

        const auto fc = FakeCall{.call_pc = mcl::bit_cast<u64>(block->entry_point + patch_entry->fc_offset)};

        if (patch_entry->recompile) {
            const DoNotFastmemMarker marker{patch_entry->marker_location, patch_entry->marker_index};
            fastmem_manager.MarkDoNotFastmem(marker);
            InvalidateBasicBlocks({std::get<0>(marker)});
        }

        return fc;
    }

fail:
    fmt::print("dynarmic: Segfault happened within JITted code at host_pc = {:016x}\n", host_pc);
    fmt::print("Segfault wasn't at a fastmem patch location!\n");
    ASSERT_FALSE("segfault");
}

}  // namespace Dynarmic::Backend::Arm64
