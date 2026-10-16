/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include "dynarmic/backend/arm64/reg_alloc.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <bit>
#include <cstdlib>
#include <iterator>

#include <mcl/assert.hpp>
#include <mcl/bit/bit_field.hpp>
#include <mcl/bit_cast.hpp>
#include <mcl/mp/metavalue/lift_value.hpp>
#include <mcl/stdint.hpp>

#include "dynarmic/backend/arm64/abi.h"
#include "dynarmic/backend/arm64/emit_context.h"
#include "dynarmic/backend/arm64/fpsr_manager.h"
#include "dynarmic/backend/arm64/verbose_debugging_output.h"
#include "dynarmic/common/always_false.h"

namespace Dynarmic::Backend::Arm64 {

using namespace oaknut::util;

constexpr size_t spill_offset = offsetof(StackLayout, spill);
constexpr size_t spill_slot_size = sizeof(decltype(StackLayout::spill)::value_type);

namespace {
/// Omnidroid patch 0103: the allocator's own consistency checks -- that a value is not defined
/// twice (a search of every location that always runs to the end) and that every location is
/// unlocked after each instruction (a walk of all 129) -- run with the IR verification
/// (`OMNI_JIT_VERIFY=1`, patch 0077), as the x64 allocator's do (patch 0080). They assert and
/// change nothing.
bool VerifyAllocation() {
    static const bool on = [] {
        const char* v = std::getenv("OMNI_JIT_VERIFY");
        return v != nullptr && v[0] == '1';
    }();
    return on;
}
}  // namespace

std::size_t RegAlloc::IndexOf(HostLoc loc) {
    switch (loc.kind) {
    case HostLoc::Kind::Gpr:
        return static_cast<std::size_t>(loc.index);
    case HostLoc::Kind::Fpr:
        return 32 + static_cast<std::size_t>(loc.index);
    case HostLoc::Kind::Flags:
        return 64;
    case HostLoc::Kind::Spill:
        return 65 + static_cast<std::size_t>(loc.index);
    }
    UNREACHABLE();
}

HostLoc RegAlloc::LocationAt(std::size_t index) {
    if (index < 32) {
        return HostLoc{HostLoc::Kind::Gpr, static_cast<int>(index)};
    }
    if (index < 64) {
        return HostLoc{HostLoc::Kind::Fpr, static_cast<int>(index - 32)};
    }
    if (index == 64) {
        return HostLoc{HostLoc::Kind::Flags, 0};
    }
    return HostLoc{HostLoc::Kind::Spill, static_cast<int>(index - 65)};
}

void RegAlloc::Hint(const HostLocInfo& info, HostLoc loc) const {
    const auto hint = static_cast<std::uint8_t>(IndexOf(loc));
    for (const IR::Inst* value : info.values) {
        value->SetHostLocHint(hint);
    }
}

u64 RegAlloc::NextSeed() {
    // Per thread: one shared counter's cache line went back and forth between the threads that
    // translate side by side.
    thread_local u64 counter = 0x2545F4914F6CDD1D ^ reinterpret_cast<u64>(&counter);
    counter += 0x9E3779B97F4A7C15;
    return counter | 1;
}

static bool IsValuelessType(IR::Type type) {
    switch (type) {
    case IR::Type::Table:
        return true;
    default:
        return false;
    }
}

IR::Type Argument::GetType() const {
    return value.GetType();
}

bool Argument::IsImmediate() const {
    return value.IsImmediate();
}

bool Argument::GetImmediateU1() const {
    return value.GetU1();
}

u8 Argument::GetImmediateU8() const {
    const u64 imm = value.GetImmediateAsU64();
    ASSERT(imm < 0x100);
    return u8(imm);
}

u16 Argument::GetImmediateU16() const {
    const u64 imm = value.GetImmediateAsU64();
    ASSERT(imm < 0x10000);
    return u16(imm);
}

u32 Argument::GetImmediateU32() const {
    const u64 imm = value.GetImmediateAsU64();
    ASSERT(imm < 0x100000000);
    return u32(imm);
}

u64 Argument::GetImmediateU64() const {
    return value.GetImmediateAsU64();
}

IR::Cond Argument::GetImmediateCond() const {
    ASSERT(IsImmediate() && GetType() == IR::Type::Cond);
    return value.GetCond();
}

IR::AccType Argument::GetImmediateAccType() const {
    ASSERT(IsImmediate() && GetType() == IR::Type::AccType);
    return value.GetAccType();
}

HostLoc::Kind Argument::CurrentLocationKind() const {
    return reg_alloc.ValueLocation(value.GetInst())->kind;
}

bool HostLocInfo::Contains(const IR::Inst* value) const {
    return std::find(values.begin(), values.end(), value) != values.end();
}

void HostLocInfo::SetupScratchLocation() {
    ASSERT(IsCompletelyEmpty());
    realized = true;
}

void HostLocInfo::SetupLocation(const IR::Inst* value) {
    ASSERT(IsCompletelyEmpty());
    values.clear();
    values.push_back(value);
    realized = true;
    uses_this_inst = 0;
    accumulated_uses = 0;
    expected_uses = value->UseCount();
}

bool HostLocInfo::IsCompletelyEmpty() const {
    return values.empty() && !locked && !realized && !accumulated_uses && !expected_uses && !uses_this_inst;
}

bool HostLocInfo::MaybeAllocatable() const {
    return !locked && !realized;
}

bool HostLocInfo::IsOneRemainingUse() const {
    return accumulated_uses + 1 == expected_uses && uses_this_inst == 1;
}

void HostLocInfo::UpdateUses() {
    accumulated_uses += uses_this_inst;
    uses_this_inst = 0;

    if (accumulated_uses == expected_uses) {
        values.clear();
        accumulated_uses = 0;
        expected_uses = 0;
    }
}

RegAlloc::ArgumentInfo RegAlloc::GetArgumentInfo(IR::Inst* inst) {
    ArgumentInfo ret = {Argument{*this}, Argument{*this}, Argument{*this}, Argument{*this}};
    for (size_t i = 0; i < inst->NumArgs(); i++) {
        const IR::Value arg = inst->GetArg(i);
        ret[i].value = arg;
        if (!arg.IsImmediate() && !IsValuelessType(arg.GetType())) {
            const auto location = ValueLocation(arg.GetInst());  // patch 0103: looked up once
            ASSERT_MSG(location, "argument must already been defined");
            ValueInfo(*location).uses_this_inst++;
            Touch(*location);
        }
    }
    return ret;
}

bool RegAlloc::WasValueDefined(IR::Inst* inst) const {
    return inst->BackendDefined();  // patch 0103
}

void RegAlloc::PrepareForCall(std::optional<Argument::copyable_reference> arg0, std::optional<Argument::copyable_reference> arg1, std::optional<Argument::copyable_reference> arg2, std::optional<Argument::copyable_reference> arg3) {
    fpsr_manager.Spill();
    SpillFlags();

    // TODO: Spill into callee-save registers

    for (int i = 0; i < 32; i++) {
        if (mcl::bit::get_bit(i, static_cast<u32>(ABI_CALLER_SAVE))) {
            SpillGpr(i);
        }
    }

    for (int i = 0; i < 32; i++) {
        if (mcl::bit::get_bit(i, static_cast<u32>(ABI_CALLER_SAVE >> 32))) {
            SpillFpr(i);
        }
    }

    const std::array<std::optional<Argument::copyable_reference>, 4> args{arg0, arg1, arg2, arg3};

    // AAPCS64 Next General-purpose Register Number
    int ngrn = 0;
    // AAPCS64 Next SIMD and Floating-point Register Number
    int nsrn = 0;

    for (int i = 0; i < 4; i++) {
        if (args[i]) {
            if (args[i]->get().GetType() == IR::Type::U128) {
                ASSERT(fprs[nsrn].IsCompletelyEmpty());
                LoadCopyInto(args[i]->get().value, oaknut::QReg{nsrn});
                nsrn++;
            } else {
                ASSERT(gprs[ngrn].IsCompletelyEmpty());
                LoadCopyInto(args[i]->get().value, oaknut::XReg{ngrn});
                ngrn++;
            }
        } else {
            // Gaps are assumed to be in general-purpose registers
            // TODO: should there be a separate list passed for FPRs instead?
            ngrn++;
        }
    }
}

void RegAlloc::DefineAsExisting(IR::Inst* inst, Argument& arg) {
    inst->SetBackendDefined();

    ASSERT(!VerifyAllocation() || !ValueLocation(inst));

    if (arg.value.IsImmediate()) {
        inst->ReplaceUsesWith(arg.value);
        return;
    }

    const auto location = ValueLocation(arg.value.GetInst());
    ASSERT(location);
    auto& info = ValueInfo(*location);
    info.values.push_back(inst);
    info.expected_uses += inst->UseCount();
    inst->SetHostLocHint(static_cast<std::uint8_t>(IndexOf(*location)));
    Touch(*location);
}

void RegAlloc::DefineAsRegister(IR::Inst* inst, oaknut::Reg reg) {
    inst->SetBackendDefined();

    ASSERT(!VerifyAllocation() || !ValueLocation(inst));
    const HostLoc location{reg.is_vector() ? HostLoc::Kind::Fpr : HostLoc::Kind::Gpr, reg.index()};
    auto& info = ValueInfo(location);
    ASSERT(info.IsCompletelyEmpty());
    info.values.push_back(inst);
    info.expected_uses += inst->UseCount();
    inst->SetHostLocHint(static_cast<std::uint8_t>(IndexOf(location)));
    Touch(location);
}

void RegAlloc::UpdateAllUses() {
    // Omnidroid patch 0103: only the locations this instruction touched -- an argument's use
    // counted, a value defined or moved there -- can have anything to update: elsewhere no use was
    // counted, and a location's values all used up was cleared by the update after its last use.
    for (std::size_t word = 0; word < touched.size(); word++) {
        for (std::uint64_t bits = std::exchange(touched[word], 0); bits != 0; bits &= bits - 1) {
            ValueInfo(LocationAt(word * 64 + static_cast<std::size_t>(std::countr_zero(bits)))).UpdateUses();
        }
    }
}

void RegAlloc::AssertAllUnlocked() const {
    if (!VerifyAllocation()) {
        return;  // patch 0103
    }
    const auto is_unlocked = [](const auto& i) { return !i.locked && !i.realized; };
    ASSERT(std::all_of(gprs.begin(), gprs.end(), is_unlocked));
    ASSERT(std::all_of(fprs.begin(), fprs.end(), is_unlocked));
    ASSERT(is_unlocked(flags));
    ASSERT(std::all_of(spills.begin(), spills.end(), is_unlocked));
}

void RegAlloc::AssertNoMoreUses() const {
    const auto is_empty = [](const auto& i) { return i.IsCompletelyEmpty(); };
    ASSERT(std::all_of(gprs.begin(), gprs.end(), is_empty));
    ASSERT(std::all_of(fprs.begin(), fprs.end(), is_empty));
    ASSERT(is_empty(flags));
    ASSERT(std::all_of(spills.begin(), spills.end(), is_empty));
}

void RegAlloc::EmitVerboseDebuggingOutput() {
    code.MOV(X19, mcl::bit_cast<u64>(&PrintVerboseDebuggingOutputLine));  // Non-volatile register

    const auto do_location = [&](HostLocInfo& info, HostLocType type, size_t index) {
        using namespace oaknut::util;
        for (const IR::Inst* value : info.values) {
            code.MOV(X0, SP);
            code.MOV(X1, static_cast<u64>(type));
            code.MOV(X2, index);
            code.MOV(X3, value->GetName());
            code.MOV(X4, static_cast<u64>(value->GetType()));
            code.BLR(X19);
        }
    };

    for (size_t i = 0; i < gprs.size(); i++) {
        do_location(gprs[i], HostLocType::X, i);
    }
    for (size_t i = 0; i < fprs.size(); i++) {
        do_location(fprs[i], HostLocType::Q, i);
    }
    do_location(flags, HostLocType::Nzcv, 0);
    for (size_t i = 0; i < spills.size(); i++) {
        do_location(spills[i], HostLocType::Spill, i);
    }
}

template<HostLoc::Kind kind>
int RegAlloc::GenerateImmediate(const IR::Value& value) {
    ASSERT(value.GetType() != IR::Type::U1);
    if constexpr (kind == HostLoc::Kind::Gpr) {
        const int new_location_index = AllocateRegister(gprs, gpr_order);
        SpillGpr(new_location_index);
        gprs[new_location_index].SetupScratchLocation();

        code.MOV(oaknut::XReg{new_location_index}, value.GetImmediateAsU64());

        return new_location_index;
    } else if constexpr (kind == HostLoc::Kind::Fpr) {
        const int new_location_index = AllocateRegister(fprs, fpr_order);
        SpillFpr(new_location_index);
        fprs[new_location_index].SetupScratchLocation();

        code.MOV(Xscratch0, value.GetImmediateAsU64());
        code.FMOV(oaknut::DReg{new_location_index}, Xscratch0);

        return new_location_index;
    } else if constexpr (kind == HostLoc::Kind::Flags) {
        SpillFlags();
        flags.SetupScratchLocation();

        code.MOV(Xscratch0, value.GetImmediateAsU64());
        code.MSR(oaknut::SystemReg::NZCV, Xscratch0);

        return 0;
    } else {
        static_assert(Common::always_false_v<mcl::mp::lift_value<kind>>);
    }
}

template<HostLoc::Kind required_kind>
int RegAlloc::RealizeReadImpl(const IR::Value& value) {
    if (value.IsImmediate()) {
        return GenerateImmediate<required_kind>(value);
    }

    const auto current_location = ValueLocation(value.GetInst());
    ASSERT(current_location);

    if (current_location->kind == required_kind) {
        ValueInfo(*current_location).realized = true;
        return current_location->index;
    }

    ASSERT(!ValueInfo(*current_location).realized);
    ASSERT(ValueInfo(*current_location).locked);

    if constexpr (required_kind == HostLoc::Kind::Gpr) {
        const int new_location_index = AllocateRegister(gprs, gpr_order);
        SpillGpr(new_location_index);

        switch (current_location->kind) {
        case HostLoc::Kind::Gpr:
            ASSERT_FALSE("Logic error");
            break;
        case HostLoc::Kind::Fpr:
            code.FMOV(oaknut::XReg{new_location_index}, oaknut::DReg{current_location->index});
            // ASSERT size fits
            break;
        case HostLoc::Kind::Spill:
            code.LDR(oaknut::XReg{new_location_index}, SP, spill_offset + current_location->index * spill_slot_size);
            break;
        case HostLoc::Kind::Flags:
            code.MRS(oaknut::XReg{new_location_index}, oaknut::SystemReg::NZCV);
            break;
        }

        gprs[new_location_index] = std::exchange(ValueInfo(*current_location), {});
        gprs[new_location_index].realized = true;
        Hint(gprs[new_location_index], HostLoc{HostLoc::Kind::Gpr, new_location_index});  // patch 0103
        Touch(HostLoc{HostLoc::Kind::Gpr, new_location_index});
        return new_location_index;
    } else if constexpr (required_kind == HostLoc::Kind::Fpr) {
        const int new_location_index = AllocateRegister(fprs, fpr_order);
        SpillFpr(new_location_index);

        switch (current_location->kind) {
        case HostLoc::Kind::Gpr:
            code.FMOV(oaknut::DReg{new_location_index}, oaknut::XReg{current_location->index});
            break;
        case HostLoc::Kind::Fpr:
            ASSERT_FALSE("Logic error");
            break;
        case HostLoc::Kind::Spill:
            code.LDR(oaknut::QReg{new_location_index}, SP, spill_offset + current_location->index * spill_slot_size);
            break;
        case HostLoc::Kind::Flags:
            ASSERT_FALSE("Moving from flags into fprs is not currently supported");
            break;
        }

        fprs[new_location_index] = std::exchange(ValueInfo(*current_location), {});
        fprs[new_location_index].realized = true;
        Hint(fprs[new_location_index], HostLoc{HostLoc::Kind::Fpr, new_location_index});  // patch 0103
        Touch(HostLoc{HostLoc::Kind::Fpr, new_location_index});
        return new_location_index;
    } else if constexpr (required_kind == HostLoc::Kind::Flags) {
        ASSERT_FALSE("A simple read from flags is likely a logic error.");
    } else {
        static_assert(Common::always_false_v<mcl::mp::lift_value<required_kind>>);
    }
}

template<HostLoc::Kind kind>
int RegAlloc::RealizeWriteImpl(const IR::Inst* value) {
    value->SetBackendDefined();

    ASSERT(!VerifyAllocation() || !ValueLocation(value));

    if constexpr (kind == HostLoc::Kind::Gpr) {
        const int new_location_index = AllocateRegister(gprs, gpr_order);
        SpillGpr(new_location_index);
        gprs[new_location_index].SetupLocation(value);
        value->SetHostLocHint(static_cast<std::uint8_t>(new_location_index));  // patch 0103
        Touch(HostLoc{HostLoc::Kind::Gpr, new_location_index});
        return new_location_index;
    } else if constexpr (kind == HostLoc::Kind::Fpr) {
        const int new_location_index = AllocateRegister(fprs, fpr_order);
        SpillFpr(new_location_index);
        fprs[new_location_index].SetupLocation(value);
        value->SetHostLocHint(static_cast<std::uint8_t>(32 + new_location_index));  // patch 0103
        Touch(HostLoc{HostLoc::Kind::Fpr, new_location_index});
        return new_location_index;
    } else if constexpr (kind == HostLoc::Kind::Flags) {
        SpillFlags();
        flags.SetupLocation(value);
        value->SetHostLocHint(64);  // patch 0103
        Touch(HostLoc{HostLoc::Kind::Flags, 0});
        return 0;
    } else {
        static_assert(Common::always_false_v<mcl::mp::lift_value<kind>>);
    }
}

template<HostLoc::Kind kind>
int RegAlloc::RealizeReadWriteImpl(const IR::Value& read_value, const IR::Inst* write_value) {
    write_value->SetBackendDefined();

    // TODO: Move elimination

    const int write_loc = RealizeWriteImpl<kind>(write_value);

    if constexpr (kind == HostLoc::Kind::Gpr) {
        LoadCopyInto(read_value, oaknut::XReg{write_loc});
        return write_loc;
    } else if constexpr (kind == HostLoc::Kind::Fpr) {
        LoadCopyInto(read_value, oaknut::QReg{write_loc});
        return write_loc;
    } else if constexpr (kind == HostLoc::Kind::Flags) {
        ASSERT_FALSE("Incorrect function for ReadWrite of flags");
    } else {
        static_assert(Common::always_false_v<mcl::mp::lift_value<kind>>);
    }
}

template int RegAlloc::RealizeReadImpl<HostLoc::Kind::Gpr>(const IR::Value& value);
template int RegAlloc::RealizeReadImpl<HostLoc::Kind::Fpr>(const IR::Value& value);
template int RegAlloc::RealizeReadImpl<HostLoc::Kind::Flags>(const IR::Value& value);
template int RegAlloc::RealizeWriteImpl<HostLoc::Kind::Gpr>(const IR::Inst* value);
template int RegAlloc::RealizeWriteImpl<HostLoc::Kind::Fpr>(const IR::Inst* value);
template int RegAlloc::RealizeWriteImpl<HostLoc::Kind::Flags>(const IR::Inst* value);
template int RegAlloc::RealizeReadWriteImpl<HostLoc::Kind::Gpr>(const IR::Value&, const IR::Inst*);
template int RegAlloc::RealizeReadWriteImpl<HostLoc::Kind::Fpr>(const IR::Value&, const IR::Inst*);
template int RegAlloc::RealizeReadWriteImpl<HostLoc::Kind::Flags>(const IR::Value&, const IR::Inst*);

int RegAlloc::AllocateRegister(const std::array<HostLocInfo, 32>& regs, std::span<const int> order) const {
    const auto empty = std::find_if(order.begin(), order.end(), [&](int i) { return regs[i].IsCompletelyEmpty(); });
    if (empty != order.end()) {
        return *empty;
    }

    // Omnidroid patch 0103: on the stack, and a random pick by xorshift.
    std::array<int, 32> candidates;
    std::size_t count = 0;
    for (const int i : order) {
        if (regs[i].MaybeAllocatable()) {
            candidates[count++] = i;
        }
    }
    ASSERT_MSG(count > 0, "no register to allocate");

    // TODO: LRU
    rand_state ^= rand_state << 13;
    rand_state ^= rand_state >> 7;
    rand_state ^= rand_state << 17;
    return candidates[rand_state % count];
}

void RegAlloc::SpillGpr(int index) {
    ASSERT(!gprs[index].locked && !gprs[index].realized);
    if (gprs[index].values.empty()) {
        return;
    }
    const int new_location_index = FindFreeSpill();
    code.STR(oaknut::XReg{index}, SP, spill_offset + new_location_index * spill_slot_size);
    spills[new_location_index] = std::exchange(gprs[index], {});
    Hint(spills[new_location_index], HostLoc{HostLoc::Kind::Spill, new_location_index});  // patch 0103
    Touch(HostLoc{HostLoc::Kind::Spill, new_location_index});
}

void RegAlloc::SpillFpr(int index) {
    ASSERT(!fprs[index].locked && !fprs[index].realized);
    if (fprs[index].values.empty()) {
        return;
    }
    const int new_location_index = FindFreeSpill();
    code.STR(oaknut::QReg{index}, SP, spill_offset + new_location_index * spill_slot_size);
    spills[new_location_index] = std::exchange(fprs[index], {});
    Hint(spills[new_location_index], HostLoc{HostLoc::Kind::Spill, new_location_index});  // patch 0103
    Touch(HostLoc{HostLoc::Kind::Spill, new_location_index});
}

void RegAlloc::ReadWriteFlags(Argument& read, IR::Inst* write) {
    if (write) {
        write->SetBackendDefined();
    }

    const auto current_location = ValueLocation(read.value.GetInst());
    ASSERT(current_location);

    if (current_location->kind == HostLoc::Kind::Flags) {
        if (!flags.IsOneRemainingUse()) {
            SpillFlags();
        }
    } else if (current_location->kind == HostLoc::Kind::Gpr) {
        if (!flags.values.empty()) {
            SpillFlags();
        }
        code.MSR(oaknut::SystemReg::NZCV, oaknut::XReg{current_location->index});
    } else if (current_location->kind == HostLoc::Kind::Spill) {
        if (!flags.values.empty()) {
            SpillFlags();
        }
        code.LDR(Wscratch0, SP, spill_offset + current_location->index * spill_slot_size);
        code.MSR(oaknut::SystemReg::NZCV, Xscratch0);
    } else {
        ASSERT_FALSE("Invalid current location for flags");
    }

    if (write) {
        flags.SetupLocation(write);
        flags.realized = false;
        write->SetHostLocHint(64);  // patch 0103
        Touch(HostLoc{HostLoc::Kind::Flags, 0});
    }
}

void RegAlloc::SpillFlags() {
    ASSERT(!flags.locked && !flags.realized);
    if (flags.values.empty()) {
        return;
    }
    const int new_location_index = AllocateRegister(gprs, gpr_order);
    SpillGpr(new_location_index);
    code.MRS(oaknut::XReg{new_location_index}, oaknut::SystemReg::NZCV);
    gprs[new_location_index] = std::exchange(flags, {});
    Hint(gprs[new_location_index], HostLoc{HostLoc::Kind::Gpr, new_location_index});  // patch 0103
    Touch(HostLoc{HostLoc::Kind::Gpr, new_location_index});
}

int RegAlloc::FindFreeSpill() const {
    const auto iter = std::find_if(spills.begin(), spills.end(), [](const HostLocInfo& info) { return info.values.empty(); });
    ASSERT_MSG(iter != spills.end(), "All spill locations are full");
    return static_cast<int>(iter - spills.begin());
}

void RegAlloc::LoadCopyInto(const IR::Value& value, oaknut::XReg reg) {
    if (value.IsImmediate()) {
        code.MOV(reg, value.GetImmediateAsU64());
        return;
    }

    const auto current_location = ValueLocation(value.GetInst());
    ASSERT(current_location);
    switch (current_location->kind) {
    case HostLoc::Kind::Gpr:
        code.MOV(reg, oaknut::XReg{current_location->index});
        break;
    case HostLoc::Kind::Fpr:
        code.FMOV(reg, oaknut::DReg{current_location->index});
        // ASSERT size fits
        break;
    case HostLoc::Kind::Spill:
        code.LDR(reg, SP, spill_offset + current_location->index * spill_slot_size);
        break;
    case HostLoc::Kind::Flags:
        code.MRS(reg, oaknut::SystemReg::NZCV);
        break;
    }
}

void RegAlloc::LoadCopyInto(const IR::Value& value, oaknut::QReg reg) {
    if (value.IsImmediate()) {
        code.MOV(Xscratch0, value.GetImmediateAsU64());
        code.FMOV(reg.toD(), Xscratch0);
        return;
    }

    const auto current_location = ValueLocation(value.GetInst());
    ASSERT(current_location);
    switch (current_location->kind) {
    case HostLoc::Kind::Gpr:
        code.FMOV(reg.toD(), oaknut::XReg{current_location->index});
        break;
    case HostLoc::Kind::Fpr:
        code.MOV(reg.B16(), oaknut::QReg{current_location->index}.B16());
        break;
    case HostLoc::Kind::Spill:
        // TODO: Minimize move size to max value width
        code.LDR(reg, SP, spill_offset + current_location->index * spill_slot_size);
        break;
    case HostLoc::Kind::Flags:
        ASSERT_FALSE("Moving from flags into fprs is not currently supported");
        break;
    }
}

std::optional<HostLoc> RegAlloc::ValueLocation(const IR::Inst* value) const {
    const auto at = [this](std::size_t index) -> const HostLocInfo& {
        if (index < 32) {
            return gprs[index];
        }
        if (index < 64) {
            return fprs[index - 32];
        }
        if (index == 64) {
            return flags;
        }
        return spills[index - 65];
    };
    // Omnidroid patch 0103: where the value was last seen, if it is still there. A value is in one
    // location at a time (a move takes it out of the old one), so that is where the search below
    // would find it.
    const std::size_t hint = value->HostLocHint();
    if (hint < location_count && at(hint).Contains(value)) {
        return LocationAt(hint);
    }
    for (std::size_t i = 0; i < location_count; i++) {
        if (at(i).Contains(value)) {
            value->SetHostLocHint(static_cast<std::uint8_t>(i));
            return LocationAt(i);
        }
    }
    return std::nullopt;
}

HostLocInfo& RegAlloc::ValueInfo(HostLoc host_loc) {
    switch (host_loc.kind) {
    case HostLoc::Kind::Gpr:
        return gprs[static_cast<size_t>(host_loc.index)];
    case HostLoc::Kind::Fpr:
        return fprs[static_cast<size_t>(host_loc.index)];
    case HostLoc::Kind::Flags:
        return flags;
    case HostLoc::Kind::Spill:
        return spills[static_cast<size_t>(host_loc.index)];
    }
    ASSERT_FALSE("RegAlloc::ValueInfo: Invalid HostLoc::Kind");
}

HostLocInfo& RegAlloc::ValueInfo(const IR::Inst* value) {
    const auto location = ValueLocation(value);  // patch 0103: the hint first
    ASSERT_MSG(location, "RegAlloc::ValueInfo: Value not found");
    return ValueInfo(*location);
}

}  // namespace Dynarmic::Backend::Arm64
