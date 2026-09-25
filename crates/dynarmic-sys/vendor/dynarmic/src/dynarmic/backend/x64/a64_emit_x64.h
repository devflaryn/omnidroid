/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <array>
#include <map>
#include <memory>
#include <optional>
#include <tuple>

#include "dynarmic/backend/block_range_information.h"
#include "dynarmic/backend/x64/a64_jitstate.h"
#include "dynarmic/backend/x64/devirtualize.h"
#include "dynarmic/backend/x64/emit_x64.h"
#include "dynarmic/frontend/A64/a64_location_descriptor.h"
#include "dynarmic/interface/A64/a64.h"
#include "dynarmic/interface/A64/config.h"
#include "dynarmic/ir/terminal.h"

namespace Dynarmic::Backend::X64 {

class RegAlloc;

struct A64EmitContext final : public EmitContext {
    A64EmitContext(const A64::UserConfig& conf, RegAlloc& reg_alloc, IR::Block& block);

    A64::LocationDescriptor Location() const;
    bool IsSingleStep() const;
    FP::FPCR FPCR(bool fpcr_controlled = true) const override;

    bool HasOptimization(OptimizationFlag flag) const override {
        return conf.HasOptimization(flag);
    }

    const A64::UserConfig& conf;
};

class A64EmitX64 final : public EmitX64 {
public:
    /// `shared` (Omnidroid patch 0022): the code goes into a cache several Jits share, so every
    /// per-thread value is read from JitState at run time and links go through slots; set up
    /// `shared_lock` before emitting any block.
    A64EmitX64(BlockOfCode& code, A64::UserConfig conf, A64::Jit* jit_interface, bool shared = false);
    ~A64EmitX64() override;

    /**
     * Emit host machine code for a basic block with intermediate representation `block`.
     * @note block is modified.
     */
    BlockDescriptor Emit(IR::Block& block);

    void ClearCache() override;

    void InvalidateCacheRanges(const boost::icl::interval_set<u64>& ranges);

    // Omnidroid patch 0022, shared code cache only (the caller holds the cache's lock exclusively).
    /// InvalidateCacheRanges, returning how many blocks it dropped.
    size_t InvalidateCacheRangesCounted(const boost::icl::interval_set<u64>& ranges);
    /// Forget every block: unlink every slot and empty the block map, the link table and the
    /// guest ranges. The code stays where it is (threads may be running it), and so do its
    /// fastmem records (patch 0025: its region's), which a thread faulting in that code still needs.
    size_t ForgetAllBlocks();
    /// Drop the fastmem records of faulting sites in `[begin, end)`: that memory is being given
    /// back and nothing can execute it any more (patch 0025: the region's records, whole).
    void PurgeFastmemPatchInfo(const void* begin, const void* end);
    /// Bytes of one thread's fast-dispatch table, and resetting one (each thread of a shared
    /// cache owns its table; the handler finds it through JitState::od_fast_dispatch_table).
    static size_t FastDispatchTableBytes();
    static void ResetFastDispatchTable(void* table);
    /// The code `table` (one thread's) holds for `descriptor`, or null -- the probe the emitted
    /// fast-dispatch handler makes, callable from the dispatcher's lookup so that a thread finds
    /// what it has already looked up without the cache's lock.
    CodePtr ProbeFastDispatchTable(void* table, u64 descriptor) const;
    /// Record `code` for `descriptor` in `table`, as the emitted handler does on a miss.
    void FillFastDispatchTable(void* table, u64 descriptor, CodePtr code) const;
    /// Omnidroid patch 0024: what each per-block table holds, for a memory report. The caller
    /// holds the cache's lock (shared is enough).
    A64::SharedCodeCache::Tables Census() const;

protected:
    /// Patch 0022: a callback of the thread's UserCallbacks -- an immediate `this` in a Jit with
    /// its own cache, `this` read from JitState in a shared one.
    template<auto mfp>
    ArgCallback UserCallback() const {
        if (shared_code) {
            return DevirtualizeFromJitState<mfp>(conf.callbacks, offsetof(A64JitState, od_callbacks));
        }
        return Devirtualize<mfp>(conf.callbacks);
    }
    /// Patch 0022: `reg = &conf` of the running thread.
    void EmitLoadConfPointer(Xbyak::Reg64 reg);
    /// Patch 0022: `reg` = the running thread's reservation-address / reserved-value slot.
    void EmitLoadExclusiveAddressPointer(Xbyak::Reg64 reg);
    void EmitLoadExclusiveValuePointer(Xbyak::Reg64 reg);
    /// Patch 0022: whether the global monitor's reservation scan may skip the storing
    /// processor's own slot, which is only known at emit time in a Jit with its own cache.
    bool SkipOwnMonitorSlot() const { return !shared_code; }
    /// Patch 0022: `jmp [slot]` to `target`, with the slot's unlinked value -- set the guest PC and
    /// enter the dispatcher -- emitted right after it.
    void EmitSlotJump(const IR::LocationDescriptor& target);

    const A64::UserConfig conf;
    A64::Jit* jit_interface;
    BlockRangeInformation<u64> block_ranges;

    struct FastDispatchEntry {
        u64 location_descriptor = 0xFFFF'FFFF'FFFF'FFFFull;
        const void* code_ptr = nullptr;
    };
    static_assert(sizeof(FastDispatchEntry) == 0x10);
    // Omnidroid patch 0019: 4,096 entries (64 KiB), not 1,048,576 (16 MiB). The table is per
    // guest thread and FastDispatch is now on (the handler checks the budget and the halt flag),
    // so its size is paid by every thread; a miss costs what the dispatcher costs anyway (D35).
    static constexpr u64 fast_dispatch_table_mask = 0xFFF0;
    static constexpr size_t fast_dispatch_table_size = 0x1000;
    static_assert(fast_dispatch_table_mask == (fast_dispatch_table_size - 1) * sizeof(FastDispatchEntry));
    // Omnidroid patch 0017: allocated only when FastDispatch is enabled. As a by-value member it
    // was 16 MiB constructed (and written: the entries have a non-zero initializer) in every Jit.
    std::unique_ptr<std::array<FastDispatchEntry, fast_dispatch_table_size>> fast_dispatch_table;
    void ClearFastDispatchTable();

    void (*memory_read_128)();
    void (*memory_write_128)();
    void (*memory_exclusive_write_128)();
    void GenMemory128Accessors();

    std::map<std::tuple<bool, size_t, int, int>, void (*)()> read_fallbacks;
    std::map<std::tuple<bool, size_t, int, int>, void (*)()> write_fallbacks;
    std::map<std::tuple<bool, size_t, int, int>, void (*)()> exclusive_write_fallbacks;
    void GenFastmemFallbacks();

    const void* terminal_handler_pop_rsb_hint;

    // Omnidroid patch 0022, shared code cache only: every SVC callback is called from the prelude,
    // not from its block, so that a thread parked inside one has no return address into a region
    // on its stack. The block jumps to `svc_trampoline` with its resume address in rax and the
    // immediate in the callback's second argument; the trampoline publishes the resume address in
    // `JitState::od_callback_return`, calls, then takes it back with an `xchg` (clearing it) and
    // jumps there. A reclaimer that finds a parked thread's resume address in a retiring region
    // swaps it -- one compare-exchange -- for `svc_resume_retired`, which leaves the run as the
    // block's own halt test would have: the region is then free of that thread.
    const void* svc_trampoline = nullptr;
    const void* svc_resume_retired = nullptr;
    void GenSharedSvcTrampolines();

public:
    /// Shared code cache only: where a parked thread is sent instead of a retired region.
    const void* SvcResumeRetired() const { return svc_resume_retired; }

protected:
    const void* terminal_handler_fast_dispatch_hint = nullptr;
    FastDispatchEntry& (*fast_dispatch_table_lookup)(u64) = nullptr;
    /// Patch 0022: the same hash, for a table given as the second argument (a thread's own).
    FastDispatchEntry& (*fast_dispatch_table_lookup_in)(u64, void*) = nullptr;
    void GenTerminalHandlers();

    // Microinstruction emitters
    void EmitPushRSB(EmitContext& ctx, IR::Inst* inst);
#define OPCODE(...)
#define A32OPC(...)
#define A64OPC(name, type, ...) void EmitA64##name(A64EmitContext& ctx, IR::Inst* inst);
#include "dynarmic/ir/opcodes.inc"
#undef OPCODE
#undef A32OPC
#undef A64OPC

    // Helpers
    std::string LocationDescriptorToFriendlyName(const IR::LocationDescriptor&) const override;

    // Fastmem information
    using DoNotFastmemMarker = std::tuple<IR::LocationDescriptor, unsigned>;
    struct FastmemPatchInfo {
        u64 resume_rip;
        u64 callback;
        DoNotFastmemMarker marker;
        bool recompile;
    };
    tsl::robin_map<u64, FastmemPatchInfo> fastmem_patch_info;
    std::set<DoNotFastmemMarker> do_not_fastmem;
    std::optional<DoNotFastmemMarker> ShouldFastmem(A64EmitContext& ctx, IR::Inst* inst) const;
    FakeCall FastmemCallback(u64 rip);
    /// Omnidroid patch 0025: record a fastmem patch site -- in `fastmem_patch_info`, or, in a
    /// shared code cache, as a sorted record of its region.
    void RecordFastmemSite(u64 site, u64 resume, u64 callback, const DoNotFastmemMarker& marker, bool recompile);

    // Memory access helpers
    void EmitCheckMemoryAbort(A64EmitContext& ctx, IR::Inst* inst, Xbyak::Label* end = nullptr);
    template<std::size_t bitsize, auto callback>
    void EmitMemoryRead(A64EmitContext& ctx, IR::Inst* inst);
    template<std::size_t bitsize, auto callback>
    void EmitMemoryWrite(A64EmitContext& ctx, IR::Inst* inst);
    template<std::size_t bitsize, auto callback>
    void EmitExclusiveReadMemory(A64EmitContext& ctx, IR::Inst* inst);
    template<std::size_t bitsize, auto callback>
    void EmitExclusiveWriteMemory(A64EmitContext& ctx, IR::Inst* inst);
    template<std::size_t bitsize, auto callback>
    void EmitExclusiveReadMemoryInline(A64EmitContext& ctx, IR::Inst* inst);
    template<std::size_t bitsize, auto callback>
    void EmitExclusiveWriteMemoryInline(A64EmitContext& ctx, IR::Inst* inst);

    // Terminal instruction emitters
    void EmitTerminalImpl(IR::Term::Interpret terminal, IR::LocationDescriptor initial_location, bool is_single_step) override;
    void EmitTerminalImpl(IR::Term::ReturnToDispatch terminal, IR::LocationDescriptor initial_location, bool is_single_step) override;
    void EmitTerminalImpl(IR::Term::LinkBlock terminal, IR::LocationDescriptor initial_location, bool is_single_step) override;
    void EmitTerminalImpl(IR::Term::LinkBlockFast terminal, IR::LocationDescriptor initial_location, bool is_single_step) override;
    void EmitTerminalImpl(IR::Term::PopRSBHint terminal, IR::LocationDescriptor initial_location, bool is_single_step) override;
    void EmitTerminalImpl(IR::Term::FastDispatchHint terminal, IR::LocationDescriptor initial_location, bool is_single_step) override;
    void EmitTerminalImpl(IR::Term::If terminal, IR::LocationDescriptor initial_location, bool is_single_step) override;
    void EmitTerminalImpl(IR::Term::CheckBit terminal, IR::LocationDescriptor initial_location, bool is_single_step) override;
    void EmitTerminalImpl(IR::Term::CheckHalt terminal, IR::LocationDescriptor initial_location, bool is_single_step) override;

    // Patching
    void Unpatch(const IR::LocationDescriptor& target_desc) override;
    void EmitPatchJg(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr = nullptr) override;
    void EmitPatchJz(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr = nullptr) override;
    void EmitPatchJmp(const IR::LocationDescriptor& target_desc, CodePtr target_code_ptr = nullptr) override;
    void EmitPatchMovRcx(CodePtr target_code_ptr = nullptr) override;
};

}  // namespace Dynarmic::Backend::X64
