# Patches carried against the pin

`vendor/dynarmic/` is upstream `9d45823` plus every `.patch` here, applied in order (D5: "a pinned
fork we carry patches against"). There is no 0023 in this tree: it is on branch
`arm64-clear-audit` (arm64 RSB reset on a mid-run cache clear) and not merged.

Upstream suite (`dynarmic_tests`, A64 frontend, Release): x64/MSVC **201,698 assertions in 84
cases** with 0001-0022 and 0024-0027, not re-run with 0028; arm64/M1 (AppleClang,
`-DDYNARMIC_WARNINGS_AS_ERRORS=OFF`) **201,698 in 83** with 0001-0021.

Rows named `mac-*` are in `tools/mutate_mac/`; `S*` rows in `tools/mutate_0022.py` (hand
mutations of vendored C++, rebuilt and restored with a SHA-1 check).

## How a patch is carried

Edit `vendor/dynarmic/` directly, commit the `.patch` here, add it to `vendor/PIN.txt` and touch
that file (the only vendored path Cargo watches).
`python3 crates/dynarmic-sys/tools/verify_patches.py` checks, offline and in git object space,
that the pristine tree (vendored at `64034d4`) plus the patches in order equals `vendor/dynarmic/` byte for byte, and that it reverse-applies to pristine.

## Applied

### 0001 — `MRS Xt, CNTVCT_EL0` reads the counter `CNTPCT_EL0` reads

Both backends (A64 frontend). The pin fell to the interpreter on `CNTVCT_EL0`, killing a guest
worker (`libroblox.so` reads it at link `0x229d184`). Linux arm64 clears `CNTVOFF_EL2`, so the two
counters are equal on a device. Verified:
`omni-cpu/tests/counter.rs::cntvct_reads_the_same_clock_as_cntpct`.

### 0002 — arm64: the `Interpret` terminal calls the interpreter fallback

arm64. The pin asserted on `IR::Term::Interpret`, so the first undecodable word (e.g. any LSE
atomic) ended the process. Now does what x64 does: charge ticks, store PC, host `FPCR`, call
`InterpreterFallback`, restore, return to the dispatcher. Verified: `tests/interpret.rs`.

### 0003 — arm64: scalar saturating add, subtract and doubling multiply-high

arm64. 18 scalar `SQADD/UQADD/SQSUB/UQSUB/SQDMULH` IR ops were `ASSERT_FALSE`; now the host's own
instruction, with `FPSR.QC` folded in. Verified: `tests/a64_saturation.rs` (ARM ARM pseudocode).

### 0004 — arm64: half-precision arithmetic, and a fallback that dropped its result

arm64. Implements the reachable FP16 ops via dynarmic's `FP::` routines (as x64 does), and fixes
`EmitTwoOpFallbackWithoutRegAlloc`'s save mask, which restored the old value over `FRINT* .8H`'s
result. Unreachable forms are left as they were. Verified: `tests/a64_fp16.rs`.

### 0005 — arm64: the SM4 substitution box

arm64. `SM4AccessSubstitutionBox` was unimplemented; now calls the same C++ as x64.
Verified: `tests/a64_sm4.rs` (the SM4 spec's example ciphertext).

### 0006 — arm64: 64-bit unsigned max/min, which is how `CMHS`/`CMHI` compare

arm64. `VectorMaxU64`/`VectorMinU64` were unimplemented (found by `hostile.rs`'s fuzzer at trial
158,510); now `CMHI` + `BSL`. Verified: `tests/a64_compare.rs`.

### 0007 — arm64: `fastmem_exclusive_access` is honoured (inline `LDXR`/`STXR`)

arm64. The backend ignored the flag, so every exclusive pair took the callback path, which
`omni-cpu` refuses as `DegradedMemoryPath`. Adds x64's inline protocol against the same monitor
and lock. Verified: `tests/exclusive.rs` (every width, failure cases, 4-thread no-lost-update).

### 0008 — arm64: the memory-abort check reads the halt word as the 32 bits it is

arm64. `EmitA64CheckMemoryAbort` did a 64-bit `LDAR` of a `u32`, an alignment fault that killed the
process on any unmapped guest access. Verified: `tests/host_fault.rs`.

### 0009 — arm64: the prelude invalidates what it wrote, not the whole code cache

arm64. `EmitPrelude` invalidated the whole cache, which on macOS faults every page in (32 MiB per
jit). Verified: `tests/code_cache_charge.rs` (a 32 MiB-cache jit costs < 2 MiB).

### 0010 — arm64: keep what is read of an emitted block, in flat records

arm64, bookkeeping only. Replaces per-block robin_maps (~2,100 B/block; 1.19 GiB at 594k blocks
on macOS) with sorted 24-byte records for blocks, fastmem sites and links. Verified:
`tests/bookkeeping.rs` (1,785 -> 457 B/block, bound 700; relink and patch-site lookup tests).

### 0011 — arm64: the guest ranges of emitted blocks, compact and cleared with the cache

arm64. The shared `BlockRangeInformation` icl map was never cleared. Now one 24-byte `GuestRange`
per block, indexed by 4 KiB page, cleared with the cache. Verified: `tests/bookkeeping.rs` (0-1 B
held after a clear, bound 16; two-page, >64-page and 16 GiB invalidation tests).

### 0012 — arm64: an invalidation that leaves no block standing is a clear

arm64. After whole-space invalidations (from `omni-android`'s cross-thread `CodeWatch` queue
overflow) jits kept dead copies of their code; now `ClearCache` runs when no block is left.
Verified: `tests/bookkeeping.rs`; rows `mac-mem-A7`, `mac-mem-B2`.

### 0013 — arm64: the bookkeeping's large arrays are pages of their own

arm64. Adds `page_backed_allocator.h`: arrays >= 256 KiB are `mmap`ed so a clear returns them to
the OS. Adds `od_page_backed_bytes()` (additive). Verified: `tests/bookkeeping.rs` (footprint drops
on a clear); row `mac-mem-A8`.

### 0014 — arm64: the store-exclusive is a fastmem patch location too

arm64, fixes 0007. Only the load of the inline store-exclusive was registered, so a store to a
read-only page (sealed relro) aborted the process. Verified: `tests/host_fault.rs`,
`omni-cpu/tests/exclusive_store_fault.rs`.

### 0015 — arm64: an invalidation walks only the chunks that hold translated code

arm64, on 0011. Page-by-page lookups for every guest `mmap`/`munmap`/`mprotect` took 64-69% of two
threads; now only 2 MiB chunks known to hold code are walked. Adds `od_invalidation_page_probes()`.
Verified: `tests/invalidation_probes.rs` (256 -> 0 probes); rows `mac-cpu-I1`, `mac-cpu-I2`.

### 0016 — arm64: a location translated again keeps one range, not one per translation

arm64, fixes 0011. Retranslated code piled up dead ranges (97-98% of three threads' time); dead
ranges are now dropped as walked. Adds `od_invalidation_ranges_checked()`.
Verified: `tests/invalidation_probes.rs` (201 -> 1 range checked); row `mac-cpu-I3`.

### 0017 — x64: a guest thread's fixed cost: the fast-dispatch table and the prelude commit

x64, D32. The 16 MiB fast-dispatch table (item 4) is allocated only with `FastDispatch` on, and
`PRELUDE_COMMIT_SIZE` drops 16 -> 2 MiB. Measured at the landing (45 jits): commit 3,157 -> 2,105
MiB. Verified: `tests/pin_constants.rs` (`OD_FIXED_PER_JIT_BYTES`) and the upstream suite.

### 0018 — x64: a return-stack-buffer hit checks the budget and the halt flag

x64, D33. The `PopRSBHint` hit path now leaves through `ReturnFromRunCode` when budget or halt is
set, so `INTERRUPTIBLE` keeps the RSB (132 -> 24 ns per call+return at 262k blocks). Verified:
`the_stoppability_matrix` (`return:*`, `return-ring:*` cells), mutated by hand.

### 0019 — x64: the fast-dispatch handler checks the budget and the halt flag, and its table is 64 KiB

x64, D35. The same checks before the table probe (every `BR`/`BLR` and RSB miss), and the table
shrinks to 4,096 entries (64 KiB), so `INTERRUPTIBLE` is `ALL_SAFE`. Verified:
`the_stoppability_matrix` (`indirect`, `return-miss`),
`a64_exec.rs::an_invalidated_translation_is_not_served_from_the_fast_dispatch_table`.

### 0020 — arm64: the return-stack buffer's hit checks the budget and the halt word, in one handler

arm64, D35. 0018 for arm64, with the hit test moved once into the prelude (inline cost more than
it saved: 20.2 vs 47.4 ns at 262k blocks). Verified: `the_stoppability_matrix`
(`return-ring:*`); rows `mac-cpu-R1`..`R3`.

### 0021 — arm64: the inline exclusives honour `Unsafe_IgnoreGlobalMonitor` (value-compare)

arm64, D31 amendment 1. 0007 ignored the flag, so value-compare still took the global lock and
scanned all slots (1,307 -> 9.5 ns per atomic increment, one thread). Verified:
`tests/exclusive.rs` (`value_compare_inline_exclusives_neither_take_nor_release_the_monitor_lock`,
`under_value_compare_another_processor_s_same_value_store_leaves_the_reservation`),
`omni-cpu/tests/exclusive.rs` ABA test; rows `mac-cpu-V1`..`V3`.

### 0022 — x64: one code cache shared by every jit of a guest address space (opt-in)

x64, D38, design in `docs/research/shared-jit-cache.md`. Adds `A64::SharedCodeCache`: per-thread
values read from `JitState`, links through 8-byte slots (shared code is never rewritten),
serialized translation, generation-based invalidation, regions retired and given back with
epochs. A jit without a cache emits what it did. `omni-cpu` turns it on by default on x64 (D38
amendment 2; `OMNI_JIT_SHARED_CACHE=0` turns it off). Verified: `tests/shared_cache.rs`, the whole
suite under `OD_TEST_SHARED_CACHE=1`, rows S1-S13.

### 0024 — x64: a census of the shared cache's per-block tables

x64, read-only. `SharedCodeCache::GetTables()` / `od_code_cache_tables_of` (ABI 3) report what each
per-block table holds, so `OMNI_MEM_REPORT` can name the 272/224/80/64 MiB heap blocks seen in a
world. Verified: `tests/shared_bookkeeping.rs` (census within 15% of heap growth).

### 0025 — x64 shared cache: link slots and fastmem sites as flat records

x64, shared cache bookkeeping. Link slots become 24-byte `LinkRecord` lists, fastmem sites 16-byte
sorted per-region runs; `link_heads` uses load factor 0.75 (0.8 broke `omni-cpu/tests/thunk.rs`
via MXCSR). Verified: `tests/shared_bookkeeping.rs` (1,144 -> 401 B/block), `tests/host_fault.rs`,
`tests/shared_cache.rs` (list unlink cases).

### 0026 — x64: the guest ranges of emitted blocks, compact

x64, own and shared caches. 0011 for x64: page-indexed `GuestRange` records replace the icl map.
Verified: `tests/shared_bookkeeping.rs` (401 -> 289 B/block), 0011's three invalidation tests.

### 0027 — x64: a shared cache's block map at a load factor of 0.75

x64, shared caches. `block_descriptors` at 0.75 instead of 0.5 (64 -> 32 MiB in a world);
dispatcher lookups unchanged within noise. Verified: `tests/shared_bookkeeping.rs` (289 -> 225
B/block, bound 260; `bench_dispatcher_lookups_in_a_shared_cache`).

### 0028 — x64 shared cache: a full region is not a flush; the oldest region is retired, alone

x64, shared caches, D38 amendment 3. A full region stays live; past `live_bytes` (new
`od_code_cache_new` argument, ABI 4) the oldest region's blocks are forgotten and it is retired
(16 MiB region: ~18 ms per eviction on Windows). Verified: `tests/shared_cache.rs`
(`a_full_region_is_not_a_flush_and_the_oldest_region_goes_first`,
`a_block_translated_again_elsewhere_survives_its_old_region_s_eviction`,
`threads_keep_their_working_set_while_another_streams_cold_code_through_the_cache`); rows S14-S19.

## Known candidates, not yet applied

### 1. `hook_hint_instructions` is never plumbed into the A64 frontend

Only A32 reads it, so every `YIELD` exits the JIT. One line; deferred until something needs hint
handling.

### 2. Terminals that check the cycle counter and the halt flag exclusively

`tests/hostile.rs::the_stoppability_matrix` executes every cell. `ReturnFromRunCode` checks halt
and then budget; `0x0000_FFF8` (no `BlockLinking`, RSB or fast dispatch) routes everything there.

- **2a.** RSB and fast-dispatch handlers checked nothing. Closed by 0018/0019 (x64) and 0020
  (arm64; arm64 has no fast dispatch).
- **2b.** `LinkBlock` checks the budget when cycle counting is on, else the halt, never both. Open.
  Clearing `BlockLinking` fixes it at ~7x on 4-instruction blocks; the runtime instead uses short
  counted budgets (D16).
- **2c.** The cycle compare is signed, so a budget above `i64::MAX` reads as spent. Open; pinned
  by `a_cycle_budget_above_i64_max_reads_as_already_spent`.

### 3. `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT` crashes on Windows x86-64

With it on, dynarmic's own `[a64]` tests segfault; cause not characterised. So x64's cache stays
`PAGE_EXECUTE_READWRITE` (D12 exception), reported as `code_cache::W_AND_X` and pinned by
`a64_exec.rs::the_code_cache_is_writable_and_executable_at_once`. Under identity fastmem the cache
is guest-addressable in principle; only ASLR hides it.

### 4. `A64EmitX64` holds a 16 MiB fast-dispatch table by value, and fills it even when fast dispatch is off

Applied as 0017, and shrunk to 64 KiB by 0019.

### 0029 — x64: an ordered load of 64 bits or fewer is a plain `mov`

x64. `LDAR`/`LDAPR` were emitted as `lock xadd [addr], 0`, a read-modify-write that faults on a
read-only page, so the site fell to the callback path for good (`DegradedMemoryPath`). The real
bionic hits it at the top of `malloc`: `ldar x8, [__libc_globals + 0x48]`, a page libc
write-protects. x86 loads already have acquire semantics and every ordered store is an `xchg` (a
full barrier), so release-then-acquire ordering is kept. 128-bit ordered loads keep `cmpxchg16b`.
It also applies to the Roblox path and to the inline exclusive reads (`LDXR`/`LDAXR`, which pass
`ordered = true`): under the value-compare monitor (D31) the store-exclusive's `cmpxchg` is what
decides success, so a plain load is sufficient there too.
Verified: `omni-cpu/tests/tbi.rs::a_load_acquire_from_a_read_only_page_stays_on_the_direct_path`
(fails without the patch, with and without Top Byte Ignore).
