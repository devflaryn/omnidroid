/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include <array>
#include <cstdlib>
#include <initializer_list>
#include <optional>
#include <mutex>
#include <tuple>
#include <utility>
#include <vector>

#include <tsl/robin_set.h>

#include <fmt/format.h>
#include <fmt/ostream.h>
#include <mcl/type_traits/integer_of_size.hpp>
#include <xbyak/xbyak.h>

#include "dynarmic/backend/x64/a64_emit_x64.h"
#include "dynarmic/backend/x64/abi.h"
#include "dynarmic/backend/x64/devirtualize.h"
#include "dynarmic/backend/x64/emit_x64_memory.h"
#include "dynarmic/backend/x64/exclusive_monitor_friend.h"
#include "dynarmic/backend/x64/perf_map.h"
#include "dynarmic/common/spin_lock_x64.h"
#include "dynarmic/common/x64_disassemble.h"
#include "dynarmic/interface/exclusive_monitor.h"

namespace Dynarmic::Backend::X64 {

using namespace Xbyak::util;

std::atomic<std::uint32_t> live_fastmem_mask_by_and{0};
std::atomic<std::uint32_t> live_fastmem_tbi_unmasked{0};
std::atomic<std::uint32_t> live_compact_code{0};

// Omnidroid patch 0041.
std::atomic<std::uint64_t> tbi_sites_noted{0};
namespace {
std::mutex tbi_sites_mutex;
tsl::robin_set<u64> tbi_masked_sites;
std::vector<u64> tbi_sites_in_order;
}  // namespace

void NoteTbiTaggedSite(std::uint64_t location) {
    std::lock_guard lock{tbi_sites_mutex};
    if (tbi_masked_sites.insert(location).second) {
        tbi_sites_in_order.push_back(A64::LocationDescriptor{IR::LocationDescriptor{location}}.PC());
        tbi_sites_noted.store(tbi_sites_in_order.size(), std::memory_order_release);
    }
}

bool IsTbiMaskedSite(std::uint64_t location) {
    if (tbi_sites_noted.load(std::memory_order_acquire) == 0) {
        return false;
    }
    std::lock_guard lock{tbi_sites_mutex};
    return tbi_masked_sites.count(location) != 0;
}

std::vector<std::uint64_t> TbiSitesFrom(std::size_t first) {
    std::lock_guard lock{tbi_sites_mutex};
    if (first >= tbi_sites_in_order.size()) {
        return {};
    }
    return {tbi_sites_in_order.begin() + static_cast<std::ptrdiff_t>(first), tbi_sites_in_order.end()};
}

void A64EmitX64::GenMemory128Accessors() {
    code.align();
    memory_read_128 = code.getCurr<void (*)()>();
#ifdef _WIN32
    UserCallback<&A64::UserCallbacks::MemoryRead128>().EmitCallWithReturnPointer(code, [&](Xbyak::Reg64 return_value_ptr, [[maybe_unused]] RegList args) {
        code.mov(code.ABI_PARAM3, code.ABI_PARAM2);
        code.sub(rsp, 8 + 16 + ABI_SHADOW_SPACE);
        code.lea(return_value_ptr, ptr[rsp + ABI_SHADOW_SPACE]);
    });
    code.movups(xmm1, xword[code.ABI_RETURN]);
    code.add(rsp, 8 + 16 + ABI_SHADOW_SPACE);
#else
    code.sub(rsp, 8);
    UserCallback<&A64::UserCallbacks::MemoryRead128>().EmitCall(code);
    if (code.HasHostFeature(HostFeature::SSE41)) {
        code.movq(xmm1, code.ABI_RETURN);
        code.pinsrq(xmm1, code.ABI_RETURN2, 1);
    } else {
        code.movq(xmm1, code.ABI_RETURN);
        code.movq(xmm2, code.ABI_RETURN2);
        code.punpcklqdq(xmm1, xmm2);
    }
    code.add(rsp, 8);
#endif
    code.ret();
    PerfMapRegister(memory_read_128, code.getCurr(), "a64_memory_read_128");

    code.align();
    memory_write_128 = code.getCurr<void (*)()>();
#ifdef _WIN32
    code.sub(rsp, 8 + 16 + ABI_SHADOW_SPACE);
    code.lea(code.ABI_PARAM3, ptr[rsp + ABI_SHADOW_SPACE]);
    code.movaps(xword[code.ABI_PARAM3], xmm1);
    UserCallback<&A64::UserCallbacks::MemoryWrite128>().EmitCall(code);
    code.add(rsp, 8 + 16 + ABI_SHADOW_SPACE);
#else
    code.sub(rsp, 8);
    if (code.HasHostFeature(HostFeature::SSE41)) {
        code.movq(code.ABI_PARAM3, xmm1);
        code.pextrq(code.ABI_PARAM4, xmm1, 1);
    } else {
        code.movq(code.ABI_PARAM3, xmm1);
        code.punpckhqdq(xmm1, xmm1);
        code.movq(code.ABI_PARAM4, xmm1);
    }
    UserCallback<&A64::UserCallbacks::MemoryWrite128>().EmitCall(code);
    code.add(rsp, 8);
#endif
    code.ret();
    PerfMapRegister(memory_write_128, code.getCurr(), "a64_memory_write_128");

    code.align();
    memory_exclusive_write_128 = code.getCurr<void (*)()>();
#ifdef _WIN32
    code.sub(rsp, 8 + 32 + ABI_SHADOW_SPACE);
    code.lea(code.ABI_PARAM3, ptr[rsp + ABI_SHADOW_SPACE]);
    code.lea(code.ABI_PARAM4, ptr[rsp + ABI_SHADOW_SPACE + 16]);
    code.movaps(xword[code.ABI_PARAM3], xmm1);
    code.movaps(xword[code.ABI_PARAM4], xmm2);
    UserCallback<&A64::UserCallbacks::MemoryWriteExclusive128>().EmitCall(code);
    code.add(rsp, 8 + 32 + ABI_SHADOW_SPACE);
#else
    code.sub(rsp, 8);
    if (code.HasHostFeature(HostFeature::SSE41)) {
        code.movq(code.ABI_PARAM3, xmm1);
        code.pextrq(code.ABI_PARAM4, xmm1, 1);
        code.movq(code.ABI_PARAM5, xmm2);
        code.pextrq(code.ABI_PARAM6, xmm2, 1);
    } else {
        code.movq(code.ABI_PARAM3, xmm1);
        code.punpckhqdq(xmm1, xmm1);
        code.movq(code.ABI_PARAM4, xmm1);
        code.movq(code.ABI_PARAM5, xmm2);
        code.punpckhqdq(xmm2, xmm2);
        code.movq(code.ABI_PARAM6, xmm2);
    }
    UserCallback<&A64::UserCallbacks::MemoryWriteExclusive128>().EmitCall(code);
    code.add(rsp, 8);
#endif
    code.ret();
    PerfMapRegister(memory_exclusive_write_128, code.getCurr(), "a64_memory_exclusive_write_128");
}

namespace {

// Omnidroid patch 0078: small fastmem fallbacks (`OMNI_JIT_SMALL_FALLBACKS`, on unless `0`).
bool SmallFallbacks() {
    static const bool on = [] {
        const char* v = std::getenv("OMNI_JIT_SMALL_FALLBACKS");
        return v == nullptr || v[0] != '0';
    }();
    return on;
}

// A fallback body's frame. Its trampoline pushed `extra` operands after the block's call pushed the
// return address; the body saves the caller-saved registers but `except` above them, aligned for
// the call it makes (the parity of `ABI_PushRegistersAndAdjustStack`, counting those operands).
struct BodyFrame {
    std::vector<HostLoc> gprs;
    std::vector<HostLoc> xmms;
    size_t sub = 0;
};

BodyFrame BodyEnter(BlockOfCode& code, std::optional<HostLoc> except, size_t extra) {
    BodyFrame f;
    for (const HostLoc r : ABI_ALL_CALLER_SAVE) {
        if (except && r == *except) {
            continue;
        }
        if (HostLocIsGPR(r)) {
            f.gprs.push_back(r);
        } else if (HostLocIsXMM(r)) {
            f.xmms.push_back(r);
        }
    }
    // At entry rsp % 16 == (8 - 8 * extra) % 16; after the pushes, the subtraction makes it 0.
    const size_t align = ((extra + f.gprs.size()) % 2 == 0) ? 8 : 0;
    f.sub = align + f.xmms.size() * 16 + ABI_SHADOW_SPACE;
    for (const HostLoc r : f.gprs) {
        code.push(HostLocToReg64(r));
    }
    if (f.sub != 0) {
        code.sub(code.rsp, static_cast<u32>(f.sub));
    }
    size_t off = ABI_SHADOW_SPACE;
    for (const HostLoc r : f.xmms) {
        if (code.HasHostFeature(HostFeature::AVX)) {
            code.vmovaps(code.xword[code.rsp + off], HostLocToXmm(r));
        } else {
            code.movaps(code.xword[code.rsp + off], HostLocToXmm(r));
        }
        off += 16;
    }
    return f;
}

// The `i`th operand the trampoline pushed (0: the last pushed), from inside the body.
Xbyak::Address PushedArg(BlockOfCode& code, const BodyFrame& f, size_t i) {
    return code.qword[code.rsp + f.sub + 8 * f.gprs.size() + 8 * i];
}

void BodyLeave(BlockOfCode& code, const BodyFrame& f) {
    size_t off = ABI_SHADOW_SPACE;
    for (const HostLoc r : f.xmms) {
        if (code.HasHostFeature(HostFeature::AVX)) {
            code.vmovaps(HostLocToXmm(r), code.xword[code.rsp + off]);
        } else {
            code.movaps(HostLocToXmm(r), code.xword[code.rsp + off]);
        }
        off += 16;
    }
    if (f.sub != 0) {
        code.add(code.rsp, static_cast<u32>(f.sub));
    }
    for (auto it = f.gprs.rbegin(); it != f.gprs.rend(); ++it) {
        code.pop(HostLocToReg64(*it));
    }
}

// Drop the trampoline's operands (flags left alone, as `lea` does) and return to the block.
void BodyReturn(BlockOfCode& code, size_t extra) {
    code.lea(code.rsp, code.ptr[code.rsp + 8 * extra]);
    code.ret();
}

}  // namespace

// **Omnidroid patch 0078: small fastmem fallbacks.** `GenFastmemFallbacks` made one whole thunk
// per (ordered, size, address register, value register): ~6,000 of them, each saving and restoring
// every caller-saved register -- about 1 MiB of code written into every code cache's prelude
// (patch 0017 measured the prelude at ~1.1 MiB beyond the constant pool), in every guest process,
// before its first block. Here each (ordered, size, address register, value register) is a
// trampoline of a few bytes that pushes its operands and jumps to a body shared by every register
// that is only an input: a read's body is per value register (its result), a write's and an
// exclusive write's per size (128-bit ones per value register, which no push can move). The body
// takes the operands from the stack. What each fallback does -- the registers it keeps, the
// callback it calls with which arguments, the fences, the zero extension -- is unchanged.
void A64EmitX64::GenSmallFastmemFallbacks() {
    const std::array<std::pair<size_t, ArgCallback>, 4> read_callbacks{{
        {8, UserCallback<&A64::UserCallbacks::MemoryRead8>()},
        {16, UserCallback<&A64::UserCallbacks::MemoryRead16>()},
        {32, UserCallback<&A64::UserCallbacks::MemoryRead32>()},
        {64, UserCallback<&A64::UserCallbacks::MemoryRead64>()},
    }};
    const std::array<std::pair<size_t, ArgCallback>, 4> write_callbacks{{
        {8, UserCallback<&A64::UserCallbacks::MemoryWrite8>()},
        {16, UserCallback<&A64::UserCallbacks::MemoryWrite16>()},
        {32, UserCallback<&A64::UserCallbacks::MemoryWrite32>()},
        {64, UserCallback<&A64::UserCallbacks::MemoryWrite64>()},
    }};
    const std::array<std::pair<size_t, ArgCallback>, 4> exclusive_write_callbacks{{
        {8, UserCallback<&A64::UserCallbacks::MemoryWriteExclusive8>()},
        {16, UserCallback<&A64::UserCallbacks::MemoryWriteExclusive16>()},
        {32, UserCallback<&A64::UserCallbacks::MemoryWriteExclusive32>()},
        {64, UserCallback<&A64::UserCallbacks::MemoryWriteExclusive64>()},
    }};
    const auto usable = [](int idx) { return idx != 4 && idx != 15; };  // not rsp, not r15

    for (bool ordered : {false, true}) {
        // The bodies.
        std::array<const void*, 16> read128{}, write128{}, exclusive128{};
        std::array<std::array<const void*, 16>, 4> read{};
        std::array<const void*, 4> write{}, exclusive{};
        for (int value_idx = 0; value_idx < 16; value_idx++) {
            code.align();
            read128[value_idx] = code.getCurr();
            {
                const BodyFrame f = BodyEnter(code, HostLocXmmIdx(value_idx), 1);
                code.mov(code.ABI_PARAM2, PushedArg(code, f, 0));
                if (ordered) {
                    code.mfence();
                }
                code.call(memory_read_128);
                if (value_idx != 1) {
                    code.movaps(Xbyak::Xmm{value_idx}, xmm1);
                }
                BodyLeave(code, f);
                BodyReturn(code, 1);
            }
            PerfMapRegister(read128[value_idx], code.getCurr(), "a64_read_fallback_128");

            code.align();
            write128[value_idx] = code.getCurr();
            {
                const BodyFrame f = BodyEnter(code, std::nullopt, 1);
                code.mov(code.ABI_PARAM2, PushedArg(code, f, 0));
                if (value_idx != 1) {
                    code.movaps(xmm1, Xbyak::Xmm{value_idx});
                }
                code.call(memory_write_128);
                if (ordered) {
                    code.mfence();
                }
                BodyLeave(code, f);
                BodyReturn(code, 1);
            }
            PerfMapRegister(write128[value_idx], code.getCurr(), "a64_write_fallback_128");

            code.align();
            exclusive128[value_idx] = code.getCurr();
            {
                const BodyFrame f = BodyEnter(code, HostLoc::RAX, 1);
                if (value_idx != 1) {
                    code.movaps(xmm1, Xbyak::Xmm{value_idx});
                }
                if (code.HasHostFeature(HostFeature::SSE41)) {
                    code.movq(xmm2, rax);
                    code.pinsrq(xmm2, rdx, 1);
                } else {
                    code.movq(xmm2, rax);
                    code.movq(xmm0, rdx);
                    code.punpcklqdq(xmm2, xmm0);
                }
                code.mov(code.ABI_PARAM2, PushedArg(code, f, 0));
                code.call(memory_exclusive_write_128);
                BodyLeave(code, f);
                BodyReturn(code, 1);
            }
            PerfMapRegister(exclusive128[value_idx], code.getCurr(), "a64_exclusive_write_fallback_128");

            if (!usable(value_idx)) {
                continue;
            }
            for (size_t i = 0; i < read_callbacks.size(); i++) {
                const auto& [bitsize, callback] = read_callbacks[i];
                code.align();
                read[i][value_idx] = code.getCurr();
                const BodyFrame f = BodyEnter(code, HostLocRegIdx(value_idx), 1);
                code.mov(code.ABI_PARAM2, PushedArg(code, f, 0));
                if (ordered) {
                    code.mfence();
                }
                callback.EmitCall(code);
                if (value_idx != code.ABI_RETURN.getIdx()) {
                    code.mov(Xbyak::Reg64{value_idx}, code.ABI_RETURN);
                }
                BodyLeave(code, f);
                code.ZeroExtendFrom(bitsize, Xbyak::Reg64{value_idx});
                BodyReturn(code, 1);
                PerfMapRegister(read[i][value_idx], code.getCurr(), fmt::format("a64_read_fallback_{}", bitsize));
            }
        }
        for (size_t i = 0; i < write_callbacks.size(); i++) {
            const auto& [bitsize, callback] = write_callbacks[i];
            code.align();
            write[i] = code.getCurr();
            const BodyFrame f = BodyEnter(code, std::nullopt, 2);
            code.mov(code.ABI_PARAM3, PushedArg(code, f, 1));
            code.mov(code.ABI_PARAM2, PushedArg(code, f, 0));
            code.ZeroExtendFrom(bitsize, code.ABI_PARAM3);
            callback.EmitCall(code);
            if (ordered) {
                code.mfence();
            }
            BodyLeave(code, f);
            BodyReturn(code, 2);
            PerfMapRegister(write[i], code.getCurr(), fmt::format("a64_write_fallback_{}", bitsize));
        }
        for (size_t i = 0; i < exclusive_write_callbacks.size(); i++) {
            const auto& [bitsize, callback] = exclusive_write_callbacks[i];
            code.align();
            exclusive[i] = code.getCurr();
            const BodyFrame f = BodyEnter(code, HostLoc::RAX, 2);
            code.mov(code.ABI_PARAM3, PushedArg(code, f, 1));
            code.mov(code.ABI_PARAM2, PushedArg(code, f, 0));
            code.ZeroExtendFrom(bitsize, code.ABI_PARAM3);
            code.mov(code.ABI_PARAM4, rax);
            code.ZeroExtendFrom(bitsize, code.ABI_PARAM4);
            callback.EmitCall(code);
            BodyLeave(code, f);
            BodyReturn(code, 2);
            PerfMapRegister(exclusive[i], code.getCurr(), fmt::format("a64_exclusive_write_fallback_{}", bitsize));
        }

        // The trampolines: the address (and a general value) pushed, the body jumped to.
        code.align();
        for (int vaddr_idx = 0; vaddr_idx < 16; vaddr_idx++) {
            if (!usable(vaddr_idx)) {
                continue;
            }
            const Xbyak::Reg64 vaddr{vaddr_idx};
            for (int value_idx = 0; value_idx < 16; value_idx++) {
                const auto key128 = std::make_tuple(ordered, 128, vaddr_idx, value_idx);
                read_fallbacks[key128] = code.getCurr<void (*)()>();
                code.push(vaddr);
                code.jmp(read128[value_idx], Xbyak::CodeGenerator::T_NEAR);
                write_fallbacks[key128] = code.getCurr<void (*)()>();
                code.push(vaddr);
                code.jmp(write128[value_idx], Xbyak::CodeGenerator::T_NEAR);
                exclusive_write_fallbacks[key128] = code.getCurr<void (*)()>();
                code.push(vaddr);
                code.jmp(exclusive128[value_idx], Xbyak::CodeGenerator::T_NEAR);
                if (!usable(value_idx)) {
                    continue;
                }
                const Xbyak::Reg64 value{value_idx};
                for (size_t i = 0; i < 4; i++) {
                    const int bitsize = static_cast<int>(read_callbacks[i].first);
                    const auto key = std::make_tuple(ordered, bitsize, vaddr_idx, value_idx);
                    read_fallbacks[key] = code.getCurr<void (*)()>();
                    code.push(vaddr);
                    code.jmp(read[i][value_idx], Xbyak::CodeGenerator::T_NEAR);
                    write_fallbacks[key] = code.getCurr<void (*)()>();
                    code.push(value);
                    code.push(vaddr);
                    code.jmp(write[i], Xbyak::CodeGenerator::T_NEAR);
                    exclusive_write_fallbacks[key] = code.getCurr<void (*)()>();
                    code.push(value);
                    code.push(vaddr);
                    code.jmp(exclusive[i], Xbyak::CodeGenerator::T_NEAR);
                }
            }
        }
    }
}

void A64EmitX64::GenFastmemFallbacks() {
    // Omnidroid patch 0041: `rax` = a guest location (descriptor) to note; everything caller-saved kept.
    code.align();
    tbi_note_thunk = code.getCurr<const void*>();
    ABI_PushCallerSaveRegistersAndAdjustStack(code);
    code.mov(code.ABI_PARAM1, code.rax);
    code.CallFunction(&NoteTbiTaggedSite);
    // Leave `Run` at the next halt check (a return, an indirect branch, a checked terminal), where
    // the host drops the noted sites' translations: blocks already translated unmasked would
    // otherwise keep faulting until the run slice ends.
    code.lock();
    code.or_(code.dword[code.r15 + offsetof(A64JitState, halt_reason)], static_cast<u32>(HaltReason::CacheInvalidation));
    ABI_PopCallerSaveRegistersAndAdjustStack(code);
    code.ret();
    PerfMapRegister(tbi_note_thunk, code.getCurr(), "a64_tbi_note");

    // Omnidroid patch 0061: the memory-abort check of a fastmem site's slow path, shared. Called
    // right after the fallback, with the faulting instruction's guest PC as 8 bytes of data after
    // the call: no abort -- step the return address over the data and return (flags only
    // touched); an abort -- take the PC from the data, store it, leave Run, as
    // `EmitCheckMemoryAbort` does inline (~38 bytes a site, against 5 + 8 here).
    {
        code.align();
        memory_abort_check_thunk = code.getCurr<const void*>();
        Xbyak::Label exit;
        code.test(code.byte[code.r15 + offsetof(A64JitState, halt_reason)], static_cast<u8>(HaltReason::MemoryAbort));
        code.jnz(exit);
        code.add(code.qword[code.rsp], 8);
        code.ret();
        code.L(exit);
        code.pop(code.rax);
        code.mov(code.rax, code.qword[code.rax]);
        code.mov(code.qword[code.r15 + offsetof(A64JitState, pc)], code.rax);
        code.ForceReturnFromRunCode();
        PerfMapRegister(memory_abort_check_thunk, code.getCurr(), "a64_memory_abort_check");
    }

    if (SmallFallbacks()) {  // patch 0078
        GenSmallFastmemFallbacks();
        return;
    }

    const std::initializer_list<int> idxes{0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15};
    const std::array<std::pair<size_t, ArgCallback>, 4> read_callbacks{{
        {8, UserCallback<&A64::UserCallbacks::MemoryRead8>()},
        {16, UserCallback<&A64::UserCallbacks::MemoryRead16>()},
        {32, UserCallback<&A64::UserCallbacks::MemoryRead32>()},
        {64, UserCallback<&A64::UserCallbacks::MemoryRead64>()},
    }};
    const std::array<std::pair<size_t, ArgCallback>, 4> write_callbacks{{
        {8, UserCallback<&A64::UserCallbacks::MemoryWrite8>()},
        {16, UserCallback<&A64::UserCallbacks::MemoryWrite16>()},
        {32, UserCallback<&A64::UserCallbacks::MemoryWrite32>()},
        {64, UserCallback<&A64::UserCallbacks::MemoryWrite64>()},
    }};
    const std::array<std::pair<size_t, ArgCallback>, 4> exclusive_write_callbacks{{
        {8, UserCallback<&A64::UserCallbacks::MemoryWriteExclusive8>()},
        {16, UserCallback<&A64::UserCallbacks::MemoryWriteExclusive16>()},
        {32, UserCallback<&A64::UserCallbacks::MemoryWriteExclusive32>()},
        {64, UserCallback<&A64::UserCallbacks::MemoryWriteExclusive64>()},
    }};

    for (bool ordered : {false, true}) {
        for (int vaddr_idx : idxes) {
            if (vaddr_idx == 4 || vaddr_idx == 15) {
                continue;
            }

            for (int value_idx : idxes) {
                code.align();
                read_fallbacks[std::make_tuple(ordered, 128, vaddr_idx, value_idx)] = code.getCurr<void (*)()>();
                ABI_PushCallerSaveRegistersAndAdjustStackExcept(code, HostLocXmmIdx(value_idx));
                if (vaddr_idx != code.ABI_PARAM2.getIdx()) {
                    code.mov(code.ABI_PARAM2, Xbyak::Reg64{vaddr_idx});
                }
                if (ordered) {
                    code.mfence();
                }
                code.call(memory_read_128);
                if (value_idx != 1) {
                    code.movaps(Xbyak::Xmm{value_idx}, xmm1);
                }
                ABI_PopCallerSaveRegistersAndAdjustStackExcept(code, HostLocXmmIdx(value_idx));
                code.ret();
                PerfMapRegister(read_fallbacks[std::make_tuple(ordered, 128, vaddr_idx, value_idx)], code.getCurr(), "a64_read_fallback_128");

                code.align();
                write_fallbacks[std::make_tuple(ordered, 128, vaddr_idx, value_idx)] = code.getCurr<void (*)()>();
                ABI_PushCallerSaveRegistersAndAdjustStack(code);
                if (vaddr_idx != code.ABI_PARAM2.getIdx()) {
                    code.mov(code.ABI_PARAM2, Xbyak::Reg64{vaddr_idx});
                }
                if (value_idx != 1) {
                    code.movaps(xmm1, Xbyak::Xmm{value_idx});
                }
                code.call(memory_write_128);
                if (ordered) {
                    code.mfence();
                }
                ABI_PopCallerSaveRegistersAndAdjustStack(code);
                code.ret();
                PerfMapRegister(write_fallbacks[std::make_tuple(ordered, 128, vaddr_idx, value_idx)], code.getCurr(), "a64_write_fallback_128");

                code.align();
                exclusive_write_fallbacks[std::make_tuple(ordered, 128, vaddr_idx, value_idx)] = code.getCurr<void (*)()>();
                ABI_PushCallerSaveRegistersAndAdjustStackExcept(code, HostLoc::RAX);
                if (value_idx != 1) {
                    code.movaps(xmm1, Xbyak::Xmm{value_idx});
                }
                if (code.HasHostFeature(HostFeature::SSE41)) {
                    code.movq(xmm2, rax);
                    code.pinsrq(xmm2, rdx, 1);
                } else {
                    code.movq(xmm2, rax);
                    code.movq(xmm0, rdx);
                    code.punpcklqdq(xmm2, xmm0);
                }
                if (vaddr_idx != code.ABI_PARAM2.getIdx()) {
                    code.mov(code.ABI_PARAM2, Xbyak::Reg64{vaddr_idx});
                }
                code.call(memory_exclusive_write_128);
                ABI_PopCallerSaveRegistersAndAdjustStackExcept(code, HostLoc::RAX);
                code.ret();
                PerfMapRegister(exclusive_write_fallbacks[std::make_tuple(ordered, 128, vaddr_idx, value_idx)], code.getCurr(), "a64_exclusive_write_fallback_128");

                if (value_idx == 4 || value_idx == 15) {
                    continue;
                }

                for (const auto& [bitsize, callback] : read_callbacks) {
                    code.align();
                    read_fallbacks[std::make_tuple(ordered, bitsize, vaddr_idx, value_idx)] = code.getCurr<void (*)()>();
                    ABI_PushCallerSaveRegistersAndAdjustStackExcept(code, HostLocRegIdx(value_idx));
                    if (vaddr_idx != code.ABI_PARAM2.getIdx()) {
                        code.mov(code.ABI_PARAM2, Xbyak::Reg64{vaddr_idx});
                    }
                    if (ordered) {
                        code.mfence();
                    }
                    callback.EmitCall(code);
                    if (value_idx != code.ABI_RETURN.getIdx()) {
                        code.mov(Xbyak::Reg64{value_idx}, code.ABI_RETURN);
                    }
                    ABI_PopCallerSaveRegistersAndAdjustStackExcept(code, HostLocRegIdx(value_idx));
                    code.ZeroExtendFrom(bitsize, Xbyak::Reg64{value_idx});
                    code.ret();
                    PerfMapRegister(read_fallbacks[std::make_tuple(ordered, bitsize, vaddr_idx, value_idx)], code.getCurr(), fmt::format("a64_read_fallback_{}", bitsize));
                }

                for (const auto& [bitsize, callback] : write_callbacks) {
                    code.align();
                    write_fallbacks[std::make_tuple(ordered, bitsize, vaddr_idx, value_idx)] = code.getCurr<void (*)()>();
                    ABI_PushCallerSaveRegistersAndAdjustStack(code);
                    if (vaddr_idx == code.ABI_PARAM3.getIdx() && value_idx == code.ABI_PARAM2.getIdx()) {
                        code.xchg(code.ABI_PARAM2, code.ABI_PARAM3);
                    } else if (vaddr_idx == code.ABI_PARAM3.getIdx()) {
                        code.mov(code.ABI_PARAM2, Xbyak::Reg64{vaddr_idx});
                        if (value_idx != code.ABI_PARAM3.getIdx()) {
                            code.mov(code.ABI_PARAM3, Xbyak::Reg64{value_idx});
                        }
                    } else {
                        if (value_idx != code.ABI_PARAM3.getIdx()) {
                            code.mov(code.ABI_PARAM3, Xbyak::Reg64{value_idx});
                        }
                        if (vaddr_idx != code.ABI_PARAM2.getIdx()) {
                            code.mov(code.ABI_PARAM2, Xbyak::Reg64{vaddr_idx});
                        }
                    }
                    code.ZeroExtendFrom(bitsize, code.ABI_PARAM3);
                    callback.EmitCall(code);
                    if (ordered) {
                        code.mfence();
                    }
                    ABI_PopCallerSaveRegistersAndAdjustStack(code);
                    code.ret();
                    PerfMapRegister(write_fallbacks[std::make_tuple(ordered, bitsize, vaddr_idx, value_idx)], code.getCurr(), fmt::format("a64_write_fallback_{}", bitsize));
                }

                for (const auto& [bitsize, callback] : exclusive_write_callbacks) {
                    code.align();
                    exclusive_write_fallbacks[std::make_tuple(ordered, bitsize, vaddr_idx, value_idx)] = code.getCurr<void (*)()>();
                    ABI_PushCallerSaveRegistersAndAdjustStackExcept(code, HostLoc::RAX);
                    if (vaddr_idx == code.ABI_PARAM3.getIdx() && value_idx == code.ABI_PARAM2.getIdx()) {
                        code.xchg(code.ABI_PARAM2, code.ABI_PARAM3);
                    } else if (vaddr_idx == code.ABI_PARAM3.getIdx()) {
                        code.mov(code.ABI_PARAM2, Xbyak::Reg64{vaddr_idx});
                        if (value_idx != code.ABI_PARAM3.getIdx()) {
                            code.mov(code.ABI_PARAM3, Xbyak::Reg64{value_idx});
                        }
                    } else {
                        if (value_idx != code.ABI_PARAM3.getIdx()) {
                            code.mov(code.ABI_PARAM3, Xbyak::Reg64{value_idx});
                        }
                        if (vaddr_idx != code.ABI_PARAM2.getIdx()) {
                            code.mov(code.ABI_PARAM2, Xbyak::Reg64{vaddr_idx});
                        }
                    }
                    code.ZeroExtendFrom(bitsize, code.ABI_PARAM3);
                    code.mov(code.ABI_PARAM4, rax);
                    code.ZeroExtendFrom(bitsize, code.ABI_PARAM4);
                    callback.EmitCall(code);
                    ABI_PopCallerSaveRegistersAndAdjustStackExcept(code, HostLoc::RAX);
                    code.ret();
                    PerfMapRegister(exclusive_write_fallbacks[std::make_tuple(ordered, bitsize, vaddr_idx, value_idx)], code.getCurr(), fmt::format("a64_exclusive_write_fallback_{}", bitsize));
                }
            }
        }
    }
}

void A64EmitX64::EmitLoadExclusiveAddressPointer(Xbyak::Reg64 reg) {
    if (shared_code) {
        // Omnidroid patch 0022: the running thread's slot.
        code.mov(reg, qword[r15 + offsetof(A64JitState, od_exclusive_address)]);
    } else {
        code.mov(reg, mcl::bit_cast<u64>(GetExclusiveMonitorAddressPointer(conf.global_monitor, conf.processor_id)));
    }
}

void A64EmitX64::EmitLoadExclusiveValuePointer(Xbyak::Reg64 reg) {
    if (shared_code) {
        code.mov(reg, qword[r15 + offsetof(A64JitState, od_exclusive_value)]);
    } else {
        code.mov(reg, mcl::bit_cast<u64>(GetExclusiveMonitorValuePointer(conf.global_monitor, conf.processor_id)));
    }
}

#define Axx A64
#include "dynarmic/backend/x64/emit_x64_memory.cpp.inc"
#undef Axx

void A64EmitX64::EmitA64ReadMemory8(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryRead<8, &A64::UserCallbacks::MemoryRead8>(ctx, inst);
}

void A64EmitX64::EmitA64ReadMemory16(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryRead<16, &A64::UserCallbacks::MemoryRead16>(ctx, inst);
}

void A64EmitX64::EmitA64ReadMemory32(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryRead<32, &A64::UserCallbacks::MemoryRead32>(ctx, inst);
}

void A64EmitX64::EmitA64ReadMemory64(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryRead<64, &A64::UserCallbacks::MemoryRead64>(ctx, inst);
}

void A64EmitX64::EmitA64ReadMemory128(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryRead<128, &A64::UserCallbacks::MemoryRead128>(ctx, inst);
}

void A64EmitX64::EmitA64WriteMemory8(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryWrite<8, &A64::UserCallbacks::MemoryWrite8>(ctx, inst);
}

void A64EmitX64::EmitA64WriteMemory16(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryWrite<16, &A64::UserCallbacks::MemoryWrite16>(ctx, inst);
}

void A64EmitX64::EmitA64WriteMemory32(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryWrite<32, &A64::UserCallbacks::MemoryWrite32>(ctx, inst);
}

void A64EmitX64::EmitA64WriteMemory64(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryWrite<64, &A64::UserCallbacks::MemoryWrite64>(ctx, inst);
}

void A64EmitX64::EmitA64WriteMemory128(A64EmitContext& ctx, IR::Inst* inst) {
    EmitMemoryWrite<128, &A64::UserCallbacks::MemoryWrite64>(ctx, inst);
}

void A64EmitX64::EmitA64ClearExclusive(A64EmitContext&, IR::Inst*) {
    code.mov(code.byte[r15 + offsetof(A64JitState, exclusive_state)], u8(0));
}

void A64EmitX64::EmitA64ExclusiveReadMemory8(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveReadMemoryInline<8, &A64::UserCallbacks::MemoryRead8>(ctx, inst);
    } else {
        EmitExclusiveReadMemory<8, &A64::UserCallbacks::MemoryRead8>(ctx, inst);
    }
}

void A64EmitX64::EmitA64ExclusiveReadMemory16(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveReadMemoryInline<16, &A64::UserCallbacks::MemoryRead16>(ctx, inst);
    } else {
        EmitExclusiveReadMemory<16, &A64::UserCallbacks::MemoryRead16>(ctx, inst);
    }
}

void A64EmitX64::EmitA64ExclusiveReadMemory32(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveReadMemoryInline<32, &A64::UserCallbacks::MemoryRead32>(ctx, inst);
    } else {
        EmitExclusiveReadMemory<32, &A64::UserCallbacks::MemoryRead32>(ctx, inst);
    }
}

void A64EmitX64::EmitA64ExclusiveReadMemory64(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveReadMemoryInline<64, &A64::UserCallbacks::MemoryRead64>(ctx, inst);
    } else {
        EmitExclusiveReadMemory<64, &A64::UserCallbacks::MemoryRead64>(ctx, inst);
    }
}

void A64EmitX64::EmitA64ExclusiveReadMemory128(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveReadMemoryInline<128, &A64::UserCallbacks::MemoryRead128>(ctx, inst);
    } else {
        EmitExclusiveReadMemory<128, &A64::UserCallbacks::MemoryRead128>(ctx, inst);
    }
}

void A64EmitX64::EmitA64ExclusiveWriteMemory8(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveWriteMemoryInline<8, &A64::UserCallbacks::MemoryWriteExclusive8>(ctx, inst);
    } else {
        EmitExclusiveWriteMemory<8, &A64::UserCallbacks::MemoryWriteExclusive8>(ctx, inst);
    }
}

void A64EmitX64::EmitA64ExclusiveWriteMemory16(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveWriteMemoryInline<16, &A64::UserCallbacks::MemoryWriteExclusive16>(ctx, inst);
    } else {
        EmitExclusiveWriteMemory<16, &A64::UserCallbacks::MemoryWriteExclusive16>(ctx, inst);
    }
}

void A64EmitX64::EmitA64ExclusiveWriteMemory32(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveWriteMemoryInline<32, &A64::UserCallbacks::MemoryWriteExclusive32>(ctx, inst);
    } else {
        EmitExclusiveWriteMemory<32, &A64::UserCallbacks::MemoryWriteExclusive32>(ctx, inst);
    }
}

void A64EmitX64::EmitA64ExclusiveWriteMemory64(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveWriteMemoryInline<64, &A64::UserCallbacks::MemoryWriteExclusive64>(ctx, inst);
    } else {
        EmitExclusiveWriteMemory<64, &A64::UserCallbacks::MemoryWriteExclusive64>(ctx, inst);
    }
}

void A64EmitX64::EmitA64ExclusiveWriteMemory128(A64EmitContext& ctx, IR::Inst* inst) {
    if (conf.fastmem_exclusive_access) {
        EmitExclusiveWriteMemoryInline<128, &A64::UserCallbacks::MemoryWriteExclusive128>(ctx, inst);
    } else {
        EmitExclusiveWriteMemory<128, &A64::UserCallbacks::MemoryWriteExclusive128>(ctx, inst);
    }
}

void A64EmitX64::EmitCheckMemoryAbort(A64EmitContext&, IR::Inst* inst, Xbyak::Label* end) {
    if (!conf.check_halt_on_memory_access) {
        return;
    }

    Xbyak::Label skip;

    const A64::LocationDescriptor current_location{IR::LocationDescriptor{inst->GetArg(0).GetU64()}};

    code.test(dword[r15 + offsetof(A64JitState, halt_reason)], static_cast<u32>(HaltReason::MemoryAbort));
    if (end) {
        code.jz(*end, code.T_NEAR);
    } else {
        code.jz(skip, code.T_NEAR);
    }
    code.mov(rax, current_location.PC());
    code.mov(qword[r15 + offsetof(A64JitState, pc)], rax);
    code.ForceReturnFromRunCode();
    code.L(skip);
}

}  // namespace Dynarmic::Backend::X64
