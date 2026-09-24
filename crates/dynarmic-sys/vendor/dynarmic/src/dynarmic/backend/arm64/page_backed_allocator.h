/* Omnidroid patch 0013, carried against dynarmic (see crates/dynarmic-sys/patches/README.md).
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <atomic>
#include <cstddef>
#include <new>

#if defined(__APPLE__) || defined(__unix__)
#    include <sys/mman.h>
#    include <unistd.h>
#    define DYNARMIC_PAGE_BACKED_ALLOCATOR_MMAP 1
#endif

namespace Dynarmic::Backend::Arm64 {

/// Bytes currently mapped by every `PageBackedAllocator`, for measurement (`od_page_backed_bytes`):
/// what these arrays hold is no longer in the C++ heap's statistics.
inline std::atomic<std::size_t> page_backed_bytes{0};

/// An allocator for the address space's per-block bookkeeping that takes large arrays straight from
/// the kernel and gives them straight back.
///
/// The bookkeeping grows by doubling and is given back whole by `ClearCache`, so its large arrays
/// are allocated and freed many times over a jit's life. Through the C++ heap a freed large array is
/// the host allocator's to keep, dirty and charged to the process: MEASURED on macOS
/// (`tests/bookkeeping.rs`, n = 2), a clear of 11.4 MiB of bookkeeping took 0.00 MiB off
/// `phys_footprint` through the heap and 9.06 MiB with this allocator. An array of at least
/// `threshold` bytes is an anonymous mapping here -- charged for the pages that are touched, not for
/// a vector's spare capacity -- and freeing it unmaps it; a smaller one uses `operator new` as
/// before.
template<typename T>
struct PageBackedAllocator {
    using value_type = T;
    static constexpr std::size_t threshold = 256 * 1024;

    PageBackedAllocator() noexcept = default;
    template<typename U>
    PageBackedAllocator(const PageBackedAllocator<U>&) noexcept {}

    T* allocate(std::size_t n) {
        const std::size_t bytes = n * sizeof(T);
#ifdef DYNARMIC_PAGE_BACKED_ALLOCATOR_MMAP
        if (bytes >= threshold) {
            void* p = mmap(nullptr, RoundUp(bytes), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
            if (p == MAP_FAILED) {
                throw std::bad_alloc{};
            }
            page_backed_bytes.fetch_add(RoundUp(bytes), std::memory_order_relaxed);
            return static_cast<T*>(p);
        }
#endif
        return static_cast<T*>(::operator new(bytes, std::align_val_t{alignof(T)}));
    }

    void deallocate(T* p, std::size_t n) noexcept {
        const std::size_t bytes = n * sizeof(T);
#ifdef DYNARMIC_PAGE_BACKED_ALLOCATOR_MMAP
        if (bytes >= threshold) {
            munmap(p, RoundUp(bytes));
            page_backed_bytes.fetch_sub(RoundUp(bytes), std::memory_order_relaxed);
            return;
        }
#endif
        ::operator delete(p, std::align_val_t{alignof(T)});
    }

    template<typename U>
    bool operator==(const PageBackedAllocator<U>&) const noexcept { return true; }
    template<typename U>
    bool operator!=(const PageBackedAllocator<U>&) const noexcept { return false; }

private:
#ifdef DYNARMIC_PAGE_BACKED_ALLOCATOR_MMAP
    static std::size_t RoundUp(std::size_t bytes) {
        const std::size_t page = static_cast<std::size_t>(getpagesize());
        return (bytes + page - 1) / page * page;
    }
#endif
};

}  // namespace Dynarmic::Backend::Arm64
