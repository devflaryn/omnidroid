/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include "dynarmic/ir/opcodes.h"

#include <array>
#include <initializer_list>
#include <stdexcept>
#include <vector>

#include "dynarmic/ir/type.h"

namespace Dynarmic::IR {

// Opcode information

namespace OpcodeInfo {

struct Meta {
    const char* name;
    Type type;
    std::vector<Type> arg_types;
};

constexpr Type Void = Type::Void;
constexpr Type A32Reg = Type::A32Reg;
constexpr Type A32ExtReg = Type::A32ExtReg;
constexpr Type A64Reg = Type::A64Reg;
constexpr Type A64Vec = Type::A64Vec;
constexpr Type Opaque = Type::Opaque;
constexpr Type U1 = Type::U1;
constexpr Type U8 = Type::U8;
constexpr Type U16 = Type::U16;
constexpr Type U32 = Type::U32;
constexpr Type U64 = Type::U64;
constexpr Type U128 = Type::U128;
constexpr Type CoprocInfo = Type::CoprocInfo;
constexpr Type NZCV = Type::NZCVFlags;
constexpr Type Cond = Type::Cond;
constexpr Type Table = Type::Table;
constexpr Type AccType = Type::AccType;

static const std::array opcode_info{
#define OPCODE(name, type, ...) Meta{#name, type, {__VA_ARGS__}},
#define A32OPC(name, type, ...) Meta{#name, type, {__VA_ARGS__}},
#define A64OPC(name, type, ...) Meta{#name, type, {__VA_ARGS__}},
#include "./opcodes.inc"
#undef OPCODE
#undef A32OPC
#undef A64OPC
};

// A braced list, as `opcode_info`'s: some lines of `opcodes.inc` end their arguments with a comma.
constexpr detail::OpcodeArgs MakeArgs(std::initializer_list<Type> types) {
    detail::OpcodeArgs a{static_cast<std::uint8_t>(types.size()), {}};
    size_t i = 0;
    for (const Type t : types) {
        a.types[i++] = t;
    }
    return a;
}

// Patch 0077: built at compile time from the same `opcodes.inc` as `opcode_info` (here, where the
// argument types' short names are these constants rather than IR's value classes).
constexpr std::array<detail::OpcodeArgs, static_cast<size_t>(Opcode::NUM_OPCODE)> opcode_args_table{
#define OPCODE(name, type, ...) MakeArgs({__VA_ARGS__}),
#define A32OPC(name, type, ...) MakeArgs({__VA_ARGS__}),
#define A64OPC(name, type, ...) MakeArgs({__VA_ARGS__}),
#include "./opcodes.inc"
#undef OPCODE
#undef A32OPC
#undef A64OPC
};

}  // namespace OpcodeInfo

namespace detail {
const std::array<OpcodeArgs, static_cast<size_t>(Opcode::NUM_OPCODE)> opcode_args = OpcodeInfo::opcode_args_table;

void ArgIndexOutOfRange(Opcode op, size_t arg_index) {
    // What `std::vector::at` did before patch 0077.
    throw std::out_of_range(fmt::format("argument {} of {}", arg_index, GetNameOf(op)));
}
}  // namespace detail

Type GetTypeOf(Opcode op) {
    return OpcodeInfo::opcode_info.at(static_cast<size_t>(op)).type;
}

std::string GetNameOf(Opcode op) {
    return OpcodeInfo::opcode_info.at(static_cast<size_t>(op)).name;
}

}  // namespace Dynarmic::IR
