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

### 0030 — x64 shared cache: a clear gives back the region being filled

x64. A clear (`ClearCache`, what `omni-linux`'s `code_trim` does to a quiet process) forgot every
block but retired only the full regions: the region being filled stayed committed and went on
filling from where it was. A cache whose code fits in one 16 MiB region -- most of the ~60 services
of the system's host process, which translate their start and then wait in binder -- kept all of it
through the trim: 65 caches held 462 MiB after the trims had run (run 2026-09-29, `OMNI_MEM_TRACE`).
Now the clear retires that region too, through the same retire-and-reclaim path as a full one (its
threads asked to leave generated code, a parked thread's resume redirected), and the next block
starts a fresh region, committed as it fills.
Verified: `tests/shared_cache.rs::a_clear_gives_back_the_region_being_filled` (5.7 -> 3.0 MB
committed, the prelude kept) and `a_full_region_is_not_a_flush_and_the_oldest_region_goes_first`'s
clear (no live region after it).

### 0031 — x64 shared cache: a clear gives back the block map

x64. `ForgetAllBlocks` emptied the block map (and the patch-information map) with `clear()`, which
keeps a `robin_map`'s bucket array: a process whose translations a trim dropped kept the map of its
busiest moment. In PS99 the system's host process held 86 MiB of block maps for 562k live blocks
(~157 bytes a block against ~64 at the load factor; run 2026-09-29, `OMNI_MEM_TRACE`'s census).
Now a clear swaps in a new map at patch 0027's load factor (64 buckets, as a new cache starts).
Verified: `tests/shared_cache.rs::a_clear_gives_back_the_region_being_filled` (4 MiB of block map
for 60,001 blocks -> a new map); dynarmic-sys and omni-cpu suites green (incl. the MXCSR check).
### 0032 — arm64: a low window for the guest's first 4 GiB

arm64 (D41). `UserConfig::fastmem_low_window`: `fastmem_pointer` is added only to an address below
2^32 (after the top-byte mirror); at and above it the host address is the guest address.
`FastmemEmitVAddrLookup` emits `tst offset, #0xffffffff00000000; csel base, Xfastmem, xzr, eq` and
the access goes through `[base, offset]` -- two instructions, no branch, no callback -- and the
inline exclusives take the same path (`EmitExclusiveHostAddress`). For macOS, which maps nothing
below 4 GiB, running ART, whose heap must be there. Off by default; the x64 backend ignores the
field and the shim refuses it on an x64 host. Verified: `omni-cpu/tests/low_window.rs` (a load,
a store and an exclusive pair at a low address reach `W + g`, a high address stays the identity,
demand paging in the window, zero slow-path entries, with and without Top Byte Ignore).

### 0033 — arm64: a mid-run cache clear forgets the return-stack buffer

arm64. `AddressSpace::Emit` clears the whole cache when a block is emitted with less than 1 MiB
left, and that happens inside a run (the dispatcher calls into the emitter). The return-stack
buffer lives in the run's frame and kept the host code of the blocks just cleared, where other
blocks are then written: a guest `ret` whose target matched jumped into the translation of another
function. It is `docs/ports/macos.md`'s m11 ("a call landed in the translation of another
function") and, on the real-AOSP path, `system_server`'s `android.bg` thread jumping into its own
stack. The dispatcher now passes its frame (`SP`) to the emit callback, which points every RSB
entry at the dispatcher again when the cache was cleared (`AddressSpace::CacheClears`), as the
prelude leaves them. (Branch `arm64-clear-audit`'s patch 0023 addressed the same defect and is
not in this tree.) Verified: `omni-cpu/tests/cache_clear_rsb.rs` -- one call site, a first call
that returns at once, a second through a chain of 400,000 conditional-branch blocks (several 8 MiB
caches): (2, 2) without the patch, the chain run twice; (2, 1) with it.

### 0034 — x64: the unsafe floating-point flags, switched at run time

x64. `live_fp_optimizations`, one process-wide atomic that `A64EmitContext::HasOptimization`
ORs into `Unsafe_UnfuseFMA` / `Unsafe_ReducedErrorFP` / `Unsafe_InaccurateNaN` /
`Unsafe_IgnoreStandardFPCRValue` where the config's `unsafe_optimizations` gate is open (every
Omnidroid config: the value-compare monitor opens it). A block emitted after a switch uses the new
flags; the host clears the cache so every block is emitted again (`od_set_live_fp_optimizations`,
`omni-linux`'s `OMNI_LEVER_FILE` `jit_fp=` lever and `OMNI_JIT_UNSAFE_FP`). Default 0: nothing
changes unless a host asks. For an in-session A/B of the flags (docs/NIGHT-2026-10-02.md).
### 0035 — x64: a thread's fast-dispatch table at its process's size

x64, shared cache. `UserConfig::od_fast_dispatch_entries` (a power of two from 0x40 to 0x10000;
anything else is the pin's 0x1000): the shared cache's emitter masks the fast-dispatch hash with
its value (`fast_dispatch_mask`, set before the terminal handlers are emitted) and every thread
of the cache allocates and resets its table at that size (`FastDispatchEntries(shared->conf)`).
Omnidroid keeps the pin's 4,096 by default; `OMNI_JIT_FAST_DISPATCH_SYSTEM=1024` gives the system's host
process 16 KiB a thread (~900 threads: 56 MiB of tables at 64 KiB) -- not the default until its
lookup cost there is measured. Verified: `omni-cpu/tests/fast_dispatch_size.rs` -- 64
entries for 200 `BLR` targets (every probe colliding) run every call to its own code, and 32
threads at 64 entries hold ~2 MiB less C heap than at 4,096 (measured, within 10%).

### 0036 — x64 shared cache: which guest block a host code address is in

x64, read-only, shared caches. `SharedCodeCache::GuestPcsOf` / `od_code_cache_guest_pcs_of`: for
ascending host addresses, the guest PC of the block whose emitted code holds each (`~0` for the
prelude, far code, link slots, forgotten blocks). One pass over the block map under the cache's lock
taken shared -- nothing is kept, so emission pays nothing; for `omni-linux`'s `OMNI_GUEST_PROF`
report thread only. Verified: `omni-cpu/tests/guest_pc_of_host.rs` (264 of 264 sampled code-cache
addresses of a thread spinning in a known block resolved to it).

### 0037 — x64: `GetSetElimination` under the memory-abort check, precise at every access

x64 (the IR passes are shared; only x64's `TranslateBlock` asks for the precise form). Upstream
skips `GetSetElimination` whenever `check_halt_on_memory_access` is set, which Omnidroid always
sets (guests fault on purpose), so every guest register read was a load from `JitState` and every
write a store. `A64GetSetEliminationOptions::precise_at_memory_aborts` keeps the state exact where
the block can leave early: at any guest data access (or other side-effecting instruction) no earlier
Set is erased, while known values are still forwarded to later Gets; a supervisor call, exception,
cache operation or host call forgets what is known (omni-cpu serves syscalls and inline thunks
inside `CallSVC`). Process-wide switch `live_precise_get_set` (`od_set_precise_get_set`,
`OMNI_JIT_PRECISE_GETSET=0|1`, `omni-linux`'s `jit_getset=` lever), read at translation; default 1.
Two fixes the differential test found, kept on regardless of the switch: `DeadCodeElimination`
keeps every memory read under the check (`DeadCodeEliminationOptions::keep_memory_reads`) -- it
dropped a read with no uses, so `LDR XZR/WZR, [Xn]` (ART's stack-overflow probe) never faulted
whenever `ConstProp` ran -- and a `SetNZCVRaw` value is no longer forwarded to `GetNZCVRaw`
(the store keeps bits 28-31; upstream forwarded the unmasked word).
Measured (`omni-cpu/tests/bench.rs::the_precise_get_set_elimination`, E-cores, idle priority,
ns/iteration off -> on): register-bound 2.70 -> 2.43, memory-heavy 2.80 -> 2.42, mixed 12-insn
4.28 -> 3.22 (the check-off bound: 2.39, 2.44, 3.24). Verified: `omni-cpu/tests/precise_getset.rs`
(a mid-block fault after eliminated writes stops with `X0 == 2`, the flags and `Q7` exact, for 9
access kinds; unread loads fault; 4,000 random blocks, ~2/3 faulting mid-block, identical state
with the pass and without; all three fail with the precision removed).

### 0038 — x64: the prelude's fresh pages are not cleared (the constant pool stays out of RAM)

x64, every code cache. `AllocateFromCodeSpace` cleared what it handed out with `memset`; before
the prelude is complete it hands out only pages nothing has written yet -- freshly committed
(Windows) or freshly mapped -- which read zero already, and the clear made every one of them
resident: the 2 MiB constant pool of every code cache, of which a few KiB hold constants. PS99
in-world, the system's host process (~65 guest processes, a shared cache each) held 84 MiB of
resident all-zero pages in its executable regions (`wsscan.ps1`, 2026-10-08). Now those
allocations are not cleared; one made after the prelude (none today) still is, and
`OMNI_JIT_POOL_MEMSET=1` clears these too (the pin's behaviour, read once per process, for an A/B).
Verified: `tests/resident.rs` (`QueryWorkingSetEx` over a new cache's own reservation: 0.88 MiB
resident of 4.00 committed, 2.88 MiB with `OMNI_JIT_POOL_MEMSET=1`).

### 0039 — x64: scalar floating-point operands stay in XMM registers (switch, off by default)

x64. The A64 frontend reads every scalar FP operand as `VectorGetElement(GetQ(v), 0)`, which the
pin emitted as `movq gpr, xmm`; the SSE consumer then moved it straight back (`movq xmm, gpr`), and
a single-precision result written back went through `ZeroExtendWordToLong` in a GPR the same way:
two cross-domain moves per operand on the dependency chain. With `live_scalar_fp_in_xmm` on,
`VectorGetElement64/32` element 0 is `movq xmm, xmm` / `insertps` (lanes 1-3 zeroed) and
`ZeroExtendWordToLong` of an XMM value is `insertps`: the very values the round trip produced
(element, zeros above), so every consumer sees what it saw. Not aliased to the vector register
itself (upstream's TODO): `PostProcessNaN`'s packed `cmpunordp` on the vector's other lanes raised
`FPSR.IOC` from a signalling NaN there -- the differential test below found that in 923 trials.
Switch: `od_set_scalar_fp_in_xmm`, `OMNI_JIT_SCALAR_FP_XMM=0|1`, `omni-linux`'s `jit_fpxmm=` lever;
default 0 until an in-world A/B. Measured (`tests/codegen_bench.rs`, E-cores, idle priority, shared
cache, ns per loop of 4 ops, 1.5 of it loop): dependent `FADD D` 22.8 -> 11.5, `FADD S` 32.6 ->
12.6-14.3, `FMADD D` 27.4 -> 17.7; independent `FADD D` 8.3 -> 3.6-3.8, `FADD S` 13.2 -> 8.3;
`FCVTZS` 7.6 -> 4.2; host `addsd` chain 6.4. Verified: `tests/scalar_fp_xmm.rs` (3,000 random
blocks of scalar FP, conversions, FMOV, FCSEL/FCMP, vector ops, scalar loads/stores under seven
`FPCR` settings, every upper lane full: registers, NZCV, `FPSR` and memory identical off and on),
and the dynarmic-sys and omni-cpu suites with the switch defaulted on (only the known
`shared_cache` timing, `low_window` and `subpage` failures).

### 0040 — x64: Top Byte Ignore's mask as one `and` (switch, off by default)

x64. A mirrored fastmem address wider than 32 bits (omni-linux's Top Byte Ignore: 56) was masked
with `mov; shl; shr` before every guest access; with `live_fastmem_mask_by_and` on it is
`mov; and tmp, [rip + pool constant]` -- the same address, one cycle on its path instead of two
(the `mov` is eliminated, the load is off the path). Switch: `od_set_fastmem_mask_by_and`,
`OMNI_JIT_TBI_AND=0|1`, `omni-linux`'s `jit_tbiand=` lever; default 0 until an in-world A/B.
Measured (`omni-cpu/tests/bench.rs::the_cost_of_top_byte_ignore`, E-cores, idle, two runs, ns per
iteration): memory-heavy 3.13 -> 2.68-2.85 (64-bit identity 2.42-2.51), mixed 3.62-3.71 ->
3.52-3.61 (identity 3.24-3.25). Verified: `omni-cpu/tests/tbi.rs` (tagged load, store and
exclusive pair on the direct path with it on). Second switch, `live_fastmem_tbi_unmasked`
(`od_set_tbi_unmasked`, `omni-linux`'s `jit_tbi=0|1` lever): a 56-bit mirrored address emitted
unmasked, as a 64-bit one -- a tagged address is then non-canonical, takes a general-protection
fault (Windows: an access violation at "address" `u64::MAX`), which omni's demand pager declines
and dynarmic's fastmem handler (keyed on the faulting instruction's address, not the data address)
sends to the callback, where omni-cpu clears the tag, serves and counts it. The start-time form is
omni-cpu's `DynarmicOptions::tbi_direct_mask` (`OMNI_JIT_TBI=0`: 64-bit identity config). Correct
for every access kind (`omni-cpu/tests/tbi.rs`, `tbi_live.rs`: plain, pair, 128-bit, byte,
ordered, exclusive), untagged accesses at identity speed (memory-heavy 2.48 against the mask's
3.13 ns/iter), but a tagged one costs ~2.4 us, and Android 15's scudo tags heap pointers `0x02`
(`omni-linux/tests/tbi_off.rs`: 820 tagged accesses in `toybox echo`, ~19,000 in `ls -lR` +
`sha256sum`), so it is not expected to win in a game while the heap is tagged -- which 0041 fixes.

### 0041 — x64: with Top Byte Ignore's mask off, an instruction that meets a tag learns it

x64. Where does the tag come from: `libc.so`'s scudo (`orr xN, xM, #0x200000000000000` in
`Allocator<AndroidNormalConfig>::allocate`, `deallocate`, `reallocate`, `getAllocSize`,
`iterateOverChunks`, `initChunkWithMemoryTagging`, `quarantineOrDeallocateChunk`) is
`addHeaderTag`: the address through which scudo reads and writes every chunk header carries a fixed
tag 2 whenever the allocator *may* support memory tagging -- a compile-time property on arm64
(`archSupportsMemoryTagging`: "we assume that Top-Byte Ignore is enabled"), independent of
bionic's heap tagging level (no `0xb4` user-pointer tag was seen: that level is already NONE with
`PR_SET_TAGGED_ADDR_CTRL` refused) and of zygote's memtag flags. So no supported setting removes it.
Instead, with `live_fastmem_tbi_unmasked` on, a fastmem site's slow path (after the fallback has
served the access) checks the address's top byte and, if set, calls `tbi_note_thunk`, which notes
the instruction's location (process-wide set, `NoteTbiTaggedSite`) and sets the jit's
`CacheInvalidation` halt bit, so `Run` returns at its next halt check; the shim's `od_jit_run`
then invalidates the noted PCs (once per cache, `TbiSitesFrom`), and `EmitFastmemVAddr` emits a
noted location masked (patch 0040's 56-bit mask) and every other one unmasked. The inline
exclusive load (no slow-path block of its own) resumes into a stub that does the same. The note
costs nothing on the direct path. Measured (`omni-linux/tests/tbi_off.rs`, real toybox with
`OMNI_JIT_TBI=0`): 279 tagged accesses per process, the same for `ls -lR /system/etc` (444 lines)
and `/system` (2,959 lines), against ~19,000 for two small programs without learning; ~95
instructions learned. (Learned sites are by guest location and libc.so is mapped at a different
address in each process, so each process learns its own: ~0.7 ms of faults at startup.) Verified:
`omni-cpu/tests/tbi_live.rs` (six access kinds fault once, then masked; a loop faults once).

### 0042 — x64: the dispatch hints' hit paths inside each block (switch, off by default)

x64. A `RET` block (`PopRSBHint`) and a `BR`/`BLR` block (`FastDispatchHint`) stored the target PC
and jumped to one shared handler, which loaded the PC back from `JitState`, built the location,
checked the budget and the halt word, and probed the return-stack buffer or the thread's
fast-dispatch table -- every guest indirect branch funnelled through one host indirect jump. With
`live_fast_dispatch_inline` on, `EmitA64SetPC`, when it is the block's last instruction before a
hint (and the block does not set FPCR), also leaves the target in rbp, and the terminal emits the
hit path itself: the location from rbp and the block's own FPCR part, the same checks (patches
0018/0019), the same probe and hash, and a host indirect jump of the site's own; a miss continues in
the shared handler (`terminal_handler_fast_dispatch_probe` with rbx the location after an RSB miss,
`terminal_handler_fast_dispatch_miss` with rbx and rbp the entry after a table miss). ~70 bytes a
site instead of 5. Switch: `od_set_fast_dispatch_inline`, `OMNI_JIT_FASTDISP=0|1`, `omni-linux`'s
`jit_fastdisp=` lever; default 0 until an in-world A/B. Measured (`tests/codegen_bench.rs`, E-cores,
idle; shared cache / per-thread): 8 calls + 8 returns 35.6 -> 28.5 / 31.2 -> 27.2 ns; threaded
dispatch random 13.2 -> 12.8 / 13.5 -> 12.4 ns/op, cyclic 3.20 -> 2.91 / 3.24 -> 2.83 (host `match`
7.3 and 1.0). Verified with the switch defaulted on: dynarmic-sys and omni-cpu suites (the known
`low_window`/`subpage` only; `hostile.rs`'s stoppability matrix included), omni-linux `a1_toybox`,
`a4_threads`, `a5_signals`, `b_hello_dex` (ART), `tbi_off`.

### 0050 — x64 shared cache: an eviction on demand

x64, shared caches. `SharedCodeCache::EvictTo(keep_bytes)` (`od_code_cache_evict_to`): what patch
0028 does when a full region meets the live limit, asked for -- the oldest live regions' blocks
forgotten and the regions retired (threads asked to leave generated code, given back once none
holds one) until at most `keep_bytes` of regions are live; the region being filled always stays.
For `omni-linux`'s age pass (`code_trim::age`): the system's busy processes (system_server, 119
MiB of translations in a world, 2026-10-09) are never quiet enough for the clear, and most of what
they hold is boot code. Verified: `tests/shared_cache.rs::an_eviction_on_demand_keeps_the_newest_
region_and_w_runs_on` (5 of 6 regions retired, the working set translated again once, ~9-17 us a
small block; 4 regions of 315k blocks evicted in 92-150 ms here, ~25-40 ms a region).

### 0051 — x64 shared cache: fastmem site records of 8 bytes

x64, shared caches. A `FastmemSite` held a 64-bit fallback and a 32-bit resume offset (16 bytes,
1.3M of them in a world: 23 MiB in the game's host process, 25 MiB in the system's). The resume is
a short way after its site and the fallbacks are a few hundred prelude thunks: now a 16-bit resume
delta and a 16-bit index into the cache's fallback table (8 bytes); a site that does not fit (a
resume past 64 KiB or before its site, past 65,535 fallbacks) is kept whole in a second sorted
list searched after the first. Verified: `tests/shared_bookkeeping.rs` (fastmem sites 22.5 -> 11.3
bytes each with the vectors' slack, the census 225 -> 214 bytes a one-site block), `host_fault.rs`,
`a64_exec.rs`, `shared_cache.rs`.

### 0052 — x64: the block map, the link heads and the guest ranges, compact

x64, own and shared caches. A robin_map bucket is its probe distance padded to the value's
alignment, then the value: with a `u64` key the block map's buckets were 8 + 24 = 32 bytes and
the link heads' 8 + 16 = 24. Their keys are now `Key64` (a 64-bit value at 4-byte alignment,
hashed as the `u64` was) and the block map stores `StoredBlock` (the entry point as a 32-bit offset
from the code buffer's start, size, first link: 12 bytes) -- `GetBasicBlock` still returns a
`BlockDescriptor`. Buckets: 24 and 16 bytes. A `GuestRange` is 20 bytes (the location as a
`Key64`, the first byte in two halves, a 32-bit length), not 24. A flat page index (one 8-byte
entry per block and page, linked) was tried and dropped: where many blocks share a page it costs
more than the per-page vectors (4 bytes an entry) it would replace. MEASURED
(`tests/shared_bookkeeping.rs`): the census 214 -> 176 bytes a block (block map 64 -> 48, link
heads 48 -> 32, guest ranges 40 -> 34); a world's tables (OMNI_MEM_TRACE 2026-10-09: 531k blocks,
650k link targets in the game's host; 580k / 729k in the system's) ~18-20 MiB smaller in each.
Lookups (`bench_dispatcher_lookups_in_a_shared_cache`, 100k blocks, E-cores, 7 runs each): a locked
dispatcher lookup 127 -> 105 ns median (the block map 8 -> 6 MiB); `codegen_bench.rs` threaded
dispatch with FastDispatch off (every jump a block-map lookup) 30.5 -> 30.8 ns/op, with it on
unchanged -- inside the noise. Verified: the dynarmic-sys suite, and omni-cpu's thunk, exclusive,
tbi, faults and lifecycle tests under `OD_TEST_SHARED_CACHE=1`.

### 0060 — x64: a census of the emitted code

x64. Process-wide relaxed counters of what `A64EmitX64::Emit` writes, by part of a block (entry
padding, memory accesses, guest register reads/writes, NZCV, SetPC, other IR, the cycle
subtraction, the terminal, deferred out-of-line code, link slots) and counts (blocks, IR and guest
instructions, memory accesses, deferred emits); `od_codegen_census` / `od_codegen_census_reset`,
names in `dynarmic_sys::CODEGEN_PARTS`. The code emitted does not change. Measured
(`tests/code_size.rs`: bionic `libc.so` from the pinned sysroot, every function's first blocks, as
omni-cpu configures the jit, shared cache, 0042 on; 4,855 blocks): **414 bytes a block, 67 a guest
instruction**. Out-of-line code is 150 (36%: ~44 bytes for each of 3.4 memory accesses a block,
the fault path's call and its inline memory-abort check). The terminal is 83 (20%), inline memory
53, guest register traffic 48, other IR 51, slots 15, cycles 6, padding 4. `libhwui.so`: 405.

### 0061 — x64: compact code (switch bits, off by default)

x64. Two bits of `live_compact_code` (`od_set_compact_code`, `OMNI_JIT_COMPACT`, `omni-linux`'s
`jit_compact=` lever) give the same behaviour in fewer bytes.

**1, fault stubs.** A fastmem site's deferred slow path calls `fallback` only where a bounds check
jumps to it; a host fault reaches the fallback by a faked call that returns to the resume point
directly. The path then does `call memory_abort_check_thunk; dq guest_pc`. The thunk steps its
return address over the data. On an abort it stores the PC and leaves `Run`, as the inline
`EmitCheckMemoryAbort` did. `require_abort_handling` is now initialised: it was read uninitialised.

**2, link tails.** A shared cache's `LinkBlock` leaves a spent budget through its slot's own tail
(store the PC, enter the dispatcher, whose loop top makes the same checks). It no longer emits a
second tail with a forced return.

A32 is unchanged. Measured (`tests/code_size.rs` as for 0060): 414.3 bytes a block off, **327.4
with 1** (-21%; out-of-line 150 -> 62, 44 -> 18 a memory access), 394.7 with 2, 307.8 with both
(-26%). Speed (`tests/codegen_bench.rs::the_cost_of_compact_code`, 15 interleaved rounds): bit 1 is
within noise (-4.4% .. +1.2%). Bit 2 is **+17.5%** on eight linked two-instruction blocks on a
shared cache (layout, since the hot bytes are the same) and flat elsewhere.

Verified: `tests/compact_code.rs`, on private and shared caches. A host fault resumes. A memory
abort in the fallback stops at the load with nothing after it run, then re-executes the load.
Budget-spent links in 5-tick slices count exactly. The bytes go down. Also run: the dynarmic-sys
suite with `OD_TEST_COMPACT` on, per-thread and shared. With the switch defaulted on: omni-linux
`a1_toybox`, `a4_threads`, `a5_signals`, `b_hello_dex` (ART; with `OMNI_BOOT_IMAGE_UNCOMPRESSED=0`,
since that test's mapping-name check fails at HEAD either way) and `tbi_off`.

### 0062 — x64: an emit observer, for differential tests of the emitter

x64. `SetEmitObserver` (`od_set_emit_observer`): a callback told of every block emitted -- guest
PC, host entry, the bytes of its code up to its link slots, and its size with them. Null by
default: one relaxed load a block. `tests/code_size.rs::the_speed_of_emission` writes every block of
a corpus with it (`OMNI_EMIT_DUMP`), and `tools/compare_emit_dumps.py` compares two such dumps:
the same blocks, sizes and bytes, except inside host-address operands (`mov r64, imm64`, `call
rel32`), which move with the executable. Two runs of one build: identical (11,005 blocks, 4.7 MB).

