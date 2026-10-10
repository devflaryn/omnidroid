/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include "dynarmic/backend/x64/emit_x64.h"

#include <algorithm>
#include <atomic>
#include <iterator>
#include <limits>
#include <utility>

#include <mcl/assert.hpp>
#include <mcl/bit/bit_field.hpp>
#include <mcl/scope_exit.hpp>
#include <mcl/stdint.hpp>
#include <tsl/robin_set.h>

#include "dynarmic/backend/x64/block_of_code.h"
#include "dynarmic/backend/x64/nzcv_util.h"
#include "dynarmic/backend/x64/perf_map.h"
#include "dynarmic/backend/x64/stack_layout.h"
#include "dynarmic/backend/x64/verbose_debugging_output.h"
#include "dynarmic/common/variant_util.h"
#include "dynarmic/ir/basic_block.h"
#include "dynarmic/ir/microinstruction.h"
#include "dynarmic/ir/opcodes.h"

// TODO: Have ARM flags in host flags and not have them use up GPR registers unless necessary.
// TODO: Actually implement that proper instruction selector you've always wanted to sweetheart.

namespace Dynarmic::Backend::X64 {

using namespace Xbyak::util;

EmitContext::EmitContext(RegAlloc& reg_alloc, IR::Block& block)
        : reg_alloc(reg_alloc), block(block) {}

EmitContext::~EmitContext() = default;

void EmitContext::EraseInstruction(IR::Inst* inst) {
    block.Instructions().erase(inst);
    inst->ClearArgs();
}

EmitX64::EmitX64(BlockOfCode& code)
        : code(code) {
    exception_handler.Register(code);
    UseLoadFactor(link_heads, LINK_HEADS_LOAD_FACTOR);
}

EmitX64::~EmitX64() = default;

std::optional<EmitX64::BlockDescriptor> EmitX64::GetBasicBlock(IR::LocationDescriptor descriptor) const {
    const auto iter = block_descriptors.find(Key64{descriptor});
    if (iter == block_descriptors.end() || (iter->second.size & UNVERIFIED_BLOCK) != 0) {
        return std::nullopt;
    }
    return Load(iter->second);
}

std::optional<std::pair<EmitX64::BlockDescriptor, bool>> EmitX64::GetAnyBlock(IR::LocationDescriptor location) const {
    const auto iter = block_descriptors.find(Key64{location});
    if (iter == block_descriptors.end()) {
        return std::nullopt;
    }
    return std::make_pair(Load(iter->second), (iter->second.size & UNVERIFIED_BLOCK) == 0);
}

void EmitX64::SnapshotSlotsOf(u32 first_link, std::vector<SnapshotSlot>& out) const {
    if (first_link == NO_LINK) {
        return;
    }
    for (u32 i = first_link;; i++) {
        const LinkRecord& link = LinkAt(i);
        out.push_back(SnapshotSlot{link.slot & ~LAST_LINK_OF_BLOCK, link.unlinked, link.target});
        if (link.slot & LAST_LINK_OF_BLOCK) {
            break;
        }
    }
}

void EmitX64::SnapshotSitesIn(const u8* begin, const u8* end, std::vector<SnapshotSite>& out) const {
    const u64 buffer = reinterpret_cast<u64>(code.getCode());
    for (const FastmemSiteRun& run : fastmem_site_runs) {
        if (begin < run.begin || begin >= run.end) {
            continue;
        }
        const u64 base = reinterpret_cast<u64>(run.begin);
        const u32 from = static_cast<u32>(reinterpret_cast<u64>(begin) - base);
        const u32 to = static_cast<u32>(reinterpret_cast<u64>(end) - base);
        auto it = std::lower_bound(run.sites.begin(), run.sites.end(), from, [](const FastmemSite& s, u32 o) { return s.site < o; });
        auto w = std::lower_bound(run.wide.begin(), run.wide.end(), from, [](const WideFastmemSite& s, u32 o) { return s.site < o; });
        // Merged in address order, as the run would answer for them.
        for (;;) {
            const bool have_compact = it != run.sites.end() && it->site < to;
            const bool have_wide = w != run.wide.end() && w->site < to;
            if (!have_compact && !have_wide) {
                break;
            }
            if (have_compact && (!have_wide || it->site <= w->site)) {
                const u64 site = base + it->site;
                out.push_back(SnapshotSite{static_cast<u32>(site - buffer), static_cast<u32>(site + it->resume_delta - buffer),
                                           static_cast<u32>(fastmem_callbacks[it->callback] - buffer)});
                ++it;
            } else {
                out.push_back(SnapshotSite{static_cast<u32>(base + w->site - buffer), static_cast<u32>(base + w->resume - buffer),
                                           static_cast<u32>(w->callback - buffer)});
                ++w;
            }
        }
        return;
    }
}

void EmitX64::RestoreBlock(IR::LocationDescriptor location, u32 entry, u32 size, const SnapshotSlot* slots, size_t slot_count, const SnapshotSite* sites, size_t site_count, bool lazily) {
    ASSERT(shared_code && (size & UNVERIFIED_BLOCK) == 0);
    u8* const buffer = const_cast<u8*>(code.getCode());
    u32 first = NO_LINK;
    if (slot_count != 0) {
        ASSERT(static_cast<u64>(NextLinkSerial()) + slot_count < NO_LINK);
        first = NextLinkSerial();
        for (size_t i = 0; i < slot_count; i++) {
            const SnapshotSlot& s = slots[i];
            // Unlinked, whatever it linked to when saved: nothing restored is entered unverified.
            // (Patch 0075: lazily, nothing is there yet; set when the page is read in.)
            if (!lazily) {
                *reinterpret_cast<u64*>(buffer + s.slot) = reinterpret_cast<u64>(buffer + s.unlinked);
            }
            const u32 index = NextLinkSerial();
            LinkRecord record{s.target, s.slot, s.unlinked, NO_LINK, NO_LINK};
            // Patch 0064: the head beside the target's block when it has one (restored before
            // this one), else in `link_heads`.
            u32* const head = HeadOf(s.target, true);
            if (*head != NO_LINK) {
                record.next = *head;
                LinkAt(*head).prev = index;
            }
            *head = index;
            link_records.push_back(record);
        }
        link_records.back().slot |= LAST_LINK_OF_BLOCK;
    }
    for (size_t i = 0; i < site_count; i++) {
        const SnapshotSite& s = sites[i];
        RecordSharedFastmemSite(reinterpret_cast<u64>(buffer + s.site), reinterpret_cast<u64>(buffer + s.resume), reinterpret_cast<u64>(buffer + s.callback));
    }
    CommitSharedFastmemSites();
    const auto [stored, inserted] = block_descriptors.insert({Key64{location}, StoredBlock{entry, size | UNVERIFIED_BLOCK, first}});
    // Patch 0064: the links already waiting for this location move their head beside the block.
    if (inserted) {
        if (const auto waiting = link_heads.find(Key64{location.Value()}); waiting != link_heads.end()) {
            stored.value().head = waiting->second;
            link_heads.erase(waiting);
        }
    }
}

void EmitX64::MarkVerified(IR::LocationDescriptor location) {
    const auto iter = block_descriptors.find(Key64{location});
    ASSERT(iter != block_descriptors.end() && (iter->second.size & UNVERIFIED_BLOCK) != 0);
    StoredBlock stored = iter->second;
    stored.size &= ~UNVERIFIED_BLOCK;
    iter.value() = stored;
    const BlockDescriptor block = Load(stored);
    // Its own links: to the targets that are entered now.
    if (stored.first_link != NO_LINK) {
        for (u32 i = stored.first_link;; i++) {
            const LinkRecord& link = LinkAt(i);
            if (const auto target = GetBasicBlock(IR::LocationDescriptor{link.target})) {
                StoreSlot(link, reinterpret_cast<u64>(target->entrypoint));
            }
            if (link.slot & LAST_LINK_OF_BLOCK) {
                break;
            }
        }
    }
    // And the links to it.
    Patch(location, block.entrypoint);
}

void EmitX64::RefreshSlot(u32 serial) {
    const LinkRecord& link = LinkAt(serial);
    const auto target = GetBasicBlock(IR::LocationDescriptor{link.target});
    std::atomic_ref<u64>{*LinkSlotOf(link)}.store(target ? reinterpret_cast<u64>(target->entrypoint) : LinkUnlinkedOf(link), std::memory_order_release);
}

EmitX64::StoredBlock EmitX64::Store(const BlockDescriptor& b) const {
    const u64 offset = reinterpret_cast<u64>(b.entrypoint) - reinterpret_cast<u64>(code.getCode());
    ASSERT(reinterpret_cast<u64>(b.entrypoint) >= reinterpret_cast<u64>(code.getCode()) && offset <= std::numeric_limits<u32>::max());
    return StoredBlock{static_cast<u32>(offset), b.size, b.first_link};
}

u32* EmitX64::HeadOf(u64 target, bool make) {
    if (const auto it = block_descriptors.find(Key64{target}); it != block_descriptors.end()) {
        if (it->second.head == NO_LINK && !make) {
            return nullptr;
        }
        return &it.value().head;
    }
    if (make) {
        return &link_heads.try_emplace(Key64{target}, NO_LINK).first.value();
    }
    const auto head = link_heads.find(Key64{target});
    return head == link_heads.end() ? nullptr : &head.value();
}

void EmitX64::KeepHeadOf(const Key64& key, const StoredBlock& block) {
    if (block.head != NO_LINK) {
        link_heads.insert_or_assign(key, block.head);
    }
}

EmitX64::BlockDescriptor EmitX64::Load(const StoredBlock& s) const {
    return BlockDescriptor{reinterpret_cast<CodePtr>(code.getCode() + s.entry), s.size & ~UNVERIFIED_BLOCK, s.first_link};  // patch 0070
}

void EmitX64::EmitVoid(EmitContext&, IR::Inst*) {
}

void EmitX64::EmitIdentity(EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    if (!args[0].IsImmediate()) {
        ctx.reg_alloc.DefineValue(inst, args[0]);
    }
}

void EmitX64::EmitBreakpoint(EmitContext&, IR::Inst*) {
    code.int3();
}

void EmitX64::EmitCallHostFunction(EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ctx.reg_alloc.HostCall(nullptr, args[1], args[2], args[3]);
    code.mov(rax, args[0].GetImmediateU64());
    code.call(rax);
}

void EmitX64::PushRSBHelper(Xbyak::Reg64 loc_desc_reg, Xbyak::Reg64 index_reg, IR::LocationDescriptor target) {
    using namespace Xbyak::util;

    const auto block = GetBasicBlock(target);  // patch 0070: not an unverified one
    CodePtr target_code_ptr = block ? block->entrypoint : code.GetReturnFromRunCodeAddress();

    code.mov(index_reg.cvt32(), dword[r15 + code.GetJitStateInfo().offsetof_rsb_ptr]);

    code.mov(loc_desc_reg, target.Value());

    if (shared_code) {
        // Omnidroid patch 0022: the code pointer comes from a slot, never from a rewritten
        // immediate. Unlinked, it is the dispatcher -- what upstream's patch site holds then.
        Xbyak::Label& slot = NewLinkSlot(target, reinterpret_cast<u64>(code.GetReturnFromRunCodeAddress()));
        code.mov(rcx, qword[rip + slot]);
    } else {
        patch_information[target].mov_rcx.push_back(code.getCurr());
        EmitPatchMovRcx(target_code_ptr);
    }

    code.mov(qword[r15 + index_reg * 8 + code.GetJitStateInfo().offsetof_rsb_location_descriptors], loc_desc_reg);
    code.mov(qword[r15 + index_reg * 8 + code.GetJitStateInfo().offsetof_rsb_codeptrs], rcx);

    code.add(index_reg.cvt32(), 1);
    code.and_(index_reg.cvt32(), u32(code.GetJitStateInfo().rsb_ptr_mask));
    code.mov(dword[r15 + code.GetJitStateInfo().offsetof_rsb_ptr], index_reg.cvt32());
}

void EmitX64::EmitVerboseDebuggingOutput(RegAlloc& reg_alloc) {
    code.sub(rsp, sizeof(RegisterData));
    code.stmxcsr(dword[rsp + offsetof(RegisterData, mxcsr)]);
    for (int i = 0; i < 16; i++) {
        if (rsp.getIdx() == i) {
            continue;
        }
        code.mov(qword[rsp + offsetof(RegisterData, gprs) + sizeof(u64) * i], Xbyak::Reg64{i});
    }
    for (int i = 0; i < 16; i++) {
        code.movaps(xword[rsp + offsetof(RegisterData, xmms) + 2 * sizeof(u64) * i], Xbyak::Xmm{i});
    }
    code.lea(rax, ptr[rsp + sizeof(RegisterData) + offsetof(StackLayout, spill)]);
    code.mov(xword[rsp + offsetof(RegisterData, spill)], rax);

    reg_alloc.EmitVerboseDebuggingOutput();

    for (int i = 0; i < 16; i++) {
        if (rsp.getIdx() == i) {
            continue;
        }
        code.mov(Xbyak::Reg64{i}, qword[rsp + offsetof(RegisterData, gprs) + sizeof(u64) * i]);
    }
    for (int i = 0; i < 16; i++) {
        code.movaps(Xbyak::Xmm{i}, xword[rsp + offsetof(RegisterData, xmms) + 2 * sizeof(u64) * i]);
    }
    code.ldmxcsr(dword[rsp + offsetof(RegisterData, mxcsr)]);
    code.add(rsp, sizeof(RegisterData));
}

void EmitX64::EmitPushRSB(EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);
    ASSERT(args[0].IsImmediate());
    const u64 unique_hash_of_target = args[0].GetImmediateU64();

    ctx.reg_alloc.ScratchGpr(HostLoc::RCX);
    const Xbyak::Reg64 loc_desc_reg = ctx.reg_alloc.ScratchGpr();
    const Xbyak::Reg64 index_reg = ctx.reg_alloc.ScratchGpr();

    PushRSBHelper(loc_desc_reg, index_reg, IR::LocationDescriptor{unique_hash_of_target});
}

void EmitX64::EmitGetCarryFromOp(EmitContext& ctx, IR::Inst* inst) {
    ctx.reg_alloc.RegisterPseudoOperation(inst);
}

void EmitX64::EmitGetOverflowFromOp(EmitContext& ctx, IR::Inst* inst) {
    ctx.reg_alloc.RegisterPseudoOperation(inst);
}

void EmitX64::EmitGetGEFromOp(EmitContext& ctx, IR::Inst* inst) {
    ctx.reg_alloc.RegisterPseudoOperation(inst);
}

void EmitX64::EmitGetUpperFromOp(EmitContext& ctx, IR::Inst* inst) {
    ctx.reg_alloc.RegisterPseudoOperation(inst);
}

void EmitX64::EmitGetLowerFromOp(EmitContext& ctx, IR::Inst* inst) {
    ctx.reg_alloc.RegisterPseudoOperation(inst);
}

void EmitX64::EmitGetNZFromOp(EmitContext& ctx, IR::Inst* inst) {
    if (ctx.reg_alloc.IsValueLive(inst)) {
        ctx.reg_alloc.RegisterPseudoOperation(inst);
        return;
    }

    auto args = ctx.reg_alloc.GetArgumentInfo(inst);

    const int bitsize = [&] {
        switch (args[0].GetType()) {
        case IR::Type::U8:
            return 8;
        case IR::Type::U16:
            return 16;
        case IR::Type::U32:
            return 32;
        case IR::Type::U64:
            return 64;
        default:
            UNREACHABLE();
        }
    }();

    const Xbyak::Reg64 nz = ctx.reg_alloc.ScratchGpr(HostLoc::RAX);
    const Xbyak::Reg value = ctx.reg_alloc.UseGpr(args[0]).changeBit(bitsize);
    code.test(value, value);
    code.lahf();
    code.movzx(eax, ah);
    ctx.reg_alloc.DefineValue(inst, nz);
}

void EmitX64::EmitGetNZCVFromOp(EmitContext& ctx, IR::Inst* inst) {
    if (ctx.reg_alloc.IsValueLive(inst)) {
        ctx.reg_alloc.RegisterPseudoOperation(inst);
        return;
    }

    auto args = ctx.reg_alloc.GetArgumentInfo(inst);

    const int bitsize = [&] {
        switch (args[0].GetType()) {
        case IR::Type::U8:
            return 8;
        case IR::Type::U16:
            return 16;
        case IR::Type::U32:
            return 32;
        case IR::Type::U64:
            return 64;
        default:
            UNREACHABLE();
        }
    }();

    const Xbyak::Reg64 nzcv = ctx.reg_alloc.ScratchGpr(HostLoc::RAX);
    const Xbyak::Reg value = ctx.reg_alloc.UseGpr(args[0]).changeBit(bitsize);
    code.test(value, value);
    code.lahf();
    code.mov(al, 0);
    ctx.reg_alloc.DefineValue(inst, nzcv);
}

void EmitX64::EmitGetCFlagFromNZCV(EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);

    if (args[0].IsImmediate()) {
        const Xbyak::Reg32 result = ctx.reg_alloc.ScratchGpr().cvt32();
        const u32 value = (args[0].GetImmediateU32() >> 8) & 1;
        code.mov(result, value);
        ctx.reg_alloc.DefineValue(inst, result);
    } else {
        const Xbyak::Reg32 result = ctx.reg_alloc.UseScratchGpr(args[0]).cvt32();
        code.shr(result, 8);
        code.and_(result, 1);
        ctx.reg_alloc.DefineValue(inst, result);
    }
}

void EmitX64::EmitNZCVFromPackedFlags(EmitContext& ctx, IR::Inst* inst) {
    auto args = ctx.reg_alloc.GetArgumentInfo(inst);

    if (args[0].IsImmediate()) {
        const Xbyak::Reg32 nzcv = ctx.reg_alloc.ScratchGpr().cvt32();
        u32 value = 0;
        value |= mcl::bit::get_bit<31>(args[0].GetImmediateU32()) ? (1 << 15) : 0;
        value |= mcl::bit::get_bit<30>(args[0].GetImmediateU32()) ? (1 << 14) : 0;
        value |= mcl::bit::get_bit<29>(args[0].GetImmediateU32()) ? (1 << 8) : 0;
        value |= mcl::bit::get_bit<28>(args[0].GetImmediateU32()) ? (1 << 0) : 0;
        code.mov(nzcv, value);
        ctx.reg_alloc.DefineValue(inst, nzcv);
    } else if (code.HasHostFeature(HostFeature::FastBMI2)) {
        const Xbyak::Reg32 nzcv = ctx.reg_alloc.UseScratchGpr(args[0]).cvt32();
        const Xbyak::Reg32 tmp = ctx.reg_alloc.ScratchGpr().cvt32();

        code.shr(nzcv, 28);
        code.mov(tmp, NZCV::x64_mask);
        code.pdep(nzcv, nzcv, tmp);

        ctx.reg_alloc.DefineValue(inst, nzcv);
    } else {
        const Xbyak::Reg32 nzcv = ctx.reg_alloc.UseScratchGpr(args[0]).cvt32();

        code.shr(nzcv, 28);
        code.imul(nzcv, nzcv, NZCV::to_x64_multiplier);
        code.and_(nzcv, NZCV::x64_mask);

        ctx.reg_alloc.DefineValue(inst, nzcv);
    }
}

void EmitX64::EmitAddCycles(size_t cycles) {
    ASSERT(cycles < std::numeric_limits<s32>::max());
    code.sub(qword[rsp + ABI_SHADOW_SPACE + offsetof(StackLayout, cycles_remaining)], static_cast<u32>(cycles));
}

Xbyak::Label EmitX64::EmitCond(IR::Cond cond) {
    Xbyak::Label pass;

    code.mov(eax, dword[r15 + code.GetJitStateInfo().offsetof_cpsr_nzcv]);

    code.LoadRequiredFlagsForCondFromRax(cond);

    switch (cond) {
    case IR::Cond::EQ:
        code.jz(pass);
        break;
    case IR::Cond::NE:
        code.jnz(pass);
        break;
    case IR::Cond::CS:
        code.jc(pass);
        break;
    case IR::Cond::CC:
        code.jnc(pass);
        break;
    case IR::Cond::MI:
        code.js(pass);
        break;
    case IR::Cond::PL:
        code.jns(pass);
        break;
    case IR::Cond::VS:
        code.jo(pass);
        break;
    case IR::Cond::VC:
        code.jno(pass);
        break;
    case IR::Cond::HI:
        code.ja(pass);
        break;
    case IR::Cond::LS:
        code.jna(pass);
        break;
    case IR::Cond::GE:
        code.jge(pass);
        break;
    case IR::Cond::LT:
        code.jl(pass);
        break;
    case IR::Cond::GT:
        code.jg(pass);
        break;
    case IR::Cond::LE:
        code.jle(pass);
        break;
    default:
        ASSERT_MSG(false, "Unknown cond {}", static_cast<size_t>(cond));
        break;
    }

    return pass;
}

EmitX64::BlockDescriptor EmitX64::RegisterBlock(const IR::LocationDescriptor& descriptor, CodePtr entrypoint, size_t size) {
    if (PerfMapEnabled()) {  // patch 0094: the name (a formatted string) only for a perf map
        PerfMapRegister(entrypoint, code.getCurr(), LocationDescriptorToFriendlyName(descriptor));
    }
    Patch(descriptor, entrypoint);

    ASSERT(size <= std::numeric_limits<u32>::max());
    BlockDescriptor block_desc{entrypoint, static_cast<u32>(size)};
    // Omnidroid patch 0025: the block's link records, which EmitPendingSlots has just made.
    block_desc.first_link = std::exchange(pending_first_link, NO_LINK);
    const auto [stored, inserted] = block_descriptors.insert({Key64{descriptor.Value()}, Store(block_desc)});
    // Patch 0064: the links already waiting for this location move their head beside the block.
    if (inserted) {
        if (const auto waiting = link_heads.find(Key64{descriptor.Value()}); waiting != link_heads.end()) {
            stored.value().head = waiting->second;
            link_heads.erase(waiting);
        }
    }
    return block_desc;
}

void EmitX64::EmitTerminal(IR::Terminal terminal, IR::LocationDescriptor initial_location, bool is_single_step) {
    Common::VisitVariant<void>(terminal, [this, initial_location, is_single_step](auto x) {
        using T = std::decay_t<decltype(x)>;
        if constexpr (!std::is_same_v<T, IR::Term::Invalid>) {
            this->EmitTerminalImpl(x, initial_location, is_single_step);
        } else {
            ASSERT_MSG(false, "Invalid terminal");
        }
    });
}

void EmitX64::Patch(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr) {
    if (shared_code) {
        // Omnidroid patch 0022: a shared cache's links. One aligned 8-byte store each: a thread
        // loading the slot sees the old target or the new one, and both are code. Patch 0025:
        // they are the records listed from the target's head -- and a target nothing links to
        // has no entry, rather than one made here for every block emitted.
        const u32* const head = HeadOf(target_desc.Value(), false);  // patch 0064
        if (head == nullptr) {
            return;
        }
        for (u32 i = *head; i != NO_LINK; i = LinkAt(i).next) {
            const LinkRecord& link = LinkAt(i);
            const u64 value = target_code_ptr ? reinterpret_cast<u64>(target_code_ptr) : LinkUnlinkedOf(link);
            StoreSlot(link, value);
        }
        return;
    }

    const CodePtr save_code_ptr = code.getCurr();
    const PatchInformation& patch_info = patch_information[target_desc];

    for (CodePtr location : patch_info.jg) {
        code.SetCodePtr(location);
        EmitPatchJg(target_desc, target_code_ptr);
    }

    for (CodePtr location : patch_info.jz) {
        code.SetCodePtr(location);
        EmitPatchJz(target_desc, target_code_ptr);
    }

    for (CodePtr location : patch_info.jmp) {
        code.SetCodePtr(location);
        EmitPatchJmp(target_desc, target_code_ptr);
    }

    for (CodePtr location : patch_info.mov_rcx) {
        code.SetCodePtr(location);
        EmitPatchMovRcx(target_code_ptr);
    }

    code.SetCodePtr(save_code_ptr);
}

u64* EmitX64::LinkSlotOf(const LinkRecord& record) const {
    return reinterpret_cast<u64*>(const_cast<u8*>(code.getCode()) + (record.slot & ~LAST_LINK_OF_BLOCK));
}

u64 EmitX64::LinkUnlinkedOf(const LinkRecord& record) const {
    return reinterpret_cast<u64>(code.getCode() + record.unlinked);
}

Xbyak::Label& EmitX64::NewLinkSlot(const IR::LocationDescriptor& target, u64 unlinked, std::shared_ptr<Xbyak::Label> tail) {
    ASSERT(shared_code);
    pending_slots.push_back(PendingSlot{std::make_shared<Xbyak::Label>(), target, unlinked, std::move(tail)});
    return *pending_slots.back().label;
}

void EmitX64::EmitPendingSlots(const IR::LocationDescriptor&) {
    pending_first_link = NO_LINK;
    if (pending_slots.empty()) {
        return;
    }
    // Offsets from the buffer's start, below LAST_LINK_OF_BLOCK: a shared cache is at most 2 GiB.
    const u8* const base = code.getCode();
    const auto offset_of = [base](u64 address) {
        const u64 offset = address - reinterpret_cast<u64>(base);
        ASSERT(address >= reinterpret_cast<u64>(base) && offset < LAST_LINK_OF_BLOCK);
        return static_cast<u32>(offset);
    };
    // Patch 0028: serials, which a shared cache that never forgets every block keeps counting.
    ASSERT(static_cast<u64>(NextLinkSerial()) + pending_slots.size() < NO_LINK);
    code.align(8);
    pending_first_link = NextLinkSerial();
    for (PendingSlot& pending : pending_slots) {
        code.L(*pending.label);
        u64* const slot = code.getCurr<u64*>();
        code.dq(0);
        const u64 unlinked = pending.tail ? reinterpret_cast<u64>(pending.tail->getAddress()) : pending.unlinked;
        const auto target_block = GetBasicBlock(pending.target);  // patch 0070: not an unverified one
        // Not yet published: no thread has been given this block, so a plain store is enough. The
        // block becomes reachable through the block map or another slot, both written after this.
        *slot = target_block ? reinterpret_cast<u64>(target_block->entrypoint) : unlinked;
        // Patch 0025: the newest record heads its target's list.
        const u32 index = NextLinkSerial();
        const u64 target = pending.target.Value();
        LinkRecord record{target, offset_of(reinterpret_cast<u64>(slot)), offset_of(unlinked), NO_LINK, NO_LINK};
        u32* const head = HeadOf(target, true);  // patch 0064
        if (*head != NO_LINK) {
            record.next = *head;
            LinkAt(*head).prev = index;
        }
        *head = index;
        link_records.push_back(record);
    }
    link_records.back().slot |= LAST_LINK_OF_BLOCK;
    pending_slots.clear();
}

void EmitX64::ForgetOutgoingSlots(u32 first_link) {
    if (first_link == NO_LINK) {
        return;
    }
    for (u32 i = first_link;; i++) {
        LinkRecord& link = LinkAt(i);
        // A thread still running the dropped block leaves it for the dispatcher at this link.
        StoreSlot(link, LinkUnlinkedOf(link));
        // Out of its target's list; the record itself stays, dead, until the maps are emptied or
        // (patch 0028) the records of its region are dropped.
        if (link.prev != NO_LINK) {
            LinkAt(link.prev).next = link.next;
        } else if (link.next != NO_LINK) {
            *HeadOf(link.target, true) = link.next;  // patch 0064
        } else if (const auto it = block_descriptors.find(Key64{link.target}); it != block_descriptors.end()) {
            it.value().head = NO_LINK;
        } else {
            link_heads.erase(Key64{link.target});
        }
        if (link.next != NO_LINK) {
            LinkAt(link.next).prev = link.prev;
        }
        link.next = link.prev = NO_LINK;
        if (link.slot & LAST_LINK_OF_BLOCK) {
            break;
        }
    }
}

void EmitX64::UnlinkAllSlots() {
    // Every record, live or dead: a dead one's slot already holds its unlinked value, and its
    // block's code is still there (records are emptied with the maps, before any region they
    // name is given back).
    for (const LinkRecord& link : link_records) {
        StoreSlot(link, LinkUnlinkedOf(link));
    }
}

void EmitX64::TrimLinkRecords(u32 base) {
    ASSERT(shared_code);
    if (pending_first_link != NO_LINK) {
        ForgetOutgoingSlots(std::exchange(pending_first_link, NO_LINK));
    }
    ASSERT(base >= link_base && base <= NextLinkSerial());
    const size_t dropped = base - link_base;
    if (dropped == 0) {
        return;
    }
#ifndef NDEBUG
    for (size_t i = 0; i < dropped; i++) {
        // Dead: out of every target's list (a record at a list's head has no `prev` either, so
        // the head map is what tells).
        const LinkRecord& link = link_records[i];
        ASSERT(link.next == NO_LINK && link.prev == NO_LINK);
        const u32* const head = HeadOf(link.target, false);
        ASSERT(head == nullptr || *head != link_base + i);
    }
#endif
    link_records.erase(link_records.begin(), link_records.begin() + dropped);
    link_base = base;
    // Given back once it is mostly empty, not kept at the high-water mark.
    if (link_records.capacity() > 2 * link_records.size() + 4096) {
        link_records.shrink_to_fit();
    }
}

void EmitX64::BeginFastmemSites(const void* begin, const void* end) {
    ASSERT(shared_code);
    const u8* const b = static_cast<const u8*>(begin);
    const u8* const e = static_cast<const u8*>(end);
    // Offsets from `begin` are 32 bits: a region is far smaller than the 2 GiB a shared cache may be.
    ASSERT(b < e && static_cast<u64>(e - b) <= std::numeric_limits<u32>::max());
    for (FastmemSiteRun& run : fastmem_site_runs) {
        if (run.begin == b) {
            ASSERT(run.end == e);
            std::vector<FastmemSite>{}.swap(run.sites);
            std::vector<WideFastmemSite>{}.swap(run.wide);  // patch 0051
            return;
        }
        // Runs are regions: they never overlap.
        ASSERT(e <= run.begin || run.end <= b);
    }
    fastmem_site_runs.push_back(FastmemSiteRun{b, e, {}});
}

size_t EmitX64::ForgetUnverifiedBlocks(const std::vector<u64>& locations, std::vector<std::pair<u32, u32>>& dead_links, std::vector<std::pair<const u8*, const u8*>>& dead_code) {
    ASSERT(shared_code);
    code.EnableWriting();
    SCOPE_EXIT {
        code.DisableWriting();
    };
    size_t forgotten = 0;
    for (const u64 value : locations) {
        const IR::LocationDescriptor location{value};
        const auto it = block_descriptors.find(Key64{location});
        if (it == block_descriptors.end() || (it->second.size & UNVERIFIED_BLOCK) == 0) {
            continue;  // dropped since, or verified (or translated again)
        }
        const StoredBlock stored = it->second;
        if (stored.first_link != NO_LINK) {
            u32 last = stored.first_link;
            while ((LinkAt(last).slot & LAST_LINK_OF_BLOCK) == 0) {
                last++;
            }
            dead_links.emplace_back(stored.first_link, last);
        }
        const u8* const entry = code.getCode() + stored.entry;
        dead_code.emplace_back(entry, entry + (stored.size & ~UNVERIFIED_BLOCK));
        Unpatch(location);
        ForgetOutgoingSlots(stored.first_link);
        KeepHeadOf(it->first, stored);  // patch 0064
        block_descriptors.erase(it);
        forgotten++;
    }
    return forgotten;
}

size_t EmitX64::ForgetFastmemSitesIn(const std::vector<std::pair<const u8*, const u8*>>& spans) {
    ASSERT(shared_code);
    // Whether `at` lies in one of the spans: the last span starting at or below it.
    const auto dead = [&spans](const u8* at) {
        auto it = std::upper_bound(spans.begin(), spans.end(), at, [](const u8* a, const auto& span) { return a < span.first; });
        return it != spans.begin() && at < std::prev(it)->second;
    };
    size_t dropped = 0;
    for (FastmemSiteRun& run : fastmem_site_runs) {
        const size_t before = run.sites.size() + run.wide.size();
        const u8* const base = run.begin;
        std::erase_if(run.sites, [&](const FastmemSite& s) { return dead(base + s.site); });
        std::erase_if(run.wide, [&](const WideFastmemSite& s) { return dead(base + s.site); });
        const size_t after = run.sites.size() + run.wide.size();
        if (after != before) {
            run.sites.shrink_to_fit();
            run.wide.shrink_to_fit();
        }
        dropped += before - after;
    }
    return dropped;
}

void EmitX64::PurgeFastmemSites(const void* begin, const void* end) {
    ASSERT(shared_code);
    const u8* const b = static_cast<const u8*>(begin);
    const u8* const e = static_cast<const u8*>(end);
    for (FastmemSiteRun& run : fastmem_site_runs) {
        if (b <= run.begin && run.end <= e) {
            std::vector<FastmemSite>{}.swap(run.sites);
            std::vector<WideFastmemSite>{}.swap(run.wide);  // patch 0051
        } else {
            // A run is purged whole, with the region it is: never in part.
            ASSERT(e <= run.begin || run.end <= b);
        }
    }
}

void EmitX64::RecordSharedFastmemSite(u64 site, u64 resume, u64 callback) {
    ASSERT(shared_code);
    pending_fastmem_sites.push_back(PendingFastmemSite{site, resume, callback});
}

void EmitX64::CommitSharedFastmemSites() {
    if (pending_fastmem_sites.empty()) {
        return;
    }
    SCOPE_EXIT {
        pending_fastmem_sites.clear();
    };
    // Stable: of two records for one instruction the first stands, as `emplace` into the map did.
    std::stable_sort(pending_fastmem_sites.begin(), pending_fastmem_sites.end(),
                     [](const PendingFastmemSite& a, const PendingFastmemSite& b) { return a.site < b.site; });
    const u64 first = pending_fastmem_sites.front().site;
    const auto run = std::find_if(fastmem_site_runs.begin(), fastmem_site_runs.end(), [first](const FastmemSiteRun& r) {
        return reinterpret_cast<u64>(r.begin) <= first && first < reinterpret_cast<u64>(r.end);
    });
    // Every block of a shared cache is emitted inside a region, and every region is a run.
    ASSERT(run != fastmem_site_runs.end());
    const u64 base = reinterpret_cast<u64>(run->begin);
    const u64 limit = reinterpret_cast<u64>(run->end);
    // A block is emitted above everything emitted in its region before it, so a record at or above
    // its first site describes code that is no longer there (an emission that threw after its
    // sites were committed) -- dropped rather than left to answer for this block's code.
    const u32 first_offset = static_cast<u32>(first - base);
    while (!run->sites.empty() && run->sites.back().site >= first_offset) {
        run->sites.pop_back();
    }
    while (!run->wide.empty() && run->wide.back().site >= first_offset) {  // patch 0051
        run->wide.pop_back();
    }
    bool any = false;
    u32 last = 0;
    for (const PendingFastmemSite& p : pending_fastmem_sites) {
        ASSERT(p.site >= base && p.site < limit && p.resume >= base && p.resume < limit);
        const u32 site = static_cast<u32>(p.site - base);
        const u32 resume = static_cast<u32>(p.resume - base);
        if (any && last == site) {
            continue;  // the first record for an instruction stands
        }
        any = true;
        last = site;
        // Patch 0051: compact when the resume is a short way on and the fallback has an index.
        u32 callback = 0xFFFF;
        if (const auto it = fastmem_callback_index.find(p.callback); it != fastmem_callback_index.end()) {
            callback = it->second;
        } else if (fastmem_callbacks.size() < 0xFFFF) {
            callback = static_cast<u32>(fastmem_callbacks.size());
            fastmem_callbacks.push_back(p.callback);
            fastmem_callback_index.emplace(p.callback, static_cast<u16>(callback));
        }
        if (resume >= site && resume - site <= 0xFFFF && callback < 0xFFFF) {
            run->sites.push_back(FastmemSite{site, static_cast<u16>(resume - site), static_cast<u16>(callback)});
        } else {
            run->wide.push_back(WideFastmemSite{site, resume, p.callback});
        }
    }
}

std::optional<FakeCall> EmitX64::FindSharedFastmemSite(u64 fault_rip) const {
    for (const FastmemSiteRun& run : fastmem_site_runs) {
        const u64 base = reinterpret_cast<u64>(run.begin);
        if (fault_rip < base || fault_rip >= reinterpret_cast<u64>(run.end)) {
            continue;
        }
        const u32 offset = static_cast<u32>(fault_rip - base);
        const auto it = std::lower_bound(run.sites.begin(), run.sites.end(), offset,
                                         [](const FastmemSite& s, u32 o) { return s.site < o; });
        if (it != run.sites.end() && it->site == offset) {
            return FakeCall{.call_rip = fastmem_callbacks[it->callback], .ret_rip = base + offset + it->resume_delta};
        }
        // Patch 0051: the few kept whole.
        const auto w = std::lower_bound(run.wide.begin(), run.wide.end(), offset,
                                        [](const WideFastmemSite& s, u32 o) { return s.site < o; });
        if (w == run.wide.end() || w->site != offset) {
            return std::nullopt;
        }
        return FakeCall{.call_rip = w->callback, .ret_rip = base + w->resume};
    }
    return std::nullopt;
}

void EmitX64::Unpatch(const IR::LocationDescriptor& target_desc) {
    if (shared_code || patch_information.count(target_desc)) {
        Patch(target_desc, nullptr);
    }
}

void EmitX64::ClearCache() {
    block_descriptors.clear();
    patch_information.clear();
    // Omnidroid patch 0025: given back, not kept at capacity -- the records of every block the
    // cache held, which it forgets whole only when a region is retired or the cache cleared.
    std::vector<LinkRecord>{}.swap(link_records);
    link_base = 0;  // patch 0028
    link_heads = {};
    UseLoadFactor(link_heads, LINK_HEADS_LOAD_FACTOR);
    pending_first_link = NO_LINK;

    PerfMapClear();
}

void EmitX64::InvalidateBasicBlocks(const tsl::robin_set<IR::LocationDescriptor>& locations) {
    code.EnableWriting();
    SCOPE_EXIT {
        code.DisableWriting();
    };

    for (const auto& descriptor : locations) {
        const auto it = block_descriptors.find(Key64{descriptor});
        if (it == block_descriptors.end()) {
            continue;
        }

        Unpatch(descriptor);
        if (shared_code) {
            ForgetOutgoingSlots(it->second.first_link);  // Omnidroid patches 0022, 0025
            KeepHeadOf(it->first, it->second);           // patch 0064
        }

        block_descriptors.erase(it);
    }
}

}  // namespace Dynarmic::Backend::X64
