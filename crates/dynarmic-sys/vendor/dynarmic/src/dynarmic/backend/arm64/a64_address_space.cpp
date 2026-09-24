/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include "dynarmic/backend/arm64/a64_address_space.h"

#include <limits>

#include "dynarmic/backend/arm64/a64_jitstate.h"
#include "dynarmic/backend/arm64/abi.h"
#include "dynarmic/backend/arm64/devirtualize.h"
#include "dynarmic/backend/arm64/emit_arm64.h"
#include "dynarmic/backend/arm64/stack_layout.h"
#include "dynarmic/common/cast_util.h"
#include "dynarmic/frontend/A64/a64_location_descriptor.h"
#include "dynarmic/frontend/A64/translate/a64_translate.h"
#include "dynarmic/interface/A64/config.h"
#include "dynarmic/interface/exclusive_monitor.h"
#include "dynarmic/ir/opt/passes.h"

namespace Dynarmic::Backend::Arm64 {

template<auto mfp, typename T>
static void* EmitCallTrampoline(oaknut::CodeGenerator& code, T* this_) {
    using namespace oaknut::util;

    const auto info = Devirtualize<mfp>(this_);

    oaknut::Label l_addr, l_this;

    void* target = code.xptr<void*>();
    code.LDR(X0, l_this);
    code.LDR(Xscratch0, l_addr);
    code.BR(Xscratch0);

    code.align(8);
    code.l(l_this);
    code.dx(info.this_ptr);
    code.l(l_addr);
    code.dx(info.fn_ptr);

    return target;
}

template<auto mfp, typename T>
static void* EmitWrappedReadCallTrampoline(oaknut::CodeGenerator& code, T* this_) {
    using namespace oaknut::util;

    const auto info = Devirtualize<mfp>(this_);

    oaknut::Label l_addr, l_this;

    constexpr u64 save_regs = ABI_CALLER_SAVE & ~ToRegList(Xscratch0);

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, save_regs, 0);
    code.LDR(X0, l_this);
    code.MOV(X1, Xscratch0);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    code.MOV(Xscratch0, X0);
    ABI_PopRegisters(code, save_regs, 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(info.this_ptr);
    code.l(l_addr);
    code.dx(info.fn_ptr);

    return target;
}

template<auto callback, typename T>
static void* EmitExclusiveReadCallTrampoline(oaknut::CodeGenerator& code, const A64::UserConfig& conf) {
    using namespace oaknut::util;

    oaknut::Label l_addr, l_this;

    auto fn = [](const A64::UserConfig& conf, A64::VAddr vaddr) -> T {
        return conf.global_monitor->ReadAndMark<T>(conf.processor_id, vaddr, [&]() -> T {
            return (conf.callbacks->*callback)(vaddr);
        });
    };

    void* target = code.xptr<void*>();
    code.LDR(X0, l_this);
    code.LDR(Xscratch0, l_addr);
    code.BR(Xscratch0);

    code.align(8);
    code.l(l_this);
    code.dx(mcl::bit_cast<u64>(&conf));
    code.l(l_addr);
    code.dx(mcl::bit_cast<u64>(Common::FptrCast(fn)));

    return target;
}

template<auto mfp, typename T>
static void* EmitWrappedWriteCallTrampoline(oaknut::CodeGenerator& code, T* this_) {
    using namespace oaknut::util;

    const auto info = Devirtualize<mfp>(this_);

    oaknut::Label l_addr, l_this;

    constexpr u64 save_regs = ABI_CALLER_SAVE;

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, save_regs, 0);
    code.LDR(X0, l_this);
    code.MOV(X1, Xscratch0);
    code.MOV(X2, Xscratch1);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    ABI_PopRegisters(code, save_regs, 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(info.this_ptr);
    code.l(l_addr);
    code.dx(info.fn_ptr);

    return target;
}

template<auto callback, typename T>
static void* EmitExclusiveWriteCallTrampoline(oaknut::CodeGenerator& code, const A64::UserConfig& conf) {
    using namespace oaknut::util;

    oaknut::Label l_addr, l_this;

    auto fn = [](const A64::UserConfig& conf, A64::VAddr vaddr, T value) -> u32 {
        return conf.global_monitor->DoExclusiveOperation<T>(conf.processor_id, vaddr,
                                                            [&](T expected) -> bool {
                                                                return (conf.callbacks->*callback)(vaddr, value, expected);
                                                            })
                 ? 0
                 : 1;
    };

    void* target = code.xptr<void*>();
    code.LDR(X0, l_this);
    code.LDR(Xscratch0, l_addr);
    code.BR(Xscratch0);

    code.align(8);
    code.l(l_this);
    code.dx(mcl::bit_cast<u64>(&conf));
    code.l(l_addr);
    code.dx(mcl::bit_cast<u64>(Common::FptrCast(fn)));

    return target;
}

// Omnidroid patch 0007: the fallbacks of the inline exclusive accesses (emit_arm64_memory.cpp), in the
// Wrapped* calling convention -- address in Xscratch0, value in Xscratch1 (Q0 for 128 bits), result in
// Xscratch0 (Q0), every other caller-saved register preserved -- and through the global monitor
// exactly as the Exclusive* trampolines above, so a fallback is the callback-only path, nothing less.
// Values cross as u64 and are narrowed in C++: Apple's arm64 ABI has the *caller* extend sub-32-bit
// arguments, which generated code does not promise.
template<auto callback, typename T>
static void* EmitWrappedExclusiveReadCallTrampoline(oaknut::CodeGenerator& code, const A64::UserConfig& conf) {
    using namespace oaknut::util;

    oaknut::Label l_addr, l_this;

    auto fn = [](const A64::UserConfig& conf, A64::VAddr vaddr) -> u64 {
        return conf.global_monitor->ReadAndMark<T>(conf.processor_id, vaddr, [&]() -> T {
            return (conf.callbacks->*callback)(vaddr);
        });
    };

    constexpr u64 save_regs = ABI_CALLER_SAVE & ~ToRegList(Xscratch0);

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, save_regs, 0);
    code.LDR(X0, l_this);
    code.MOV(X1, Xscratch0);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    code.MOV(Xscratch0, X0);
    ABI_PopRegisters(code, save_regs, 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(mcl::bit_cast<u64>(&conf));
    code.l(l_addr);
    code.dx(mcl::bit_cast<u64>(Common::FptrCast(fn)));

    return target;
}

static void* EmitWrappedExclusiveRead128CallTrampoline(oaknut::CodeGenerator& code, const A64::UserConfig& conf) {
    using namespace oaknut::util;

    oaknut::Label l_addr, l_this;

    auto fn = [](const A64::UserConfig& conf, A64::VAddr vaddr) -> Vector {
        return conf.global_monitor->ReadAndMark<Vector>(conf.processor_id, vaddr, [&]() -> Vector {
            return conf.callbacks->MemoryRead128(vaddr);
        });
    };

    constexpr u64 save_regs = ABI_CALLER_SAVE & ~ToRegList(Q0);

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, save_regs, 0);
    code.LDR(X0, l_this);
    code.MOV(X1, Xscratch0);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    code.FMOV(D0, X0);
    code.FMOV(V0.D()[1], X1);
    ABI_PopRegisters(code, save_regs, 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(mcl::bit_cast<u64>(&conf));
    code.l(l_addr);
    code.dx(mcl::bit_cast<u64>(Common::FptrCast(fn)));

    return target;
}

template<auto callback, typename T>
static void* EmitWrappedExclusiveWriteCallTrampoline(oaknut::CodeGenerator& code, const A64::UserConfig& conf) {
    using namespace oaknut::util;

    oaknut::Label l_addr, l_this;

    auto fn = [](const A64::UserConfig& conf, A64::VAddr vaddr, u64 value) -> u64 {
        return conf.global_monitor->DoExclusiveOperation<T>(conf.processor_id, vaddr,
                                                            [&](T expected) -> bool {
                                                                return (conf.callbacks->*callback)(vaddr, static_cast<T>(value), expected);
                                                            })
                 ? 0
                 : 1;
    };

    constexpr u64 save_regs = ABI_CALLER_SAVE & ~ToRegList(Xscratch0);

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, save_regs, 0);
    code.LDR(X0, l_this);
    code.MOV(X1, Xscratch0);
    code.MOV(X2, Xscratch1);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    code.MOV(Xscratch0, X0);
    ABI_PopRegisters(code, save_regs, 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(mcl::bit_cast<u64>(&conf));
    code.l(l_addr);
    code.dx(mcl::bit_cast<u64>(Common::FptrCast(fn)));

    return target;
}

static void* EmitWrappedExclusiveWrite128CallTrampoline(oaknut::CodeGenerator& code, const A64::UserConfig& conf) {
    using namespace oaknut::util;

    oaknut::Label l_addr, l_this;

    auto fn = [](const A64::UserConfig& conf, A64::VAddr vaddr, u64 value_lo, u64 value_hi) -> u64 {
        const Vector value{value_lo, value_hi};
        return conf.global_monitor->DoExclusiveOperation<Vector>(conf.processor_id, vaddr,
                                                                 [&](Vector expected) -> bool {
                                                                     return conf.callbacks->MemoryWriteExclusive128(vaddr, value, expected);
                                                                 })
                 ? 0
                 : 1;
    };

    constexpr u64 save_regs = ABI_CALLER_SAVE & ~ToRegList(Xscratch0);

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, save_regs, 0);
    code.LDR(X0, l_this);
    code.MOV(X1, Xscratch0);
    code.FMOV(X2, D0);
    code.FMOV(X3, V0.D()[1]);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    code.MOV(Xscratch0, X0);
    ABI_PopRegisters(code, save_regs, 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(mcl::bit_cast<u64>(&conf));
    code.l(l_addr);
    code.dx(mcl::bit_cast<u64>(Common::FptrCast(fn)));

    return target;
}

static void* EmitRead128CallTrampoline(oaknut::CodeGenerator& code, A64::UserCallbacks* this_) {
    using namespace oaknut::util;

    const auto info = Devirtualize<&A64::UserCallbacks::MemoryRead128>(this_);

    oaknut::Label l_addr, l_this;

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, (1ull << 29) | (1ull << 30), 0);
    code.LDR(X0, l_this);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    code.FMOV(D0, X0);
    code.FMOV(V0.D()[1], X1);
    ABI_PopRegisters(code, (1ull << 29) | (1ull << 30), 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(info.this_ptr);
    code.l(l_addr);
    code.dx(info.fn_ptr);

    return target;
}

static void* EmitWrappedRead128CallTrampoline(oaknut::CodeGenerator& code, A64::UserCallbacks* this_) {
    using namespace oaknut::util;

    const auto info = Devirtualize<&A64::UserCallbacks::MemoryRead128>(this_);

    oaknut::Label l_addr, l_this;

    constexpr u64 save_regs = ABI_CALLER_SAVE & ~ToRegList(Q0);

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, save_regs, 0);
    code.LDR(X0, l_this);
    code.MOV(X1, Xscratch0);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    code.FMOV(D0, X0);
    code.FMOV(V0.D()[1], X1);
    ABI_PopRegisters(code, save_regs, 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(info.this_ptr);
    code.l(l_addr);
    code.dx(info.fn_ptr);

    return target;
}

static void* EmitExclusiveRead128CallTrampoline(oaknut::CodeGenerator& code, const A64::UserConfig& conf) {
    using namespace oaknut::util;

    oaknut::Label l_addr, l_this;

    auto fn = [](const A64::UserConfig& conf, A64::VAddr vaddr) -> Vector {
        return conf.global_monitor->ReadAndMark<Vector>(conf.processor_id, vaddr, [&]() -> Vector {
            return conf.callbacks->MemoryRead128(vaddr);
        });
    };

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, (1ull << 29) | (1ull << 30), 0);
    code.LDR(X0, l_this);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    code.FMOV(D0, X0);
    code.FMOV(V0.D()[1], X1);
    ABI_PopRegisters(code, (1ull << 29) | (1ull << 30), 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(mcl::bit_cast<u64>(&conf));
    code.l(l_addr);
    code.dx(mcl::bit_cast<u64>(Common::FptrCast(fn)));

    return target;
}

static void* EmitWrite128CallTrampoline(oaknut::CodeGenerator& code, A64::UserCallbacks* this_) {
    using namespace oaknut::util;

    const auto info = Devirtualize<&A64::UserCallbacks::MemoryWrite128>(this_);

    oaknut::Label l_addr, l_this;

    void* target = code.xptr<void*>();
    code.LDR(X0, l_this);
    code.FMOV(X2, D0);
    code.FMOV(X3, V0.D()[1]);
    code.LDR(Xscratch0, l_addr);
    code.BR(Xscratch0);

    code.align(8);
    code.l(l_this);
    code.dx(info.this_ptr);
    code.l(l_addr);
    code.dx(info.fn_ptr);

    return target;
}

static void* EmitWrappedWrite128CallTrampoline(oaknut::CodeGenerator& code, A64::UserCallbacks* this_) {
    using namespace oaknut::util;

    const auto info = Devirtualize<&A64::UserCallbacks::MemoryWrite128>(this_);

    oaknut::Label l_addr, l_this;

    constexpr u64 save_regs = ABI_CALLER_SAVE;

    void* target = code.xptr<void*>();
    ABI_PushRegisters(code, save_regs, 0);
    code.LDR(X0, l_this);
    code.MOV(X1, Xscratch0);
    code.FMOV(X2, D0);
    code.FMOV(X3, V0.D()[1]);
    code.LDR(Xscratch0, l_addr);
    code.BLR(Xscratch0);
    ABI_PopRegisters(code, save_regs, 0);
    code.RET();

    code.align(8);
    code.l(l_this);
    code.dx(info.this_ptr);
    code.l(l_addr);
    code.dx(info.fn_ptr);

    return target;
}

static void* EmitExclusiveWrite128CallTrampoline(oaknut::CodeGenerator& code, const A64::UserConfig& conf) {
    using namespace oaknut::util;

    oaknut::Label l_addr, l_this;

    auto fn = [](const A64::UserConfig& conf, A64::VAddr vaddr, Vector value) -> u32 {
        return conf.global_monitor->DoExclusiveOperation<Vector>(conf.processor_id, vaddr,
                                                                 [&](Vector expected) -> bool {
                                                                     return conf.callbacks->MemoryWriteExclusive128(vaddr, value, expected);
                                                                 })
                 ? 0
                 : 1;
    };

    void* target = code.xptr<void*>();
    code.LDR(X0, l_this);
    code.FMOV(X2, D0);
    code.FMOV(X3, V0.D()[1]);
    code.LDR(Xscratch0, l_addr);
    code.BR(Xscratch0);

    code.align(8);
    code.l(l_this);
    code.dx(mcl::bit_cast<u64>(&conf));
    code.l(l_addr);
    code.dx(mcl::bit_cast<u64>(Common::FptrCast(fn)));

    return target;
}

A64AddressSpace::A64AddressSpace(const A64::UserConfig& conf)
        : AddressSpace(conf.code_cache_size)
        , conf(conf) {
    EmitPrelude();
}

IR::Block A64AddressSpace::GenerateIR(IR::LocationDescriptor descriptor) const {
    const auto get_code = [this](u64 vaddr) { return conf.callbacks->MemoryReadCode(vaddr); };
    IR::Block ir_block = A64::Translate(A64::LocationDescriptor{descriptor}, get_code,
                                        {conf.define_unpredictable_behaviour, conf.wall_clock_cntpct});

    Optimization::A64CallbackConfigPass(ir_block, conf);
    Optimization::NamingPass(ir_block);
    if (conf.HasOptimization(OptimizationFlag::GetSetElimination) && !conf.check_halt_on_memory_access) {
        Optimization::A64GetSetElimination(ir_block);
        Optimization::DeadCodeElimination(ir_block);
    }
    if (conf.HasOptimization(OptimizationFlag::ConstProp)) {
        Optimization::ConstantPropagation(ir_block);
        Optimization::DeadCodeElimination(ir_block);
    }
    if (conf.HasOptimization(OptimizationFlag::MiscIROpt)) {
        Optimization::A64MergeInterpretBlocksPass(ir_block, conf.callbacks);
    }
    Optimization::VerificationPass(ir_block);

    return ir_block;
}

void A64AddressSpace::InvalidateCacheRanges(const boost::icl::interval_set<u64>& ranges) {
    // Omnidroid patch 0011: every location registered with a range that intersects one of
    // `ranges` -- what `BlockRangeInformation::InvalidateRanges` returned.
    tsl::robin_set<IR::LocationDescriptor> erase_locations;
    for (const auto& interval : ranges) {
        const u64 first = boost::icl::first(interval);
        const u64 last = boost::icl::last(interval);
        const auto consider = [&](u32 index) {
            invalidation_ranges_checked.fetch_add(1, std::memory_order_relaxed);
            GuestRange& range = guest_ranges[index];
            if (!range.dead && range.first <= last && first <= range.last) {
                erase_locations.insert(range.location);
                range.dead = true;  // patch 0016
            }
        };
        // Patch 0016: consider every index of one page's list, then drop the dead ones from it.
        // Answers whether the list is now empty, so that the caller can drop the page.
        const auto consider_list = [&](std::vector<u32>& indices) {
            for (const u32 index : indices) {
                consider(index);
            }
            std::erase_if(indices, [&](u32 index) { return guest_ranges[index].dead; });
            return indices.empty();
        };

        for (const u32 index : wide_guest_ranges) {
            consider(index);
        }
        std::erase_if(wide_guest_ranges, [&](u32 index) { return guest_ranges[index].dead; });

        const u64 first_page = first >> guest_page_bits;
        const u64 last_page = last >> guest_page_bits;
        // Omnidroid patch 0015: only the chunks that hold translated pages are walked, page by page;
        // `probes` counts the page lookups for `od_invalidation_page_probes`.
        u64 probes = 0;
        const auto walk_pages = [&](u64 from_page, u64 to_page) {
            for (u64 page = from_page;; ++page) {
                ++probes;
                if (auto iter = guest_range_pages.find(page); iter != guest_range_pages.end()) {
                    if (consider_list(iter.value())) {
                        guest_range_pages.erase(iter);
                    }
                }
                if (page == to_page) {
                    break;
                }
            }
        };
        constexpr unsigned chunk_shift = guest_chunk_bits - guest_page_bits;
        const auto walk_chunk = [&](u64 chunk) {
            const u64 chunk_first = chunk << chunk_shift;
            const u64 chunk_last = chunk_first + ((u64{1} << chunk_shift) - 1);
            walk_pages(std::max(first_page, chunk_first), std::min(last_page, chunk_last));
        };
        const u64 first_chunk = first_page >> chunk_shift;
        const u64 last_chunk = last_page >> chunk_shift;
        if (last_page - first_page >= guest_range_pages.size()) {
            // More pages asked about than have anything on them: walk what there is.
            for (auto iter = guest_range_pages.begin(); iter != guest_range_pages.end();) {
                ++probes;
                const u64 page = iter->first;
                if (page >= first_page && page <= last_page && consider_list(iter.value())) {
                    iter = guest_range_pages.erase(iter);
                } else {
                    ++iter;
                }
            }
        } else if (last_chunk - first_chunk >= guest_range_chunks.size()) {
            for (const u64 chunk : guest_range_chunks) {
                if (chunk >= first_chunk && chunk <= last_chunk) {
                    walk_chunk(chunk);
                }
            }
        } else {
            for (u64 chunk = first_chunk;; ++chunk) {
                if (guest_range_chunks.contains(chunk)) {
                    walk_chunk(chunk);
                }
                if (chunk == last_chunk) {
                    break;
                }
            }
        }
        invalidation_page_probes.fetch_add(probes, std::memory_order_relaxed);
    }
    InvalidateBasicBlocks(erase_locations);

    // Omnidroid patch 0012: an invalidation that has left no block standing is a clear. This runs
    // only from `Jit::Impl::PerformRequestedCacheInvalidation`, before or after `RunCode` -- never
    // with generated code on the stack -- so no invalidated block can still be executing (the
    // return stack buffer is rebuilt on every entry), and nothing links to one: every link to an
    // invalidated location was pointed back at the dispatcher. What the invalidated blocks still
    // hold -- their records, and their code in the cache -- can never be used again, which is
    // what `ClearCache` gives back. The pin kept both until the cache filled.
    if (block_entries.empty()) {
        ClearCache();
    }
}

void A64AddressSpace::ClearCache() {
    AddressSpace::ClearCache();
    // Omnidroid patch 0011: the pin's `A64AddressSpace` never cleared `block_ranges`. Every block
    // is gone, so nothing registered before the clear can name a block that exists: a location
    // translated again afterwards registers the range of its new translation, which is the range
    // its code now depends on.
    decltype(guest_ranges){}.swap(guest_ranges);
    guest_range_pages = {};
    std::vector<u32>{}.swap(wide_guest_ranges);
    decltype(guest_range_chunks){}.swap(guest_range_chunks);  // patch 0015
}

void A64AddressSpace::EmitPrelude() {
    using namespace oaknut::util;

    UnprotectCodeMemory();

    prelude_info.read_memory_8 = EmitCallTrampoline<&A64::UserCallbacks::MemoryRead8>(code, conf.callbacks);
    prelude_info.read_memory_16 = EmitCallTrampoline<&A64::UserCallbacks::MemoryRead16>(code, conf.callbacks);
    prelude_info.read_memory_32 = EmitCallTrampoline<&A64::UserCallbacks::MemoryRead32>(code, conf.callbacks);
    prelude_info.read_memory_64 = EmitCallTrampoline<&A64::UserCallbacks::MemoryRead64>(code, conf.callbacks);
    prelude_info.read_memory_128 = EmitRead128CallTrampoline(code, conf.callbacks);
    prelude_info.wrapped_read_memory_8 = EmitWrappedReadCallTrampoline<&A64::UserCallbacks::MemoryRead8>(code, conf.callbacks);
    prelude_info.wrapped_read_memory_16 = EmitWrappedReadCallTrampoline<&A64::UserCallbacks::MemoryRead16>(code, conf.callbacks);
    prelude_info.wrapped_read_memory_32 = EmitWrappedReadCallTrampoline<&A64::UserCallbacks::MemoryRead32>(code, conf.callbacks);
    prelude_info.wrapped_read_memory_64 = EmitWrappedReadCallTrampoline<&A64::UserCallbacks::MemoryRead64>(code, conf.callbacks);
    prelude_info.wrapped_read_memory_128 = EmitWrappedRead128CallTrampoline(code, conf.callbacks);
    prelude_info.exclusive_read_memory_8 = EmitExclusiveReadCallTrampoline<&A64::UserCallbacks::MemoryRead8, u8>(code, conf);
    prelude_info.exclusive_read_memory_16 = EmitExclusiveReadCallTrampoline<&A64::UserCallbacks::MemoryRead16, u16>(code, conf);
    prelude_info.exclusive_read_memory_32 = EmitExclusiveReadCallTrampoline<&A64::UserCallbacks::MemoryRead32, u32>(code, conf);
    prelude_info.exclusive_read_memory_64 = EmitExclusiveReadCallTrampoline<&A64::UserCallbacks::MemoryRead64, u64>(code, conf);
    prelude_info.exclusive_read_memory_128 = EmitExclusiveRead128CallTrampoline(code, conf);
    prelude_info.write_memory_8 = EmitCallTrampoline<&A64::UserCallbacks::MemoryWrite8>(code, conf.callbacks);
    prelude_info.write_memory_16 = EmitCallTrampoline<&A64::UserCallbacks::MemoryWrite16>(code, conf.callbacks);
    prelude_info.write_memory_32 = EmitCallTrampoline<&A64::UserCallbacks::MemoryWrite32>(code, conf.callbacks);
    prelude_info.write_memory_64 = EmitCallTrampoline<&A64::UserCallbacks::MemoryWrite64>(code, conf.callbacks);
    prelude_info.write_memory_128 = EmitWrite128CallTrampoline(code, conf.callbacks);
    prelude_info.wrapped_write_memory_8 = EmitWrappedWriteCallTrampoline<&A64::UserCallbacks::MemoryWrite8>(code, conf.callbacks);
    prelude_info.wrapped_write_memory_16 = EmitWrappedWriteCallTrampoline<&A64::UserCallbacks::MemoryWrite16>(code, conf.callbacks);
    prelude_info.wrapped_write_memory_32 = EmitWrappedWriteCallTrampoline<&A64::UserCallbacks::MemoryWrite32>(code, conf.callbacks);
    prelude_info.wrapped_write_memory_64 = EmitWrappedWriteCallTrampoline<&A64::UserCallbacks::MemoryWrite64>(code, conf.callbacks);
    prelude_info.wrapped_write_memory_128 = EmitWrappedWrite128CallTrampoline(code, conf.callbacks);
    prelude_info.exclusive_write_memory_8 = EmitExclusiveWriteCallTrampoline<&A64::UserCallbacks::MemoryWriteExclusive8, u8>(code, conf);
    prelude_info.exclusive_write_memory_16 = EmitExclusiveWriteCallTrampoline<&A64::UserCallbacks::MemoryWriteExclusive16, u16>(code, conf);
    prelude_info.exclusive_write_memory_32 = EmitExclusiveWriteCallTrampoline<&A64::UserCallbacks::MemoryWriteExclusive32, u32>(code, conf);
    prelude_info.exclusive_write_memory_64 = EmitExclusiveWriteCallTrampoline<&A64::UserCallbacks::MemoryWriteExclusive64, u64>(code, conf);
    prelude_info.exclusive_write_memory_128 = EmitExclusiveWrite128CallTrampoline(code, conf);
    prelude_info.call_svc = EmitCallTrampoline<&A64::UserCallbacks::CallSVC>(code, conf.callbacks);
    prelude_info.exception_raised = EmitCallTrampoline<&A64::UserCallbacks::ExceptionRaised>(code, conf.callbacks);
    prelude_info.isb_raised = EmitCallTrampoline<&A64::UserCallbacks::InstructionSynchronizationBarrierRaised>(code, conf.callbacks);
    prelude_info.ic_raised = EmitCallTrampoline<&A64::UserCallbacks::InstructionCacheOperationRaised>(code, conf.callbacks);
    prelude_info.dc_raised = EmitCallTrampoline<&A64::UserCallbacks::DataCacheOperationRaised>(code, conf.callbacks);
    prelude_info.get_cntpct = EmitCallTrampoline<&A64::UserCallbacks::GetCNTPCT>(code, conf.callbacks);
    prelude_info.add_ticks = EmitCallTrampoline<&A64::UserCallbacks::AddTicks>(code, conf.callbacks);
    prelude_info.get_ticks_remaining = EmitCallTrampoline<&A64::UserCallbacks::GetTicksRemaining>(code, conf.callbacks);
    prelude_info.interpreter_fallback = EmitCallTrampoline<&A64::UserCallbacks::InterpreterFallback>(code, conf.callbacks);
    if (conf.global_monitor) {
        prelude_info.wrapped_exclusive_read_memory_8 = EmitWrappedExclusiveReadCallTrampoline<&A64::UserCallbacks::MemoryRead8, u8>(code, conf);
        prelude_info.wrapped_exclusive_read_memory_16 = EmitWrappedExclusiveReadCallTrampoline<&A64::UserCallbacks::MemoryRead16, u16>(code, conf);
        prelude_info.wrapped_exclusive_read_memory_32 = EmitWrappedExclusiveReadCallTrampoline<&A64::UserCallbacks::MemoryRead32, u32>(code, conf);
        prelude_info.wrapped_exclusive_read_memory_64 = EmitWrappedExclusiveReadCallTrampoline<&A64::UserCallbacks::MemoryRead64, u64>(code, conf);
        prelude_info.wrapped_exclusive_read_memory_128 = EmitWrappedExclusiveRead128CallTrampoline(code, conf);
        prelude_info.wrapped_exclusive_write_memory_8 = EmitWrappedExclusiveWriteCallTrampoline<&A64::UserCallbacks::MemoryWriteExclusive8, u8>(code, conf);
        prelude_info.wrapped_exclusive_write_memory_16 = EmitWrappedExclusiveWriteCallTrampoline<&A64::UserCallbacks::MemoryWriteExclusive16, u16>(code, conf);
        prelude_info.wrapped_exclusive_write_memory_32 = EmitWrappedExclusiveWriteCallTrampoline<&A64::UserCallbacks::MemoryWriteExclusive32, u32>(code, conf);
        prelude_info.wrapped_exclusive_write_memory_64 = EmitWrappedExclusiveWriteCallTrampoline<&A64::UserCallbacks::MemoryWriteExclusive64, u64>(code, conf);
        prelude_info.wrapped_exclusive_write_memory_128 = EmitWrappedExclusiveWrite128CallTrampoline(code, conf);
    }

    oaknut::Label return_from_run_code, l_return_to_dispatcher;

    prelude_info.run_code = code.xptr<PreludeInfo::RunCodeFuncType>();
    {
        ABI_PushRegisters(code, ABI_CALLEE_SAVE | (1 << 30), sizeof(StackLayout));

        code.MOV(X19, X0);
        code.MOV(Xstate, X1);
        code.MOV(Xhalt, X2);
        if (conf.page_table) {
            code.MOV(Xpagetable, mcl::bit_cast<u64>(conf.page_table));
        }
        if (conf.fastmem_pointer) {
            code.MOV(Xfastmem, *conf.fastmem_pointer);
        }

        if (conf.HasOptimization(OptimizationFlag::ReturnStackBuffer)) {
            code.LDR(Xscratch0, l_return_to_dispatcher);
            for (size_t i = 0; i < RSBCount; i++) {
                code.STR(Xscratch0, SP, offsetof(StackLayout, rsb) + offsetof(RSBEntry, code_ptr) + i * sizeof(RSBEntry));
            }
        }

        if (conf.enable_cycle_counting) {
            code.BL(prelude_info.get_ticks_remaining);
            code.MOV(Xticks, X0);
            code.STR(Xticks, SP, offsetof(StackLayout, cycles_to_run));
        }

        code.MRS(Xscratch1, oaknut::SystemReg::FPCR);
        code.STR(Wscratch1, SP, offsetof(StackLayout, save_host_fpcr));
        code.LDR(Wscratch0, Xstate, offsetof(A64JitState, fpcr));
        code.MSR(oaknut::SystemReg::FPCR, Xscratch0);

        code.LDAR(Wscratch0, Xhalt);
        code.CBNZ(Wscratch0, return_from_run_code);

        code.BR(X19);
    }

    prelude_info.step_code = code.xptr<PreludeInfo::RunCodeFuncType>();
    {
        ABI_PushRegisters(code, ABI_CALLEE_SAVE | (1 << 30), sizeof(StackLayout));

        code.MOV(X19, X0);
        code.MOV(Xstate, X1);
        code.MOV(Xhalt, X2);
        if (conf.page_table) {
            code.MOV(Xpagetable, mcl::bit_cast<u64>(conf.page_table));
        }
        if (conf.fastmem_pointer) {
            code.MOV(Xfastmem, *conf.fastmem_pointer);
        }

        if (conf.HasOptimization(OptimizationFlag::ReturnStackBuffer)) {
            code.LDR(Xscratch0, l_return_to_dispatcher);
            for (size_t i = 0; i < RSBCount; i++) {
                code.STR(Xscratch0, SP, offsetof(StackLayout, rsb) + offsetof(RSBEntry, code_ptr) + i * sizeof(RSBEntry));
            }
        }

        if (conf.enable_cycle_counting) {
            code.MOV(Xticks, 1);
            code.STR(Xticks, SP, offsetof(StackLayout, cycles_to_run));
        }

        code.MRS(Xscratch1, oaknut::SystemReg::FPCR);
        code.STR(Wscratch1, SP, offsetof(StackLayout, save_host_fpcr));
        code.LDR(Wscratch0, Xstate, offsetof(A64JitState, fpcr));
        code.MSR(oaknut::SystemReg::FPCR, Xscratch0);

        oaknut::Label step_hr_loop;
        code.l(step_hr_loop);
        code.LDAXR(Wscratch0, Xhalt);
        code.CBNZ(Wscratch0, return_from_run_code);
        code.ORR(Wscratch0, Wscratch0, static_cast<u32>(HaltReason::Step));
        code.STLXR(Wscratch1, Wscratch0, Xhalt);
        code.CBNZ(Wscratch1, step_hr_loop);

        code.BR(X19);
    }

    prelude_info.return_to_dispatcher = code.xptr<void*>();
    {
        oaknut::Label l_this, l_addr;

        code.LDAR(Wscratch0, Xhalt);
        code.CBNZ(Wscratch0, return_from_run_code);

        if (conf.enable_cycle_counting) {
            code.CMP(Xticks, 0);
            code.B(LE, return_from_run_code);
        }

        code.LDR(X0, l_this);
        code.MOV(X1, Xstate);
        code.LDR(Xscratch0, l_addr);
        code.BLR(Xscratch0);
        code.BR(X0);

        const auto fn = [](A64AddressSpace& self, A64JitState& context) -> CodePtr {
            return self.GetOrEmit(context.GetLocationDescriptor());
        };

        code.align(8);
        code.l(l_this);
        code.dx(mcl::bit_cast<u64>(this));
        code.l(l_addr);
        code.dx(mcl::bit_cast<u64>(Common::FptrCast(fn)));
    }

    prelude_info.return_from_run_code = code.xptr<void*>();
    {
        code.l(return_from_run_code);

        if (conf.enable_cycle_counting) {
            code.LDR(X1, SP, offsetof(StackLayout, cycles_to_run));
            code.SUB(X1, X1, Xticks);
            code.BL(prelude_info.add_ticks);
        }

        code.LDR(Wscratch0, SP, offsetof(StackLayout, save_host_fpcr));
        code.MSR(oaknut::SystemReg::FPCR, Xscratch0);

        oaknut::Label exit_hr_loop;
        code.l(exit_hr_loop);
        code.LDAXR(W0, Xhalt);
        code.STLXR(Wscratch0, WZR, Xhalt);
        code.CBNZ(Wscratch0, exit_hr_loop);

        ABI_PopRegisters(code, ABI_CALLEE_SAVE | (1 << 30), sizeof(StackLayout));
        code.RET();
    }

    code.align(8);
    code.l(l_return_to_dispatcher);
    code.dx(mcl::bit_cast<u64>(prelude_info.return_to_dispatcher));

    prelude_info.end_of_prelude = code.offset();

    // Omnidroid patch 0009: invalidate the prelude that was written, not the whole code cache.
    // `invalidate_all` runs the cache-maintenance loop over every page of the cache, and on macOS
    // (`sys_icache_invalidate`) that faults each untouched page in: a 32 MiB `MAP_JIT` cache went
    // from 0.00 to 32.03 MiB of phys_footprint from this one call (measured), per jit, per guest
    // thread. Everything emitted later is invalidated block by block (`AddressSpace::Emit`).
    mem.invalidate(mem.ptr(), prelude_info.end_of_prelude);
    ProtectCodeMemory();
}

EmitConfig A64AddressSpace::GetEmitConfig() {
    return EmitConfig{
        .optimizations = conf.unsafe_optimizations ? conf.optimizations : conf.optimizations & all_safe_optimizations,

        .hook_isb = conf.hook_isb,

        .cntfreq_el0 = conf.cntfrq_el0,
        .ctr_el0 = conf.ctr_el0,
        .dczid_el0 = conf.dczid_el0,
        .tpidrro_el0 = conf.tpidrro_el0,
        .tpidr_el0 = conf.tpidr_el0,

        .check_halt_on_memory_access = conf.check_halt_on_memory_access,

        .page_table_pointer = mcl::bit_cast<u64>(conf.page_table),
        .page_table_address_space_bits = conf.page_table_address_space_bits,
        .page_table_pointer_mask_bits = conf.page_table_pointer_mask_bits,
        .silently_mirror_page_table = conf.silently_mirror_page_table,
        .absolute_offset_page_table = conf.absolute_offset_page_table,
        .detect_misaligned_access_via_page_table = conf.detect_misaligned_access_via_page_table,
        .only_detect_misalignment_via_page_table_on_page_boundary = conf.only_detect_misalignment_via_page_table_on_page_boundary,

        .fastmem_pointer = conf.fastmem_pointer,
        .recompile_on_fastmem_failure = conf.recompile_on_fastmem_failure,
        .fastmem_address_space_bits = conf.fastmem_address_space_bits,
        .silently_mirror_fastmem = conf.silently_mirror_fastmem,

        .fastmem_exclusive_access = conf.fastmem_exclusive_access,
        .global_monitor = conf.global_monitor,
        .processor_id = conf.processor_id,

        .wall_clock_cntpct = conf.wall_clock_cntpct,
        .enable_cycle_counting = conf.enable_cycle_counting,

        .always_little_endian = true,

        .descriptor_to_fpcr = [](const IR::LocationDescriptor& location) { return A64::LocationDescriptor{location}.FPCR(); },
        .emit_cond = EmitA64Cond,
        .emit_condition_failed_terminal = EmitA64ConditionFailedTerminal,
        .emit_terminal = EmitA64Terminal,
        .emit_check_memory_abort = EmitA64CheckMemoryAbort,

        .state_nzcv_offset = offsetof(A64JitState, cpsr_nzcv),
        .state_fpsr_offset = offsetof(A64JitState, fpsr),
        .state_exclusive_state_offset = offsetof(A64JitState, exclusive_state),

        .coprocessors{},

        .very_verbose_debugging_output = conf.very_verbose_debugging_output,
    };
}

void A64AddressSpace::RegisterNewBasicBlock(const IR::Block& block, const EmittedBlockInfo&) {
    const A64::LocationDescriptor descriptor{block.Location()};
    const A64::LocationDescriptor end_location{block.EndLocation()};
    // Omnidroid patch 0011: the pin's `closed(descriptor.PC(), end_location.PC() - 1)`, which is
    // empty -- and was never returned -- when the block covers no bytes.
    const u64 first = descriptor.PC();
    const u64 last = end_location.PC() - 1;
    if (last < first) {
        return;
    }
    ASSERT(guest_ranges.size() < std::numeric_limits<u32>::max());
    const u32 index = static_cast<u32>(guest_ranges.size());
    guest_ranges.push_back(GuestRange{descriptor, first, last});

    const u64 first_page = first >> guest_page_bits;
    const u64 last_page = last >> guest_page_bits;
    if (last_page - first_page >= max_indexed_pages) {
        wide_guest_ranges.push_back(index);
        return;
    }
    for (u64 page = first_page;; ++page) {
        guest_range_pages[page].push_back(index);
        guest_range_chunks.insert(page >> (guest_chunk_bits - guest_page_bits));  // patch 0015
        if (page == last_page) {
            break;
        }
    }
}

}  // namespace Dynarmic::Backend::Arm64
