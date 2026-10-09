/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include <algorithm>
#include <cstdio>
#include <vector>

#include <mcl/assert.hpp>
#include <mcl/stdint.hpp>

#include "dynarmic/ir/basic_block.h"
#include "dynarmic/ir/microinstruction.h"
#include "dynarmic/ir/opcodes.h"
#include "dynarmic/ir/opt/passes.h"
#include "dynarmic/ir/type.h"

namespace Dynarmic::Optimization {

void VerificationPass(const IR::Block& block) {
    for (const auto& inst : block) {
        for (size_t i = 0; i < inst.NumArgs(); i++) {
            const IR::Type t1 = inst.GetArg(i).GetType();
            const IR::Type t2 = IR::GetArgTypeOf(inst.GetOpcode(), i);
            if (!IR::AreTypesCompatible(t1, t2)) {
                std::puts(IR::DumpBlock(block).c_str());
                ASSERT_FALSE("above block failed validation");
            }
        }
    }

    // Omnidroid patch 0063: the same check -- every instruction used as an argument is used as many
    // times as it counts -- over a sorted list in a per-thread buffer rather than a std::map (a
    // node allocation per used instruction, every block translated).
    thread_local std::vector<IR::Inst*> uses;
    uses.clear();
    for (const auto& inst : block) {
        for (size_t i = 0; i < inst.NumArgs(); i++) {
            const auto arg = inst.GetArg(i);
            if (!arg.IsImmediate()) {
                uses.push_back(arg.GetInst());
            }
        }
    }
    std::sort(uses.begin(), uses.end());
    for (size_t i = 0; i < uses.size();) {
        size_t j = i + 1;
        while (j < uses.size() && uses[j] == uses[i]) {
            j++;
        }
        ASSERT(uses[i]->UseCount() == j - i);
        i = j;
    }
}

}  // namespace Dynarmic::Optimization
