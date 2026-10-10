/* This file is part of the dynarmic project.
 * Copyright (c) 2018 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>
#include <vector>

#include "dynarmic/interface/A64/config.h"
#include "dynarmic/interface/halt_reason.h"

namespace Dynarmic {
namespace A64 {

class Jit final {
public:
    explicit Jit(UserConfig conf);
    ~Jit();

    /**
     * Runs the emulated CPU.
     * Cannot be recursively called.
     */
    HaltReason Run();

    /**
     * Step the emulated CPU for one instruction.
     * Cannot be recursively called.
     */
    HaltReason Step();

    /**
     * Clears the code cache of all compiled code.
     * Can be called at any time. Halts execution if called within a callback.
     */
    void ClearCache();

    /**
     * Invalidate the code cache at a range of addresses.
     * @param start_address The starting address of the range to invalidate.
     * @param length The length (in bytes) of the range to invalidate.
     */
    void InvalidateCacheRange(std::uint64_t start_address, std::size_t length);

    /**
     * Reset CPU state to state at startup. Does not clear code cache.
     * Cannot be called from a callback.
     */
    void Reset();

    /**
     * Stops execution in Jit::Run.
     */
    void HaltExecution(HaltReason hr = HaltReason::UserDefined1);

    /**
     * Clears a halt reason from flags.
     * Warning: Only use this if you're sure this won't introduce races.
     */
    void ClearHalt(HaltReason hr = HaltReason::UserDefined1);

    /// Read Stack Pointer
    std::uint64_t GetSP() const;
    /// Modify Stack Pointer
    void SetSP(std::uint64_t value);

    /// Read Program Counter
    std::uint64_t GetPC() const;
    /// Modify Program Counter
    void SetPC(std::uint64_t value);

    /// Read general-purpose register.
    std::uint64_t GetRegister(std::size_t index) const;
    /// Modify general-purpose register.
    void SetRegister(size_t index, std::uint64_t value);

    /// Read all general-purpose registers.
    std::array<std::uint64_t, 31> GetRegisters() const;
    /// Modify all general-purpose registers.
    void SetRegisters(const std::array<std::uint64_t, 31>& value);

    /// Read floating point and SIMD register.
    Vector GetVector(std::size_t index) const;
    /// Modify floating point and SIMD register.
    void SetVector(std::size_t index, Vector value);

    /// Read all floating point and SIMD registers.
    std::array<Vector, 32> GetVectors() const;
    /// Modify all floating point and SIMD registers.
    void SetVectors(const std::array<Vector, 32>& value);

    /// View FPCR.
    std::uint32_t GetFpcr() const;
    /// Modify FPCR.
    void SetFpcr(std::uint32_t value);

    /// View FPSR.
    std::uint32_t GetFpsr() const;
    /// Modify FPSR.
    void SetFpsr(std::uint32_t value);

    /// View PSTATE
    std::uint32_t GetPstate() const;
    /// Modify PSTATE
    void SetPstate(std::uint32_t value);

    /// Clears exclusive state for this core.
    void ClearExclusiveState();

    /**
     * Returns true if Jit::Run was called but hasn't returned yet.
     * i.e.: We're in a callback.
     */
    bool IsExecuting() const;

    /// Debugging: Dump a disassembly all of compiled code to the console.
    void DumpDisassembly() const;

    /*
     * Disassemble the instructions following the current pc and return
     * the resulting instructions as a vector of their string representations.
     */
    std::vector<std::string> Disassemble() const;

private:
    struct Impl;
    std::unique_ptr<Impl> impl;
};

/**
 * Omnidroid patch 0022 (x64 backend only): one code cache -- prelude, translated blocks, block
 * map -- shared by every Jit of one guest address space (UserConfig::shared_code_cache), so a
 * block translated by one thread is executed by all of them.
 *
 * `template_config` supplies every field that shapes emitted code; `callbacks` must point at an
 * object of the same class as every attached Jit's callbacks (it is never called), and the
 * TPIDR pointers only need to be non-null iff the attached Jits' are. The recompile-on-fastmem-
 * failure flags are forced off: a shared block is never recompiled from inside a fault handler.
 *
 * `total_bytes` is address space, committed as code is emitted; after the prelude it is divided
 * into regions of `region_bytes`, filled one at a time. Patch 0028: a full region stays live -- its
 * blocks are still run and linked to -- and the next free one is filled. At most `live_bytes` of
 * regions are live (0: all but one); when filling another would exceed that, the oldest live region
 * is retired: its blocks alone are forgotten (a thread that needs one translates it again, into the
 * newest region), and its memory is given back once no thread can still be executing it (every
 * attached Jit has left or re-entered Run since). A retirement forgets at most one region's blocks.
 *
 * Translation is serialized by one lock; execution takes none. Invalidation (a Jit's
 * InvalidateCacheRange/ClearCache, or the calls below) applies to every Jit: each stops using a
 * dropped translation no later than its next Run.
 */
class SharedCodeCache final {
public:
    SharedCodeCache(const UserConfig& template_config, std::size_t total_bytes, std::size_t region_bytes, std::size_t live_bytes = 0);
    ~SharedCodeCache();

    SharedCodeCache(const SharedCodeCache&) = delete;
    SharedCodeCache& operator=(const SharedCodeCache&) = delete;

    struct Stats {
        std::uint64_t blocks_emitted = 0;        ///< blocks translated and emitted into this cache
        std::uint64_t code_bytes_emitted = 0;    ///< host code bytes of those blocks
        std::uint64_t translations_raced = 0;    ///< misses another thread had emitted meanwhile
        std::uint64_t translations_redone = 0;   ///< translations redone because the code changed meanwhile
        std::uint64_t translate_ns = 0;          ///< time translating (frontend and IR passes), outside the lock
        std::uint64_t emit_ns = 0;               ///< time emitting host code, holding the lock
        std::uint64_t locked_lookups = 0;        ///< dispatcher lookups its threads' own tables could not answer
        std::uint64_t invalidations = 0;         ///< invalidation requests applied
        std::uint64_t blocks_invalidated = 0;    ///< blocks those requests dropped
        std::uint64_t generation = 0;            ///< bumped by every request that dropped a block
        std::uint64_t regions_total = 0;
        std::uint64_t regions_retired = 0;       ///< region retirements so far (evictions, and full regions a ClearCache emptied)
        std::uint64_t regions_evicted = 0;       ///< of those, the oldest live region retired to make room (patch 0028)
        std::uint64_t blocks_evicted = 0;        ///< blocks those evictions forgot
        std::uint64_t blocks_reemitted = 0;      ///< blocks emitted again at a location the latest eviction forgot
        std::uint64_t evict_ns = 0;              ///< time spent evicting, holding the lock
        std::uint64_t evict_max_ns = 0;          ///< the longest single eviction
        std::uint64_t regions_live = 0;          ///< regions whose blocks are live now (the one being filled included)
        std::uint64_t regions_live_max = 0;      ///< how many may be
        std::uint64_t regions_reclaimed = 0;     ///< retired regions given back so far
        std::uint64_t regions_pinned = 0;        ///< retired regions a running thread still holds
        std::uint64_t parked_redirected = 0;     ///< threads parked in an SVC whose resume was moved out of a retiring region
        std::uint64_t reclaim_attempts = 0;      ///< passes over the retired regions
        std::uint64_t committed_bytes = 0;       ///< code-cache bytes committed now, prelude included
        std::uint64_t attached = 0;              ///< Jits attached now
        // Omnidroid patch 0070: translation snapshots.
        std::uint64_t snapshot_blocks_restored = 0;  ///< blocks installed from a snapshot, unverified
        std::uint64_t snapshot_blocks_verified = 0;  ///< of those, found unchanged when first looked up and entered
        std::uint64_t snapshot_blocks_rejected = 0;  ///< of those, whose guest code had changed: dropped, translated again
        std::uint64_t snapshot_save_lock_ns = 0;     ///< the latest save's time holding the cache's lock (copying out)
        std::uint64_t snapshot_pages_read = 0;       ///< patch 0075: pages of a lazily restored snapshot read in, as entered
        std::uint64_t snapshot_blocks_forgotten = 0; ///< patch 0076: restored blocks never entered, forgotten by ForgetUnverified
    };
    Stats GetStats() const;

    /// Omnidroid patch 0024: what one of the emitter's per-block tables holds on the C heap.
    struct Table {
        std::uint64_t entries = 0;          ///< what it holds
        std::uint64_t bytes = 0;            ///< its arrays at their capacity, and its entries' own allocations
        std::uintptr_t largest_address = 0; ///< an address inside its largest single allocation, 0 if none
        std::uint64_t largest_bytes = 0;    ///< that allocation's size
    };
    struct Tables {
        Table blocks;         ///< location -> translated block (the dispatcher's map)
        Table link_targets;   ///< link target -> the link slots that jump to it
        Table links;          ///< each block's own link slots
        Table fastmem_sites;  ///< host fault site -> fallback, for fastmem accesses
        Table guest_ranges;   ///< the guest bytes each block was translated from
    };
    /// Takes the cache's lock, shared: a census for a memory report, not for a hot path.
    Tables GetTables() const;

    /// Omnidroid patch 0036: for each of `count` host code addresses `hosts` (ascending), the
    /// guest PC of the translated block whose code holds it, or ~0 where none does (the prelude,
    /// far code, a link slot, a block since forgotten). Takes the cache's lock, shared, and walks
    /// every block once: for a sampling profiler's report, not for a hot path.
    void GuestPcsOf(const std::uint64_t* hosts, std::size_t count, std::uint64_t* guest_pcs) const;

    /// Invalidate [start, start + length) for every attached Jit, now. Must not be called from
    /// inside a callback of a Jit attached to this cache (use Jit::InvalidateCacheRange there).
    void InvalidateCacheRange(std::uint64_t start_address, std::size_t length);
    /// Drop every translation, now. Same restriction.
    void ClearCache();
    /// Omnidroid patch 0050: retire the oldest full regions until at most `keep_bytes` of regions
    /// are live (the region being filled counts; at least one stays). Their blocks are forgotten
    /// and translated again if they run again. Same restriction. How many regions were retired.
    std::size_t EvictTo(std::size_t keep_bytes);

    /// Omnidroid patch 0100: where `address` is in the shared caches alive in this process -- a
    /// cache's prelude or a region of it, the region's state, its sequence and retirement epoch,
    /// whether the page is a lazily restored one not read in -- as one line in `out` (at most `cap`
    /// bytes, NUL-terminated); its length, 0 if no cache holds it. For a crash report: it takes no
    /// lock and allocates nothing, and what it reads may be changing as it reads.
    static std::size_t DescribeAddress(std::uint64_t address, char* out, std::size_t cap);

    /// Omnidroid patch 0070: translation snapshots. `EnableSnapshots` makes the cache remember, for
    /// every block it emits from now on, a hash of the guest code it was translated from (read
    /// back through the translating thread's `MemoryReadCode`). `SaveSnapshot` writes every live
    /// block that has one -- the regions' code bytes, each block's guest range and hash, link slots
    /// and fastmem sites, the constant pool -- to `path`, tagged with `key` and with a hash of
    /// everything that shapes the code (the prelude's bytes, the configuration, the live switches,
    /// the host's features). Returns the blocks written, or a negative error; nothing is written
    /// past `max_bytes`. Blocks restored from a snapshot and never entered since are written too
    /// (with their stored hashes) when `include_unverified`. `LoadSnapshot`, on a cache that has emitted nothing yet, installs a
    /// snapshot with the same key and shape at the same offsets: each block unverified, entered
    /// only once a lookup has read its guest code again and found the same hash (a block whose
    /// code differs is dropped and translated as usual). Returns the blocks installed, or a
    /// negative error (the cache unchanged). Same restriction as InvalidateCacheRange.
    void EnableSnapshots();
    std::int64_t SaveSnapshot(const char* path, const char* key, std::uint64_t max_bytes, bool include_unverified = true);
    /// Omnidroid patch 0075: `lazily`, nothing of the code is read or committed at load; a page is
    /// read in from the file (kept open) when a block on it is first entered, so code never entered
    /// costs no memory.
    std::int64_t LoadSnapshot(const char* path, const char* key, bool lazily = false);
    /// Omnidroid patch 0076: forget every restored block not entered yet -- its block-map entry,
    /// its links, its fastmem sites, its guest range's page-index entries, what verifying it needed
    /// -- and shrink the tables. A location looked up later is translated as usual. Returns how
    /// many were forgotten. Same restriction as InvalidateCacheRange.
    std::int64_t ForgetUnverified();
    /// Patch 0076: the guest PCs of the restored blocks not entered yet -- up to `capacity` of
    /// them into `out` (in no order). Returns how many there are.
    std::size_t UnverifiedPcs(std::uint64_t* out, std::size_t capacity) const;

    struct Impl;
    std::unique_ptr<Impl> impl;
};

}  // namespace A64
}  // namespace Dynarmic
