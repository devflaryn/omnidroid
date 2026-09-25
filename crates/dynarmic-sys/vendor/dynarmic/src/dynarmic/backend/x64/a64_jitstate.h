/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <array>

#include <mcl/stdint.hpp>

#include "dynarmic/backend/x64/nzcv_util.h"
#include "dynarmic/frontend/A64/a64_location_descriptor.h"

namespace Dynarmic::Backend::X64 {

class BlockOfCode;

#ifdef _MSC_VER
#    pragma warning(push)
#    pragma warning(disable : 4324)  // Structure was padded due to alignment specifier
#endif

struct A64JitState {
    using ProgramCounterType = u64;

    A64JitState() { ResetRSB(); }

    std::array<u64, 31> reg{};
    u64 sp = 0;
    u64 pc = 0;

    u32 cpsr_nzcv = 0;

    u32 GetPstate() const {
        return NZCV::FromX64(cpsr_nzcv);
    }
    void SetPstate(u32 new_pstate) {
        cpsr_nzcv = NZCV::ToX64(new_pstate);
    }

    alignas(16) std::array<u64, 64> vec{};  // Extension registers.

    // For internal use (See: BlockOfCode::RunCode)
    u32 guest_MXCSR = 0x00001f80;
    u32 asimd_MXCSR = 0x00009fc0;
    volatile u32 halt_reason = 0;

    // Exclusive state
    static constexpr u64 RESERVATION_GRANULE_MASK = 0xFFFF'FFFF'FFFF'FFF0ull;
    u8 exclusive_state = 0;

    static constexpr size_t RSBSize = 8;  // MUST be a power of 2.
    static constexpr size_t RSBPtrMask = RSBSize - 1;
    u32 rsb_ptr = 0;
    std::array<u64, RSBSize> rsb_location_descriptors;
    std::array<u64, RSBSize> rsb_codeptrs;
    void ResetRSB() {
        rsb_location_descriptors.fill(0xFFFFFFFFFFFFFFFFull);
        rsb_codeptrs.fill(0);
    }

    u32 fpsr_exc = 0;
    u32 fpsr_qc = 0;
    u32 fpcr = 0;
    u32 GetFpcr() const;
    u32 GetFpsr() const;
    void SetFpcr(u32 value);
    void SetFpsr(u32 value);

    u64 GetUniqueHash() const noexcept {
        const u64 fpcr_u64 = static_cast<u64>(fpcr & A64::LocationDescriptor::fpcr_mask) << A64::LocationDescriptor::fpcr_shift;
        const u64 pc_u64 = pc & A64::LocationDescriptor::pc_mask;
        return pc_u64 | fpcr_u64;
    }

    // Omnidroid patch 0022: what code emitted into a shared code cache (A64::SharedCodeCache)
    // reads per thread at run time instead of baking in as an immediate. At the end of the
    // struct so that no other offset moves; zero and unread in a Jit with its own cache.
    u64 od_callbacks = 0;            ///< A64::UserCallbacks*: `this` of every callback
    u64 od_conf = 0;                 ///< const A64::UserConfig*: the non-inline exclusives' argument
    u64 od_lookup_arg = 0;           ///< the Jit::Impl* the dispatcher's LookupBlock is called with
    u64 od_exclusive_address = 0;    ///< this processor's reservation-address slot in the monitor
    u64 od_exclusive_value = 0;      ///< this processor's reserved-value slot in the monitor
    u64 od_tpidr_el0 = 0;            ///< u64*: TPIDR_EL0
    u64 od_tpidrro_el0 = 0;          ///< const u64*: TPIDRRO_EL0
    u64 od_fast_dispatch_table = 0;  ///< this thread's fast-dispatch table
    /// Where this thread returns into generated code from the SVC callback it is inside, or 0
    /// (written by the SVC call sequence itself). A thread parked in a callback holds only the few
    /// bytes after that site of a retired region, so the region can be reused around them.
    u64 od_callback_return = 0;
};

#ifdef _MSC_VER
#    pragma warning(pop)
#endif

using CodePtr = const void*;

}  // namespace Dynarmic::Backend::X64
