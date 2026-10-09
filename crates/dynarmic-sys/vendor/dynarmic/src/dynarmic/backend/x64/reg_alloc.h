/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <array>
#include <functional>
#include <optional>
#include <span>
#include <utility>
#include <vector>

#include <mcl/stdint.hpp>
#include <xbyak/xbyak.h>

#include "dynarmic/backend/x64/block_of_code.h"
#include "dynarmic/backend/x64/hostloc.h"
#include "dynarmic/backend/x64/oparg.h"
#include "dynarmic/backend/x64/stack_layout.h"
#include "dynarmic/ir/cond.h"
#include "dynarmic/ir/microinstruction.h"
#include "dynarmic/ir/value.h"

namespace Dynarmic::IR {
enum class AccType;
}  // namespace Dynarmic::IR

namespace Dynarmic::Backend::X64 {

class RegAlloc;

/// Omnidroid patch 0063: the values a host location holds -- almost always one or two -- inline,
/// rather than a `std::vector` (a heap allocation per location per block, and an out-of-line
/// `std::find` on every lookup). The same order and the same contents.
class HostLocValues {
public:
    bool empty() const { return size == 0; }
    void clear() {
        size = 0;
        spilled.clear();
    }
    void push_back(IR::Inst* inst) {
        if (size < inline_values.size()) {
            inline_values[size++] = inst;
            return;
        }
        if (size == inline_values.size()) {
            spilled.assign(inline_values.begin(), inline_values.end());
        }
        spilled.push_back(inst);
        size++;
    }
    IR::Inst* const* begin() const { return size <= inline_values.size() ? inline_values.data() : spilled.data(); }
    IR::Inst* const* end() const { return begin() + size; }
    bool contains(const IR::Inst* inst) const {
        for (IR::Inst* const* i = begin(), *const* e = end(); i != e; ++i) {
            if (*i == inst) {
                return true;
            }
        }
        return false;
    }

private:
    std::size_t size = 0;
    std::array<IR::Inst*, 3> inline_values{};
    std::vector<IR::Inst*> spilled;  ///< all of them, past the inline capacity
};

struct HostLocInfo {
public:
    bool IsLocked() const;
    bool IsEmpty() const;
    bool IsLastUse() const;

    void SetLastUse();

    void ReadLock();
    void WriteLock();
    void AddArgReference();
    void ReleaseOne();
    void ReleaseAll();

    bool ContainsValue(const IR::Inst* inst) const;
    size_t GetMaxBitWidth() const;

    void AddValue(IR::Inst* inst);

    void EmitVerboseDebuggingOutput(BlockOfCode& code, size_t host_loc_index) const;

private:
    // Current instruction state
    size_t is_being_used_count = 0;
    bool is_scratch = false;
    bool is_set_last_use = false;

    // Block state
    size_t current_references = 0;
    size_t accumulated_uses = 0;
    size_t total_uses = 0;

    // Value state
    HostLocValues values;  // Omnidroid patch 0063: inline
    size_t max_bit_width = 0;
};

struct Argument {
public:
    using copyable_reference = std::reference_wrapper<Argument>;

    IR::Type GetType() const;
    bool IsImmediate() const;
    bool IsVoid() const;

    bool FitsInImmediateU32() const;
    bool FitsInImmediateS32() const;

    bool GetImmediateU1() const;
    u8 GetImmediateU8() const;
    u16 GetImmediateU16() const;
    u32 GetImmediateU32() const;
    u64 GetImmediateS32() const;
    u64 GetImmediateU64() const;
    IR::Cond GetImmediateCond() const;
    IR::AccType GetImmediateAccType() const;

    /// Is this value currently in a GPR?
    bool IsInGpr() const;
    /// Is this value currently in a XMM?
    bool IsInXmm() const;
    /// Is this value currently in memory?
    bool IsInMemory() const;

private:
    friend class RegAlloc;
    explicit Argument(RegAlloc& reg_alloc)
            : reg_alloc(reg_alloc) {}

    bool allocated = false;
    RegAlloc& reg_alloc;
    IR::Value value;
};

class RegAlloc final {
public:
    using ArgumentInfo = std::array<Argument, IR::max_arg_count>;

    /// Omnidroid patch 0063: the orders are viewed, not copied (two allocations a block): they
    /// must outlive the allocator.
    explicit RegAlloc(BlockOfCode& code, std::span<const HostLoc> gpr_order, std::span<const HostLoc> xmm_order);

    ArgumentInfo GetArgumentInfo(IR::Inst* inst);
    void RegisterPseudoOperation(IR::Inst* inst);
    bool IsValueLive(IR::Inst* inst) const;

    Xbyak::Reg64 UseGpr(Argument& arg);
    Xbyak::Xmm UseXmm(Argument& arg);
    OpArg UseOpArg(Argument& arg);
    void Use(Argument& arg, HostLoc host_loc);

    Xbyak::Reg64 UseScratchGpr(Argument& arg);
    Xbyak::Xmm UseScratchXmm(Argument& arg);
    void UseScratch(Argument& arg, HostLoc host_loc);

    void DefineValue(IR::Inst* inst, const Xbyak::Reg& reg);
    void DefineValue(IR::Inst* inst, Argument& arg);

    void Release(const Xbyak::Reg& reg);

    Xbyak::Reg64 ScratchGpr();
    Xbyak::Reg64 ScratchGpr(HostLoc desired_location);
    Xbyak::Xmm ScratchXmm();
    Xbyak::Xmm ScratchXmm(HostLoc desired_location);

    void HostCall(IR::Inst* result_def = nullptr,
                  std::optional<Argument::copyable_reference> arg0 = {},
                  std::optional<Argument::copyable_reference> arg1 = {},
                  std::optional<Argument::copyable_reference> arg2 = {},
                  std::optional<Argument::copyable_reference> arg3 = {});

    // TODO: Values in host flags

    void AllocStackSpace(size_t stack_space);
    void ReleaseStackSpace(size_t stack_space);

    void EndOfAllocScope();

    void AssertNoMoreUses();

    void EmitVerboseDebuggingOutput();

private:
    friend struct Argument;

    std::span<const HostLoc> gpr_order;
    std::span<const HostLoc> xmm_order;

    // Omnidroid patch 0063: the desired locations as a span -- one location, or an order -- rather
    // than a `std::vector` built (allocated) for each call.
    using Locations = std::span<const HostLoc>;
    HostLoc SelectARegister(Locations desired_locations) const;
    std::optional<HostLoc> ValueLocation(const IR::Inst* value) const;

    HostLoc UseImpl(IR::Value use_value, Locations desired_locations);
    HostLoc UseScratchImpl(IR::Value use_value, Locations desired_locations);
    HostLoc ScratchImpl(Locations desired_locations);
    void DefineValueImpl(IR::Inst* def_inst, HostLoc host_loc);
    void DefineValueImpl(IR::Inst* def_inst, const IR::Value& use_inst);

    HostLoc LoadImmediate(IR::Value imm, HostLoc host_loc);
    void Move(HostLoc to, HostLoc from);
    void CopyToScratch(size_t bit_width, HostLoc to, HostLoc from);
    void Exchange(HostLoc a, HostLoc b);
    void MoveOutOfTheWay(HostLoc reg);

    void SpillRegister(HostLoc loc);
    HostLoc FindFreeSpill() const;

    static constexpr std::size_t HostLocCount = NonSpillHostLocCount + SpillCount;
    // Omnidroid patch 0063: in place, not a vector allocated per block. And two masks over it:
    // `touched`, every location written through `LocInfo` since the last `EndOfAllocScope` (the
    // only ones it has anything to release); `occupied`, a superset of the locations holding
    // values (the only ones `ValueLocation` searches). Neither changes a decision.
    std::array<HostLocInfo, HostLocCount> hostloc_info;
    std::array<std::uint64_t, (HostLocCount + 63) / 64> touched{};
    std::array<std::uint64_t, (HostLocCount + 63) / 64> occupied{};
    void MarkOccupied(HostLoc loc) {
        const auto i = static_cast<std::size_t>(loc);
        occupied[i / 64] |= std::uint64_t{1} << (i % 64);
    }
    HostLocInfo& LocInfo(HostLoc loc);
    const HostLocInfo& LocInfo(HostLoc loc) const;

    BlockOfCode& code;
    size_t reserved_stack_space = 0;
    void EmitMove(size_t bit_width, HostLoc to, HostLoc from);
    void EmitExchange(HostLoc a, HostLoc b);

    Xbyak::Address SpillToOpArg(HostLoc loc);
};

}  // namespace Dynarmic::Backend::X64
