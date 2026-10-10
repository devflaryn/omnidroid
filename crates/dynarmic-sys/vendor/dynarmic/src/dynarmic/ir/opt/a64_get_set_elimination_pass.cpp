/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include <array>

#include <mcl/stdint.hpp>

#include "dynarmic/frontend/A64/a64_types.h"
#include "dynarmic/ir/basic_block.h"
#include "dynarmic/ir/opcodes.h"
#include "dynarmic/ir/opt/passes.h"
#include "dynarmic/ir/value.h"

namespace Dynarmic::Optimization {

void A64GetSetElimination(IR::Block& block, A64GetSetEliminationOptions opt) {
    using Iterator = IR::Block::iterator;

    enum class TrackingType {
        W,
        X,
        S,
        D,
        Q,
        SP,
        NZCV,
        NZCVRaw,
    };
    struct RegisterInfo {
        IR::Value register_value;
        TrackingType tracking_type;
        /// Patch 0081: with an X value from a W write, the W value written (W reads take it).
        IR::Value w_value;
        bool set_instruction_present = false;
        /// Patch 0091: the barrier count when that Set was made; it may be erased only while no
        /// access has come since (`barrier` unchanged) -- in place of clearing every register's
        /// `set_instruction_present` at each access.
        std::uint32_t set_barrier = 0;
        Iterator last_set_instruction;
    };
    std::array<RegisterInfo, 31> reg_info;
    std::array<RegisterInfo, 32> vec_info;
    RegisterInfo sp_info;
    RegisterInfo nzcv_info;
    std::uint32_t barrier = 0;  // patch 0091

    const auto do_set = [&block, &barrier](RegisterInfo& info, IR::Value value, Iterator set_inst, TrackingType tracking_type) {
        if (info.set_instruction_present && info.set_barrier == barrier) {
            info.last_set_instruction->Invalidate();
            block.Instructions().erase(info.last_set_instruction);
        }

        info.register_value = value;
        info.tracking_type = tracking_type;
        info.w_value = {};  // patch 0081: set again by the caller for a W write
        info.set_instruction_present = true;
        info.set_barrier = barrier;
        info.last_set_instruction = set_inst;
    };

    const auto do_get = [&block, &opt](RegisterInfo& info, Iterator get_inst, TrackingType tracking_type) {
        const auto do_nothing = [&] {
            info = {};
            info.register_value = IR::Value(&*get_inst);
            info.tracking_type = tracking_type;
        };

        if (info.register_value.IsEmpty()) {
            do_nothing();
            return;
        }

        if (info.tracking_type == tracking_type) {
            get_inst->ReplaceUsesWith(info.register_value);
            return;
        }

        // Omnidroid patch 0081: a W read of an X value is its low word (whether the X value was
        // written or read). Not the other way: an X read after a W *read* has unknown upper bits;
        // a W write is turned into an X one below, so its X reads match above.
        if (opt.forward_width_changes && info.tracking_type == TrackingType::X && tracking_type == TrackingType::W) {
            if (!info.w_value.IsEmpty()) {
                get_inst->ReplaceUsesWith(info.w_value);
                return;
            }
            const auto low = block.PrependNewInst(get_inst, IR::Opcode::LeastSignificantWord, {info.register_value});
            get_inst->ReplaceUsesWith(IR::Value(&*low));
            return;
        }

        do_nothing();
    };

    for (auto inst = block.begin(); inst != block.end(); ++inst) {
        switch (inst->GetOpcode()) {
        case IR::Opcode::A64GetW: {
            const size_t index = A64::RegNumber(inst->GetArg(0).GetA64RegRef());
            do_get(reg_info.at(index), inst, TrackingType::W);
            break;
        }
        case IR::Opcode::A64GetX: {
            const size_t index = A64::RegNumber(inst->GetArg(0).GetA64RegRef());
            do_get(reg_info.at(index), inst, TrackingType::X);
            break;
        }
        case IR::Opcode::A64GetS: {
            const size_t index = A64::VecNumber(inst->GetArg(0).GetA64VecRef());
            do_get(vec_info.at(index), inst, TrackingType::S);
            break;
        }
        case IR::Opcode::A64GetD: {
            const size_t index = A64::VecNumber(inst->GetArg(0).GetA64VecRef());
            do_get(vec_info.at(index), inst, TrackingType::D);
            break;
        }
        case IR::Opcode::A64GetQ: {
            const size_t index = A64::VecNumber(inst->GetArg(0).GetA64VecRef());
            do_get(vec_info.at(index), inst, TrackingType::Q);
            break;
        }
        case IR::Opcode::A64GetSP: {
            do_get(sp_info, inst, TrackingType::SP);
            break;
        }
        case IR::Opcode::A64GetNZCVRaw: {
            do_get(nzcv_info, inst, TrackingType::NZCVRaw);
            break;
        }
        case IR::Opcode::A64SetW: {
            const size_t index = A64::RegNumber(inst->GetArg(0).GetA64RegRef());
            const IR::Value written = inst->GetArg(1);
            if (opt.forward_width_changes && !written.IsImmediate()) {
                // Patch 0081: `SetX (ZeroExtendWordToLong v)` in its place, tracked as the X value
                // (and `v` for W reads). The emitted store is the same: the value zero-extended,
                // all 64 bits of the register.
                const A64::Reg reg_ref = inst->GetArg(0).GetA64RegRef();
                const auto wide = block.PrependNewInst(inst, IR::Opcode::ZeroExtendWordToLong, {written});
                const auto set_x = block.PrependNewInst(inst, IR::Opcode::A64SetX, {IR::Value(reg_ref), IR::Value(&*wide)});
                inst->Invalidate();
                do_set(reg_info.at(index), IR::Value(&*wide), set_x, TrackingType::X);
                reg_info.at(index).w_value = written;
                break;
            }
            do_set(reg_info.at(index), inst->GetArg(1), inst, TrackingType::W);
            break;
        }
        case IR::Opcode::A64SetX: {
            const size_t index = A64::RegNumber(inst->GetArg(0).GetA64RegRef());
            do_set(reg_info.at(index), inst->GetArg(1), inst, TrackingType::X);
            break;
        }
        case IR::Opcode::A64SetS: {
            const size_t index = A64::VecNumber(inst->GetArg(0).GetA64VecRef());
            do_set(vec_info.at(index), inst->GetArg(1), inst, TrackingType::S);
            break;
        }
        case IR::Opcode::A64SetD: {
            const size_t index = A64::VecNumber(inst->GetArg(0).GetA64VecRef());
            do_set(vec_info.at(index), inst->GetArg(1), inst, TrackingType::D);
            break;
        }
        case IR::Opcode::A64SetQ: {
            const size_t index = A64::VecNumber(inst->GetArg(0).GetA64VecRef());
            do_set(vec_info.at(index), inst->GetArg(1), inst, TrackingType::Q);
            break;
        }
        case IR::Opcode::A64SetSP: {
            do_set(sp_info, inst->GetArg(0), inst, TrackingType::SP);
            break;
        }
        case IR::Opcode::A64SetNZCV: {
            do_set(nzcv_info, inst->GetArg(0), inst, TrackingType::NZCV);
            break;
        }
        case IR::Opcode::A64SetNZCVRaw: {
            do_set(nzcv_info, inst->GetArg(0), inst, TrackingType::NZCVRaw);
            // Omnidroid patch 0037: the store keeps bits 28-31 only, and `GetNZCVRaw` reads back
            // only those, so the value written is not the value a later read sees (`MSR NZCV, Xt`
            // with any low bit set, then `MRS Xt, NZCV`). Upstream forwarded it unmasked; found by
            // omni-cpu's `tests/precise_getset.rs` differential. Not forwarded: the next read
            // loads, and starts tracking from there.
            nzcv_info.register_value = {};
            break;
        }
        default: {
            if (opt.precise_at_memory_aborts) {
                // Omnidroid patch 0037. With `check_halt_on_memory_access` the block can return to
                // the dispatcher from inside: `EmitCheckMemoryAbort`, after any guest data access,
                // stores this instruction's PC and leaves, and the guest state in `JitState` must
                // then be exactly what the instructions before it wrote. So an access is a point
                // past which no earlier Set may be erased (a later Set to the same register would
                // otherwise take its place, and the fault would see the value before both). What
                // is known of each register is kept: the access writes no guest register, and on
                // the path that does return, nothing after it runs.
                //
                // Instructions that call into the host with the guest state in `JitState` -- a
                // supervisor call (omni-cpu serves syscalls and inline thunks there, reading and
                // writing guest registers, and continues), an exception, a cache operation, a host
                // function -- also forget what is known, as the callee may have changed it.
                if (inst->CausesCPUExceptionFlag()
                    || inst->GetOpcode() == IR::Opcode::A64DataCacheOperationRaised
                    || inst->GetOpcode() == IR::Opcode::A64InstructionCacheOperationRaised
                    || inst->GetOpcode() == IR::Opcode::CallHostFunction) {
                    reg_info = {};
                    vec_info = {};
                    sp_info = {};
                    nzcv_info = {};
                } else if (inst->IsMemoryReadOrWrite() || inst->MayHaveSideEffects()) {
                    barrier++;  // patch 0091: no Set before this point is erased
                }
            }
            if (inst->ReadsOrWritesCPSR()) {  // patch 0091: from the table
                nzcv_info = {};
            }
            if (inst->ReadsOrWritesCoreRegister()) {
                reg_info = {};
                vec_info = {};
                sp_info = {};
            }
            break;
        }
        }
    }
}

}  // namespace Dynarmic::Optimization
