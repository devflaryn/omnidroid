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
        size_t size;         // Length in bytes of emitted code
    };

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
    /// one aligned store to the slot. `unlinked` is what the slot holds while the target has no
    /// translation.
    struct LinkSlot {
        u64* slot;
        u64 unlinked;
    };
    struct PatchInformation {
        std::vector<CodePtr> jg;
        std::vector<CodePtr> jz;
        std::vector<CodePtr> jmp;
        std::vector<CodePtr> mov_rcx;
        std::vector<LinkSlot> slots;  // patch 0022, shared code cache only
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
    /// Emit the pending slots of the block at `location`, each holding its target's entry point if
    /// it has one and its unlinked value otherwise, and record them so that emitting or
    /// invalidating a target updates them.
    void EmitPendingSlots(const IR::LocationDescriptor& location);
    /// Shared code cache only: each block's own slots, by target, so that dropping the block
    /// takes them out of its targets' records -- which would otherwise grow by one slot per
    /// translation of every block linking to a target, for as long as the region lives.
    tsl::robin_map<IR::LocationDescriptor, std::vector<std::pair<IR::LocationDescriptor, u64*>>> outgoing_slots;
    /// Shared code cache only: unlink and forget the slots of the dropped block at `location`.
    void ForgetOutgoingSlots(const IR::LocationDescriptor& location);
};

}  // namespace Dynarmic::Backend::X64
