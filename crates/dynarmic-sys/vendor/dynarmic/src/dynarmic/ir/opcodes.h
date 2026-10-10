/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <array>
#include <string>

#include <fmt/format.h>
#include <mcl/stdint.hpp>

namespace Dynarmic::IR {

enum class Type;

/**
 * The Opcodes of our intermediate representation.
 * Type signatures for each opcode can be found in opcodes.inc
 */
enum class Opcode {
#define OPCODE(name, type, ...) name,
#define A32OPC(name, type, ...) A32##name,
#define A64OPC(name, type, ...) A64##name,
#include "./opcodes.inc"
#undef OPCODE
#undef A32OPC
#undef A64OPC
    NUM_OPCODE
};

constexpr size_t OpcodeCount = static_cast<size_t>(Opcode::NUM_OPCODE);

namespace detail {
/// Omnidroid patch 0077: an opcode's arguments, in a table the inline accessors below read --
/// `GetNumArgsOf` and `GetArgTypeOf` were calls into another file reading a `std::vector` through
/// `at()`, made several times for every argument every pass and the emitter read (the asserts of
/// `Inst::GetArg` and `SetArg` alone).
struct OpcodeArgs {
    std::uint8_t count;
    std::array<Type, 4> types;
    Type ret;  // patch 0087: the opcode's return type, for the inline `GetTypeOf`
};
extern const std::array<OpcodeArgs, static_cast<size_t>(Opcode::NUM_OPCODE)> opcode_args;
[[noreturn]] void ArgIndexOutOfRange(Opcode op, size_t arg_index);
}  // namespace detail

/// Get return type of an opcode (patch 0087: inline, from the table)
inline Type GetTypeOf(Opcode op) {
    return detail::opcode_args[static_cast<size_t>(op)].ret;
}

/// Get the number of arguments an opcode accepts
inline size_t GetNumArgsOf(Opcode op) {
    return detail::opcode_args[static_cast<size_t>(op)].count;
}

/// Get the required type of an argument of an opcode
inline Type GetArgTypeOf(Opcode op, size_t arg_index) {
    const auto& a = detail::opcode_args[static_cast<size_t>(op)];
    if (arg_index >= a.count) [[unlikely]] {
        detail::ArgIndexOutOfRange(op, arg_index);
    }
    return a.types[arg_index];
}

/// Get the name of an opcode.
std::string GetNameOf(Opcode op);

}  // namespace Dynarmic::IR

template<>
struct fmt::formatter<Dynarmic::IR::Opcode> : fmt::formatter<std::string> {
    template<typename FormatContext>
    auto format(Dynarmic::IR::Opcode op, FormatContext& ctx) const {
        return formatter<std::string>::format(Dynarmic::IR::GetNameOf(op), ctx);
    }
};
