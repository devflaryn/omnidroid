/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include <memory>
#include <mutex>

#include <boost/icl/interval_set.hpp>
#include <mcl/assert.hpp>
#include <mcl/bit_cast.hpp>
#include <mcl/scope_exit.hpp>
#include <mcl/stdint.hpp>

#include "dynarmic/backend/arm64/a64_address_space.h"
#include "dynarmic/backend/arm64/a64_core.h"
#include "dynarmic/backend/arm64/a64_jitstate.h"
#include "dynarmic/backend/x64/exclusive_monitor_friend.h"
#include "dynarmic/common/atomic.h"
#include "dynarmic/interface/A64/a64.h"
#include "dynarmic/interface/A64/config.h"

namespace Dynarmic::A64 {

using namespace Backend::Arm64;

struct Jit::Impl final {
    Impl(Jit*, A64::UserConfig conf)
            : conf(conf)
            , current_address_space(conf)
            , core(conf) {
        OdFillThreadValues();  // patch 0112
    }

    HaltReason Run() {
        ASSERT(!is_executing);
        PerformRequestedCacheInvalidation(static_cast<HaltReason>(Atomic::Load(&halt_reason)));

        is_executing = true;
        SCOPE_EXIT {
            is_executing = false;
        };

        HaltReason hr = core.Run(current_address_space, current_state, &halt_reason);

        PerformRequestedCacheInvalidation(hr);

        return hr;
    }

    HaltReason Step() {
        ASSERT(!is_executing);
        PerformRequestedCacheInvalidation(static_cast<HaltReason>(Atomic::Load(&halt_reason)));

        is_executing = true;
        SCOPE_EXIT {
            is_executing = false;
        };

        HaltReason hr = core.Step(current_address_space, current_state, &halt_reason);

        PerformRequestedCacheInvalidation(hr);

        return hr;
    }

    void ClearCache() {
        std::unique_lock lock{invalidation_mutex};
        invalidate_entire_cache = true;
        HaltExecution(HaltReason::CacheInvalidation);
    }

    void InvalidateCacheRange(std::uint64_t start_address, std::size_t length) {
        std::unique_lock lock{invalidation_mutex};
        invalid_cache_ranges.add(boost::icl::discrete_interval<u64>::closed(start_address, start_address + length - 1));
        HaltExecution(HaltReason::CacheInvalidation);
    }

    void Reset() {
        current_state = {};
        // Omnidroid patch 0112: the guest's state goes, the thread's own values stay -- generated
        // code reads them from here at run time, and a zero would be a callback on a null object
        // or a load through a null slot pointer.
        OdFillThreadValues();
    }

    void HaltExecution(HaltReason hr) {
        Atomic::Or(&halt_reason, static_cast<u32>(hr));
    }

    void ClearHalt(HaltReason hr) {
        Atomic::And(&halt_reason, ~static_cast<u32>(hr));
    }

    std::uint64_t PC() const {
        return current_state.pc;
    }

    void SetPC(std::uint64_t value) {
        current_state.pc = value;
    }

    std::uint64_t SP() const {
        return current_state.sp;
    }

    void SetSP(std::uint64_t value) {
        current_state.sp = value;
    }

    std::array<std::uint64_t, 31>& Regs() {
        return current_state.reg;
    }

    const std::array<std::uint64_t, 31>& Regs() const {
        return current_state.reg;
    }

    std::array<std::uint64_t, 64>& VecRegs() {
        return current_state.vec;
    }

    const std::array<std::uint64_t, 64>& VecRegs() const {
        return current_state.vec;
    }

    std::uint32_t Fpcr() const {
        return current_state.fpcr;
    }

    void SetFpcr(std::uint32_t value) {
        current_state.fpcr = value;
    }

    std::uint32_t Fpsr() const {
        return current_state.fpsr;
    }

    void SetFpsr(std::uint32_t value) {
        current_state.fpsr = value;
    }

    std::uint32_t Pstate() const {
        return current_state.cpsr_nzcv;
    }

    void SetPstate(std::uint32_t value) {
        current_state.cpsr_nzcv = value;
    }

    void ClearExclusiveState() {
        current_state.exclusive_state = false;
    }

    bool IsExecuting() const {
        return is_executing;
    }

    void DumpDisassembly() const {
        current_address_space.DumpDisassembly();
    }

    std::size_t OdCodeBytesInUse() const {
        return current_address_space.OdCodeBytesInUse();
    }

    std::uint64_t OdCodeBytesEmitted() const {
        return current_address_space.OdCodeBytesEmitted();
    }

    std::size_t OdDiscardParked(std::uint64_t resume, const std::atomic<std::uint64_t>& parked) {
        std::unique_lock lock{invalidation_mutex};
        invalidate_entire_cache = true;
        HaltExecution(HaltReason::CacheInvalidation);
        std::atomic_thread_fence(std::memory_order_seq_cst);
        // The halt is up before this look: a thread that left the callback since will see it.
        if (resume == 0 || parked.load(std::memory_order_seq_cst) != resume) {
            return 0;
        }
        // Omnidroid patch 0108: given back already, and not run since (its clear is still to come):
        // nothing more to give. The same pages were counted again at every pass while the thread
        // stayed parked, which overstated what the trim gave back.
        if (current_address_space.OdTablesGivenBack()) {
            return 0;
        }
        const std::size_t given = current_address_space.OdDiscardExcept(resume);
        current_address_space.OdGiveBackTables();  // patch 0107
        return given;
    }

    std::size_t OdTableBytes() const {
        return current_address_space.OdTableBytes();
    }

    // Omnidroid patch 0106: see the header. A parked thread's tables are read as 0104 gives back its
    // code: the halt is up before the look, so a thread that has left the callback since goes from
    // the rest of its block to the dispatcher, sees the halt before any lookup, and leaves the run;
    // it then waits on this lock before it clears or emits. That edge does only what was asked of
    // it already: the halt alone asks nothing.
    bool OdForEachBlock(Jit::OdBlockVisitor fn, void* ctx, std::uint64_t resume, const std::atomic<std::uint64_t>* parked) {
        if (parked == nullptr) {
            current_address_space.OdForEachBlock(fn, ctx);
            return true;
        }
        std::unique_lock lock{invalidation_mutex};
        HaltExecution(HaltReason::CacheInvalidation);
        std::atomic_thread_fence(std::memory_order_seq_cst);
        if (resume == 0 || parked->load(std::memory_order_seq_cst) != resume) {
            return false;
        }
        current_address_space.OdForEachBlock(fn, ctx);
        return true;
    }

    void OdSetEmitHook(Jit::OdBlockVisitor fn, void* ctx) {
        current_address_space.OdSetEmitHook(fn, ctx);
    }

    Jit::OdThreadValues OdGetThreadValues() const {
        return Jit::OdThreadValues{
            .callbacks = current_state.od_callbacks,
            .conf = current_state.od_conf,
            .tpidr_el0 = current_state.od_tpidr_el0,
            .tpidrro_el0 = current_state.od_tpidrro_el0,
            .exclusive_address = current_state.od_exclusive_address,
            .exclusive_value = current_state.od_exclusive_value,
        };
    }

    Jit::OdBlockLinkInfo OdBlockLinks(std::uint64_t location, Jit::OdLinkVisitor fn, void* ctx) const {
        Jit::OdBlockLinkInfo info{};
        info.entry_point = mcl::bit_cast<u64>(current_address_space.OdBlockLinks(IR::LocationDescriptor{location}, info.host_bytes, fn, ctx));
        info.unlinked_slot = mcl::bit_cast<u64>(current_address_space.OdUnlinkedSlot());
        info.relink_maintained = current_address_space.OdRelinkMaintained();
        return info;
    }

    std::vector<std::string> Disassemble() const {
        ASSERT_FALSE("Unimplemented");
    }

private:
    // Omnidroid patch 0112: this thread's values, where generated code reads them (see
    // `A64JitState`). `conf` is this jit's own copy, so `od_conf` lives as long as the jit; the
    // monitor slots are this processor's (`processor_id`), and only the lock and the slot scan --
    // the same for every thread of a monitor -- are still immediates.
    void OdFillThreadValues() {
        current_state.od_callbacks = reinterpret_cast<u64>(conf.callbacks);
        current_state.od_conf = reinterpret_cast<u64>(&conf);
        current_state.od_tpidr_el0 = reinterpret_cast<u64>(conf.tpidr_el0);
        current_state.od_tpidrro_el0 = reinterpret_cast<u64>(conf.tpidrro_el0);
        current_state.od_exclusive_address = conf.global_monitor
                                               ? reinterpret_cast<u64>(GetExclusiveMonitorAddressPointer(conf.global_monitor, conf.processor_id))
                                               : 0;
        current_state.od_exclusive_value = conf.global_monitor
                                             ? reinterpret_cast<u64>(GetExclusiveMonitorValuePointer(conf.global_monitor, conf.processor_id))
                                             : 0;
        current_state.od_thread = 0;
    }

    void PerformRequestedCacheInvalidation(HaltReason hr) {
        if (Has(hr, HaltReason::CacheInvalidation)) {
            std::unique_lock lock{invalidation_mutex};

            ClearHalt(HaltReason::CacheInvalidation);

            if (invalidate_entire_cache) {
                current_address_space.ClearCache();

                invalidate_entire_cache = false;
                invalid_cache_ranges.clear();
                return;
            }

            if (!invalid_cache_ranges.empty()) {
                current_address_space.InvalidateCacheRanges(invalid_cache_ranges);

                invalid_cache_ranges.clear();
                return;
            }
        }
    }

    A64::UserConfig conf;
    A64JitState current_state{};
    A64AddressSpace current_address_space;
    A64Core core;

    volatile u32 halt_reason = 0;

    std::mutex invalidation_mutex;
    boost::icl::interval_set<u64> invalid_cache_ranges;
    bool invalidate_entire_cache = false;
    bool is_executing = false;
};

Jit::Jit(UserConfig conf)
        : impl{std::make_unique<Jit::Impl>(this, conf)} {
}

Jit::~Jit() = default;

HaltReason Jit::Run() {
    return impl->Run();
}

HaltReason Jit::Step() {
    return impl->Step();
}

void Jit::ClearCache() {
    impl->ClearCache();
}

void Jit::InvalidateCacheRange(std::uint64_t start_address, std::size_t length) {
    impl->InvalidateCacheRange(start_address, length);
}

void Jit::Reset() {
    impl->Reset();
}

void Jit::HaltExecution(HaltReason hr) {
    impl->HaltExecution(hr);
}

void Jit::ClearHalt(HaltReason hr) {
    impl->ClearHalt(hr);
}

std::uint64_t Jit::GetSP() const {
    return impl->SP();
}

void Jit::SetSP(std::uint64_t value) {
    impl->SetSP(value);
}

std::uint64_t Jit::GetPC() const {
    return impl->PC();
}

void Jit::SetPC(std::uint64_t value) {
    impl->SetPC(value);
}

std::uint64_t Jit::GetRegister(std::size_t index) const {
    return impl->Regs()[index];
}

void Jit::SetRegister(size_t index, std::uint64_t value) {
    impl->Regs()[index] = value;
}

std::array<std::uint64_t, 31> Jit::GetRegisters() const {
    return impl->Regs();
}

void Jit::SetRegisters(const std::array<std::uint64_t, 31>& value) {
    impl->Regs() = value;
}

Vector Jit::GetVector(std::size_t index) const {
    auto& vec = impl->VecRegs();
    return {vec[index * 2], vec[index * 2 + 1]};
}

void Jit::SetVector(std::size_t index, Vector value) {
    auto& vec = impl->VecRegs();
    vec[index * 2] = value[0];
    vec[index * 2 + 1] = value[1];
}

std::array<Vector, 32> Jit::GetVectors() const {
    std::array<Vector, 32> ret;
    std::memcpy(ret.data(), impl->VecRegs().data(), sizeof(ret));
    return ret;
}

void Jit::SetVectors(const std::array<Vector, 32>& value) {
    std::memcpy(impl->VecRegs().data(), value.data(), sizeof(value));
}

std::uint32_t Jit::GetFpcr() const {
    return impl->Fpcr();
}

void Jit::SetFpcr(std::uint32_t value) {
    impl->SetFpcr(value);
}

std::uint32_t Jit::GetFpsr() const {
    return impl->Fpsr();
}

void Jit::SetFpsr(std::uint32_t value) {
    impl->SetFpsr(value);
}

std::uint32_t Jit::GetPstate() const {
    return impl->Pstate();
}

void Jit::SetPstate(std::uint32_t value) {
    impl->SetPstate(value);
}

void Jit::ClearExclusiveState() {
    impl->ClearExclusiveState();
}

bool Jit::IsExecuting() const {
    return impl->IsExecuting();
}

void Jit::DumpDisassembly() const {
    impl->DumpDisassembly();
}

std::vector<std::string> Jit::Disassemble() const {
    return impl->Disassemble();
}

std::size_t Jit::OdCodeBytesInUse() const {
    return impl->OdCodeBytesInUse();
}

std::uint64_t Jit::OdCodeBytesEmitted() const {
    return impl->OdCodeBytesEmitted();
}

std::size_t Jit::OdDiscardParked(std::uint64_t resume, const std::atomic<std::uint64_t>& parked) {
    return impl->OdDiscardParked(resume, parked);
}

std::size_t Jit::OdTableBytes() const {
    return impl->OdTableBytes();
}

bool Jit::OdForEachBlock(OdBlockVisitor fn, void* ctx, std::uint64_t resume, const std::atomic<std::uint64_t>* parked) {
    return impl->OdForEachBlock(fn, ctx, resume, parked);
}

void Jit::OdSetEmitHook(OdBlockVisitor fn, void* ctx) {
    impl->OdSetEmitHook(fn, ctx);
}

Jit::OdThreadValues Jit::OdGetThreadValues() const {
    return impl->OdGetThreadValues();
}

Jit::OdBlockLinkInfo Jit::OdBlockLinks(std::uint64_t location, OdLinkVisitor fn, void* ctx) const {
    return impl->OdBlockLinks(location, fn, ctx);
}

}  // namespace Dynarmic::A64
