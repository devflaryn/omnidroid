/* This file is part of the dynarmic project (Omnidroid patch 0063).
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <cstddef>
#include <memory>
#include <new>
#include <type_traits>
#include <utility>

namespace Dynarmic::Backend::X64 {

/// Omnidroid patch 0063: a deferred emit -- a `void()` callable -- held in place when it fits
/// (every out-of-line path's lambda does), rather than a `std::function`, which allocates for any
/// capture past two pointers: a heap allocation and a free per memory access emitted. Move-only;
/// a callable larger than the buffer is still held on the heap.
class DeferredEmit {
public:
    static constexpr std::size_t capacity = 256;

    template<typename F, typename = std::enable_if_t<!std::is_same_v<std::decay_t<F>, DeferredEmit>>>
    DeferredEmit(F&& f) {  // NOLINT(google-explicit-constructor): as std::function
        using T = std::decay_t<F>;
        if constexpr (sizeof(T) <= capacity && alignof(T) <= alignof(std::max_align_t) && std::is_nothrow_move_constructible_v<T>) {
            ::new (static_cast<void*>(storage)) T(std::forward<F>(f));
            ops = &inline_ops<T>;
        } else {
            *reinterpret_cast<T**>(storage) = new T(std::forward<F>(f));
            ops = &heap_ops<T>;
        }
    }

    DeferredEmit(DeferredEmit&& other) noexcept
            : ops(other.ops) {
        if (ops) {
            ops->move(storage, other.storage);
            other.ops = nullptr;
        }
    }
    DeferredEmit& operator=(DeferredEmit&& other) noexcept {
        if (this != &other) {
            reset();
            ops = other.ops;
            if (ops) {
                ops->move(storage, other.storage);
                other.ops = nullptr;
            }
        }
        return *this;
    }
    DeferredEmit(const DeferredEmit&) = delete;
    DeferredEmit& operator=(const DeferredEmit&) = delete;
    ~DeferredEmit() { reset(); }

    void operator()() { ops->invoke(storage); }

private:
    struct Ops {
        void (*invoke)(void* self);
        void (*move)(void* to, void* from);  ///< move-constructs `to` from `from`, then destroys `from`
        void (*destroy)(void* self);
    };

    template<typename T>
    static constexpr Ops inline_ops{
        [](void* self) { (*static_cast<T*>(self))(); },
        [](void* to, void* from) {
            ::new (to) T(std::move(*static_cast<T*>(from)));
            std::destroy_at(static_cast<T*>(from));
        },
        [](void* self) { std::destroy_at(static_cast<T*>(self)); },
    };

    template<typename T>
    static constexpr Ops heap_ops{
        [](void* self) { (**static_cast<T**>(self))(); },
        [](void* to, void* from) { *static_cast<T**>(to) = *static_cast<T**>(from); },
        [](void* self) { delete *static_cast<T**>(self); },
    };

    void reset() {
        if (ops) {
            ops->destroy(storage);
            ops = nullptr;
        }
    }

    const Ops* ops = nullptr;
    alignas(std::max_align_t) unsigned char storage[capacity];
};

/// Omnidroid patch 0063: the fastmem fallbacks by (ordered, bit size, address register, value
/// register), in a flat array rather than a `std::map` searched (and, through `operator[]`,
/// inserted into) for every memory access emitted. The same pointers; null where none was made,
/// as the map's default-constructed value was.
class FallbackTable {
public:
    using Fn = void (*)();
    template<typename Key>
    Fn& operator[](const Key& key) {
        const auto& [ordered, bitsize, vaddr_idx, value_idx] = key;
        return table[Index(ordered, static_cast<std::size_t>(bitsize), static_cast<int>(vaddr_idx), static_cast<int>(value_idx))];
    }

private:
    static std::size_t Index(bool ordered, std::size_t bitsize, int vaddr_idx, int value_idx) {
        std::size_t size_code = 0;
        switch (bitsize) {
        case 8: size_code = 0; break;
        case 16: size_code = 1; break;
        case 32: size_code = 2; break;
        case 64: size_code = 3; break;
        case 128: size_code = 4; break;
        default: size_code = 5; break;  // never made: reads null, as a map miss did
        }
        if (vaddr_idx < 0 || vaddr_idx > 15 || value_idx < 0 || value_idx > 15) {
            size_code = 5;
            vaddr_idx = 0;
            value_idx = 0;
        }
        return ((static_cast<std::size_t>(ordered) * 6 + size_code) * 16 + static_cast<std::size_t>(vaddr_idx)) * 16 + static_cast<std::size_t>(value_idx);
    }

    Fn table[2 * 6 * 16 * 16]{};
};

}  // namespace Dynarmic::Backend::X64
