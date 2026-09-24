/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include "dynarmic/backend/arm64/emit_arm64_memory.h"

#include <optional>
#include <utility>

#include <mcl/bit_cast.hpp>
#include <oaknut/oaknut.hpp>

#include "dynarmic/backend/arm64/abi.h"
#include "dynarmic/backend/arm64/emit_arm64.h"
#include "dynarmic/backend/arm64/emit_context.h"
#include "dynarmic/backend/arm64/fastmem.h"
#include "dynarmic/backend/arm64/fpsr_manager.h"
#include "dynarmic/backend/arm64/reg_alloc.h"
#include "dynarmic/backend/x64/exclusive_monitor_friend.h"
#include "dynarmic/common/spin_lock_arm64.h"
#include "dynarmic/ir/acc_type.h"
#include "dynarmic/ir/basic_block.h"
#include "dynarmic/ir/microinstruction.h"
#include "dynarmic/ir/opcodes.h"
#include "dynarmic/interface/exclusive_monitor.h"

namespace Dynarmic::Backend::Arm64 {

using namespace oaknut::util;

namespace {

bool IsOrdered(IR::AccType acctype) {
    return acctype == IR::AccType::ORDERED || acctype == IR::AccType::ORDEREDRW || acctype == IR::AccType::LIMITEDORDERED;
}

LinkTarget ReadMemoryLinkTarget(size_t bitsize) {
    switch (bitsize) {
    case 8:
        return LinkTarget::ReadMemory8;
    case 16:
        return LinkTarget::ReadMemory16;
    case 32:
        return LinkTarget::ReadMemory32;
    case 64:
        return LinkTarget::ReadMemory64;
    case 128:
        return LinkTarget::ReadMemory128;
    }
    UNREACHABLE();
}

LinkTarget WriteMemoryLinkTarget(size_t bitsize) {
    switch (bitsize) {
    case 8:
        return LinkTarget::WriteMemory8;
    case 16:
        return LinkTarget::WriteMemory16;
    case 32:
        return LinkTarget::WriteMemory32;
    case 64:
        return LinkTarget::WriteMemory64;
    case 128:
        return LinkTarget::WriteMemory128;
    }
    UNREACHABLE();
}

LinkTarget WrappedReadMemoryLinkTarget(size_t bitsize) {
    switch (bitsize) {
    case 8:
        return LinkTarget::WrappedReadMemory8;
    case 16:
        return LinkTarget::WrappedReadMemory16;
    case 32:
        return LinkTarget::WrappedReadMemory32;
    case 64:
        return LinkTarget::WrappedReadMemory64;
    case 128:
        return LinkTarget::WrappedReadMemory128;
    }
    UNREACHABLE();
}

LinkTarget WrappedWriteMemoryLinkTarget(size_t bitsize) {
    switch (bitsize) {
    case 8:
        return LinkTarget::WrappedWriteMemory8;
    case 16:
        return LinkTarget::WrappedWriteMemory16;
    case 32:
        return LinkTarget::WrappedWriteMemory32;
    case 64:
        return LinkTarget::WrappedWriteMemory64;
    case 128:
        return LinkTarget::WrappedWriteMemory128;
    }
    UNREACHABLE();
}

LinkTarget ExclusiveReadMemoryLinkTarget(size_t bitsize) {
    switch (bitsize) {
    case 8:
        return LinkTarget::ExclusiveReadMemory8;
    case 16:
        return LinkTarget::ExclusiveReadMemory16;
    case 32:
        return LinkTarget::ExclusiveReadMemory32;
    case 64:
        return LinkTarget::ExclusiveReadMemory64;
    case 128:
        return LinkTarget::ExclusiveReadMemory128;
    }
    UNREACHABLE();
}

LinkTarget ExclusiveWriteMemoryLinkTarget(size_t bitsize) {
    switch (bitsize) {
    case 8:
        return LinkTarget::ExclusiveWriteMemory8;
    case 16:
        return LinkTarget::ExclusiveWriteMemory16;
    case 32:
        return LinkTarget::ExclusiveWriteMemory32;
    case 64:
        return LinkTarget::ExclusiveWriteMemory64;
    case 128:
        return LinkTarget::ExclusiveWriteMemory128;
    }
    UNREACHABLE();
}

template<size_t bitsize>
void CallbackOnlyEmitReadMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ctx.reg_alloc.PrepareForCall({}, args[1]);
    const bool ordered = IsOrdered(args[2].GetImmediateAccType());

    EmitRelocation(code, ctx, ReadMemoryLinkTarget(bitsize));
    if (ordered) {
        code.DMB(oaknut::BarrierOp::ISH);
    }

    if constexpr (bitsize == 128) {
        code.MOV(Q8.B16(), Q0.B16());
        ctx.reg_alloc.DefineAsRegister(inst, Q8);
    } else {
        ctx.reg_alloc.DefineAsRegister(inst, X0);
    }
}

template<size_t bitsize>
void CallbackOnlyEmitExclusiveReadMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ctx.reg_alloc.PrepareForCall({}, args[1]);
    const bool ordered = IsOrdered(args[2].GetImmediateAccType());

    code.MOV(Wscratch0, 1);
    code.STRB(Wscratch0, Xstate, ctx.conf.state_exclusive_state_offset);
    EmitRelocation(code, ctx, ExclusiveReadMemoryLinkTarget(bitsize));
    if (ordered) {
        code.DMB(oaknut::BarrierOp::ISH);
    }

    if constexpr (bitsize == 128) {
        code.MOV(Q8.B16(), Q0.B16());
        ctx.reg_alloc.DefineAsRegister(inst, Q8);
    } else {
        ctx.reg_alloc.DefineAsRegister(inst, X0);
    }
}

template<size_t bitsize>
void CallbackOnlyEmitWriteMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ctx.reg_alloc.PrepareForCall({}, args[1], args[2]);
    const bool ordered = IsOrdered(args[3].GetImmediateAccType());

    if (ordered) {
        code.DMB(oaknut::BarrierOp::ISH);
    }
    EmitRelocation(code, ctx, WriteMemoryLinkTarget(bitsize));
    if (ordered) {
        code.DMB(oaknut::BarrierOp::ISH);
    }
}

template<size_t bitsize>
void CallbackOnlyEmitExclusiveWriteMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ctx.reg_alloc.PrepareForCall({}, args[1], args[2]);
    const bool ordered = IsOrdered(args[3].GetImmediateAccType());

    oaknut::Label end;

    if (ordered) {
        code.DMB(oaknut::BarrierOp::ISH);
    }
    code.MOV(W0, 1);
    code.LDRB(Wscratch0, Xstate, ctx.conf.state_exclusive_state_offset);
    code.CBZ(Wscratch0, end);
    code.STRB(WZR, Xstate, ctx.conf.state_exclusive_state_offset);
    EmitRelocation(code, ctx, ExclusiveWriteMemoryLinkTarget(bitsize));
    if (ordered) {
        code.DMB(oaknut::BarrierOp::ISH);
    }
    code.l(end);
    ctx.reg_alloc.DefineAsRegister(inst, X0);
}

constexpr size_t page_bits = 12;
constexpr size_t page_size = 1 << page_bits;
constexpr size_t page_mask = (1 << page_bits) - 1;

// This function may use Xscratch0 as a scratch register
// Trashes NZCV
template<size_t bitsize>
void EmitDetectMisalignedVAddr(oaknut::CodeGenerator& code, EmitContext& ctx, oaknut::XReg Xaddr, const SharedLabel& fallback) {
    static_assert(bitsize == 8 || bitsize == 16 || bitsize == 32 || bitsize == 64 || bitsize == 128);

    if (bitsize == 8 || (ctx.conf.detect_misaligned_access_via_page_table & bitsize) == 0) {
        return;
    }

    if (!ctx.conf.only_detect_misalignment_via_page_table_on_page_boundary) {
        const u64 align_mask = []() -> u64 {
            switch (bitsize) {
            case 16:
                return 0b1;
            case 32:
                return 0b11;
            case 64:
                return 0b111;
            case 128:
                return 0b1111;
            default:
                UNREACHABLE();
            }
        }();

        code.TST(Xaddr, align_mask);
        code.B(NE, *fallback);
    } else {
        // If (addr & page_mask) > page_size - byte_size, use fallback.
        code.AND(Xscratch0, Xaddr, page_mask);
        code.CMP(Xscratch0, page_size - bitsize / 8);
        code.B(HI, *fallback);
    }
}

// Outputs Xscratch0 = page_table[addr >> page_bits]
// May use Xscratch1 as scratch register
// Address to read/write = [ret0 + ret1], ret0 is always Xscratch0 and ret1 is either Xaddr or Xscratch1
// Trashes NZCV
template<size_t bitsize>
std::pair<oaknut::XReg, oaknut::XReg> InlinePageTableEmitVAddrLookup(oaknut::CodeGenerator& code, EmitContext& ctx, oaknut::XReg Xaddr, const SharedLabel& fallback) {
    const size_t valid_page_index_bits = ctx.conf.page_table_address_space_bits - page_bits;
    const size_t unused_top_bits = 64 - ctx.conf.page_table_address_space_bits;

    EmitDetectMisalignedVAddr<bitsize>(code, ctx, Xaddr, fallback);

    if (ctx.conf.silently_mirror_page_table || unused_top_bits == 0) {
        code.UBFX(Xscratch0, Xaddr, page_bits, valid_page_index_bits);
    } else {
        code.LSR(Xscratch0, Xaddr, page_bits);
        code.TST(Xscratch0, u64(~u64(0)) << valid_page_index_bits);
        code.B(NE, *fallback);
    }

    code.LDR(Xscratch0, Xpagetable, Xscratch0, LSL, 3);

    if (ctx.conf.page_table_pointer_mask_bits != 0) {
        const u64 mask = u64(~u64(0)) << ctx.conf.page_table_pointer_mask_bits;
        code.AND(Xscratch0, Xscratch0, mask);
    }

    code.CBZ(Xscratch0, *fallback);

    if (ctx.conf.absolute_offset_page_table) {
        return std::make_pair(Xscratch0, Xaddr);
    }
    code.AND(Xscratch1, Xaddr, page_mask);
    return std::make_pair(Xscratch0, Xscratch1);
}

template<std::size_t bitsize>
CodePtr EmitMemoryLdr(oaknut::CodeGenerator& code, int value_idx, oaknut::XReg Xbase, oaknut::XReg Xoffset, bool ordered, bool extend32 = false) {
    const auto index_ext = extend32 ? oaknut::IndexExt::UXTW : oaknut::IndexExt::LSL;
    const auto add_ext = extend32 ? oaknut::AddSubExt::UXTW : oaknut::AddSubExt::LSL;
    const auto Roffset = extend32 ? oaknut::RReg{Xoffset.toW()} : oaknut::RReg{Xoffset};

    CodePtr fastmem_location = code.xptr<CodePtr>();

    if (ordered) {
        code.ADD(Xscratch0, Xbase, Roffset, add_ext);

        fastmem_location = code.xptr<CodePtr>();

        switch (bitsize) {
        case 8:
            code.LDARB(oaknut::WReg{value_idx}, Xscratch0);
            break;
        case 16:
            code.LDARH(oaknut::WReg{value_idx}, Xscratch0);
            break;
        case 32:
            code.LDAR(oaknut::WReg{value_idx}, Xscratch0);
            break;
        case 64:
            code.LDAR(oaknut::XReg{value_idx}, Xscratch0);
            break;
        case 128:
            code.LDR(oaknut::QReg{value_idx}, Xscratch0);
            code.DMB(oaknut::BarrierOp::ISH);
            break;
        default:
            ASSERT_FALSE("Invalid bitsize");
        }
    } else {
        fastmem_location = code.xptr<CodePtr>();

        switch (bitsize) {
        case 8:
            code.LDRB(oaknut::WReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        case 16:
            code.LDRH(oaknut::WReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        case 32:
            code.LDR(oaknut::WReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        case 64:
            code.LDR(oaknut::XReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        case 128:
            code.LDR(oaknut::QReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        default:
            ASSERT_FALSE("Invalid bitsize");
        }
    }

    return fastmem_location;
}

template<std::size_t bitsize>
CodePtr EmitMemoryStr(oaknut::CodeGenerator& code, int value_idx, oaknut::XReg Xbase, oaknut::XReg Xoffset, bool ordered, bool extend32 = false) {
    const auto index_ext = extend32 ? oaknut::IndexExt::UXTW : oaknut::IndexExt::LSL;
    const auto add_ext = extend32 ? oaknut::AddSubExt::UXTW : oaknut::AddSubExt::LSL;
    const auto Roffset = extend32 ? oaknut::RReg{Xoffset.toW()} : oaknut::RReg{Xoffset};

    CodePtr fastmem_location;

    if (ordered) {
        code.ADD(Xscratch0, Xbase, Roffset, add_ext);

        fastmem_location = code.xptr<CodePtr>();

        switch (bitsize) {
        case 8:
            code.STLRB(oaknut::WReg{value_idx}, Xscratch0);
            break;
        case 16:
            code.STLRH(oaknut::WReg{value_idx}, Xscratch0);
            break;
        case 32:
            code.STLR(oaknut::WReg{value_idx}, Xscratch0);
            break;
        case 64:
            code.STLR(oaknut::XReg{value_idx}, Xscratch0);
            break;
        case 128:
            code.DMB(oaknut::BarrierOp::ISH);
            code.STR(oaknut::QReg{value_idx}, Xscratch0);
            code.DMB(oaknut::BarrierOp::ISH);
            break;
        default:
            ASSERT_FALSE("Invalid bitsize");
        }
    } else {
        fastmem_location = code.xptr<CodePtr>();

        switch (bitsize) {
        case 8:
            code.STRB(oaknut::WReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        case 16:
            code.STRH(oaknut::WReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        case 32:
            code.STR(oaknut::WReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        case 64:
            code.STR(oaknut::XReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        case 128:
            code.STR(oaknut::QReg{value_idx}, Xbase, Roffset, index_ext);
            break;
        default:
            ASSERT_FALSE("Invalid bitsize");
        }
    }

    return fastmem_location;
}

template<size_t bitsize>
void InlinePageTableEmitReadMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    auto Xaddr = ctx.reg_alloc.ReadX(args[1]);
    auto Rvalue = [&] {
        if constexpr (bitsize == 128) {
            return ctx.reg_alloc.WriteQ(inst);
        } else {
            return ctx.reg_alloc.WriteReg<std::max<std::size_t>(bitsize, 32)>(inst);
        }
    }();
    const bool ordered = IsOrdered(args[2].GetImmediateAccType());
    ctx.fpsr.Spill();
    ctx.reg_alloc.SpillFlags();
    RegAlloc::Realize(Xaddr, Rvalue);

    SharedLabel fallback = GenSharedLabel(), end = GenSharedLabel();

    const auto [Xbase, Xoffset] = InlinePageTableEmitVAddrLookup<bitsize>(code, ctx, Xaddr, fallback);
    EmitMemoryLdr<bitsize>(code, Rvalue->index(), Xbase, Xoffset, ordered);

    ctx.deferred_emits.emplace_back([&code, &ctx, inst, Xaddr = *Xaddr, Rvalue = *Rvalue, ordered, fallback, end] {
        code.l(*fallback);
        code.MOV(Xscratch0, Xaddr);
        EmitRelocation(code, ctx, WrappedReadMemoryLinkTarget(bitsize));
        if (ordered) {
            code.DMB(oaknut::BarrierOp::ISH);
        }
        if constexpr (bitsize == 128) {
            code.MOV(Rvalue.B16(), Q0.B16());
        } else {
            code.MOV(Rvalue.toX(), Xscratch0);
        }
        ctx.conf.emit_check_memory_abort(code, ctx, inst, *end);
        code.B(*end);
    });

    code.l(*end);
}

template<size_t bitsize>
void InlinePageTableEmitWriteMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    auto Xaddr = ctx.reg_alloc.ReadX(args[1]);
    auto Rvalue = [&] {
        if constexpr (bitsize == 128) {
            return ctx.reg_alloc.ReadQ(args[2]);
        } else {
            return ctx.reg_alloc.ReadReg<std::max<std::size_t>(bitsize, 32)>(args[2]);
        }
    }();
    const bool ordered = IsOrdered(args[3].GetImmediateAccType());
    ctx.fpsr.Spill();
    ctx.reg_alloc.SpillFlags();
    RegAlloc::Realize(Xaddr, Rvalue);

    SharedLabel fallback = GenSharedLabel(), end = GenSharedLabel();

    const auto [Xbase, Xoffset] = InlinePageTableEmitVAddrLookup<bitsize>(code, ctx, Xaddr, fallback);
    EmitMemoryStr<bitsize>(code, Rvalue->index(), Xbase, Xoffset, ordered);

    ctx.deferred_emits.emplace_back([&code, &ctx, inst, Xaddr = *Xaddr, Rvalue = *Rvalue, ordered, fallback, end] {
        code.l(*fallback);
        if constexpr (bitsize == 128) {
            code.MOV(Xscratch0, Xaddr);
            code.MOV(Q0.B16(), Rvalue.B16());
        } else {
            code.MOV(Xscratch0, Xaddr);
            code.MOV(Xscratch1, Rvalue.toX());
        }
        if (ordered) {
            code.DMB(oaknut::BarrierOp::ISH);
        }
        EmitRelocation(code, ctx, WrappedWriteMemoryLinkTarget(bitsize));
        if (ordered) {
            code.DMB(oaknut::BarrierOp::ISH);
        }
        ctx.conf.emit_check_memory_abort(code, ctx, inst, *end);
        code.B(*end);
    });

    code.l(*end);
}

std::optional<DoNotFastmemMarker> ShouldFastmem(EmitContext& ctx, IR::Inst* inst) {
    if (!ctx.conf.fastmem_pointer || !ctx.fastmem.SupportsFastmem()) {
        return std::nullopt;
    }

    const auto marker = std::make_tuple(ctx.block.Location(), inst->GetName());
    if (ctx.fastmem.ShouldFastmem(marker)) {
        return marker;
    }
    return std::nullopt;
}

inline bool ShouldExt32(EmitContext& ctx) {
    return ctx.conf.fastmem_address_space_bits == 32 && ctx.conf.silently_mirror_fastmem;
}

// May use Xscratch0 as scratch register
// Address to read/write = [ret0 + ret1], ret0 is always Xfastmem and ret1 is either Xaddr or Xscratch0
// Trashes NZCV
template<size_t bitsize>
std::pair<oaknut::XReg, oaknut::XReg> FastmemEmitVAddrLookup(oaknut::CodeGenerator& code, EmitContext& ctx, oaknut::XReg Xaddr, const SharedLabel& fallback) {
    if (ctx.conf.fastmem_address_space_bits == 64 || ShouldExt32(ctx)) {
        return std::make_pair(Xfastmem, Xaddr);
    }

    if (ctx.conf.silently_mirror_fastmem) {
        code.UBFX(Xscratch0, Xaddr, 0, ctx.conf.fastmem_address_space_bits);
        return std::make_pair(Xfastmem, Xscratch0);
    }

    code.LSR(Xscratch0, Xaddr, ctx.conf.fastmem_address_space_bits);
    code.CBNZ(Xscratch0, *fallback);
    return std::make_pair(Xfastmem, Xaddr);
}

template<size_t bitsize>
void FastmemEmitReadMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst, DoNotFastmemMarker marker) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    auto Xaddr = ctx.reg_alloc.ReadX(args[1]);
    auto Rvalue = [&] {
        if constexpr (bitsize == 128) {
            return ctx.reg_alloc.WriteQ(inst);
        } else {
            return ctx.reg_alloc.WriteReg<std::max<std::size_t>(bitsize, 32)>(inst);
        }
    }();
    const bool ordered = IsOrdered(args[2].GetImmediateAccType());
    ctx.fpsr.Spill();
    ctx.reg_alloc.SpillFlags();
    RegAlloc::Realize(Xaddr, Rvalue);

    SharedLabel fallback = GenSharedLabel(), end = GenSharedLabel();

    const auto [Xbase, Xoffset] = FastmemEmitVAddrLookup<bitsize>(code, ctx, Xaddr, fallback);
    const auto fastmem_location = EmitMemoryLdr<bitsize>(code, Rvalue->index(), Xbase, Xoffset, ordered, ShouldExt32(ctx));

    ctx.deferred_emits.emplace_back([&code, &ctx, inst, marker, Xaddr = *Xaddr, Rvalue = *Rvalue, ordered, fallback, end, fastmem_location] {
        ctx.ebi.fastmem_patch_info.emplace(
            fastmem_location - ctx.ebi.entry_point,
            FastmemPatchInfo{
                .marker = marker,
                .fc = FakeCall{
                    .call_pc = mcl::bit_cast<u64>(code.xptr<void*>()),
                },
                .recompile = ctx.conf.recompile_on_fastmem_failure,
            });

        code.l(*fallback);
        code.MOV(Xscratch0, Xaddr);
        EmitRelocation(code, ctx, WrappedReadMemoryLinkTarget(bitsize));
        if (ordered) {
            code.DMB(oaknut::BarrierOp::ISH);
        }
        if constexpr (bitsize == 128) {
            code.MOV(Rvalue.B16(), Q0.B16());
        } else {
            code.MOV(Rvalue.toX(), Xscratch0);
        }
        ctx.conf.emit_check_memory_abort(code, ctx, inst, *end);
        code.B(*end);
    });

    code.l(*end);
}

template<size_t bitsize>
void FastmemEmitWriteMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst, DoNotFastmemMarker marker) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    auto Xaddr = ctx.reg_alloc.ReadX(args[1]);
    auto Rvalue = [&] {
        if constexpr (bitsize == 128) {
            return ctx.reg_alloc.ReadQ(args[2]);
        } else {
            return ctx.reg_alloc.ReadReg<std::max<std::size_t>(bitsize, 32)>(args[2]);
        }
    }();
    const bool ordered = IsOrdered(args[3].GetImmediateAccType());
    ctx.fpsr.Spill();
    ctx.reg_alloc.SpillFlags();
    RegAlloc::Realize(Xaddr, Rvalue);

    SharedLabel fallback = GenSharedLabel(), end = GenSharedLabel();

    const auto [Xbase, Xoffset] = FastmemEmitVAddrLookup<bitsize>(code, ctx, Xaddr, fallback);
    const auto fastmem_location = EmitMemoryStr<bitsize>(code, Rvalue->index(), Xbase, Xoffset, ordered, ShouldExt32(ctx));

    ctx.deferred_emits.emplace_back([&code, &ctx, inst, marker, Xaddr = *Xaddr, Rvalue = *Rvalue, ordered, fallback, end, fastmem_location] {
        ctx.ebi.fastmem_patch_info.emplace(
            fastmem_location - ctx.ebi.entry_point,
            FastmemPatchInfo{
                .marker = marker,
                .fc = FakeCall{
                    .call_pc = mcl::bit_cast<u64>(code.xptr<void*>()),
                },
                .recompile = ctx.conf.recompile_on_fastmem_failure,
            });

        code.l(*fallback);
        if constexpr (bitsize == 128) {
            code.MOV(Xscratch0, Xaddr);
            code.MOV(Q0.B16(), Rvalue.B16());
        } else {
            code.MOV(Xscratch0, Xaddr);
            code.MOV(Xscratch1, Rvalue.toX());
        }
        if (ordered) {
            code.DMB(oaknut::BarrierOp::ISH);
        }
        EmitRelocation(code, ctx, WrappedWriteMemoryLinkTarget(bitsize));
        if (ordered) {
            code.DMB(oaknut::BarrierOp::ISH);
        }
        ctx.conf.emit_check_memory_abort(code, ctx, inst, *end);
        code.B(*end);
    });

    code.l(*end);
}

}  // namespace

// Omnidroid patch 0007: the inline (fastmem) exclusive accesses. The pin's arm64 backend accepted
// `fastmem_exclusive_access` and ignored it: every LDXR/STXR went through the callback trampolines
// (a slow-path read *and* an exclusive-write callback per pair), which the x64 backend avoids with
// EmitExclusiveReadMemoryInline / EmitExclusiveWriteMemoryInline. These are those, on arm64, with
// the same monitor protocol, so the inline and callback paths can serve different threads of one
// monitor at once:
//
//   read:  lock; exclusive_state = 1; monitor.address[pid] = vaddr; value = [vaddr] (acquire);
//          monitor.value[pid] = value; unlock
//   write: lock; status = 1; if exclusive_state && monitor.address[pid] == vaddr:
//              compare-and-swap [vaddr]: monitor.value[pid] -> value (status = 0 on success);
//              clear every processor's reservation of vaddr;
//          exclusive_state = 0; unlock
//
// The lock is the monitor's own SpinLock word, taken with the same sequence SpinLock::Lock runs.
// The compare-and-swap is an acquire/release exclusive pair on the host (ARMv8.0; no LSE assumed),
// retried until it succeeds or the value differs. A host fault on the guest address (a fastmem miss)
// lands on a patch location whose fallback first releases the lock and then does the whole access
// through the Wrapped* trampolines -- i.e. through the monitor and the user callbacks, exactly the
// callback-only path -- and, with recompile_on_fastmem_failure, the block is rebuilt without the
// inline path.

namespace {

void EmitMonitorLock(oaknut::CodeGenerator& code, EmitContext& ctx) {
    code.MOV(Xscratch2, mcl::bit_cast<u64>(GetExclusiveMonitorLockPointer(ctx.conf.global_monitor)));
    EmitSpinLockLock(code, Xscratch2);
}

void EmitMonitorUnlock(oaknut::CodeGenerator& code, EmitContext& ctx) {
    code.MOV(Xscratch2, mcl::bit_cast<u64>(GetExclusiveMonitorLockPointer(ctx.conf.global_monitor)));
    EmitSpinLockUnlock(code, Xscratch2);
}

LinkTarget WrappedExclusiveReadMemoryLinkTarget(size_t bitsize) {
    switch (bitsize) {
    case 8:
        return LinkTarget::WrappedExclusiveReadMemory8;
    case 16:
        return LinkTarget::WrappedExclusiveReadMemory16;
    case 32:
        return LinkTarget::WrappedExclusiveReadMemory32;
    case 64:
        return LinkTarget::WrappedExclusiveReadMemory64;
    case 128:
        return LinkTarget::WrappedExclusiveReadMemory128;
    }
    UNREACHABLE();
}

LinkTarget WrappedExclusiveWriteMemoryLinkTarget(size_t bitsize) {
    switch (bitsize) {
    case 8:
        return LinkTarget::WrappedExclusiveWriteMemory8;
    case 16:
        return LinkTarget::WrappedExclusiveWriteMemory16;
    case 32:
        return LinkTarget::WrappedExclusiveWriteMemory32;
    case 64:
        return LinkTarget::WrappedExclusiveWriteMemory64;
    case 128:
        return LinkTarget::WrappedExclusiveWriteMemory128;
    }
    UNREACHABLE();
}

// Xdest = host address of the guest access. Uses Xscratch0 (via FastmemEmitVAddrLookup).
template<size_t bitsize>
void EmitExclusiveHostAddress(oaknut::CodeGenerator& code, EmitContext& ctx, oaknut::XReg Xdest, oaknut::XReg Xaddr, const SharedLabel& fallback) {
    const auto [Xbase, Xoffset] = FastmemEmitVAddrLookup<bitsize>(code, ctx, Xaddr, fallback);
    if (ShouldExt32(ctx)) {
        code.ADD(Xdest, Xbase, Xoffset.toW(), oaknut::AddSubExt::UXTW);
    } else {
        code.ADD(Xdest, Xbase, Xoffset);
    }
}

template<size_t bitsize>
void FastmemEmitExclusiveReadMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst, DoNotFastmemMarker marker) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    auto Xaddr = ctx.reg_alloc.ReadX(args[1]);
    auto Rvalue = [&] {
        if constexpr (bitsize == 128) {
            return ctx.reg_alloc.WriteQ(inst);
        } else {
            return ctx.reg_alloc.WriteReg<std::max<std::size_t>(bitsize, 32)>(inst);
        }
    }();
    ctx.fpsr.Spill();
    ctx.reg_alloc.SpillFlags();
    RegAlloc::Realize(Xaddr, Rvalue);

    SharedLabel fallback = GenSharedLabel(), end = GenSharedLabel();

    EmitMonitorLock(code, ctx);
    code.MOV(Wscratch0, 1);
    code.STRB(Wscratch0, Xstate, ctx.conf.state_exclusive_state_offset);
    code.MOV(Xscratch0, mcl::bit_cast<u64>(GetExclusiveMonitorAddressPointer(ctx.conf.global_monitor, ctx.conf.processor_id)));
    code.STR(*Xaddr, Xscratch0);

    EmitExclusiveHostAddress<bitsize>(code, ctx, Xscratch1, *Xaddr, fallback);
    const CodePtr fastmem_location = EmitMemoryLdr<bitsize>(code, Rvalue->index(), Xscratch1, XZR, true);

    code.MOV(Xscratch0, mcl::bit_cast<u64>(GetExclusiveMonitorValuePointer(ctx.conf.global_monitor, ctx.conf.processor_id)));
    switch (bitsize) {
    case 8:
        code.STRB(oaknut::WReg{Rvalue->index()}, Xscratch0);
        break;
    case 16:
        code.STRH(oaknut::WReg{Rvalue->index()}, Xscratch0);
        break;
    case 32:
        code.STR(oaknut::WReg{Rvalue->index()}, Xscratch0);
        break;
    case 64:
        code.STR(oaknut::XReg{Rvalue->index()}, Xscratch0);
        break;
    case 128:
        code.STR(oaknut::QReg{Rvalue->index()}, Xscratch0);
        break;
    }
    EmitMonitorUnlock(code, ctx);

    ctx.deferred_emits.emplace_back([&code, &ctx, inst, marker, Xaddr = *Xaddr, Rvalue = *Rvalue, fallback, end, fastmem_location] {
        ctx.ebi.fastmem_patch_info.emplace(
            fastmem_location - ctx.ebi.entry_point,
            FastmemPatchInfo{
                .marker = marker,
                .fc = FakeCall{
                    .call_pc = mcl::bit_cast<u64>(code.xptr<void*>()),
                },
                .recompile = ctx.conf.recompile_on_fastmem_failure,
            });

        code.l(*fallback);
        EmitMonitorUnlock(code, ctx);
        code.MOV(Xscratch0, Xaddr);
        EmitRelocation(code, ctx, WrappedExclusiveReadMemoryLinkTarget(bitsize));
        if constexpr (bitsize == 128) {
            code.MOV(Rvalue.B16(), Q0.B16());
        } else {
            code.MOV(Rvalue.toX(), Xscratch0);
        }
        ctx.conf.emit_check_memory_abort(code, ctx, inst, *end);
        code.B(*end);
    });

    code.l(*end);
}

template<size_t bitsize>
void FastmemEmitExclusiveWriteMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst, DoNotFastmemMarker marker) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    auto Xaddr = ctx.reg_alloc.ReadX(args[1]);
    auto Rvalue = [&] {
        if constexpr (bitsize == 128) {
            return ctx.reg_alloc.ReadQ(args[2]);
        } else {
            return ctx.reg_alloc.ReadReg<std::max<std::size_t>(bitsize, 32)>(args[2]);
        }
    }();
    auto Wstatus = ctx.reg_alloc.WriteW(inst);
    ctx.fpsr.Spill();
    ctx.reg_alloc.SpillFlags();
    RegAlloc::Realize(Xaddr, Rvalue, Wstatus);

    SharedLabel fallback = GenSharedLabel(), end = GenSharedLabel();
    oaknut::Label no_reservation, retry, cas_failed, cas_done, clear_loop, clear_next;

    // 128 bits needs four more general-purpose temporaries than Xscratch0-2 and Wstatus provide:
    // borrow four that hold neither operand, on the stack for the length of the sequence.
    std::array<oaknut::XReg, 4> borrowed{X0, X1, X2, X3};
    if constexpr (bitsize == 128) {
        size_t n = 0;
        for (int i = 0; i < 16 && n < borrowed.size(); i++) {
            if (i != Xaddr->index() && i != Wstatus->index()) {
                borrowed[n++] = oaknut::XReg{i};
            }
        }
    }
    const auto borrow = [&] {
        code.STP(borrowed[0], borrowed[1], SP, oaknut::PreIndexed{}, -32);
        code.STP(borrowed[2], borrowed[3], SP, 16);
    };
    const auto give_back = [&] {
        code.LDP(borrowed[2], borrowed[3], SP, 16);
        code.LDP(borrowed[0], borrowed[1], SP, oaknut::PostIndexed{}, 32);
    };

    EmitMonitorLock(code, ctx);
    code.MOV(*Wstatus, 1);
    code.LDRB(Wscratch0, Xstate, ctx.conf.state_exclusive_state_offset);
    code.CBZ(Wscratch0, no_reservation);
    code.MOV(Xscratch0, mcl::bit_cast<u64>(GetExclusiveMonitorAddressPointer(ctx.conf.global_monitor, ctx.conf.processor_id)));
    code.LDR(Xscratch0, Xscratch0);
    code.CMP(Xscratch0, *Xaddr);
    code.B(NE, no_reservation);

    // Xscratch2 = host address (the lock pointer is no longer needed; unlock reloads it).
    EmitExclusiveHostAddress<bitsize>(code, ctx, Xscratch2, *Xaddr, fallback);
    code.MOV(Xscratch0, mcl::bit_cast<u64>(GetExclusiveMonitorValuePointer(ctx.conf.global_monitor, ctx.conf.processor_id)));

    CodePtr fastmem_location;
    // Omnidroid patch 0014: the store-release faults too. A page the host lets the load-acquire read
    // but not the store write (read-only: a sealed relro page) faults *here*, not at the load.
    CodePtr store_location;
    if constexpr (bitsize == 128) {
        const auto [Xval_lo, Xval_hi, Xld_lo, Xld_hi] = borrowed;
        borrow();
        code.FMOV(Xval_lo, Rvalue->toD());
        code.FMOV(Xval_hi, Rvalue->Delem()[1]);
        code.LDP(Xscratch0, Xscratch1, Xscratch0);  // expected lo, hi
        code.l(retry);
        fastmem_location = code.xptr<CodePtr>();
        code.LDAXP(Xld_lo, Xld_hi, Xscratch2);
        code.CMP(Xld_lo, Xscratch0);
        code.CCMP(Xld_hi, Xscratch1, 0, EQ);
        code.B(NE, cas_failed);
        store_location = code.xptr<CodePtr>();
        code.STLXP(*Wstatus, Xval_lo, Xval_hi, Xscratch2);
        code.CBNZ(*Wstatus, retry);
        code.B(cas_done);
        code.l(cas_failed);
        code.CLREX();
        code.MOV(*Wstatus, 1);
        code.l(cas_done);
        give_back();
    } else {
        switch (bitsize) {
        case 8:
            code.LDRB(Wscratch1, Xscratch0);
            break;
        case 16:
            code.LDRH(Wscratch1, Xscratch0);
            break;
        case 32:
            code.LDR(Wscratch1, Xscratch0);
            break;
        case 64:
            code.LDR(Xscratch1, Xscratch0);
            break;
        }
        code.l(retry);
        fastmem_location = code.xptr<CodePtr>();
        switch (bitsize) {
        case 8:
            code.LDAXRB(Wscratch0, Xscratch2);
            code.CMP(Wscratch0, Wscratch1);
            break;
        case 16:
            code.LDAXRH(Wscratch0, Xscratch2);
            code.CMP(Wscratch0, Wscratch1);
            break;
        case 32:
            code.LDAXR(Wscratch0, Xscratch2);
            code.CMP(Wscratch0, Wscratch1);
            break;
        case 64:
            code.LDAXR(Xscratch0, Xscratch2);
            code.CMP(Xscratch0, Xscratch1);
            break;
        }
        code.B(NE, cas_failed);
        store_location = code.xptr<CodePtr>();
        switch (bitsize) {
        case 8:
            code.STLXRB(*Wstatus, oaknut::WReg{Rvalue->index()}, Xscratch2);
            break;
        case 16:
            code.STLXRH(*Wstatus, oaknut::WReg{Rvalue->index()}, Xscratch2);
            break;
        case 32:
            code.STLXR(*Wstatus, oaknut::WReg{Rvalue->index()}, Xscratch2);
            break;
        case 64:
            code.STLXR(*Wstatus, oaknut::XReg{Rvalue->index()}, Xscratch2);
            break;
        }
        code.CBNZ(*Wstatus, retry);
        code.B(cas_done);
        code.l(cas_failed);
        code.CLREX();
        code.MOV(*Wstatus, 1);
        code.l(cas_done);
    }

    // Whatever the compare-and-swap decided, the reservation is spent: clear every processor's
    // reservation of this address (the monitor's CheckAndClear), this one's included.
    {
        const u64 first = mcl::bit_cast<u64>(GetExclusiveMonitorAddressPointer(ctx.conf.global_monitor, 0));
        const u64 count = GetExclusiveMonitorProcessorCount(ctx.conf.global_monitor);
        code.MOV(Xscratch0, first);
        code.MOV(Xscratch1, first + count * sizeof(VAddr));
        code.l(clear_loop);
        code.LDR(Xscratch2, Xscratch0);
        code.CMP(Xscratch2, *Xaddr);
        code.B(NE, clear_next);
        code.MOV(Xscratch2, 0xDEAD'DEAD'DEAD'DEADull);
        code.STR(Xscratch2, Xscratch0);
        code.l(clear_next);
        code.ADD(Xscratch0, Xscratch0, sizeof(VAddr));
        code.CMP(Xscratch0, Xscratch1);
        code.B(LO, clear_loop);
    }

    // A store-exclusive leaves the local monitor open whether or not it stored.
    code.l(no_reservation);
    code.STRB(WZR, Xstate, ctx.conf.state_exclusive_state_offset);
    EmitMonitorUnlock(code, ctx);

    ctx.deferred_emits.emplace_back([&code, &ctx, inst, marker, Xaddr = *Xaddr, Rvalue = *Rvalue, Wstatus = *Wstatus, fallback, end, fastmem_location, store_location, borrowed] {
        // The patch location is the load-acquire of the compare-and-swap. For 128 bits the borrowed
        // registers are on the stack at that point and are given back first.
        const u64 fault_entry = mcl::bit_cast<u64>(code.xptr<void*>());
        if constexpr (bitsize == 128) {
            code.LDP(borrowed[2], borrowed[3], SP, 16);
            code.LDP(borrowed[0], borrowed[1], SP, oaknut::PostIndexed{}, 32);
        }
        ctx.ebi.fastmem_patch_info.emplace(
            fastmem_location - ctx.ebi.entry_point,
            FastmemPatchInfo{
                .marker = marker,
                .fc = FakeCall{
                    .call_pc = fault_entry,
                },
                .recompile = ctx.conf.recompile_on_fastmem_failure,
            });
        // Patch 0014: the store-release, with the same entry -- at it the lock is held and, for 128
        // bits, the borrowed registers are on the stack, exactly as at the load-acquire.
        ctx.ebi.fastmem_patch_info.emplace(
            store_location - ctx.ebi.entry_point,
            FastmemPatchInfo{
                .marker = marker,
                .fc = FakeCall{
                    .call_pc = fault_entry,
                },
                .recompile = ctx.conf.recompile_on_fastmem_failure,
            });

        code.l(*fallback);
        EmitMonitorUnlock(code, ctx);
        code.STRB(WZR, Xstate, ctx.conf.state_exclusive_state_offset);
        code.MOV(Xscratch0, Xaddr);
        if constexpr (bitsize == 128) {
            code.MOV(Q0.B16(), Rvalue.B16());
        } else {
            code.MOV(Xscratch1, Rvalue.toX());
        }
        EmitRelocation(code, ctx, WrappedExclusiveWriteMemoryLinkTarget(bitsize));
        code.MOV(Wstatus, Wscratch0);
        ctx.conf.emit_check_memory_abort(code, ctx, inst, *end);
        code.B(*end);
    });

    code.l(*end);
}

}  // namespace

template<size_t bitsize>
void EmitReadMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    if (const auto marker = ShouldFastmem(ctx, inst)) {
        FastmemEmitReadMemory<bitsize>(code, ctx, inst, *marker);
    } else if (ctx.conf.page_table_pointer != 0) {
        InlinePageTableEmitReadMemory<bitsize>(code, ctx, inst);
    } else {
        CallbackOnlyEmitReadMemory<bitsize>(code, ctx, inst);
    }
}

template<size_t bitsize>
void EmitExclusiveReadMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    // Omnidroid patch 0007.
    if (ctx.conf.fastmem_exclusive_access && ctx.conf.global_monitor) {
        if (const auto marker = ShouldFastmem(ctx, inst)) {
            FastmemEmitExclusiveReadMemory<bitsize>(code, ctx, inst, *marker);
            return;
        }
    }
    CallbackOnlyEmitExclusiveReadMemory<bitsize>(code, ctx, inst);
}

template<size_t bitsize>
void EmitWriteMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    if (const auto marker = ShouldFastmem(ctx, inst)) {
        FastmemEmitWriteMemory<bitsize>(code, ctx, inst, *marker);
    } else if (ctx.conf.page_table_pointer != 0) {
        InlinePageTableEmitWriteMemory<bitsize>(code, ctx, inst);
    } else {
        CallbackOnlyEmitWriteMemory<bitsize>(code, ctx, inst);
    }
}

template<size_t bitsize>
void EmitExclusiveWriteMemory(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    // Omnidroid patch 0007.
    if (ctx.conf.fastmem_exclusive_access && ctx.conf.global_monitor) {
        if (const auto marker = ShouldFastmem(ctx, inst)) {
            FastmemEmitExclusiveWriteMemory<bitsize>(code, ctx, inst, *marker);
            return;
        }
    }
    CallbackOnlyEmitExclusiveWriteMemory<bitsize>(code, ctx, inst);
}

template void EmitReadMemory<8>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitReadMemory<16>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitReadMemory<32>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitReadMemory<64>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitReadMemory<128>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveReadMemory<8>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveReadMemory<16>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveReadMemory<32>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveReadMemory<64>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveReadMemory<128>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitWriteMemory<8>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitWriteMemory<16>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitWriteMemory<32>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitWriteMemory<64>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitWriteMemory<128>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveWriteMemory<8>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveWriteMemory<16>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveWriteMemory<32>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveWriteMemory<64>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);
template void EmitExclusiveWriteMemory<128>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst);

}  // namespace Dynarmic::Backend::Arm64
