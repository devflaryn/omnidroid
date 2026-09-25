/* This file is part of the dynarmic project.
 * Copyright (c) 2018 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <functional>
#include <vector>

#include <mcl/stdint.hpp>
#include <xbyak/xbyak.h>

namespace Dynarmic::Backend::X64 {

using RegList = std::vector<Xbyak::Reg64>;

class BlockOfCode;

class Callback {
public:
    virtual ~Callback();

    void EmitCall(BlockOfCode& code) const {
        EmitCall(code, [](RegList) {});
    }

    virtual void EmitCall(BlockOfCode& code, std::function<void(RegList)> fn) const = 0;
    virtual void EmitCallWithReturnPointer(BlockOfCode& code, std::function<void(Xbyak::Reg64, RegList)> fn) const = 0;
};

class SimpleCallback final : public Callback {
public:
    template<typename Function>
    SimpleCallback(Function fn)
            : fn(reinterpret_cast<void (*)()>(fn)) {}

    using Callback::EmitCall;

    void EmitCall(BlockOfCode& code, std::function<void(RegList)> fn) const override;
    void EmitCallWithReturnPointer(BlockOfCode& code, std::function<void(Xbyak::Reg64, RegList)> fn) const override;

private:
    void (*fn)();
};

class ArgCallback final : public Callback {
public:
    template<typename Function>
    ArgCallback(Function fn, u64 arg)
            : fn(reinterpret_cast<void (*)()>(fn)), arg(arg) {}

    /// Omnidroid patch 0022: the first argument is not an immediate but the 8 bytes at
    /// `[r15 + offset]` -- a field of the running thread's JitState -- read at call time, so one
    /// emitted call serves every thread of a shared code cache.
    struct FromJitState {
        size_t offset;
    };
    template<typename Function>
    ArgCallback(Function fn, FromJitState from)
            : fn(reinterpret_cast<void (*)()>(fn)), arg(from.offset), arg_in_jit_state(true) {}

    using Callback::EmitCall;

    void EmitCall(BlockOfCode& code, std::function<void(RegList)> fn) const override;
    void EmitCallWithReturnPointer(BlockOfCode& code, std::function<void(Xbyak::Reg64, RegList)> fn) const override;

    /// The same function with its argument read from `[r15 + offset]` instead. Patch 0022.
    ArgCallback WithArgFromJitState(size_t offset) const {
        ArgCallback copy = *this;
        copy.arg = offset;
        copy.arg_in_jit_state = true;
        return copy;
    }
    /// The immediate argument (meaningless once read from JitState). Patch 0022.
    u64 Arg() const { return arg; }

private:
    void (*fn)();
    u64 arg;
    bool arg_in_jit_state = false;
};

}  // namespace Dynarmic::Backend::X64
