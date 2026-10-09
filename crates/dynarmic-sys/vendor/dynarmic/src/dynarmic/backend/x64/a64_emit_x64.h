/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <array>
#include <atomic>
#include <cstdint>
#include <map>
#include <memory>
#include <optional>
#include <tuple>
#include <vector>

#include <boost/icl/interval_set.hpp>
#include <tsl/robin_map.h>
#include <tsl/robin_set.h>

#include "dynarmic/backend/x64/a64_jitstate.h"
#include "dynarmic/backend/x64/devirtualize.h"
#include "dynarmic/backend/x64/emit_x64.h"
#include "dynarmic/frontend/A64/a64_location_descriptor.h"
#include "dynarmic/interface/A64/a64.h"
#include "dynarmic/interface/A64/config.h"
#include "dynarmic/ir/terminal.h"

namespace Dynarmic::Backend::X64 {

class RegAlloc;

/// Omnidroid patch 0034: unsafe floating-point optimization flags (`Unsafe_UnfuseFMA`,
/// `Unsafe_ReducedErrorFP`, `Unsafe_InaccurateNaN`, `Unsafe_IgnoreStandardFPCRValue`) the host
/// turns on for the whole process, at run time: a block emitted after the switch uses them, a
/// block emitted before keeps what it was emitted with (the host clears the cache to have every
/// block again). Honoured only where the config's `unsafe_optimizations` gate is open.
extern std::atomic<std::uint32_t> live_fp_optimizations;
inline constexpr std::uint32_t live_fp_optimizations_allowed = 0x000F0000;

/// Omnidroid patch 0037: non-zero runs `GetSetElimination` in its precise form for every block
/// translated from now on whose config sets `check_halt_on_memory_access` (which otherwise skips
/// the pass). Process-wide, read at translation: a block translated before the switch keeps what
/// it was translated with.
extern std::atomic<std::uint32_t> live_precise_get_set;

/// Omnidroid patch 0042: non-zero emits the hit paths of the return-stack buffer and the
/// fast-dispatch table inside each `RET`/`BR`/`BLR` block, from the target PC still in a register,
/// instead of a jump to one shared handler that reloads it from `JitState`. Process-wide, read
/// when a block is emitted.
extern std::atomic<std::uint32_t> live_fast_dispatch_inline;

struct A64EmitContext final : public EmitContext {
    A64EmitContext(const A64::UserConfig& conf, RegAlloc& reg_alloc, IR::Block& block);

    A64::LocationDescriptor Location() const;
    bool IsSingleStep() const;
    FP::FPCR FPCR(bool fpcr_controlled = true) const override;

    bool HasOptimization(OptimizationFlag flag) const override {
        const auto bits = static_cast<std::uint32_t>(flag) & live_fp_optimizations_allowed;
        if (bits != 0 && conf.unsafe_optimizations && (live_fp_optimizations.load(std::memory_order_relaxed) & bits) != 0) {
            return true;
        }
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
    /// Omnidroid patch 0028: forget the blocks whose code is in `[begin, end)` -- a region being
    /// given back, oldest first -- as an invalidation would (their links in and out undone), and
    /// append their locations to `forgotten`. The candidates are the blocks registered with guest
    /// range serials `[first_range, end_range)`: the ones emitted while that region was being
    /// filled. A location emitted again elsewhere since keeps its newer block.
    size_t ForgetRegionBlocks(const void* begin, const void* end, u32 first_range, u32 end_range, std::vector<u64>& forgotten);
    /// Patch 0028: the serial of the next guest range registered -- one per emitted block.
    u32 NextRangeSerial() const { return range_base + static_cast<u32>(guest_ranges.size()); }
    /// Patch 0028: drop the guest ranges below serial `base` (with their page-index entries):
    /// every one names a block forgotten with the region it was emitted into.
    void TrimGuestRanges(u32 base);
    /// Drop the fastmem records of faulting sites in `[begin, end)`: that memory is being given
    /// back and nothing can execute it any more (patch 0025: the region's records, whole).
    void PurgeFastmemPatchInfo(const void* begin, const void* end);
    /// Bytes of one thread's fast-dispatch table, and resetting one (each thread of a shared
    /// cache owns its table; the handler finds it through JitState::od_fast_dispatch_table).
    static size_t FastDispatchTableBytes();
    static void ResetFastDispatchTable(void* table);
    /// Omnidroid patch 0035: the entries a thread's table has under `conf` (its
    /// `od_fast_dispatch_entries`, or the pin's `fast_dispatch_table_size` for 0 or a value that is
    /// not a power of two from 0x40 to 0x10000), and the table's bytes and reset at that size.
    static size_t FastDispatchEntries(const A64::UserConfig& conf);
    static size_t FastDispatchTableBytes(size_t entries);
    static void ResetFastDispatchTable(void* table, size_t entries);
    /// The code `table` (one thread's) holds for `descriptor`, or null -- the probe the emitted
    /// fast-dispatch handler makes, callable from the dispatcher's lookup so that a thread finds
    /// what it has already looked up without the cache's lock.
    CodePtr ProbeFastDispatchTable(void* table, u64 descriptor) const;
    /// Record `code` for `descriptor` in `table`, as the emitted handler does on a miss.
    void FillFastDispatchTable(void* table, u64 descriptor, CodePtr code) const;
    /// Omnidroid patch 0024: what each per-block table holds, for a memory report. The caller
    /// holds the cache's lock (shared is enough).
    A64::SharedCodeCache::Tables Census() const;
    /// Omnidroid patch 0036: `SharedCodeCache::GuestPcsOf`. The caller holds the cache's lock
    /// (shared is enough).
    void GuestPcsOf(const u64* hosts, size_t count, u64* guest_pcs) const;

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

    /// Omnidroid patch 0027: a shared cache's block map load factor (robin_map's default is 0.5).
    static constexpr float SHARED_BLOCK_MAP_LOAD_FACTOR = 0.75f;

    const A64::UserConfig conf;
    A64::Jit* jit_interface;

    // Omnidroid patch 0026: the guest bytes each emitted block was translated from, for
    // `InvalidateCacheRanges`. The pin kept them in a `BlockRangeInformation` -- a boost::icl
    // interval_map of std::sets, about 150 bytes per block in tree nodes -- as the arm64 backend
    // did before patch 0011. One `GuestRange` per emitted block (24 bytes), indexed by the 4 KiB
    // guest pages it covers; as with the pin's, a range stays until the cache is cleared (or, in a
    // shared cache, until every block is forgotten).
    //
    // Patch 0028: every emitted block has one, in emission order -- an empty range too, kept out of
    // the index -- so a shared cache finds the blocks of a region it gives back among the ranges
    // registered while it filled that region, and drops those ranges (the oldest) from the front.
    // Indices are serials: range `i` is `guest_ranges[i - range_base]`.
    struct GuestRange {
        IR::LocationDescriptor location;
        u64 first;  ///< The first guest byte, `closed(first, last)` as the pin registered it.
        u64 last;
    };
    static constexpr unsigned guest_page_bits = 12;
    /// A block covering more pages than this is kept in `wide_guest_ranges`, checked on every
    /// invalidation, instead of in every page it covers.
    static constexpr u64 max_indexed_pages = 64;
    std::vector<GuestRange> guest_ranges;
    u32 range_base = 0;
    const GuestRange& RangeAt(u32 serial) const { return guest_ranges[serial - range_base]; }
    tsl::robin_map<u64, std::vector<u32>> guest_range_pages;
    std::vector<u32> wide_guest_ranges;
    void AddGuestRange(IR::LocationDescriptor location, u64 first, u64 last);
    /// Every location registered with a range intersecting one of `ranges` -- what
    /// `BlockRangeInformation::InvalidateRanges` returned.
    tsl::robin_set<IR::LocationDescriptor> GuestRangeLocations(const boost::icl::interval_set<u64>& ranges) const;
    /// Empties the ranges and gives their memory back.
    void ClearGuestRanges();

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
    /// Omnidroid patch 0035: what the emitted handler and probe mask a hash with -- this cache's
    /// threads' table size (`FastDispatchEntries(conf)`) in a shared cache, the pin's otherwise.
    u64 fast_dispatch_mask = fast_dispatch_table_mask;
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
    /// Omnidroid patch 0042: the inline hit paths (`live_fast_dispatch_inline`) fall into the
    /// shared handler's tail: at the table probe with the location in rbx (an RSB miss), and at the
    /// lookup with rbx the location and rbp the table entry (a table miss).
    const void* terminal_handler_fast_dispatch_probe = nullptr;
    const void* terminal_handler_fast_dispatch_miss = nullptr;
    /// Patch 0042: the block being emitted ends in `SetPC` and a dispatch hint, and its last
    /// instruction left the target PC in rbp as well as in `JitState::pc`.
    bool od_pc_in_rbp = false;
    void EmitInlineLocation(const IR::LocationDescriptor& initial_location);
    void EmitInlineBudgetAndHaltChecks();
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
