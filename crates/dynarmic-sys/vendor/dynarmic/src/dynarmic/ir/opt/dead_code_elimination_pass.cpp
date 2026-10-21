/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include <mcl/iterator/reverse.hpp>

#include "dynarmic/ir/basic_block.h"
#include "dynarmic/ir/opt/passes.h"

namespace Dynarmic::Optimization {

void DeadCodeElimination(IR::Block& block, DeadCodeEliminationOptions opt) {
    if (opt.forward_identities) {
        // Omnidroid patch 0110: in the same reverse walk, every argument that is an `Identity`
        // becomes what it stands for -- so an `Identity` has no uses left by the time the walk
        // reaches it (its users come after it) -- and what is dead leaves the block. Not a
        // pseudo-operation's: it reads what its instruction did (the flags of an `ANDS` that
        // ConstantPropagation folded into an `Identity` of an operand), which the value the
        // `Identity` stands for need not have (`Inst::Use` refuses it), so that `Identity` stays.
        auto iter = block.end();
        while (iter != block.begin()) {
            --iter;
            IR::Inst& inst = *iter;
            const size_t num_args = inst.IsAPseudoOperation() ? 0 : inst.NumArgs();
            for (size_t i = 0; i < num_args; i++) {
                IR::Value arg = inst.GetArg(i);
                if (arg.IsIdentity()) {
                    do {
                        arg = arg.GetInst()->GetArg(0);
                    } while (arg.IsIdentity());
                    inst.SetArg(i, arg);
                }
            }
            if ((opt.keep_memory_reads && inst.IsMemoryRead()) || inst.HasUses() || inst.MayHaveSideEffects()) {
                continue;
            }
            inst.Invalidate();
            iter = block.Instructions().erase(iter);
        }
        return;
    }
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
