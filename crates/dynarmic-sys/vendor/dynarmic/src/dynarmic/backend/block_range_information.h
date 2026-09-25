/* This file is part of the dynarmic project.
 * Copyright (c) 2018 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <cstddef>
#include <set>
#include <utility>

#include <boost/icl/interval_map.hpp>
#include <boost/icl/interval_set.hpp>
#include <tsl/robin_set.h>

#include "dynarmic/ir/location_descriptor.h"

namespace Dynarmic::Backend {

template<typename ProgramCounterType>
class BlockRangeInformation {
public:
    void AddRange(boost::icl::discrete_interval<ProgramCounterType> range, IR::LocationDescriptor location);
    void ClearCache();
    tsl::robin_set<IR::LocationDescriptor> InvalidateRanges(const boost::icl::interval_set<ProgramCounterType>& ranges);
    /// Omnidroid patch 0024, a census for a memory report: the intervals held, and the locations
    /// their sets name between them.
    std::pair<std::size_t, std::size_t> Census() const;

private:
    boost::icl::interval_map<ProgramCounterType, std::set<IR::LocationDescriptor>> block_ranges;
};

}  // namespace Dynarmic::Backend
