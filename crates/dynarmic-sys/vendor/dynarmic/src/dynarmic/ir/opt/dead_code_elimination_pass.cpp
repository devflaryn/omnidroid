/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include <mcl/iterator/reverse.hpp>

#include "dynarmic/ir/basic_block.h"
#include "dynarmic/ir/opt/passes.h"

namespace Dynarmic::Optimization {

void DeadCodeElimination(IR::Block& block, DeadCodeEliminationOptions opt) {
    // We iterate over the instructions in reverse order.
    // This is because removing an instruction reduces the number of uses for earlier instructions.
    for (auto& inst : mcl::iterator::reverse(block)) {
        // Omnidroid patch 0037: a guest load can fault, and where faults are precise
        // (`check_halt_on_memory_access`) that is an effect even when its value is never read --
        // `LDR WZR, [Xn]` is how ART probes below the stack for overflow.
        if (opt.keep_memory_reads && inst.IsMemoryRead()) {
            continue;
        }
        if (!inst.HasUses() && !inst.MayHaveSideEffects()) {
            inst.Invalidate();
        }
    }
}

}  // namespace Dynarmic::Optimization
