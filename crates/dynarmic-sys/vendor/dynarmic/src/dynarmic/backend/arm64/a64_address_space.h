/* This file is part of the dynarmic project.
 * Copyright (c) 2022 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <vector>

#include <boost/icl/interval_set.hpp>
#include <tsl/robin_map.h>
#include <tsl/robin_set.h>

#include "dynarmic/backend/arm64/address_space.h"
#include "dynarmic/interface/A64/config.h"

namespace Dynarmic::Backend::Arm64 {

struct EmittedBlockInfo;

class A64AddressSpace final : public AddressSpace {
public:
    explicit A64AddressSpace(const A64::UserConfig& conf);

    IR::Block GenerateIR(IR::LocationDescriptor) const override;

    void InvalidateCacheRanges(const boost::icl::interval_set<u64>& ranges);

    void ClearCache() override;

protected:
    friend class A64Core;

    void EmitPrelude();
    EmitConfig GetEmitConfig() override;
    void RegisterNewBasicBlock(const IR::Block& block, const EmittedBlockInfo& block_info) override;

    const A64::UserConfig conf;

    // Omnidroid patch 0011: the guest bytes each emitted block was translated from, for
    // `InvalidateCacheRanges`. The pin kept them in a `BlockRangeInformation` -- a boost::icl
    // interval_map of std::sets, about 200 bytes per block, which `ClearCache` never cleared, so
    // it grew for as long as the jit lived. One `GuestRange` per emitted block (24 bytes), indexed
    // by the 4 KiB guest pages it covers; like the pin's, a range stays until `ClearCache`.
    struct GuestRange {
        IR::LocationDescriptor location;
        u64 first;  ///< The first guest byte, `closed(first, last)` as the pin registered it.
        u64 last;
        /// Omnidroid patch 0016: its block has been invalidated. A dead range matches nothing and
        /// is dropped from every page list an invalidation walks, so a location translated again
        /// and again leaves one live range, not one per translation.
        bool dead = false;
    };
    static constexpr unsigned guest_page_bits = 12;
    /// A block covering more pages than this is kept in `wide_guest_ranges`, checked on every
    /// invalidation, instead of in every page it covers.
    static constexpr u64 max_indexed_pages = 64;
    PageBackedVector<GuestRange> guest_ranges;
    PageBackedMap<u64, std::vector<u32>> guest_range_pages;
    std::vector<u32> wide_guest_ranges;
    // Omnidroid patch 0015: the 2 MiB chunks that hold at least one page of `guest_range_pages`.
    // An invalidation walks only the pages of the chunks it touches that are in this set, so a
    // range with no translated code in it -- a heap `munmap` -- costs a few set lookups instead of
    // one lookup per 4 KiB page, on every guest thread it is broadcast to.
    static constexpr unsigned guest_chunk_bits = 21;
    tsl::robin_set<u64> guest_range_chunks;
};

}  // namespace Dynarmic::Backend::Arm64
