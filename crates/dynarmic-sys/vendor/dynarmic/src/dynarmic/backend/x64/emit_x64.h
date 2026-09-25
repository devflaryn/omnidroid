/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <array>
#include <functional>
#include <memory>
#include <optional>
#include <shared_mutex>
#include <string>
#include <type_traits>
#include <vector>

#include <mcl/assert.hpp>
#include <mcl/bitsizeof.hpp>
#include <tsl/robin_map.h>
#include <tsl/robin_set.h>
#include <xbyak/xbyak.h>
#include <xbyak/xbyak_util.h>

#include "dynarmic/backend/exception_handler.h"
#include "dynarmic/backend/x64/reg_alloc.h"
#include "dynarmic/backend/x64/shared_code_lock.h"
#include "dynarmic/common/fp/fpcr.h"
#include "dynarmic/ir/location_descriptor.h"
#include "dynarmic/ir/terminal.h"

namespace Dynarmic::IR {
class Block;
class Inst;
}  // namespace Dynarmic::IR

namespace Dynarmic {
enum class OptimizationFlag : u32;
}  // namespace Dynarmic

namespace Dynarmic::Backend::X64 {

class BlockOfCode;

using A64FullVectorWidth = std::integral_constant<size_t, 128>;

// Array alias that always sizes itself according to the given type T
// relative to the size of a vector register. e.g. T = u32 would result
// in a std::array<u32, 4>.
template<typename T>
using VectorArray = std::array<T, A64FullVectorWidth::value / mcl::bitsizeof<T>>;

template<typename T>
using HalfVectorArray = std::array<T, A64FullVectorWidth::value / mcl::bitsizeof<T> / 2>;

struct EmitContext {
    EmitContext(RegAlloc& reg_alloc, IR::Block& block);
    virtual ~EmitContext();

    void EraseInstruction(IR::Inst* inst);

    virtual FP::FPCR FPCR(bool fpcr_controlled = true) const = 0;

    virtual bool HasOptimization(OptimizationFlag flag) const = 0;

    RegAlloc& reg_alloc;
    IR::Block& block;

    std::vector<std::function<void()>> deferred_emits;
};

using SharedLabel = std::shared_ptr<Xbyak::Label>;

inline SharedLabel GenSharedLabel() {
    return std::make_shared<Xbyak::Label>();
}

class EmitX64 {
public:
    struct BlockDescriptor {
        CodePtr entrypoint;  // Entrypoint of emitted code
        // Omnidroid patch 0025: 32 bits (a block is far smaller), and the index of the block's
        // first link record in a shared code cache -- still 16 bytes, so the block map's buckets
        // stay 32.
        u32 size;                    // Length in bytes of emitted code
        u32 first_link = 0xFFFF'FFFF;  // shared code cache: its first LinkRecord, or NO_LINK
    };
    static_assert(sizeof(BlockDescriptor) == 16);

    explicit EmitX64(BlockOfCode& code);
    virtual ~EmitX64();

    /// Looks up an emitted host block in the cache.
    std::optional<BlockDescriptor> GetBasicBlock(IR::LocationDescriptor descriptor) const;

    /// Empties the entire cache.
    virtual void ClearCache();

    /// Invalidates a selection of basic blocks.
    void InvalidateBasicBlocks(const tsl::robin_set<IR::LocationDescriptor>& locations);

protected:
    // Microinstruction emitters
#define OPCODE(name, type, ...) void Emit##name(EmitContext& ctx, IR::Inst* inst);
#define A32OPC(...)
#define A64OPC(...)
#include "dynarmic/ir/opcodes.inc"
#undef OPCODE
#undef A32OPC
#undef A64OPC

    // Helpers
    virtual std::string LocationDescriptorToFriendlyName(const IR::LocationDescriptor&) const = 0;
    void EmitAddCycles(size_t cycles);
    Xbyak::Label EmitCond(IR::Cond cond);
    BlockDescriptor RegisterBlock(const IR::LocationDescriptor& location_descriptor, CodePtr entrypoint, size_t size);
    void PushRSBHelper(Xbyak::Reg64 loc_desc_reg, Xbyak::Reg64 index_reg, IR::LocationDescriptor target);

    void EmitVerboseDebuggingOutput(RegAlloc& reg_alloc);

    // Terminal instruction emitters
    void EmitTerminal(IR::Terminal terminal, IR::LocationDescriptor initial_location, bool is_single_step);
    virtual void EmitTerminalImpl(IR::Term::Interpret terminal, IR::LocationDescriptor initial_location, bool is_single_step) = 0;
    virtual void EmitTerminalImpl(IR::Term::ReturnToDispatch terminal, IR::LocationDescriptor initial_location, bool is_single_step) = 0;
    virtual void EmitTerminalImpl(IR::Term::LinkBlock terminal, IR::LocationDescriptor initial_location, bool is_single_step) = 0;
    virtual void EmitTerminalImpl(IR::Term::LinkBlockFast terminal, IR::LocationDescriptor initial_location, bool is_single_step) = 0;
    virtual void EmitTerminalImpl(IR::Term::PopRSBHint terminal, IR::LocationDescriptor initial_location, bool is_single_step) = 0;
    virtual void EmitTerminalImpl(IR::Term::FastDispatchHint terminal, IR::LocationDescriptor initial_location, bool is_single_step) = 0;
    virtual void EmitTerminalImpl(IR::Term::If terminal, IR::LocationDescriptor initial_location, bool is_single_step) = 0;
    virtual void EmitTerminalImpl(IR::Term::CheckBit terminal, IR::LocationDescriptor initial_location, bool is_single_step) = 0;
    virtual void EmitTerminalImpl(IR::Term::CheckHalt terminal, IR::LocationDescriptor initial_location, bool is_single_step) = 0;

    // Patching
    /// Omnidroid patch 0022: in a shared code cache a link site never has its code rewritten; it
    /// jumps (or loads its RSB code pointer) through an 8-byte slot, and linking or unlinking is
    /// one aligned store to the slot. What the slot holds while the target has no translation is
    /// its unlinked value. Patch 0025 keeps a shared cache's slots as LinkRecords (below);
    /// `patch_information` is a Jit's own cache's.
    struct PatchInformation {
        std::vector<CodePtr> jg;
        std::vector<CodePtr> jz;
        std::vector<CodePtr> jmp;
        std::vector<CodePtr> mov_rcx;
    };
    void Patch(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr);
    virtual void Unpatch(const IR::LocationDescriptor& target_desc);
    virtual void EmitPatchJg(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr = nullptr) = 0;
    virtual void EmitPatchJz(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr = nullptr) = 0;
    virtual void EmitPatchJmp(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr = nullptr) = 0;
    virtual void EmitPatchMovRcx(CodePtr target_code_ptr = nullptr) = 0;

    // State
    BlockOfCode& code;
    ExceptionHandler exception_handler;
    tsl::robin_map<IR::LocationDescriptor, BlockDescriptor> block_descriptors;
    tsl::robin_map<IR::LocationDescriptor, PatchInformation> patch_information;

public:
    // Omnidroid patch 0022: this emitter writes into a code cache several Jits share
    // (A64::SharedCodeCache). Set once, before the prelude is generated, and never changed.
    bool shared_code = false;
    /// Shared code cache only: the cache's lock, which a fault handler reading the emitter's
    /// tables takes shared.
    SharedCodeLock* shared_lock = nullptr;
    /// Shared code cache only: store every link slot's unlinked value, so nothing reaches the
    /// blocks the maps are about to forget through a link.
    void UnlinkAllSlots();

protected:
    /// Shared code cache only: a link slot the block being emitted references `rip`-relatively
    /// through `label`, emitted -- 8 aligned bytes -- after the block's code by EmitPendingSlots,
    /// next to the code that reads it. Its unlinked value is `unlinked`, or the address `tail` is
    /// bound to when it is set.
    struct PendingSlot {
        std::shared_ptr<Xbyak::Label> label;
        IR::LocationDescriptor target;
        u64 unlinked;
        std::shared_ptr<Xbyak::Label> tail;
    };
    std::vector<PendingSlot> pending_slots;
    /// A new slot for a link to `target`, to be emitted with the block.
    Xbyak::Label& NewLinkSlot(const IR::LocationDescriptor& target, u64 unlinked, std::shared_ptr<Xbyak::Label> tail = nullptr);
    /// Emit the pending slots of the block being emitted, each holding its target's entry point if
    /// it has one and its unlinked value otherwise, and record them so that emitting or
    /// invalidating a target updates them. RegisterBlock gives the block its first record.
    void EmitPendingSlots(const IR::LocationDescriptor& location);

    // Omnidroid patch 0025, shared code cache only: the link slots, as one record each. A shared
    // cache kept them in `patch_information` -- five std::vectors inline in 136-byte buckets, one
    // entry for every location ever linked to or emitted, at a load factor of at most 0.5 (2^21
    // buckets, 272 MiB, in the world) -- and in `outgoing_slots`, a vector per block in 40-byte
    // buckets (80 MiB). A block's records are contiguous (BlockDescriptor::first_link, the last
    // one marked); the records linking to one target are a list threaded through them, from
    // `link_heads`.
    static constexpr u32 NO_LINK = 0xFFFF'FFFF;
    /// Bit 31 of `LinkRecord::slot`: the last record of its block. Offsets fit below it: a shared
    /// cache is at most 2 GiB.
    static constexpr u32 LAST_LINK_OF_BLOCK = 0x8000'0000;
    struct LinkRecord {
        u64 target;    ///< the location the slot links to
        u32 slot;      ///< the slot, as an offset from the code buffer's start; LAST_LINK_OF_BLOCK
        u32 unlinked;  ///< what the slot holds while `target` has no translation, as an offset
        u32 next;      ///< the next record linking to `target`, NO_LINK at the end
        u32 prev;      ///< the previous one, NO_LINK at the head
    };
    static_assert(sizeof(LinkRecord) == 24);
    std::vector<LinkRecord> link_records;
    /// Target location -> the newest record linking to it. A target with no live record has none.
    tsl::robin_map<u64, u32> link_heads;
    /// Read only when a block is emitted or dropped, never on a lookup, so fuller than the maps'
    /// 0.5: robin-hood probing stays short at 0.75, and the array is half the size.
    static constexpr float LINK_HEADS_LOAD_FACTOR = 0.75f;
    /// A robin_map at `load_factor` rather than tsl's 0.5, set on an empty map. tsl computes each
    /// size limit as `size_t(float(bucket_count) * load_factor)` -- on the host thread, while it
    /// emits, under the host's MXCSR -- and an inexact product (0.8, or 0.75 at tsl's first size of
    /// two buckets) sets MXCSR's sticky precision flag, which a handler would then see in the host
    /// word the dispatcher installs for it (`omni-cpu/tests/thunk.rs` compares it exactly). So the
    /// factor is 0.75 and the map starts at 64 buckets: every product is then an exact integer.
    template<typename Map>
    static void UseLoadFactor(Map& map, float load_factor) {
        ASSERT(map.empty());
        map.max_load_factor(load_factor);
        map.rehash(64);
    }
    /// The first record EmitPendingSlots made for the block being emitted, for RegisterBlock.
    u32 pending_first_link = NO_LINK;
    u64* LinkSlotOf(const LinkRecord& record) const;
    u64 LinkUnlinkedOf(const LinkRecord& record) const;
    /// Shared code cache only: unlink and forget the slots of a dropped block, whose first record
    /// is `first_link`, taking each out of its target's list.
    void ForgetOutgoingSlots(u32 first_link);

public:
    // Omnidroid patch 0025, shared code cache only (the caller holds the cache's lock exclusively):
    // the fastmem patch sites, which a thread faulting at one looks up (FastmemCallback), kept as
    // sorted records per region instead of `fastmem_patch_info`, a robin_map of 56-byte buckets at
    // a load factor of at most 0.5 (2^22 buckets, 224 MiB, in the world).
    /// Blocks are emitted into `[begin, end)` from now on (a region starting): its records, if it
    /// had any, are dropped.
    void BeginFastmemSites(const void* begin, const void* end);
    /// The records of the sites in `[begin, end)` are dropped and their memory given back: that
    /// span is being decommitted and nothing can execute it any more.
    void PurgeFastmemSites(const void* begin, const void* end);

protected:
    struct FastmemSite {
        u32 site;      ///< the faulting instruction, as an offset from its run's `begin`
        u32 resume;    ///< where the fallback returns to, likewise
        u64 callback;  ///< the fallback
    };
    static_assert(sizeof(FastmemSite) == 16);
    struct FastmemSiteRun {
        const u8* begin = nullptr;
        const u8* end = nullptr;
        std::vector<FastmemSite> sites;  ///< ascending `site`, one per patch site
    };
    std::vector<FastmemSiteRun> fastmem_site_runs;
    struct PendingFastmemSite {
        u64 site;
        u64 resume;
        u64 callback;
    };
    /// The sites of the block being emitted, in the order they were recorded -- which is not
    /// address order: a deferred emit records its site after the inline ones that follow it.
    std::vector<PendingFastmemSite> pending_fastmem_sites;
    /// Record one site of the block being emitted (shared code cache only).
    void RecordSharedFastmemSite(u64 site, u64 resume, u64 callback);
    /// Add the block's recorded sites to the run holding them, in address order. Called once the
    /// block's code is complete, before it is published.
    void CommitSharedFastmemSites();
    /// What the fault handler needs for `fault_rip`, if it is a recorded site.
    std::optional<FakeCall> FindSharedFastmemSite(u64 fault_rip) const;
};

}  // namespace Dynarmic::Backend::X64
