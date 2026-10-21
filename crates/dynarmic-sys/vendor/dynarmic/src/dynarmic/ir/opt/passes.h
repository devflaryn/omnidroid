/* This file is part of the dynarmic project.
 * Copyright (c) 2016 MerryMage
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#include <atomic>
#include <cstdint>

namespace Dynarmic::A32 {
struct UserCallbacks;
}

namespace Dynarmic::A64 {
struct UserCallbacks;
struct UserConfig;
}  // namespace Dynarmic::A64

namespace Dynarmic::IR {
class Block;
}

namespace Dynarmic::Optimization {

struct PolyfillOptions {
    bool sha256 = false;
    bool vector_multiply_widen = false;

    bool operator==(const PolyfillOptions&) const = default;
};

struct A32GetSetEliminationOptions {
    bool convert_nzc_to_nz = false;
    bool convert_nz_to_nzc = false;
};

/// Omnidroid patch 0037.
struct DeadCodeEliminationOptions {
    /// Keep every guest memory read, used or not: under `check_halt_on_memory_access` a read that
    /// faults must stop the guest whether or not anything reads its value.
    bool keep_memory_reads = false;
    /// Omnidroid patch 0110: rewrite every argument that is an `Identity` to the value it stands
    /// for, and take what is dead out of the block rather than leave it `Void`. `GetSetElimination`
    /// turns every Get it forwards into an `Identity`, which the emitter otherwise allocates as an
    /// alias of its value.
    bool forward_identities = false;
};

/// Omnidroid patch 0037.
struct A64GetSetEliminationOptions {
    /// Keep the guest state exact at every instruction that can leave the block early (any guest
    /// data access, under `check_halt_on_memory_access`): no Set before it is erased by a Set
    /// after it. Reads are still forwarded across it.
    bool precise_at_memory_aborts = false;
    /// Omnidroid patch 0081: a general register's value forwarded to a read of the other width. A W
    /// write (`SetW v`) becomes `SetX (ZeroExtendWordToLong v)` -- what writing Wn means: Xn's upper
    /// half zeroed -- so a later X read takes that value, and a later W read takes `v`; a W read after
    /// an X write takes the X value's low word. Instead of storing the register and loading it back.
    bool forward_width_changes = false;
};

/// Omnidroid patch 0037, shared by both backends since patch 0110: non-zero runs
/// `A64GetSetElimination` in its precise form for every block translated from now on whose config
/// sets `check_halt_on_memory_access` (which otherwise skips the pass). Process-wide, read at
/// translation: a block translated before the switch keeps what it was translated with.
extern std::atomic<std::uint32_t> live_precise_get_set;

void PolyfillPass(IR::Block& block, const PolyfillOptions& opt);
void A32ConstantMemoryReads(IR::Block& block, A32::UserCallbacks* cb);
void A32GetSetElimination(IR::Block& block, A32GetSetEliminationOptions opt);
void A64CallbackConfigPass(IR::Block& block, const A64::UserConfig& conf);
void A64GetSetElimination(IR::Block& block, A64GetSetEliminationOptions opt = {});
void A64MergeInterpretBlocksPass(IR::Block& block, A64::UserCallbacks* cb);
void ConstantPropagation(IR::Block& block);
void DeadCodeElimination(IR::Block& block, DeadCodeEliminationOptions opt = {});
void IdentityRemovalPass(IR::Block& block);
void VerificationPass(const IR::Block& block);
void NamingPass(IR::Block& block);

}  // namespace Dynarmic::Optimization
