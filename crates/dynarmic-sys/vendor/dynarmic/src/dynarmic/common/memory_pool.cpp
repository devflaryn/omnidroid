/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#include "dynarmic/common/memory_pool.h"

#include <cstdlib>
#include <utility>

namespace Dynarmic::Common {

namespace {
/// Omnidroid patch 0063: one slab kept per thread for the next pool of the same slab size -- an IR
/// block's pool lives for one translation, so every translation malloc'd and freed its slab.
struct SlabCache {
    char* slab = nullptr;
    size_t bytes = 0;
    ~SlabCache() { std::free(slab); }
};
thread_local SlabCache slab_cache;

void Release(char* slab, size_t bytes) {
    if (slab_cache.slab == nullptr) {
        slab_cache.slab = slab;
        slab_cache.bytes = bytes;
    } else {
        std::free(slab);
    }
}
}  // namespace

Pool::Pool(size_t object_size, size_t initial_pool_size)
        : object_size(object_size), slab_size(initial_pool_size) {
    AllocateNewSlab();
}

Pool::~Pool() {
    Release(current_slab, object_size * slab_size);

    for (char* slab : slabs) {
        Release(slab, object_size * slab_size);
    }
}

void* Pool::Alloc() {
    if (remaining == 0) {
        slabs.push_back(current_slab);
        AllocateNewSlab();
    }

    void* ret = static_cast<void*>(current_ptr);
    current_ptr += object_size;
    remaining--;

    return ret;
}

void Pool::AllocateNewSlab() {
    if (slab_cache.slab != nullptr && slab_cache.bytes == object_size * slab_size) {  // patch 0063
        current_slab = std::exchange(slab_cache.slab, nullptr);
    } else {
        current_slab = static_cast<char*>(std::malloc(object_size * slab_size));
    }
    current_ptr = current_slab;
    remaining = slab_size;
}

}  // namespace Dynarmic::Common
