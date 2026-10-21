/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <array>

#include <mcl/stdint.hpp>

#include "dynarmic/frontend/A64/a64_location_descriptor.h"

namespace Dynarmic::Backend::Arm64 {

struct A64JitState {
    std::array<u64, 31> reg{};
    u64 sp = 0;
    u64 pc = 0;

    u32 cpsr_nzcv = 0;

    alignas(16) std::array<u64, 64> vec{};

    u32 exclusive_state = 0;

    u32 fpsr = 0;
    u32 fpcr = 0;

    // Omnidroid patch 0112: what generated code reads per thread at run time instead of baking it
    // in as an immediate, so that one block is right for any thread (x64's patch 0022 fields). After
    // `fpcr`, so that no other offset moves. Filled by `Jit::Impl` when it is made and again by
    // `Reset`; the jit's own thread's values for as long as the jit lives.
    u64 od_callbacks = 0;          ///< A64::UserCallbacks*: `this` of every callback
    u64 od_conf = 0;               ///< const A64::UserConfig*: the non-inline exclusives' argument
    u64 od_tpidr_el0 = 0;          ///< u64*: TPIDR_EL0's slot
    u64 od_tpidrro_el0 = 0;        ///< const u64*: TPIDRRO_EL0's slot
    u64 od_exclusive_address = 0;  ///< this processor's reservation-address slot in the monitor (0: none)
    u64 od_exclusive_value = 0;    ///< this processor's reserved-value slot in the monitor (0: none)
    u64 od_thread = 0;             ///< reserved for a shared cache's per-thread record; 0

    IR::LocationDescriptor GetLocationDescriptor() const {
        const u64 fpcr_u64 = static_cast<u64>(fpcr & A64::LocationDescriptor::fpcr_mask) << A64::LocationDescriptor::fpcr_shift;
        const u64 pc_u64 = pc & A64::LocationDescriptor::pc_mask;
        return IR::LocationDescriptor{pc_u64 | fpcr_u64};
    }
};

}  // namespace Dynarmic::Backend::Arm64
