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

### 0063 — x64: a faster emitter, the same code

x64 (and the IR's allocation, both backends). Profiled with `OMNI_THREAD_CPU`'s `[thread-host]`
on a real ART start (`omni-linux/tests/b_hello_dex.rs`). Emission spent its time in
`RegAlloc::ValueLocation` (a `std::find` over 96 locations' vectors, for every argument and every
definition's assert), `RegAlloc::EndOfAllocScope` (releasing all 96 after every IR instruction)
and the heap, about a quarter of all samples together.

- **`RegAlloc`.** A location's values are held inline (`HostLocValues`, three, then a vector)
  instead of in a `std::vector`. The 96 locations live in place, not in a vector allocated per
  block. `touched` marks the locations written since the last `EndOfAllocScope`, the only ones it
  releases. `occupied` is a superset of the locations holding values, the only ones `ValueLocation`
  searches, lowest first as before. `SelectARegister` runs the same two `std::partition`s over a
  stack copy instead of a `std::vector`. The desired locations are a `std::span`, not a vector
  built for every `ScratchGpr(loc)` / `Use(arg, loc)` / host call. The allocation orders are viewed
  rather than copied, and A64's general-register order is made once.
- **Deferred emits** are `DeferredEmit` (in place up to 256 bytes), not `std::function`, which
  allocated for every memory access emitted.
- **The fastmem fallbacks** are flat tables (A64), not `std::map`s searched for every access.
- **The IR.** A block's instruction pool takes slabs of 256 instructions, not 4,096 (~400 KiB
  malloc'd and freed for each block translated), and each thread keeps one slab for the next
  block. `VerificationPass` counts uses over a sorted per-thread buffer, not a `std::map`. These
  are the same checks.

No decision changes. Verified bit for bit: `tests/code_size.rs::the_speed_of_emission` with
`OMNI_EMIT_DUMP`, compared with 0062's dump of the same corpus by `tools/compare_emit_dumps.py`:
11,005 blocks, 4.7 MB of code, identical except inside host-address operands. Also the dynarmic-sys
suite per-thread and shared (and with `OD_TEST_COMPACT=1`), omni-cpu (only the known `low_window`
and `subpage` failures), and omni-linux `a1_toybox`, `a4_threads`, `a5_signals`, `b_hello_dex`
and `tbi_off`.

Measured on the corpus, with four interleaved pairs of the 0062 and 0063 binaries (E-cores, idle
priority, medians of 5 passes):

| | Frontend (µs/block) | Emit (µs/block) | Both (µs/block) |
|---|---|---|---|
| 0062 | 12.2 | 36.7 | 48.9 |
| 0063 | 9.2 | 19.0 | 28.2 |

That is emit 1.93× and translation 1.73× faster; wall time 51 → 30 µs a block. On a real ART
start (`b_hello_dex`, `dalvikvm64`, 67k blocks), emit went from 2.08 s to 1.12 s, the frontend
from 0.67 s to 0.53 s, and the run from 3.9 s to 2.8 s.

### 0064 — x64 shared cache: a translated target's link head beside its block

x64, shared caches. `link_heads` mapped every link target to the newest link record naming it: in a
world 784k targets in a 2^21-bucket table, 32 MiB beside a 24 MiB block map (`OMNI_MEM_TRACE`,
2026-10-09). Nearly every target is a translated block, so its head now lives in the block's
`StoredBlock` (12 -> 16 bytes, buckets 24 -> 28) and `link_heads` keeps only the targets with no
translation yet: a block registered takes its waiting head from the map, a block forgotten
(invalidated, evicted) gives it back. `EmitX64::HeadOf`/`KeepHeadOf`. MEASURED
(`tests/shared_bookkeeping.rs`): the census 176 -> 152 bytes a block (block map 48 -> 56, link heads
32 -> 0); in a world's game host ~-28 MiB, in the system's ~-10 MiB. Lookups (E-cores, 5 runs each,
medians): the locked dispatcher lookup 115 -> 70 ns, threaded dispatch with FastDispatch off 30.0 ->
28.0 ns/op. Verified: the dynarmic-sys suite, `a64_exec`/`hostile`/`host_fault` under
`OD_TEST_SHARED_CACHE=1`, omni-cpu's exclusive/tbi/faults/thunk/lifecycle/exclusive_store_fault.

### 0070 — x64 shared cache: translation snapshots (off unless asked for)

x64, shared caches. A cache's translations are written to a file and installed into a later
cache, so a program does not translate again what it ran last time. Nothing changes unless a
host calls these (omni-linux: `OMNI_JIT_SNAPSHOT=<dir>`).

- **The API.** `SharedCodeCache::EnableSnapshots`, `SaveSnapshot(path, key, max_bytes,
  include_unverified)` and `LoadSnapshot(path, key)`; in the shim, `od_code_cache_enable_snapshots`,
  `od_code_cache_save_snapshot` and `od_code_cache_load_snapshot` (ABI 7). The stats count the
  blocks restored, verified and rejected.
- **What is saved.** With snapshots enabled, each block emitted keeps a hash of the guest code it
  was translated from, read back through the translating thread's `MemoryReadCode`. A save writes
  each live region's code bytes and, per block, its location, offset, guest range and hash, link
  slots (`SnapshotSlotsOf`) and fastmem sites (`SnapshotSitesIn`). It also writes the constant
  pool and the *code shape*: a hash of the prelude's code with its 64-bit immediates and its own
  addresses left out, sample host function addresses (this executable, the C runtime), the
  configuration that shapes code, the live switches (0034/0037/0039/0040/0041/0042/0061) and the
  host's features.
- **What a load does.** Only into a cache that has emitted nothing, and only with the same key
  and code shape. It replays the constant pool, then restarts the snapshot's regions in their
  order at the same indices. It copies their bytes, rewrites every slot unlinked and records the
  slots, sites and guest ranges as emission did (`RestoreBlock`, `RestoreGuestRange`). Each block
  is installed with `UNVERIFIED_BLOCK` set in its stored size. `GetBasicBlock` hides such a block,
  and no slot is linked to it.
- **Verification.** The first lookup of a restored block (`VerifyRestored`, at the top of
  `Impl::Emit`) reads its guest code back outside the lock and compares the hash. If it matches,
  the block is entered and linked both ways (`MarkVerified`). If not, it is dropped (and
  translated as usual).

**Why the code can be moved.** A shared cache's block reaches the prelude, the constant pool and
its slots `rip`-relatively, and everything per-thread or per-process through JitState (0022).
MEASURED with `tests/code_size.rs` (`OMNI_EMIT_DUMP2`): 11,005 blocks emitted into two caches at
different addresses, with different monitors, have identical bytes up to their link slots. A
block's absolute host addresses are the helper functions it calls, which the code shape checks.
So is the buffer being out of `call rel32` reach of them; otherwise only a cache at the same
address matches.

Verified:

- `tests/snapshot.rs`:
  - A program restored into another cache, at another address with another monitor, gives the
    same results with nothing translated.
  - Changed guest code drops exactly the one block it touches.
  - A host fault in a restored block is served through its restored fastmem site.
  - Another key (-9), another code shape (compact code on: -10) and a cache that has emitted
    (-7) are refused.
  - A restored cache saved again keeps its unverified blocks, or with
    `OD_SNAPSHOT_ENTERED_ONLY` none of them.
- `tests/code_size.rs::a_snapshot_of_real_code_runs_it_the_same`: bionic `libc.so`'s 4,855
  first blocks, saved and restored into a second cache. Every register file, PC and the data
  arena are identical to the first pass's, nothing is translated, and every block is verified.
- The dynarmic-sys suite, per-thread and shared.

### 0071 — x64 shared cache: restored blocks keep 0064's link heads

x64, shared caches. 0070 was written before 0064 and kept every restored link's head in
`link_heads`. With 0064 a translated target's head lives beside its block: `RestoreBlock` now
chains a link through `HeadOf` (the target's block when it was restored first), and a restored
block takes over the head already waiting for it in `link_heads`, as `RegisterBlock` does.
Without it, a restored target's later links would start a second list in `link_heads` that its
`Patch`/`Unpatch` never walk.

### 0072 — x64: a guest range without its first byte (written as 0065, renumbered after 0070/0071)

x64. A block's `GuestRange` (patch 0026's per-block record, 20 bytes since 0052) kept its first
guest byte, which is always its own location's PC: `Emit`, the only caller of `AddGuestRange`,
passes `descriptor.PC()` (now asserted). `GuestRange::First()` reads it back from the location; the
record is 12 bytes. In a world's game host (`OMNI_MEM_TRACE` 2026-10-09: 636,522 ranges) -4.9 MiB,
in the system host (233k-380k ranges over 40 caches) -1.8 to -2.9 MiB. The same behaviour.

### 0073 — x64 shared cache: the maps shrink after blocks are forgotten (a switch, off; written as 0066)

x64, shared caches. A robin_map never gives its bucket array back, so a cache whose blocks an
eviction (code aging, `EvictTo`; the live limit) or an invalidation forgot kept the block map, the
link heads and the guest-range page index of its busiest moment (0031 did it for `ForgetAllBlocks`
only). `live_shrink_tables` (`od_set_shrink_tables`; omni: `OMNI_JIT_TABLE_SHRINK=1`, lever
`jit_table_shrink=1`), off by default: after `EvictOldest` and after an invalidation that dropped
blocks, `A64EmitX64::ShrinkTables` rehashes each map holding at most half of what its array could at
its load factor down to the smallest power of two that holds it (never below 64 buckets). Host
MXCSR saved and restored around it (tsl's rehash divides in floats). Pointers into the maps are not
held across either call site. MEASURED (`tests/table_shrink.rs`, 353,015 blocks evicted to 38,448): off, the block map stays
14,336 KiB; on, 14,336 -> 1,792 KiB and the page index 7,462 -> 668 KiB (738 off); the code runs
on through the rehashed maps, an invalidation after it too. What it is for:
with `code_age_game` the game's 28 MiB block map (2^20 buckets, 636k blocks) stays at 2^20 after
aging to ~260k blocks; with this, 2^19 (-14 MiB) or less.


With 0070's snapshots: `RestoreGuestRange` passes the saved `First()`, the location's PC, so 0072's
assert holds for restored blocks too.

### 0074 — x64: a snapshot save holds the lock only to copy out

x64, shared caches (0070). `SaveSnapshot` builds the snapshot in memory under the cache's lock
and writes the file after letting the lock go. Before this, it wrote the file with the lock held,
so a game's ~230 MiB held every guest thread for as long as the disk took. The time the lock is
held is kept in the stats as `snapshot_save_lock_ns` (ABI 8). Measured on `b_hello_dex`: 3-8 ms for
5 MiB / 12k blocks and 18-22 ms for 30 MiB / 67k blocks. That scales to about 150-200 ms for the
game's ~525k blocks, once per boot.

### 0075 — x64: a snapshot's code read in as it is entered (switch, off by default)

x64, shared caches (0070). A snapshot loaded eagerly copies all of its code into committed, private
memory. Restored code that never runs again costs as much as code that does; on the device that
was +0.25 GB of private working set (s20). `LoadSnapshot(path, key, lazily)`
(`OD_SNAPSHOT_LOAD_LAZY`; omni-linux `OMNI_JIT_SNAPSHOT_LAZY=1`) instead reads and commits nothing of
the code at load:

- **At load.** The snapshot's regions are left reserved. Every page they cover is marked pending in
  `LazyPages` (a bit per 4 KiB page of the buffer). Records, sites and ranges are made as before,
  but slots are not written. The file stays open (shared for deletion, so a later save can replace
  it) and its regions' file offsets are kept.
- **On first entry.** When a block verifies, the pages it covers are committed and read from the
  file (`Materialize`). The slots on each page are then set to what they would hold had they been
  written all along: their target's entry if it is entered, their unlinked value otherwise
  (`RefreshSlot`).
- **Until then.** Every slot write (`StoreSlot`: linking, unlinking, forgetting) skips a pending
  page; nothing executes code on one, since its blocks are not entered.
- **Eviction and saves.** Evicting a region or forgetting everything drops its pending state. A
  save writes a pending page as zeros: no block it saves lies on one, as each was entered.
- **Load itself.** Both modes now stream the file rather than read all of it into memory first, so
  the eager mode reads region bytes straight into the code buffer. The game's 281 MiB load no
  longer has a 281 MiB transient.

Stats: `snapshot_pages_read` (ABI 9).

Verified: `tests/snapshot.rs` (the same program loaded lazily runs the same with nothing
translated, and saves over the very file it keeps open; changed guest code drops exactly its block,
lazily too). `tests/code_size.rs::a_snapshot_of_real_code_runs_it_the_same`: libc's 4,855 blocks
loaded lazily with a tenth of the functions run give the guest-visible state of a fresh cache
running that tenth, with nothing translated, and read in 64 of ~467 pages (14%). omni-linux
`a2_proc` and `b_hello_dex` pass with it, three boots each (eager, then lazy twice). Read in, as a
share of each snapshot: toybox 628 pages of 3.2 MiB (77%); dalvikvm64 5,703 pages of 30 MiB (74%),
with 65,129 of 67,116 blocks verified. A snapshot is the last run's working set (0074), so a run
like it re-enters most of it. The saving is the part a run does not enter, plus the share of every
read-in page that holds no entered block. dalvikvm64's 2k-17k blocks translated on a reload vary
from run to run the same way in either mode.

### 0076 — x64: restored blocks never entered, forgotten (on request)

x64, shared caches (0070). A restored block that never verifies still costs its bookkeeping: its
block-map entry, its link records and their targets' heads, its fastmem sites, its guest range's
page-index entries, and its `unverified` record (40-byte buckets at a load factor of 0.5). On the
device (s22) the game held ~636k of them, about 80 MB. `SharedCodeCache::ForgetUnverified`
(`od_code_cache_forget_unverified`; omni-linux `OMNI_JIT_SNAPSHOT_FORGET=1`, once a process has
settled) forgets every restored block still unverified, as an invalidation would. Its links out
are unlinked and taken out of their lists, and the links waiting for it stay waiting. It also drops:

- the block's fastmem sites (`ForgetFastmemSitesIn`), which are never faulted at since nothing
  entered the block;
- its guest range's page-index entries (`PruneGuestRangeIndex`, over the serials the load
  registered; the ranges stay as serials);
- its lazy slots (0075);
- the `unverified` map, swapped for an empty one.

0066's `ShrinkTables` then runs, whatever its switch. Not given back: the dead link records
(24 bytes each), which stay until their region is evicted, as an invalidation's do. A location
looked up later is translated as usual. Stats: `snapshot_blocks_forgotten` (ABI 10).

MEASURED (`tests/code_size.rs`, libc, loaded lazily, a tenth of the functions run): 4,331 of 4,855
restored blocks forgotten. Block map 224 -> 28 KiB, link heads 64 -> 8 KiB, fastmem sites 160 -> 36
KiB, guest-range index 106 -> 68 KiB, plus the `unverified` map (~320 KiB), not in the census. Block
links stay at 189 KiB. Every function run afterwards gives what the first pass gave, with exactly
the forgotten and the never-saved blocks translated. `tests/snapshot.rs`: forgotten before
anything ran (eager and lazy), the program runs the same with all of it translated again. Forgotten
after everything verified, nothing goes and nothing is translated.
### 0077 — IR: an opcode's arguments read inline; the verification pass on request

Both backends' frontends (the IR is shared). A sampled profile of first translation on the i5-4460
(Linux, `tests/code_size.rs`, a SIGPROF sampler) put ~8% of it in one-line accessors in other
files: `GetNumArgsOf` and `GetArgTypeOf` (a `std::vector` read through `at()`, called by the
always-on asserts of every `Inst::GetArg`/`SetArg`), `Inst::NumArgs`, `Value::IsImmediate`,
`IsEmpty` and `GetType`. Now:

- `opcode_args`, a table built at compile time from `opcodes.inc`, read by inline
  `GetNumArgsOf`/`GetArgTypeOf` (an index past the arguments throws `std::out_of_range`, as `at()`
  did); `Inst::NumArgs` inline.
- `Value`'s accessors answer an immediate inline; an instruction's value asks it, as before
  (`IsImmediateInst`, `GetTypeInst`).
- x64: `VerificationPass` (argument types and use counts: it asserts and changes nothing) runs only
  with `OMNI_JIT_VERIFY=1`. The arm64 backend's call is left as it was.

MEASURED (`the_speed_of_emission`, 11,005 blocks, 9 passes, medians, i5-4460): frontend **7.60 ->
4.85 us/block (-36%)**, of which the verification pass 1.6 us and the inlining 1.1 us; emit 16.25 ->
~15.8; first translation 24.5 -> ~21.3 us/block. The emitted code is byte-identical
(`tools/compare_emit_dumps.py`: 0 blocks differ against a second run of the base; base against
base differs in the same 11 blocks' `jmp rel32` to code placed elsewhere, run to run).

### 0078 — x64: small fastmem fallbacks (`OMNI_JIT_SMALL_FALLBACKS`, on unless `0`)

x64. `GenFastmemFallbacks` wrote one whole thunk per (ordered, size, address register, value
register) into every code cache's prelude -- ~6,000, each saving and restoring every caller-saved
register: most of the ~1.1 MiB prelude 0017 measured, written (so resident) in every guest process
before its first block, and the time to emit it at every process start. Now each of those keys is a
trampoline of a few bytes (`push` the address -- and a general value -- then `jmp`) into a body
shared by every register that is only an input: a read's body per value register (its result), a
write's and an exclusive write's per size, the 128-bit ones per value register (an XMM cannot be
pushed). The body saves what the thunk saved, with the stack's parity counted for the pushed
operands, takes them from the stack, calls the same callback, and leaves with `lea rsp` (flags
untouched) and `ret`. A host fault's faked call into the fallback enters the trampoline as before.

MEASURED (Linux, System V: 25 caller-saved registers; one cache): **1,888,969 -> 114,836 bytes**
of fallbacks (-94%); Windows (13 caller-saved) proportionally. The dynarmic-sys suite passes with it
on (Linux; the snapshot tests fail there with it off as well: a load answers -10), and a test
binary that makes many caches ran 0.49 -> 0.09 s.

### 0079 — Xbyak: the label manager without a heap node per label

x64 (the vendored Xbyak, `externals/xbyak`). `LabelManager` kept `Label` objects' offsets, the
labels themselves and the jumps waiting for one in `std::unordered_map`, `unordered_set` and
`unordered_multimap`: a node allocated and freed for every label defined, referenced and waited for
-- several for every label of every block, and the x64 emitter makes a few for every memory access.
Now tsl's robin map and set (open addressing; dynarmic already vendors them) hold the live labels and
their offsets, and a flat vector the waiting jumps (a handful at a time: `find` scans, `erase` moves
the last into the hole). A robin map's iterator gives its value read-only, so the one place that
changes a count goes through `value()`. Without tsl on the include path the pin's containers are
used.

MEASURED (`the_speed_of_emission`, 11,005 blocks, 9 passes, medians, i5-4460): **emit 15.7 ->
13.1 us/block (-16.5%)**; the code is byte-identical (`tools/compare_emit_dumps.py`: every
difference inside host-address operands, the same set as between two runs of the base).

### 0080 — x64: a value's host location found without a search

x64 register allocator. `RegAlloc::ValueLocation` searched every occupied host location for the
value -- twice for every argument in `GetArgumentInfo` (the assert, then the use), once more where
an identity is defined, and once for every value defined, in an assert that it is not yet anywhere
(a search that always runs to the end). After 0079 it was the hottest function of first translation
(6.7%, sampled). Now an `IR::Inst` carries a hint of where the allocator last saw it (one byte, in
what was padding), set when it is defined and whenever a search finds it, and checked before it is
trusted (the location is occupied and holds the value): a value is in one location at a time, so a
true hint is the answer the search gives, and a stale one costs only the search. The callers look
the location up once; the "not yet defined" assert runs with the IR verification
(`OMNI_JIT_VERIFY=1`, 0077).

MEASURED (`the_speed_of_emission`, i5-4460): **emit 13.1 -> 11.9 us/block (-9%)**; byte-identical
(`tools/compare_emit_dumps.py`).

### 0081 — IR: a register read forwarded across a width change (`OMNI_JIT_GETSET_WIDTH`, on unless `0`)

Both backends' IR pass (enabled by the x64 translate path; arm64 passes the old options).
`A64GetSetElimination` forwarded a register's known value only to a read of the same width, so `ldrb
w8, [x26, #4]!; ldr x8, [x21, x8, lsl #3]` -- Luau's bytecode dispatch, the hottest loop of the
game's engine worker (`libroblox+0x5f58ae0`, 5.5% of its samples) -- stored X8 and loaded it back two
instructions later, a store-to-load round trip on the dependency chain into the next load, at every
Lua instruction. Now a W write `SetW v` is emitted as `SetX (ZeroExtendWordToLong v)` (what writing Wn
means; the same 64-bit store) and tracked as that X value, with `v` kept for W reads; a W read of an
X value is its low word (`LeastSignificantWord`). An X read after a W *read* still loads (its upper
half is unknown). A first form that inserted the extension at the read instead had the register
allocator spill the W value (`SetW` zero-extends its argument in place) and forwarded X reads after
W reads -- wrong; the differential below caught it.

MEASURED: the dispatch block `mov [r15+0x40],rax; ...; mov rax,[r15+0x40]` -> `mov [r15+0x40],rax;
...; shl rax,3` (188 -> 184 bytes; its neighbours -2..-3%). First translation +2% (frontend 4.78 ->
4.90 us/block, emit unchanged). **Guest-visible differential** (a scratch test, not committed: the
first blocks of every function of libc, libart, libhwui, libandroid_runtime and every KiB of
libroblox's code, registers seeded per start, all 31 registers and the PC hashed after every block,
~385k block runs): identical hashes on and off; the wrong first form changed libart's and
libroblox's. dynarmic-sys suite: as before (Linux: the snapshot tests' known -10).

### 0082 — x64: a value moved out of the way goes to a free callee-saved register (`OMNI_JIT_SPILL_REGS`, on unless `0`)

x64 register allocator. `SpillRegister` always moved a value to a stack slot. It runs whenever a
value is moved out of a register it still has uses after -- a scratch use of a value read again later
(`UseScratchImpl`), a register an instruction needs -- so a Luau handler (`libroblox+0x5f58b8c`, 13
guest instructions) emitted `mov [rsp+0x10],r14d ... mov r14d,[rsp+0x10]` twice, with registers free:
a store and a load back on the next use. Now an empty, unlocked callee-saved register of the same kind
that the allocator may use (in its own GPR/XMM order, so not the fastmem base or the page table) is
taken first, and the stack only when there is none; a callee-saved register keeps the value across a
host call as the stack slot did. The same handler: `mov ebx,r14d ... rorx r10d,ebx,0x10`, no stack
access (500 -> 486 bytes).

MEASURED (i5-4460): code -1% (libc 391.5 -> 387.7 B/block, libart 467.7 -> 464.4), first translation
emit 12.04 -> 12.24 us/block (within noise). Guest-visible differential (`tests/differential.rs`, five
libraries with libroblox, ~385k block runs): identical hashes on and off. dynarmic-sys suite: as
before.

### 0083 — x64: a zero extension of a value already zero-extended is the same register (`OMNI_JIT_ZEXT_TRUST`, on unless `0`)

x64. `ZeroExtendByteToWord`/`HalfToWord`/`WordToLong` (and `...ToLong` through them) always
emitted `movzx`/`mov r32, r32`, because a U8/U16/U32 value in a host register may in general carry
garbage above its width. A guest load does not: `A64ReadMemory8/16/32` leave the whole register
zero above their width on every path (`movzx r32`/`mov r32` on the fast path, plain since 0029 for
ordered loads too; `ZeroExtendFrom` in the fallback thunks, 0078's included, and after a callback),
and so does a zero extension to a word. When the argument is such a value in a GPR, the result is
defined in the same register, with no code. Every register move keeps the property (`mov r32`
between GPRs and from a spill slot; a location holding the aliased 64-bit value moves 64 bits).

With 0081, Luau's dispatch is now `movzx eax, byte [..]; ...; shl rax, 3; lea; mov rax, [..]`: the
`movzx eax, al` and `mov eax, eax` after the load are gone from the chain (they were two of its
cycles, next to the store-and-reload 0081 removed). Code -0.2..-0.3% (libc 387.7 -> 386.8
B/block); first translation within noise. Guest-visible differential: identical on and off.

### 0084 — Xbyak: a byte written in place, the growing out of line

`CodeArray::db(int)` checked the room, grew the buffer or threw, and wrote the byte: too big to
inline, so every byte of emitted code was a call (`Xbyak::CodeArray::db` 4.3% of emission's
samples, the largest single function after the register selection), and `dd`/`dq` were four or
eight of them. The growing and the error are now a `noinline`/`cold` function, `db(int)` a compare
and a store that inlines into Xbyak's encoders, and the multi-byte forms check the room once. The
same bytes (`compare_emit_dumps.py`: the same 11 blocks differ as between two runs of one build, a
jump displacement 0x2000 apart).

MEASURED (`code_size.rs::the_speed_of_emission`, `OD_PROD=1` -- the live switches a device sets,
now in that test too -- i5-4460, 3 interleaved runs): emit **11.06/11.06/11.11 -> 10.51/10.50/10.57
us/block (-5%)**; the frontend unchanged.

### 0086 — IR: `Inst::GetArg` inline; `SetArg`'s type check with the verification (`OMNI_JIT_VERIFY=1`)

`Inst::GetArg` was a call into `microinstruction.cpp` for every argument any pass or emitter reads
(2.5% of emission's samples, self), and `SetArg` (2.7%) checked the new argument's type against
the opcode's on every call -- `Value::GetType` walks to the defining instruction's opcode and its
type table. mcl's asserts are on in release builds. `GetArg` is inline now, with the same two
checks (index, not empty) and their messages out of line; `SetArg`'s type check runs with the IR
verification (0077's `OMNI_JIT_VERIFY=1`), its index check always. Nothing else changes: the same
bytes (`compare_emit_dumps.py`, the same 11 blocks as between two runs of one build), and with
`OMNI_JIT_VERIFY=1` the corpus passes every check.

MEASURED (`code_size.rs::the_speed_of_emission`, `OD_PROD=1`, i5-4460, 4 interleaved runs against
0084): frontend **4.80/4.80/4.80/4.80 -> 4.58/4.64/4.68/4.60 us/block (-4%)**, emit
**10.54/10.57/10.51/10.55 -> 10.17/10.27/10.34/10.26 (-3%)**.

(0085, the register allocator's per-block state kept by the thread and reset where the last block
wrote it, measured within noise -- 10.61 -> 10.52 us/block, no consistent direction over three
pairs -- and was dropped.)

### 0087 — IR: an opcode's return type and the passes' predicates from tables

`GetTypeOf` read `opcode_info` (a `std::array` of name/type/`std::vector` args) through `at()` in
another file; `Inst::GetType` was out of line too; and `Inst::MayHaveSideEffects`, `IsMemoryRead`
and `IsMemoryReadOrWrite` -- asked of every instruction by dead-code elimination (twice a block)
and the get/set elimination -- were chains of out-of-line switches over the opcode (fifteen of them
for `MayHaveSideEffects`). The return type is now in 0077's inline table (`OpcodeArgs::ret`) and
`GetTypeOf`/`GetType` are inline; the three predicates are bits of a per-opcode table made before
`main` from the predicates themselves (each a function of the opcode alone), and inline. The same
answers, so the same IR and the same bytes (`compare_emit_dumps.py`: the same 11 blocks as between
two runs); `OMNI_JIT_VERIFY=1` passes on the corpus.

MEASURED (`code_size.rs::the_speed_of_emission`, `OD_PROD=1`, i5-4460, 4 interleaved runs against
0086): frontend **4.69/4.57/4.62/4.56 -> 4.25/4.32/4.28/4.28 us/block (-7%)**, emit
**10.36/10.23/10.29/10.16 -> 9.92/10.10/9.99/10.07 (-2%)**.

(0088, `SelectARegister`'s two partitions replayed over bitmasks of the locations -- the same swaps
as `std::partition`, so the same choice, with each location looked at once -- was **slower**: 10.12
-> 10.47 us/block over four pairs. Dropped.)

### 0089 — x64: shared labels from a per-thread free list

Every memory access a block makes takes two `SharedLabel`s (`std::make_shared<Xbyak::Label>`: the
slow path's entry and the join), and floating point more: a `malloc` and a `free` each, all within
the block's emission (the sampler put ~1.5% of emission in `malloc`/`free` under `EmitMemoryRead`/
`Write` and `~EmitContext`). `GenSharedLabel` now uses `std::allocate_shared` with an allocator whose
single objects come from, and go back to, a free list of the emitting thread (freed at the thread's
end); the same `shared_ptr`s otherwise. The same bytes (`compare_emit_dumps.py`).

MEASURED (`code_size.rs::the_speed_of_emission`, `OD_PROD=1`, i5-4460, 4 interleaved runs against
0087): emit **10.31/10.10/10.09/10.01 -> 9.75/9.81/9.81/9.80 us/block (-3.3%)**.

### 0090 — x64: a block's codegen census added once

Patch 0060's census (`codegen_census`, read only by the code-size tests) was counted with an atomic
add per part per IR instruction -- two or three `lock xadd`s an instruction, ~100 a block, on cache
lines every emitting thread of the process shares. `Emit` counts a block on the stack now and adds
it to the census once, when the block is done (`CensusTally`): the same totals after every block.
The same bytes; `compact_code.rs` (which reads the census) passes.

MEASURED (`code_size.rs::the_speed_of_emission`, `OD_PROD=1`, i5-4460, one thread, 4 interleaved
runs against 0089): emit **9.87/9.92/9.77/9.85 -> 9.70/9.76/9.75/9.73 us/block (-1.2%)**. More where
threads emit side by side (a boot's system host), which this benchmark does not do.

### 0091 — IR: A64 get/set elimination counts barriers, and asks the opcode table

With 0037's precise mode (on in omni-linux), every guest memory access is a point past which no
earlier Set may be erased; the pass marked that by clearing `set_instruction_present` in all 65
register records (31 X, 32 V, SP, NZCV) at every access. And for every instruction that is not a
Get or Set it called five out-of-line switches over the opcode (`CausesCPUException`, `ReadsFromCPSR`,
`WritesToCPSR`, `ReadsFromCoreRegister`, `WritesToCoreRegister`). Now each record keeps the barrier
count of its Set, and a Set is erased only if no access has come since (`set_barrier == barrier`):
one increment per access. The five questions are three more bits of 0087's per-opcode table. The
same IR, so the same bytes (`compare_emit_dumps.py`); `OMNI_JIT_VERIFY=1` passes on the corpus.

MEASURED (`code_size.rs::the_speed_of_emission`, `OD_PROD=1`, i5-4460, 4 interleaved runs against
0090): frontend **4.26/4.32/4.27/4.24 -> 3.95/3.96/3.98/3.98 us/block (-7%)**; emit unchanged.

### 0092 — x64: `SelectARegister` takes a free, empty first candidate at once

`SelectARegister` copies the candidates and runs two `std::partition`s (unlocked, then empty) to
pick the first of the result: the largest single function of emission after 0084-0091 (5.5% of
samples). When the first candidate is unlocked and empty, both partitions keep it first --
`std::partition` swaps only elements behind the first one that fails -- so it is the choice, and is
returned without looking at the others. Exactly the same choice, so the same bytes
(`compare_emit_dumps.py`).

MEASURED (`code_size.rs::the_speed_of_emission`, `OD_PROD=1`, i5-4460, 4 interleaved runs against
0091): emit **9.61/9.64/9.78/9.65 -> 9.45/9.50/9.56/9.47 us/block (-1.8%)**.

### 0093 — IR: what a `Value` asks of its instruction, inline

`Value::IsImmediate` and `GetType` were inline for immediates since 0077, but for an instruction's
value called `IsImmediateInst`/`GetTypeInst` in `value.cpp` (which call `IsIdentity`, also out of
line), because `value.h` cannot see `Inst`: 3.2% of emission's samples between the two, for every
argument any pass or the emitter looks at. `value.h` now includes `microinstruction.h` at its end
(after `Value` is complete; `#pragma once` makes the cycle a no-op either way round), and
`microinstruction.h` defines `IsIdentity`, `IsImmediateInst` and `GetTypeInst` inline after `Inst`, so
every user of `Value` has them. `GetInst` is inline in `value.h`. The same code, so the same bytes
(`compare_emit_dumps.py`).

MEASURED (`code_size.rs::the_speed_of_emission`, `OD_PROD=1`, i5-4460, 4 interleaved runs against
0092): frontend **4.03/4.05/4.02/4.01 -> 3.85/3.86/3.87/3.86 us/block (-4%)**, emit
**9.53/9.66/9.56/9.53 -> 9.29/9.35/9.34/9.33 (-2.5%)**.

### 0094 — x64: a block's perf-map name only when there is a perf map

`RegisterBlock` passed `LocationDescriptorToFriendlyName(descriptor)` -- a `fmt::format`ted
`std::string` -- to `PerfMapRegister` for every block emitted, on every host. On Windows and macOS
`PerfMapRegister` does nothing; on Linux it took a global mutex and read `PERF_BUILDID_DIR` from the
environment again each time (the map file is only opened when it is set, so without it every call
looked again). `PerfMapEnabled()` reads the variable once (false off Linux), `PerfMapRegister`
returns before the mutex without it, and `RegisterBlock` builds the name only with it. The same
bytes; a perf map is written as before when `PERF_BUILDID_DIR` is set at the first registration.

MEASURED (`code_size.rs::the_speed_of_emission`, `OD_PROD=1`, i5-4460 Linux, 4 interleaved runs
against 0093): emit **9.23/9.26/9.37/9.27 -> 8.82/8.79/8.72/8.72 us/block (-5.6%)**. On Windows
only the formatting (and its allocation) was wasted.
