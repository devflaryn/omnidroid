/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <array>

#include <mcl/container/intrusive_list.hpp>
#include <mcl/stdint.hpp>

#include "dynarmic/ir/opcodes.h"
#include "dynarmic/ir/value.h"

namespace Dynarmic::IR {

enum class Opcode;
enum class Type;

constexpr size_t max_arg_count = 4;

namespace detail {
/// Omnidroid patch 0087: what `Inst::MayHaveSideEffects`, `IsMemoryRead` and `IsMemoryReadOrWrite`
/// say of each opcode -- each a chain of out-of-line switches over the opcode, asked of every
/// instruction by dead-code elimination and the get/set elimination -- worked out once.
enum InstFlag : std::uint8_t {
    kMayHaveSideEffects = 1,
    kIsMemoryRead = 2,
    kIsMemoryReadOrWrite = 4,
    // Patch 0091: what A64's get/set elimination asks of every other instruction.
    kCausesCPUException = 8,
    kReadsOrWritesCPSR = 16,
    kReadsOrWritesCoreRegister = 32,
};
extern const std::array<std::uint8_t, OpcodeCount> inst_flags;
}  // namespace detail

/**
 * A representation of a microinstruction. A single ARM/Thumb instruction may be
 * converted into zero or more microinstructions.
 */
class Inst final : public mcl::intrusive_list_node<Inst> {
public:
    explicit Inst(Opcode op)
            : op(op) {}

    /// Determines whether or not this instruction performs an arithmetic shift.
    bool IsArithmeticShift() const;
    /// Determines whether or not this instruction performs a logical shift.
    bool IsLogicalShift() const;
    /// Determines whether or not this instruction performs a circular shift.
    bool IsCircularShift() const;
    /// Determines whether or not this instruction performs any kind of shift.
    bool IsShift() const;

    /// Determines whether or not this instruction is a form of barrier.
    bool IsBarrier() const;

    /// Determines whether or not this instruction performs a shared memory read.
    bool IsSharedMemoryRead() const;
    /// Determines whether or not this instruction performs a shared memory write.
    bool IsSharedMemoryWrite() const;
    /// Determines whether or not this instruction performs a shared memory read or write.
    bool IsSharedMemoryReadOrWrite() const;
    /// Determines whether or not this instruction performs an atomic memory read.
    bool IsExclusiveMemoryRead() const;
    /// Determines whether or not this instruction performs an atomic memory write.
    bool IsExclusiveMemoryWrite() const;

    /// Determines whether or not this instruction performs any kind of memory read.
    bool IsMemoryRead() const { return (detail::inst_flags[static_cast<size_t>(op)] & detail::kIsMemoryRead) != 0; }  // patch 0087
    /// Determines whether or not this instruction performs any kind of memory write.
    bool IsMemoryWrite() const;
    /// Determines whether or not this instruction performs any kind of memory access.
    bool IsMemoryReadOrWrite() const { return (detail::inst_flags[static_cast<size_t>(op)] & detail::kIsMemoryReadOrWrite) != 0; }  // patch 0087

    /// Determines whether or not this instruction reads from the CPSR.
    bool ReadsFromCPSR() const;
    /// Determines whether or not this instruction writes to the CPSR.
    bool WritesToCPSR() const;

    /// Determines whether or not this instruction writes to a system register.
    bool WritesToSystemRegister() const;

    /// Determines whether or not this instruction reads from a core register.
    bool ReadsFromCoreRegister() const;
    /// Determines whether or not this instruction writes to a core register.
    bool WritesToCoreRegister() const;

    /// Determines whether or not this instruction reads from the FPCR.
    bool ReadsFromFPCR() const;
    /// Determines whether or not this instruction writes to the FPCR.
    bool WritesToFPCR() const;

    /// Determines whether or not this instruction reads from the FPSR.
    bool ReadsFromFPSR() const;
    /// Determines whether or not this instruction writes to the FPSR.
    bool WritesToFPSR() const;

    /// Determines whether or not this instruction reads from the FPSR cumulative exception bits.
    bool ReadsFromFPSRCumulativeExceptionBits() const;
    /// Determines whether or not this instruction writes to the FPSR cumulative exception bits.
    bool WritesToFPSRCumulativeExceptionBits() const;
    /// Determines whether or not this instruction both reads from and writes to the FPSR cumulative exception bits.
    bool ReadsFromAndWritesToFPSRCumulativeExceptionBits() const;

    /// Determines whether or not this instruction reads from the FPSR cumulative saturation bit.
    bool ReadsFromFPSRCumulativeSaturationBit() const;
    /// Determines whether or not this instruction writes to the FPSR cumulative saturation bit.
    bool WritesToFPSRCumulativeSaturationBit() const;

    /// Determines whether or not this instruction alters memory-exclusivity.
    bool AltersExclusiveState() const;

    /// Determines whether or not this instruction accesses a coprocessor.
    bool IsCoprocessorInstruction() const;

    /// Determines whether or not this instruction causes a CPU exception.
    bool CausesCPUException() const;

    /// Determines whether or not this instruction is a SetCheckBit operation.
    bool IsSetCheckBitOperation() const;

    /// Determines whether or not this instruction may have side-effects.
    bool MayHaveSideEffects() const { return (detail::inst_flags[static_cast<size_t>(op)] & detail::kMayHaveSideEffects) != 0; }  // patch 0087
    /// Patch 0091: `CausesCPUException()`, `ReadsFromCPSR() || WritesToCPSR()` and
    /// `ReadsFromCoreRegister() || WritesToCoreRegister()`, from `detail::inst_flags`.
    bool CausesCPUExceptionFlag() const { return (detail::inst_flags[static_cast<size_t>(op)] & detail::kCausesCPUException) != 0; }
    bool ReadsOrWritesCPSR() const { return (detail::inst_flags[static_cast<size_t>(op)] & detail::kReadsOrWritesCPSR) != 0; }
    bool ReadsOrWritesCoreRegister() const { return (detail::inst_flags[static_cast<size_t>(op)] & detail::kReadsOrWritesCoreRegister) != 0; }

    /// Determines whether or not this instruction is a pseduo-instruction.
    /// Pseudo-instructions depend on their parent instructions for their semantics.
    bool IsAPseudoOperation() const;

    /// Determines whether or not this instruction supports the GetNZCVFromOp pseudo-operation.
    bool MayGetNZCVFromOp() const;

    /// Determines if all arguments of this instruction are immediates.
    bool AreAllArgsImmediates() const;

    size_t UseCount() const { return use_count; }
    bool HasUses() const { return use_count > 0; }

    /// Determines if there is a pseudo-operation associated with this instruction.
    bool HasAssociatedPseudoOperation() const;
    /// Gets a pseudo-operation associated with this instruction.
    Inst* GetAssociatedPseudoOperation(Opcode opcode);

    /// Get the microop this microinstruction represents.
    Opcode GetOpcode() const { return op; }
    /// Get the type this instruction returns. (Patch 0087: inline.)
    Type GetType() const {
        if (op == Opcode::Identity)
            return args[0].GetType();
        return GetTypeOf(op);
    }
    /// Get the number of arguments this instruction has.
    size_t NumArgs() const { return GetNumArgsOf(op); }  // patch 0077: inline

    /// Omnidroid patch 0086: inline -- a call per argument read by every pass and by the emitter
    /// (2.5% of emission's samples) -- with the same checks, their messages out of line.
    Value GetArg(size_t index) const {
        if (index >= GetNumArgsOf(op) || (args[index].IsEmpty() && GetArgTypeOf(op, index) != Type::Opaque)) [[unlikely]] {
            BadGetArg(index);
        }
        return args[index];
    }
    void SetArg(size_t index, Value value);

    void Invalidate();
    void ClearArgs();

    void ReplaceUsesWith(Value replacement);

    // IR name (i.e. instruction number in block). This is set in the naming pass. Treat 0 as an invalid name.
    // This is used for debugging and fastmem instruction identification.
    void SetName(unsigned value) { name = value; }
    unsigned GetName() const { return name; }

    /// Omnidroid patch 0080: where the x64 register allocator last saw this value (a HostLoc index,
    /// 0xFF none) -- a hint it checks before trusting, so a stale one costs only the old search.
    std::uint8_t HostLocHint() const { return host_loc_hint; }
    void SetHostLocHint(std::uint8_t loc) const { host_loc_hint = loc; }

private:
    [[noreturn]] void BadGetArg(size_t index) const;  // patch 0086
    // Patch 0087: what the inline predicates above were, from which `detail::inst_flags` is made.
    bool IsMemoryReadSlow() const;
    bool IsMemoryReadOrWriteSlow() const;
    bool MayHaveSideEffectsSlow() const;
    friend struct InstFlagsMaker;
    void Use(const Value& value);
    void UndoUse(const Value& value);

    Opcode op;
    unsigned use_count = 0;
    unsigned name = 0;
    mutable std::uint8_t host_loc_hint = 0xFF;  // patch 0080, in what was padding
    std::array<Value, max_arg_count> args;

    // Linked list of pseudooperations associated with this instruction.
    Inst* next_pseudoop = nullptr;
};

}  // namespace Dynarmic::IR
