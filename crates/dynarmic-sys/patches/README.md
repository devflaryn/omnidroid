# Patches carried against the pin

D5 adopted dynarmic as "a pinned fork we carry patches against". This directory
is where those patches are recorded.

**The vendored tree is upstream `9d4582339990d4eae53f1dc7160686920fc2075c`
plus the patches listed under "Applied".** Until patch 0001 it was pristine,
which kept "the 202,200 upstream assertions pass on our pin" a claim about
upstream rather than about us; that figure has **not** been re-measured with
0001 applied, and should be before it is repeated.

**Re-measured 2026-09-24, in the runtime's own configuration**
(`-DDYNARMIC_FRONTENDS=A64`, Release, MSVC 2022, `dynarmic_tests.exe` with no
filter): **All tests passed (201,698 assertions in 84 test cases)** with 0001
alone (built from a clean checkout of `83cfa6e`) **and the identical figure with
0001 + 0017, with 0001 + 0017 + 0018, with 0001-0020** (2026-09-25) **and with 0001-0022**
(2026-09-25; the suite runs the per-thread path, which 0022 leaves as it was) **and with 0001-0022
plus 0024-0027** (2026-09-25; 0026 replaces the per-thread path's guest-range bookkeeping, which the
suite's invalidation tests exercise; 0023 is on another branch). **Not re-run with 0028**: its one
change to the per-thread path is that a block covering no guest bytes now has a guest-range record
(kept out of the page index, so never returned). The older 202,200/123 was a build that also had the A32 frontend;
it is not comparable and was not re-run.

**On the arm64 backend** (Apple M1, same configuration, AppleClang, Ninja, with
`-DDYNARMIC_WARNINGS_AS_ERRORS=OFF` because 0007's inline store-exclusive has a
lambda capture clang flags as unused): **All tests passed (201,698 assertions in
83 test cases)** with 0001-0020, 2026-09-25 -- the first recorded run of the
suite on that backend, so there is no earlier arm64 figure to compare with --
**and the identical figure with 0001-0021** (2026-09-25; the suite runs
upstream's safe flags only, so it exercises 0021's global arm, whose emitted code
is unchanged). 0021 is arm64-only and not compiled on x64, so the MSVC figure
stands for it without a rerun.

## Applied

### 0001 — `MRS Xt, CNTVCT_EL0` reads the counter `CNTPCT_EL0` reads

`0001-a64-mrs-cntvct_el0.patch`. The pin's `MRS` knows `CNTPCT_EL0` and not
`CNTVCT_EL0` (`S3_3_C14_C0_2`), so the virtual count fell to
`InterpretThisInstruction()`, which Omnidroid has no interpreter behind: the
guest stopped with `UnsupportedInstruction { encoding: 0xd53be048 }`.
`ARCHITECTURE.md` section 6 and D5 amendment 4 both recorded the gap and that
Android's userspace clock reads this register rather than `CNTPCT_EL0`.

**MEASURED, and why it had to be fixed now**: `libroblox.so` executes
`mrs x8, cntvct_el0` at link `0x229d184` on a guest worker, and M6's gate lost
that thread to it on every run once the client-settings fetch completed
(2 of 2 runs, as thread 16 and as thread 3).

**Why the same value is correct and not merely convenient**: EL0 reads
`CNTVCT_EL0` as `CNTPCT_EL0 - CNTVOFF_EL2`, and Linux arm64 clears
`CNTVOFF_EL2` when it boots at EL2 (`msr cntvoff_el2, xzr` in its EL2 timer
setup). So on the platform the guest was built for, the two registers read the
same count, at the same `CNTFRQ_EL0` — which is what `omni-cpu`'s
`cntvct_reads_the_same_clock_as_cntpct` asserts, from guest `MRS`
instructions.

### 0002 — arm64: the `Interpret` terminal calls the interpreter fallback

`0002-arm64-interpret-terminal.patch`. **arm64 hosts only; the x64 backend is
untouched.** The A64 frontend ends a block with `IR::Term::Interpret` in front
of every word it cannot translate — the 231 commented-out decoder entries,
including every LSE atomic, and every visitor that calls
`InterpretThisInstruction()`. The x64 backend turns that terminal into a call to
`UserCallbacks::InterpreterFallback`; the pin's arm64 backend had
`ASSERT_FALSE("Interpret should never be emitted.")`
(`emit_arm64_a64.cpp:36`), so on an arm64 host **the first undecodable guest
word terminated the process**. MEASURED on Apple M1: `hostile.rs`'s fuzzer died
at trial 2 with exactly that message, and `tests/interpret.rs` aborted with
`SIGABRT` before the patch.

The patch gives arm64 the x64 terminal, step for step: charge the cycles used
so far (`AddTicks`), store the PC, install the **host's** `FPCR` (the x64
terminal does `SwitchMxcsrOnExit`), call
`InterpreterFallback(pc, num_instructions)` through a new prelude trampoline
(`LinkTarget::InterpreterFallback`), reload the guest's `FPCR` from `JitState`
(x64's `return_from_run_code[MXCSR_ALREADY_EXITED]` does
`SwitchMxcsrOnEntry`), re-read the budget (`GetTicksRemaining`), and return to
the dispatcher, which checks the halt flag and the budget before looking up the
next block — the same loop x64's `ReturnFromRunCode(true)` enters.

`tests/interpret.rs` asserts the contract from guest code: the preceding
instructions are committed, the PC handed over is the unknown instruction's,
`num_instructions` counts a merged run, a fallback that does not halt resumes at
the PC it left, and the fallback runs under the host's `FPCR` while the guest's
is back in force afterwards (a subnormal multiply under `FPCR.FZ`, both ways).

### 0003 — arm64: scalar saturating add, subtract and doubling multiply-high

`0003-arm64-scalar-saturation.patch`. **arm64 only.** `SignedSaturatedAdd8/16/32/64`,
`SignedSaturatedSub*`, `UnsignedSaturatedAdd*`, `UnsignedSaturatedSub*` and
`SignedSaturatedDoublingMultiplyReturnHigh16/32` were `ASSERT_FALSE("Unimplemented")`
in `emit_arm64_saturation.cpp`, and the A64 frontend reaches all eighteen from
`simd_scalar_three_same.cpp` (`SQADD`/`UQADD`/`SQSUB`/`UQSUB` scalar at every
size — there is no size guard — and `SQDMULH` scalar at H and S, also from
`simd_scalar_x_indexed_element.cpp`). One guest instruction was a terminated
process; `tests/a64_saturation.rs` aborted with `SIGABRT` before the patch.

Each is now the host's own scalar AdvSIMD instruction on the element's `B`/`H`/`S`/`D`
register, which is the ARM ARM operation exactly (these are ARMv8.0 base
instructions, present on every arm64 host) and sets the host's `FPSR.QC` on
saturation. The FPSR manager is loaded first, exactly as the vector forms in
`emit_arm64_vector_saturation.cpp` do, so the host `QC` is folded into the
guest's `FPSR` at the next spill; the x64 backend ORs the same bit into
`JitState::fpsr_qc`. `tests/a64_saturation.rs` checks every form at both bounds
and in range, the cleared upper bits of `Vd`, and `QC` (set, clear, and sticky),
against values worked from the ARM ARM pseudocode (`SatQ`, and
`(2 * a * b) >> esize` for `SQDMULH`).

### 0004 — arm64: half-precision arithmetic, and a fallback that dropped its result

`0004-arm64-half-precision.patch`. **arm64 only.** Two defects in one family.

**Unimplemented.** Every FP16 opcode the A64 frontend can emit and the arm64
backend lacked was `ASSERT_FALSE("Unimplemented")`: scalar `FPAbs16`, `FPNeg16`,
`FPMulAdd16`, `FPMulSub16`, `FPRoundInt16`, `FPRecipEstimate16`,
`FPRecipExponent16`, `FPRSqrtEstimate16`, `FPRecipStepFused16`,
`FPRSqrtStepFused16`, `FPHalfToFixed{S,U}{32,64}`, and vector `FPVectorEqual16`,
`FPVectorMulAdd16`, `FPVectorNeg16`, `FPVectorRecipEstimate16`,
`FPVectorRSqrtEstimate16`, `FPVectorRecipStepFused16`, `FPVectorRSqrtStepFused16`.
They are reached by `FMADD`/`FMSUB`/`FNMADD`/`FNMSUB`, `FABS`, `FNEG`, `FRINT*`,
`FRECPE`, `FRECPX`, `FRSQRTE`, `FRECPS`, `FRSQRTS`, `FCVT*`, `FCMEQ`, `FMLA`/`FMLS`
at `ftype == 0b11` / `.4H`/`.8H` — `hostile.rs`'s fuzzer died on `FMSUB H` at
trial 1002. The arithmetic ones now call dynarmic's own `FP::` routines, **the
same ones the x64 backend calls for these opcodes** (x64 has no half-precision
arithmetic), so both hosts produce the same bits and no host FEAT_FP16 is
assumed; `FPAbs16`/`FPNeg16`/`FPVectorNeg16` are the sign-bit operations they
are. Exceptions accumulate into the guest's `FPSR` after the FPSR manager is
spilled.

**A silent wrong answer.** `EmitTwoOpFallbackWithoutRegAlloc` saved and restored
`ABI_CALLER_SAVE & ~(1ull << Qresult.index())` — which removes the
*general-purpose* register with the result's number and keeps the result's `Q`
register in the list, so the pop put the result register's old contents back
over the computed value. It serves `FPVectorRoundInt16`, reached by
`FRINT{N,M,P,Z,A,X,I}` (vector, half), which is **active** in the decoder.
MEASURED before the patch: `FRINTN V0.8H, V1.8H` returned `[0, 0]`. The mask is
now `~ToRegList(Qresult)`, and the new three- and four-operand helpers use the
same.

**Unreachable from A64, left as they are, with the evidence:**
`FPHalfToFixedS16/U16` (only `FPToFixedS16/U16`, which only the A32 frontend
calls, `A32/translate/impl/vfp.cpp:1073`); `FPVectorToSignedFixed16`,
`FPVectorToUnsignedFixed16` (`FloatConvertToInteger` fixes esize at 32/64,
`simd_two_register_misc.cpp:107`; `ConvertFloat` rejects `immh` 0001-0011,
`simd_shift_by_immediate.cpp:197`; the half forms are `//INST` in `a64.inc`);
and the `RoundingMode::ToOdd` arms of both `EmitToFixed` helpers (A64 passes a
constant mode or `FPCR.RMode`, a 2-bit field that cannot hold `ToOdd` = 5; the
only `ToOdd` in the A64 frontend is `FCVTXN`, which is not a to-fixed
conversion).

`tests/a64_fp16.rs`: every reachable form, with encodings checked against the
LLVM assembler and values worked from IEEE binary16 and the ARM ARM (the 8-bit
estimate tables, `FPRecpX`, fused single rounding shown by a lane whose exact
result is a subnormal that a two-rounding implementation would flush to +0),
plus FPSR `IOC`/`DZC`/`IXC` where the ARM ARM raises them.

### 0005 — arm64: the SM4 substitution box

`0005-arm64-sm4-sbox.patch`. **arm64 only.** `SM4E` and `SM4EKEY` (FEAT_SM4,
active in `a64.inc`) translate to IR that looks bytes up through
`SM4AccessSubstitutionBox`, which was `ASSERT_FALSE("Unimplemented")` in
`emit_arm64_cryptography.cpp`; the first `SM4E` terminated the process
(`tests/a64_sm4.rs` aborted before the patch). It now calls
`Common::Crypto::SM4::AccessSubstitutionBox`, as the x64 backend does, through a
lambda that takes the index as a `u64` and narrows it in C++ (Apple's arm64 ABI
makes the *caller* extend sub-32-bit arguments, which generated code does not
promise). `tests/a64_sm4.rs` runs the SM4 specification's own example — key
schedule and 32 rounds through eight `SM4EKEY` and eight `SM4E` — and checks
the ciphertext `681edf34 d206965e 86b3e94f 536e4246`.

### 0006 — arm64: 64-bit unsigned max/min, which is how `CMHS`/`CMHI` compare

`0006-arm64-unsigned-compare64.patch`. **arm64 only.** The IR has no 64-bit
unsigned compare; `IREmitter::VectorGreaterEqualUnsigned` is
`VectorEqual(VectorMaxUnsigned(a, b), a)` and `VectorGreaterUnsigned` is
`NOT VectorEqual(VectorMinUnsigned(a, b), a)`. At `esize == 64` those are
`VectorMaxU64`/`VectorMinU64`, `ASSERT_FALSE("Unimplemented")` on arm64, and
`CMHS`/`CMHI` scalar (`D` only) and vector `.2D` are active decoder entries.
FOUND by `hostile.rs`'s fuzzer at trial 158,510 (`CMHS D18, D23, D24`), in a
300,000-trial run made after 0002-0005. AdvSIMD has no 64-bit `UMAX`/`UMIN`, so
each selects per lane on `CMHI` with `BSL`. `tests/a64_compare.rs` checks
both forms with operands a signed compare would order the other way.

`VectorMaxS64`/`VectorMinS64` stay unimplemented: their only caller is
`VectorMinMaxOperation` (`SMAX`/`SMIN`), which rejects `size == 0b11`
(`simd_three_same.cpp:174`), and the signed compares are built from
`VectorGreaterSigned`, not from max/min.

### 0007 — arm64: `fastmem_exclusive_access` is honoured (inline `LDXR`/`STXR`)

`0007-arm64-inline-exclusives.patch`. **arm64 only.** Root cause of
`a64_exec::atomic_load_exclusive_store_exclusive` failing on the arm64 host:
the backend accepted `fastmem_exclusive_access` and ignored it —
`EmitExclusiveReadMemory`/`EmitExclusiveWriteMemory` called the callback-only
versions unconditionally, and `EmitConfig` never carried the flag. Every
exclusive pair therefore cost a slow-path read plus an exclusive-write callback
(MEASURED: `slow_path_total == 2` where x64 gives 0), which is also exactly
what `omni-cpu`'s per-slice invariant refuses as `DegradedMemoryPath` — so on
arm64 the first `LDXR`/`STXR` in a slice would have stopped the guest.

The patch is the x64 backend's inline protocol (`EmitExclusiveReadMemoryInline`,
`EmitExclusiveWriteMemoryInline`) on arm64, against the **same** monitor
fields and the same `SpinLock` word, so inline and callback threads of one
monitor interoperate:

* read: lock; `exclusive_state = 1`; `address[pid] = vaddr`; load-acquire
  through fastmem; `value[pid] = value`; unlock.
* write: lock; `status = 1`; if the state is set and `address[pid] == vaddr`,
  compare-and-swap `value[pid] -> value` at the host address with an
  `LDAXR`/`STLXR` loop (`LDAXP`/`STLXP` for 128 bits; ARMv8.0, no LSE assumed),
  then clear every processor's reservation of the address (the monitor's
  `CheckAndClear`, this one's included); `exclusive_state = 0` either way (a
  store-exclusive always leaves the local monitor open); unlock.
* a fastmem miss — out of the fastmem range, or a host fault at the patched
  load — goes to a fallback that **releases the lock first** and then does the
  whole access through new `Wrapped*` trampolines that call the monitor exactly
  as the callback-only path does; `recompile_on_fastmem_failure` rebuilds the
  block without the inline path.

`EmitConfig` gains `fastmem_exclusive_access`, `global_monitor`, `processor_id`.
128-bit stores borrow four general-purpose registers on the stack for the length
of the sequence (the arm64 register allocator has no scratch-register request);
the host-fault entry gives them back before falling into the fallback.
The monitor accessors come from `backend/x64/exclusive_monitor_friend.h`, which
is backend-neutral. **dynarmic's exception handler is untouched.**

`tests/exclusive.rs`: every width and the pair form, inline, with
`slow_path_total == 0`; the failure cases (no reservation, `CLREX`, a spent
reservation, another address) on both paths; the fallback through a
fastmem-range miss, served by the callbacks with the same answers; and four
threads on one monitor and one arena doing 50,000 `LDAXR`/`STLXR` increments
each, all inline and inline mixed with callback threads — no lost update.

### 0008 — arm64: the memory-abort check reads the halt word as the 32 bits it is

`0008-arm64-halt-word-is-32-bit.patch`. **arm64 only.**
`EmitA64CheckMemoryAbort` — emitted on the fallback of every fastmem access
when `check_halt_on_memory_access` is set, which `omni-cpu` sets — loaded the
halt word with `LDAR Xscratch0, [Xhalt]`, a **64-bit** load-acquire, from
`A64::Jit::Impl::halt_reason`, a `u32` at a 4-byte-aligned address. A
load-acquire must be naturally aligned, so the check itself took an alignment
fault inside translated code; dynarmic's handler found no fastmem patch at that
PC and terminated the process (`Segfault wasn't at a fastmem patch location!`).
So on arm64 **every guest access to unmapped memory killed the process instead
of becoming a typed fault** — the first fastmem miss faulted correctly, was
redirected to the fallback, served, and then the abort check faulted.

MEASURED before the patch, with dynarmic's handler alone (no Omnidroid fault
handler installed): fault 1 at the patched `LDR`, redirected; fault 2 at
`c8dfff70` (`LDAR X16, [X27]`) with `X27 = 0x…1ac`. `tests/host_fault.rs`
(identity fastmem, `check_halt_on_memory_access`, guest address `0x2000` in
`__PAGEZERO`) aborted before the patch and passes after, for a load, a store,
and the inline exclusive pair and doubleword pair of 0007. The A32 twin in
`emit_arm64_a32.cpp` has the same instruction; A32 is not built here, so it is
left alone.

### 0009 — arm64: the prelude invalidates what it wrote, not the whole code cache

`0009-arm64-invalidate-only-the-prelude.patch`. **arm64 only.**
`A64AddressSpace::EmitPrelude` ended with `mem.invalidate_all()`, the cache-maintenance loop over
**every page of the code cache**. On macOS that is `sys_icache_invalidate`, and cache maintenance on
an untouched page faults it in: MEASURED with a C probe, a 32 MiB `MAP_JIT` mapping costs +0.00 MiB
of `phys_footprint` untouched and **+32.03 MiB** after one `sys_icache_invalidate` over it. So every
jit -- one per guest thread -- paid its whole code cache in memory at creation, whatever it later
emitted: 39 jits x 32 MiB at the landing screen, about 1.2 GiB of a 3.2 GiB footprint (footprint(1),
`MallocStackLogging` stacks ending in `AddressSpace::AddressSpace`). Only the prelude has been
written at that point; every block emitted later is invalidated on its own in `AddressSpace::Emit`.
`tests/code_cache_charge.rs` creates a jit with a 32 MiB cache and requires it to cost less than
2 MiB, and still to run translated code. The A32 twin (`a32_address_space.cpp`) has the same call;
A32 is not built here.

### 0010 — arm64: keep what is read of an emitted block, in flat records

`0010-arm64-compact-block-records.patch`. **arm64 only; no change to what is emitted or when.**
`AddressSpace` kept, for every block it emitted: the block's whole `EmittedBlockInfo` (a vector and
two robin_maps, 200 bytes) **inline in the buckets** of `block_infos`, a robin_map keyed by entry
point at a load factor of at most 0.5; a `std::map` node for the reverse lookup; and, for every link
target, a robin_set of referring entry points in the buckets of `block_references` (96 bytes each,
and `RelinkForDescriptor`'s `operator[]` made one for every emitted block's own location). Each
block's fastmem patch sites were a robin_map of their own -- a separate allocation per block.

**MEASURED** in the macOS gate (census built into a scratch build of the pin, counters per jit, read
at +30/45/60/75/85 s, n = 1 run; bucket and element sizes from `sizeof` on the vendored headers):
at +60 s, **593,699 blocks over 39 jits** (88,163 in the largest, 616 in the smallest), 2,099 bytes of
bookkeeping per block before malloc rounding -- `block_infos` buckets 890, `block_references`
buckets 484 + sets 46, fastmem maps 281, `relocations` vectors 134, per-block link maps 102 + 20,
`block_entries` 78, reverse map 64 -- 1.19 GiB in all, which `heap(1)` and `footprint(1)` confirm
(`MALLOC_LARGE` 912 MB, `MALLOC_SMALL` 649 MB). The engine's heavy threads each hold 20-125 thousand
blocks of the same code.

What is read after a block is emitted is only its entry point, location and size, its fastmem patch
sites (`FastmemCallback`), and where it links to each target (`RelinkForDescriptor`);
`relocations` is consumed by `Link` at emission and never read again. The patch keeps exactly that:

* `block_records`: entry point, location, size, first patch site -- 24 bytes, **appended in
  ascending entry-point order**, because emission only moves forward until `ClearCache` (asserted).
  A binary search replaces `reverse_block_entries` and `block_infos` (`ReverseGetLocation`,
  `ReverseGetEntryPoint`, `FastmemCallback`).
* `fastmem_records`: one per patch site, 24 bytes, grouped by block and sorted by offset, so the
  handler finds the site by binary search within the block -- the same key the per-block map had.
  `FakeCall::call_pc` is kept as an offset from the entry point (it is inside the block; asserted).
* `link_records` + `link_heads`: one 24-byte record per block relocation, chained per target from
  the newest; a block's records for one target are adjacent in the chain, so `RelinkForDescriptor`
  patches each referring block's links to that target and invalidates that block once, as before.

Nothing is dropped earlier than before: an invalidated block keeps its records until `ClearCache`,
exactly as it kept its `block_infos` and `block_references` entries -- `FastmemCallback` can be
entered from a block that has just been invalidated (the recompile path does that to itself), and
`RelinkForDescriptor` keeps patching stale blocks' links, as it did. `ClearCache` now **gives the
memory back** (`= {}` / swap) where `clear()` kept a robin_map's buckets and a vector's capacity.
The emitters are untouched: `EmittedBlockInfo` is still their product, and is freed after `Emit`.

`tests/bookkeeping.rs` measures bytes in use by the allocator (`malloc_zone_statistics`, every zone;
it first shows the reading sees a 16 MiB allocation) around the translation of 8,192 blocks with one
patch site and one link each: **1,785 bytes per block on the pin, 457 with the patch** (n = 8,192
blocks, of which 147 is the pin's `block_ranges`, which 0011 addresses); the bound is 700. It also
covers the two lookups the records replace, both of which pass on the pin too: three blocks linked to
one that is rewritten and invalidated must each stop running its stale translation (the chain walk),
and a host fault at the third of a block's four patch sites must be served as the third
(`FastmemCallback`'s binary search).

### 0011 — arm64: the guest ranges of emitted blocks, compact and cleared with the cache

`0011-arm64-guest-range-index.patch`. **arm64 only.** `A64AddressSpace` recorded the guest bytes each
block was translated from in a `BlockRangeInformation<u64>` (`backend/block_range_information.cpp`,
shared with x64 and not edited here): a boost::icl `interval_map` of `std::set<LocationDescriptor>`,
in which overlapping blocks split each other's intervals and copy the sets. **And nothing cleared it**:
`AddressSpace::ClearCache` did not know it existed, so it grew for the whole life of the jit, across
every cache clear, and never dropped an invalidated block's range either (upstream's own
`TODO: EFFICIENCY` in `InvalidateRanges`).

MEASURED in the gate at +60 s (`heap(1)` with `MallocStackLogging=lite`, n = 1 run): 846,107 icl
nodes (81 MB) and 874,315 set nodes (42 MB) -- 207 bytes per block, for the 594,083 blocks the jits
held, after six cache clears the map had outlived.

The patch keeps one 24-byte `GuestRange` per emitted block (location and the closed range the pin
registered, `[PC, EndLocation.PC - 1]`, skipped when empty as the icl skipped it), indexed by the
4 KiB guest pages it covers; a block covering more than 64 pages goes to a list checked on every
invalidation instead. `InvalidateCacheRanges` returns every location registered with a range
intersecting a requested one -- what `InvalidateRanges` returned -- looking the pages up, or walking
the index when more pages are asked about than it has (the whole-guest-space invalidation
`omni-android` sends when a context's cross-thread queue overflows). `ClearCache` is now virtual and
`A64AddressSpace` clears the ranges with the cache, so `Emit`'s own clear of a full cache clears them
too.

**The one difference, stated exactly.** After a clear, a range that only a *pre-clear* translation of
a location covered no longer invalidates that location's current translation. That translation was
made after the clear from the guest bytes as they then were, and registered the range it read; a
write elsewhere cannot make it stale. So what the guest executes is unchanged, and the pin's extra
invalidation there -- a retranslation of code that had not changed -- is gone.

`tests/bookkeeping.rs`, n = 32,768 blocks: **440 bytes per block with 0010 alone, 365 with 0011**;
after `od_jit_clear_cache`, **128 bytes per block still held with 0010 alone, 0-1 with 0011** (the
bound is 16). Three behaviour tests cover the index, and pass on the pin as well: a write to the
second page of a two-page block, a write to the last word of a 70,001-instruction block (more than
64 pages: the retranslation fetches all of it), and a 16 GiB invalidation reaching every block.

### 0012 — arm64: an invalidation that leaves no block standing is a clear

`0012-arm64-an-invalidation-that-leaves-nothing-is-a-clear.patch`. **arm64 only.** After
`InvalidateCacheRanges`, if no block is left in `block_entries`, `A64AddressSpace` calls
`ClearCache`.

**Why it matters here, MEASURED** (census build of the pin, gate, n = 1 run): at +85 s the jits held
835,605 block records of which **451,075 were invalidated blocks** -- 1,045,686 of 1,581,230 blocks
emitted since start had been invalidated. Almost all of it came from one request: a 16 GiB range
`[0x7000000000, 0x73ffffffff]`, the whole guest space, found up to 78,357 blocks at a time. That is
`omni-android`'s cross-thread code invalidation (`boundary.rs`, `CodeWatch::broadcast`): every
guest `munmap`/`mprotect`/`MADV_DONTNEED` is queued for every other live context, and a context
that has not crossed the boundary for 64 of them has its queue collapse to "the whole address
space". Invalidated blocks keep their records and their code until the cache is cleared, so each
such jit held a dead copy of its whole translation and kept emitting above it until the cache filled.

**Why it is the same thing the guest could see.** `InvalidateCacheRanges` runs only from
`Jit::Impl::PerformRequestedCacheInvalidation`, before or after `RunCode` -- never with generated
code on the stack. The return stack buffer is on `RunCode`'s frame and rebuilt at every entry, and
every link to an invalidated location was pointed back at the dispatcher when it was invalidated
(`RelinkForDescriptor(descriptor, nullptr)`). So when nothing is left in `block_entries`, no
invalidated block can run again, and `ClearCache` -- what `Jit::ClearCache` (the guest's
`IC IALLU`) does at the same point -- gives back exactly what cannot be used: the records and the
cache space. The fastmem recompile path, which invalidates from inside generated code, calls
`InvalidateBasicBlocks` directly and is not affected.

`tests/bookkeeping.rs`, n = 32,768 blocks: after invalidating every block between runs, **364 bytes
per block still held without the patch, 1 with it**; the chain then runs again, retranslated.
Rows mac-mem-A7 (reverted) and mac-mem-B2 (every invalidation clears: `a64_exec`'s test that a
small invalidation spares the other translations must fail).

### 0013 — arm64: the bookkeeping's large arrays are pages of their own

`0013-arm64-page-backed-bookkeeping.patch`. **arm64 only.** A new header,
`backend/arm64/page_backed_allocator.h`: `PageBackedAllocator<T>` maps an array of at least 256 KiB
anonymously (`mmap`) and unmaps it when it is freed; a smaller one uses `operator new` as before. The
containers of 0010-0011 -- `block_entries`, the three record vectors, `link_heads`, `guest_ranges` and
the page index -- allocate through it.

**Why.** Those containers grow by doubling and `ClearCache` gives them back whole, so their large
arrays are allocated and freed many times over a jit's life -- and a large array freed through the
C++ heap is the host allocator's to keep, dirty and charged to the process.

`tests/bookkeeping.rs` measures `phys_footprint` around a clear of 32,768 blocks' bookkeeping
(11.4 MiB, counted as the zones' bytes in use plus the allocator's mapped bytes, which the shim
reports through a new `od_page_backed_bytes()`; the footprint instrument is first shown seeing 16 MiB
touched): **the clear took 9.06 MiB off the footprint with the patch, 0.00 MiB with no array
page-backed** (the threshold set out of reach, n = 2), and the bound is three quarters of what was
held. Row mac-mem-A8 is that mutation.

**In the gate** (+60 s): `MALLOC_LARGE` in use went from 85-113 MiB (n = 3, after 0012) to no
in-use region at all (n = 3) -- the arrays are now mappings, charged for the pages written rather than for a
vector's spare capacity -- and the landing-screen footprint from 876-892 MiB (n = 3) to 811-841 MiB
(n = 4). **A correction, recorded because it was nearly carried as the reason for this patch:** at
the landing screen `vmmap` also shows 46-63 MiB of dirty `MALLOC_LARGE (empty)` regions -- freed
large blocks the allocator keeps -- and their sizes (3-14 MiB) suggested these arrays. With the patch
they are still there (55-61 MiB, n = 3), so they are someone else's; they are not attributed here.

`od_page_backed_bytes()` is additive in the shim (`od_dynarmic.h`): no struct or existing signature
changes, so `OD_DYNARMIC_ABI_VERSION` stands; it answers 0 where the arm64 backend is not built.

### 0014 — arm64: the store-exclusive is a fastmem patch location too

`0014-arm64-the-store-exclusive-is-a-patch-location-too.patch`. **arm64 only**, a defect in 0007.
0007's inline store-exclusive is a compare-and-swap -- a load-acquire exclusive, then a
store-release exclusive -- and it registered only the **load** as a fastmem patch location. A page
the host lets the load read but not the store write (read-only: the guest's sealed relro) faults at
the store, at a host PC dynarmic has no record of, and its handler aborts the whole process
("Segfault wasn't at a fastmem patch location!"). Found by the native-backend workstream's survey of
all 245,117 `.eh_frame` functions of `libroblox.so` (docs/ports/macos-hvf.md 4.7), reduced here to
two functions: `0x2247264` leaves a pointer into `.data.rel.ro` where `0x224822c` hands it to the
outlined `__aarch64_swp8_rel` (`LDXR` at `0x2b9e87c`, `STLXR` at `0x2b9e880`). The store-release is now registered
with the same fault entry as the load (the lock is held and, for 128 bits, the borrowed registers
are on the stack at both). x64 is unaffected: its inline compare-and-swap is one `LOCK CMPXCHG`,
which is its patch location. `tests/host_fault.rs` stores exclusively to the test binary's own
read-only data (both widths); `omni-cpu`'s `exclusive_store_fault.rs` runs the two real functions;
the full survey then runs all 245,117 functions on dynarmic without an abort (MEASURED once, 41 s).

### 0015 — arm64: an invalidation walks only the chunks that hold translated code

`0015-arm64-invalidate-only-chunks-with-code.patch`. **arm64 only**, on top of 0011. `omni-android`
hands every guest `mmap`, `munmap`, `mprotect` and `MADV_DONTNEED` range to **every** guest thread's
jit, because any of them could have held translated code; almost none did. 0011's page index
answered a range with one hash lookup per 4 KiB page whenever the range had fewer pages than the
index, on every thread. MEASURED in a game on the macOS host (`sample`, 5 s, n = 1): two guest
threads spent 64-69% of their time in `A64AddressSpace::InvalidateCacheRanges` -- 4,018 samples,
more than a core -- while the render thread waited on the game thread half the time. The patch
keeps the set of 2 MiB chunks that hold indexed pages (cleared with the cache) and walks, page by
page, only the chunks a range touches that are in it; a range with no code in it costs one set
lookup per 2 MiB, or a walk of the set when that is shorter. Nothing else changes: the same blocks
are found and invalidated. `od_invalidation_page_probes()` (shim, measurement only) counts page
lookups; `tests/invalidation_probes.rs`: 256 pages of data probed **256** pages before, **0** after,
and two code pages probe 2. Rows `mac-cpu-I1` (filter off) and `mac-cpu-I2` (chunks not recorded).

### 0016 — arm64: a location translated again keeps one range, not one per translation

`0016-arm64-a-translated-again-location-keeps-one-range.patch`. **arm64 only**, a defect in 0011,
found by measuring 0015 in a game. 0011's page index appended a `GuestRange` every time a block was
translated and dropped none until a cache clear, which never comes while any block survives (the
rest of `libroblox.so` stays translated). The pin's `BlockRangeInformation` kept a *set* of
locations per interval, so a location translated again collapsed into its old entry; 0011's lists
did not. Code the engine invalidates and translates again, over and over, therefore piled up dead
entries that every later invalidation of those pages checked: MEASURED in a game with 0015 applied
(`sample`, 5 s, n = 1), three guest threads at **97-98%** of their time in
`InvalidateCacheRanges`. A range is now marked dead when its block is invalidated, and dead ranges
are dropped from each page list an invalidation walks (and a page whose list empties is dropped).
`tests/invalidation_probes.rs`: 200 invalidate-and-translate cycles of one page, then one
invalidation checks **201** ranges before and **1** after (`od_invalidation_ranges_checked`,
measurement only). Row `mac-cpu-I3`.

### 0017 — x64: a guest thread's fixed cost: the fast-dispatch table and the prelude commit

`0017-per-thread-fixed-cost-fast-dispatch-and-prelude-commit.patch`, D32. Two
changes, both to what every `A64::Jit` costs before it has translated anything:

1. **The fast-dispatch table is allocated only when `FastDispatch` is on**
   (candidate 4 below, applied as specified there): `A64EmitX64` holds a
   `std::unique_ptr<std::array<FastDispatchEntry, …>>`, made in its constructor
   under `conf.HasOptimization(OptimizationFlag::FastDispatch)` before
   `GenTerminalHandlers` (its first reader); `ClearFastDispatchTable` and the two
   emitted table addresses dereference it, all already under the same test.
   Omnidroid runs with the optimization off (D16), so the 16 MiB -- written in
   full by the entries' non-zero initialiser -- is simply not there.
2. **`PRELUDE_COMMIT_SIZE` 16 MiB → 2 MiB** (`block_of_code.cpp`). The constant
   pool commits its own 2 MiB; the prelude after it measured about 1.1 MiB; every
   block after that is committed by `GetBlock`'s own 1 MiB-ahead
   `EnsureMemoryCommitted`. At 16 MiB each thread held ~15 MiB of commit it never
   touched. MEASURED on a live landing before the change: the least-used code
   caches committed 18.0 MiB each and touched one contiguous run of 3,200 KiB.

**MEASURED effect**, the logged-out landing at +100 s, 45 guest JITs, n = 1 each,
same scenario (`memrun.sh` in the 2026-09-24 session scratchpad): process commit
3,157 → **2,105 MiB**, working set 2,528 → **1,884 MiB**; code caches 882 → 499
MiB committed; fast-dispatch tables 44 × 16 MiB → none. Upstream suite: identical
before and after (above).

### 0018 — x64: a return-stack-buffer hit checks the budget and the halt flag

`0018-rsb-hit-checks-budget-and-halt.patch`, D33. Candidate 2a below, for the
`PopRSBHint` handler only. After a hit is confirmed (the location descriptor
computed from the guest PC in `JitState` matches the top entry) and before the
`jmp` to the predicted block, the handler now compares `cycles_remaining` with
0 when cycle counting is on, then `halt_reason` with 0, and on either leaves
through `ReturnFromRunCode` — which checks both again and returns or
dispatches. The guest PC is already in `JitState` at that point, so leaving is
exact. Four emitted instructions on the hit path (two without cycle counting);
the miss path is unchanged.

So `optimization::INTERRUPTIBLE` is now `ALL_SAFE & !FastDispatch`
(`0x0000_FFFB`) rather than also clearing `ReturnStackBuffer` (`0x0000_FFF9`).

**MEASURED** (`omni-cpu/tests/bench.rs::the_cost_of_a_call_and_return_as_the_block_map_grows`,
one `BL` + `RET` to a distinct function, median of 5, release): at 262,144
blocks, **132.0 ns → 24.1 ns** per
call and return under `INTERRUPTIBLE`, against 24–26 ns under `ALL_SAFE`; at
128 blocks, 5.96 → 2.35 ns. Without the RSB every `RET` went to the dispatcher,
whose `LookupBlock` is a hash-map probe that misses cache once the map is big.

**Detector**: `the_stoppability_matrix`, 39 cells. `return:*:halt+budget`
wedges without the halt check; `indirect-call:0000FFFF:budget` wedges without
the budget check. Both removed by hand and observed (D33); `tools/mutate.py`
cannot carry them, since the build script watches only `vendor/PIN.txt`.
**Since 0019** the `BLR`/`BR` of `indirect-call` check the budget themselves, so
0018's budget detector is `return-ring:{0000FFFF,0000FFFB}:budget` (a `RET`-only
loop), re-observed by hand with 0019 applied (D35).

### 0019 — x64: the fast-dispatch handler checks the budget and the halt flag, and its table is 64 KiB

`0019-fast-dispatch-checks-budget-and-halt.patch`, D35. Candidate 2a's other
half, and candidate 4's size.

1. **The checks.** `terminal_handler_fast_dispatch_hint` serves every `BR`/`BLR`
   (`FastDispatchHint`) and, through its `rsb_cache_miss` entry, every `RET` that
   missed the return-stack buffer. Right after that entry -- before the table is
   probed -- it now compares `cycles_remaining` with 0 when cycle counting is on
   and `halt_reason` with 0, and on either jumps to `ReturnFromRunCode` (the guest
   PC is already in `JitState`, from which the handler computed the descriptor).
   Both the hit (`jmp [entry.code_ptr]`) and the miss (`LookupBlock`, then `jmp
   rax`) start there, and neither changes the budget, so one site covers both --
   the same order the dispatcher uses (check, look up, jump). Four instructions,
   two without cycle counting.
2. **The size.** 0x1000 entries (`fast_dispatch_table_mask = 0xFFF0`), 64 KiB,
   down from 0x100000 (16 MiB); a `static_assert` ties the mask to the size. With
   `INTERRUPTIBLE` now `ALL_SAFE` on x86_64 every guest thread allocates it (0017's
   guard) and writes it at construction and at each cache clear: **64 KiB of
   commit and working set per guest thread** -- 3.75 MiB at 60 threads, 16 MiB at
   256. MEASURED (`omni-cpu/tests/bench.rs::the_cost_of_an_indirect_branch_through_a_table`,
   ns per transfer, median of 5, x64): up to 4,096 distinct targets 2^12 is as fast
   as 2^14 and 2^16 within run-to-run noise; at 16,384-65,536 targets every size up
   to 2^16 is back at the dispatcher's cost (a miss costs a hash, a store and the
   dispatcher's own `LookupBlock`), and only upstream's 2^20 helps there (28.7 vs
   ~40 ns). 16 MiB per thread for that is what 0017 took out.

**Invalidation** is dynarmic's and still runs on every path Omnidroid uses:
`od_jit_invalidate_range` (`omni-cpu`'s `invalidate_code`, the inline-thunk and
sentinel words) -> `Jit::InvalidateCacheRange` -> `PerformRequestedCacheInvalidation`
-> `InvalidateCacheRanges` -> `InvalidateBasicBlocks` -> `A64EmitX64::Unpatch`,
which clears the location's entry; `od_jit_clear_cache`, and `GetBlock`'s
evacuation of a full cache, reach `A64EmitX64::ClearCache` ->
`ClearFastDispatchTable`. `a64_exec.rs`'s
`an_invalidated_translation_is_not_served_from_the_fast_dispatch_table` rewrites a
`BR` target the table has served and requires the new code after each; removing
either clear by hand makes it fail (D35).

**Detector**: `the_stoppability_matrix`: `indirect:0000FFFF:*` (a `BR` loop served
by the table) and `return-miss:0000FFFF:*` (a `RET` loop that always misses the
buffer). Removed by hand, each check wedges its cells; moving the checks above the
`rsb_cache_miss` entry wedges the three `return-miss` cells (D35).

### 0020 — arm64: the return-stack buffer's hit checks the budget and the halt word, in one handler

`0020-arm64-rsb-hit-checks-budget-and-halt.patch`, D35. **arm64 only.** 0018 on
the arm64 backend, which D33 amendment 1 left out: its `PopRSBHint` compared the
buffer's top entry inline in every block ending in `RET` and branched to the
predicted block checking nothing (`return-ring` MEASURED unstoppable under
`0xFFFF`). The hit test now lives **once, in the prelude**
(`A64AddressSpace::EmitPrelude`, `prelude_info.pop_rsb_hint`, a new
`LinkTarget::PopRSBHint`), immediately before `return_to_dispatcher`: on a hit it
compares `Xticks` with 0 (cycle counting on) and the halt word with 0, and on
either -- or on a miss -- falls through into the dispatcher, which checks both
again. The terminal is one `B`.

**Why out of line, MEASURED** (`the_cost_of_a_call_and_return_as_the_block_map_grows`,
M1, ns per call): with the test inline, the RSB was *slower* than no RSB once the
code outgrew the caches -- 43.7 (upstream, unchecked) and 47.4 (checked, inline)
against 24.6 at 262,144 blocks -- because the ~15 inline instructions per `RET`
block cost more in footprint than the dispatcher they save (arm64's dispatcher is a
compact-map probe since 0010, 24.6 ns here against x64's 140). Out of line:
**20.2**, and the RSB is faster than no RSB at every size measured.

**Detector**: `the_stoppability_matrix` on arm64: `return-ring:{0000FFFF,0000FFFB}:*`,
which all wedged before. Rows `mac-cpu-R2` (halt check) and `mac-cpu-R3` (budget
check) remove them and are caught; `mac-cpu-R1` puts `INTERRUPTIBLE` back to
`0xFFF9`.

### 0021 — arm64: the inline exclusives honour `Unsafe_IgnoreGlobalMonitor` (value-compare)

`0021-arm64-value-compare-honours-ignore-global-monitor.patch`, D31 amendment 1. **arm64 only.**
0007 gave arm64 x64's inline exclusive protocol but not x64's switch around it: the x64 backend's
`EmitExclusiveLock`, `EmitExclusiveUnlock` and `EmitExclusiveTestAndClear`
(`emit_x64_memory.h`) each return early under `Unsafe_IgnoreGlobalMonitor`, and 0007's
`EmitMonitorLock`, `EmitMonitorUnlock` and reservation-clearing loop did not look at it. So on arm64
`omni-cpu`'s default, `ExclusiveMonitor::ValueCompare` (D31), was the global monitor under another
name: every guest `LDXR`/`STXR` took the process-wide spin lock, and every store-exclusive walked all
2,048 slots of the monitor (256 threads x stride 8) under it.

The patch is x64's three early returns, in the same three places: under the flag the inline
accesses neither take nor release the lock (the fastmem fallbacks' unlocks included -- they release
only what the inline path took), and a store-exclusive clears no other processor's reservation.
Each processor's own reservation check (`exclusive_state`, `address[pid] == vaddr`) and the
`LDAXR`/`STLXR` compare-and-swap of the reserved value are unchanged. Without the flag (the global
monitor, `OMNI_JIT_EXCLUSIVE_MONITOR=global`) nothing emitted changes.

**MEASURED** on the M1 (`omni-cpu/tests/bench.rs::the_cost_of_a_guest_atomic_increment`,
value-compare row, ns per `LDAXR`/`ADD`/`STLXR`/`CBNZ` increment, median of 7, release, same
worktree with 0021 reverse-applied for the pin column):

| value-compare, 256 threads | 1 thread | 8 threads, private words | 8 threads, one shared word |
|---|---|---|---|
| pin + 0001-0020 | 1,306.9 | 1,943.5 | 1,963.7 |
| + 0021 | **9.5** | **3.2** | **42.4** |
| x64 (Windows, for comparison; unchanged by 0021) | 12.6 | 2.6 | 32.0 |

The global rows of the same runs are unchanged (256 slots: 186.0 / 623.2 / 634.1 with 0021, 186.0 /
606.8 / 650.1 without). The 3,594-initializer run (`omni-android`'s `the_cost_of_an_initializer_run`,
one guest thread, n = 5): cold 2,181-2,192 ms without, 2,149-2,160 ms with -- it barely uses
atomics; the patch is for the threaded world, where the lock was measured saturating (D31
amendment 1).

**Detectors.** `omni-cpu`'s `tests/exclusive.rs` now runs on aarch64 as well; its
`aba_across_another_threads_exclusive_store_is_the_one_difference` FAILS on the pin + 0001-0020
(MEASURED: the value-compare ABA case answers the global monitor's `(1, 40)`) and passes with 0021.
`dynarmic-sys`'s `tests/exclusive.rs` gains, on both backends:
`value_compare_inline_exclusives_neither_take_nor_release_the_monitor_lock` (the test holds the
monitor's lock word itself; the global arm must wait for it, the value-compare arm must run through
it and leave it held), `under_value_compare_another_processor_s_same_value_store_leaves_the_reservation`
(both arms, the reservation held by single-stepping -- see below), a value-compare lost-update
stress (four inline threads), and value-compare rows in the width, pair, failure-case and
other-observer tests. Rows `mac-cpu-V1` (lock check removed), `mac-cpu-V2` (unlock check removed),
`mac-cpu-V3` (scan check removed): **3/3 caught** on the M1 -- V1 and V2 by the lock test (V1 waits on
the held lock; V2 leaves it released), V3 by the same-value test and `omni-cpu`'s ABA test -- and the
file's SHA-1 was identical afterwards (`17c2c7ba`). On the pin (0021 reverse-applied) the lock test,
the same-value test and the ABA test all fail, and nothing else in the two files does.

**Found on the way, not changed:** the x64 backend clears the local monitor at every `SVC`
(`EmitA64CallSupervisor`, "the kernel would have to execute ERET"), so on x64
`a_successful_store_exclusive_clears_another_processor_s_reservation_of_the_address`, which holds
A's reservation across an `SVC`, would pass with no scan at all. MEASURED on Windows: the first
draft of the same-value test had that shape, and under value-compare -- where x64 emits no scan --
A's store still failed. The test as committed holds the reservation by single-stepping A's `LDXR`
and asserts both arms, so the scan is pinned on both hosts.

### 0022 — x64: one code cache shared by every jit of a guest address space (opt-in)

`0022-shared-code-cache.patch`, D38, design `docs/research/shared-jit-cache.md`. **x64 only, and
nothing changes unless a jit is given a cache**: every change is under `if (shared_code)` (or the
`A64::SharedCodeCache` it belongs to), and a jit without one emits byte for byte what it did --
the only difference it sees is eight `u64` fields appended to `A64JitState` (no offset moves) and
upstream's two `BlockOfCode`/`A64EmitX64` members held behind a pointer.

`A64::SharedCodeCache` (interface `a64.h`, implementation in `a64_interface.cpp`) owns one
`BlockOfCode` -- prelude, constant pool, code -- and one `A64EmitX64` -- block map, link table,
guest ranges, fastmem table -- built from a template `UserConfig`. A `Jit` whose
`UserConfig::shared_code_cache` is set owns neither; it keeps its `JitState`, its 64 KiB
fast-dispatch table and its pending invalidations, and attaches only if every field that shapes
code equals the template's (the constructor throws otherwise; the shim returns null).

* **Per-thread values out of the code.** Everything a block or the prelude embedded per jit is
  read from `JitState` (`r15`) instead: the callbacks' `this` (`ArgCallback::FromJitState`,
  `DevirtualizeFromJitState`), the dispatcher's lookup argument, `&conf` for the non-inline
  exclusives, the thread's two monitor slots, the TPIDR boxes, the fast-dispatch table. The global
  monitor's store scan includes the storing processor's own slot, as `ExclusiveMonitor::CheckAndClear`
  does.
* **Shared code is never rewritten.** A link (`LinkBlock`, `LinkBlockFast`) jumps through an
  8-byte slot, and the RSB push loads its code pointer from one; linking and unlinking are one
  aligned store. A block's slots are emitted right after it; a dropped block's slots are unlinked
  and taken out of their targets' records.
* **Translation is serialized, execution is not.** One `shared_mutex`: dispatcher lookups take it
  shared -- after the thread's own fast-dispatch table, which the dispatcher now consults first --
  and emission exclusive. The frontend and IR passes run outside the lock; a location another
  thread is translating is waited for (spin, then a condition), not translated twice; a
  translation overtaken by an invalidation of its range is made again under the lock.
* **Invalidation** applies to the shared maps at once when the requesting jit is not executing
  (else queued with a halt, as upstream), and bumps a generation that each jit compares at `Run`
  entry and at every dispatcher lookup, emptying its RSB and fast-dispatch table when it moved.
  In shared code the fast-dispatch handler writes a missed entry whole *after* the lookup, since
  the lookup now reads the same table (found by measuring: written first, as upstream does, the
  lookup could return another location's code pointer).
* **Memory** is regions after the prelude, committed as code is emitted. A full region is retired
  (its slots unlinked, the maps emptied, every other thread halted) and given back
  (`MEM_DECOMMIT` / `MADV_DONTNEED`) once each attached thread is outside `RunCode` or entered it
  since -- epochs published at `Run` entry and at every lookup. Shared code calls every `SVC`
  callback through a prelude trampoline that publishes the resume address in
  `JitState::od_callback_return` and takes it back with an `xchg`; a thread parked in the callback
  with its resume address in a retiring region is moved, with one compare-exchange, to a prelude
  stub that leaves the run -- so a region is given back whole (D38 amendment 1: the first version
  kept a hole around each parked thread, which fragmented regions in a game world).
* Fault handling: the recompile-on-fastmem-failure flags are forced off for a shared cache (a
  declined fault reaches the fallback callback every time, as the handler routes it anyway); the
  handler's `fastmem_patch_info` lookup takes the lock shared.

**MEASURED** (Windows, release, `omni-cpu/tests/roblox.rs::the_cost_of_eight_threads_meeting_the_same_real_roblox_code`
and `dynarmic-sys/tests/shared_cache.rs`; D38 has the tables): eight threads each running the 870
real Roblox leaves cold translate them once between them instead of eight times (fetched guest
instructions 50,904 -> 6,363), commit +54 -> +1.8..5.6 MiB, and a warm call costs the same. On the
4-core Linux host the cold pass is also faster (63 -> 43 ms, half the CPU); on the idle 24-thread
Windows host, where each thread had a core to translate its own copy on, the serialized emission
makes it slower (22 -> 31 ms). Per guest thread 4.548 -> 0.055 MiB.

**Detectors** (`tests/shared_cache.rs`, 15 tests and one measurement; the whole `dynarmic-sys` suite also runs with
`OD_TEST_SHARED_CACHE=1`, every `Vm` on a cache of its own). Hand mutations of the vendored C++,
each rebuilt, run and restored with the file's SHA-1 checked (`tools/mutate_0022.py`): 13 rows,
13 caught -- see D38.

### 0024 — x64: a census of the shared cache's per-block tables

`0024-x64-a-census-of-the-shared-cache-s-tables.patch`. **x64 only, read-only: nothing is emitted,
kept or looked up differently.** `A64::SharedCodeCache::GetTables()` (interface `a64.h`) takes the
cache's lock shared and asks the emitter (`A64EmitX64::Census`) what each of its per-block tables
holds on the C heap -- entries, bytes (arrays at capacity, entries' own allocations), and an address
inside its largest single allocation with that allocation's size. `BlockRangeInformation::Census`
counts the icl intervals and the locations their sets name. The shim exports it as
`od_code_cache_tables_of` (`OD_DYNARMIC_ABI_VERSION` 3: `od_abi_layout` gained the struct's size).

**Why.** MEASURED in the world (M1 and M4, `OMNI_MEM_REPORT`, Windows, 2026-09-25): the process's C
heaps held 968 MiB committed, 905 MiB allocated, of which only 55 MiB was the runtime's Rust -- with
single allocations of **272, 224, 80 and 64 MiB**, the same in both runs. Those are exactly the
bucket arrays of the emitter's four robin_maps (load factor at most 0.5) for the ~700,000 blocks the
shared cache held: `patch_information`, 2^21 buckets of 136 bytes (five `std::vector`s inline, of
which a shared cache uses one) = 272 MiB; `fastmem_patch_info`, 2^22 of 56 = 224 MiB;
`outgoing_slots`, 2^21 of 40 = 80 MiB; `block_descriptors`, 2^21 of 32 = 64 MiB. The census lets a
report say so rather than infer it: `OMNI_MEM_REPORT` names each such allocation by its table.

`tests/shared_bookkeeping.rs` (x64; Windows reads the process heap's `HeapSummary`, Linux
`mallinfo2`; the instrument is first shown seeing 16 MiB): 65,536 blocks of the engine's shape (a
load, a store and a conditional branch; then a branch) cost **1,144 bytes per block** of heap on the
pin, and the census accounts for 1,136 of them -- block map 128, link targets 568, block links 184,
fastmem sites 112, guest ranges 144 -- and **944 are still held after the cache is cleared**. The test
requires the census within 15% of what the heap grew by, and each table's named address inside an
allocation at least as large as the one it sizes.

### 0025 — x64 shared cache: link slots and fastmem sites as flat records

`0025-x64-shared-cache-links-and-fastmem-sites-as-flat-records.patch`. **x64; what a shared cache
keeps, not what it emits or when** -- the one change a Jit with its own cache sees is
`BlockDescriptor::size` as 32 bits beside a new 32-bit `first_link` (still 16 bytes), and
`PatchInformation` without 0022's `slots` vector (upstream's four again).

* **Link slots** (0022 kept them in `patch_information`, one entry -- five `std::vector`s inline, a
  136-byte bucket -- for every location ever linked to *or emitted*, since `Patch`'s `operator[]`
  made one per block; and in `outgoing_slots`, a vector per block in 40-byte buckets): one 24-byte
  `LinkRecord` per slot -- target, the slot's and its unlinked value's offsets from the code buffer,
  and a doubly linked list of the records linking to the same target -- appended by
  `EmitPendingSlots`, the block's own records contiguous (`BlockDescriptor::first_link`, the last one
  flagged in bit 31 of its slot offset). `link_heads` maps a target to its newest record (a robin_map
  of `u64 -> u32` at a load factor of 0.75: it is read only when a block is emitted or dropped --
  0.75 and a start at 64 buckets, `UseLoadFactor`, because tsl computes each size limit as
  `float(bucket_count) * load_factor` on the emitting thread under the host's MXCSR, and an inexact
  product sets the sticky precision flag that `omni-cpu/tests/thunk.rs` then finds in the host word
  a handler runs under: 0.8 failed it). `Patch`
  walks the list (and makes no entry for a target nothing links to), `ForgetOutgoingSlots` unlinks a
  dropped block's slots and takes each record out of its list in O(1), `UnlinkAllSlots` stores every
  record's unlinked value, and `ClearCache` gives the records back. Records made for a block whose
  emission then threw are taken out at the next `Emit`.
* **Fastmem sites** (0022 kept them in `fastmem_patch_info`, a robin_map of 56-byte buckets; a
  shared cache's recompile flags are off, so the marker in each was never read): one 16-byte record
  per site -- the site and its resume address as offsets from the region, and the fallback -- in a
  run per region, in address order. A block's sites are collected while it is emitted and added,
  sorted, once its code is complete (an inline exclusive load records its site before the deferred
  plain ones before it, so recording order is not address order). `FastmemCallback` finds the run by
  address and the site by binary search, under the cache's lock as before; a region's run is emptied
  when it starts and given back when it is reclaimed.

`tests/shared_bookkeeping.rs`, n = 65,536 blocks: **1,144 -> 401 bytes per block** (the census: link
targets 568 -> 48, block links 184 -> 51, fastmem sites 112 -> 23; the bound is 550), and 944 -> 151
still held after a clear; a region given back takes its records with it (after seven retirements,
3,854 records for 327,685 blocks emitted). `tests/host_fault.rs`: four fastmem sites in one block,
recorded out of address order, the lowest faulting -- served, and the three after it inline, on a
Jit's own cache and on a shared one (removing the sort makes the lookup miss and dynarmic abort).
`tests/shared_cache.rs`: four blocks linking to one target, dropped from the middle, the head and the
tail of its list and translated again over seven rounds, each then running the target's new code
(skipping the unlink from a predecessor, or the head's update, fails it). Both `dynarmic-sys` suites
(own caches, and `OD_TEST_SHARED_CACHE=1`) pass.

In the world (~700,000 blocks, ~1-2 million fastmem sites and link slots) this is expected to take
the three arrays of 272, 224 and 80 MiB -- and their entries' vectors -- down to tens of MiB.

### 0026 — x64: the guest ranges of emitted blocks, compact

`0026-x64-the-guest-ranges-of-emitted-blocks-compact.patch`. **x64, a Jit's own cache and a shared
one alike.** 0011 for the x64 backend: `A64EmitX64` kept the guest bytes each block was translated
from in a `BlockRangeInformation<u64>` -- a boost::icl `interval_map` of `std::set`s, a tree node per
interval and per location (the census estimates 144 bytes per block; about 100 MiB of the world's
~700,000 blocks). It keeps one 24-byte `GuestRange` per emitted block (location and the closed range
the pin registered, skipped when empty as the icl skipped it), indexed by the 4 KiB guest pages it
covers; a block covering more than 64 pages goes to a list checked on every invalidation.
`InvalidateCacheRanges` (and the shared cache's counted form) returns every location registered with
a range intersecting a requested one -- what `InvalidateRanges` returned -- looking the pages up, or
walking the index when more pages are asked about than it has. The x64 backend already cleared its
ranges with the cache (and a shared cache when it forgets every block), so unlike 0011 there is no
difference in what is invalidated; the memory is now given back rather than kept.

`tests/shared_bookkeeping.rs`: **401 -> 289 bytes per block** (guest ranges 144 -> 40; the bound is
now 350). Its three behaviour tests are 0011's, on this backend and, with `OD_TEST_SHARED_CACHE=1`,
on a shared cache: a write to the second page of a two-page block, a write to the last word of a
70,001-instruction block (more than 64 pages; ignoring the wide list fails it), and a 16 GiB
invalidation reaching every block.

### 0027 — x64: a shared cache's block map at a load factor of 0.75

`0027-x64-a-shared-cache-s-block-map-at-a-load-factor-of-0.75.patch`. **x64, shared caches only.**
After 0025-0026 the largest per-block table is `block_descriptors` itself: 32-byte buckets at
robin_map's default maximum load factor of 0.5 -- in the world 2^21 buckets, the 64 MiB allocation
M1 and M4 showed. A shared cache now runs it at 0.75 (`UseLoadFactor`, 0025: from 64 buckets, so
every size limit tsl computes in floating point is exact): for ~700,000 blocks, 2^20 buckets, 32 MiB.

It is the dispatcher's map, so it was measured where it is read:
`tests/shared_bookkeeping.rs::bench_dispatcher_lookups_in_a_shared_cache` (ignored; release) calls
100,000 one-instruction functions through `BLR` in a scattered order, so nearly every call misses the
thread's 4,096-entry fast-dispatch table and takes the cache's lock and the block map (100,003
entries: 8 MiB of buckets at 0.5, 4 MiB at 0.75). Alternated builds, two rounds of three runs each,
Windows: **45.5-51.0 ns per call at 0.5 (median 49.6), 46.3-50.9 at 0.75 (median 50.2)** -- within
the noise. (A first try at 0.8 measured the same, 48.6-51.0, and failed `omni-cpu/tests/thunk.rs`:
0025's note.) The cold translation bench beside it (emission writes the tables) is 6.1 us per block
with 0025-0027 against 7.1 on 0024's maps. Bookkeeping per block (the same file's test): **289 ->
225 bytes**; the bound is 260.

### 0028 — x64 shared cache: a full region is not a flush; the oldest region is retired, alone

`0028-x64-a-full-region-is-not-a-flush-the-oldest-region-is-retired.patch`. **x64, shared caches
only** (a Jit's own cache sees one change: a block covering no guest bytes gets a `GuestRange`
record too, kept out of the page index). D38 amendment 3.

**Before it** a region that filled was retired by forgetting every block of the cache
(`ForgetAllBlocks`): every thread then translated its whole working set again. The regions were a
quarter of the cache (256 MiB) so that this would be rare; a game world emits ~245 MiB in its first
minutes and then ~0.4 MiB a minute (w27-w30's `PERF jit cache:` lines), so a 30-minute session would
have met it, and every instance kept ~245 MiB of code committed.

**Now** (`A64::SharedCodeCache(template, total, region, live_bytes)`; the shim's
`od_code_cache_new` takes `live_bytes`, `OD_DYNARMIC_ABI_VERSION` 4):

* A full region becomes **Full**: its blocks stay in the map, linked and run. The next free region
  is started.
* At most `live_bytes` of regions are live (Current or Full; 0 = all but one, at least one). Starting
  another past that **evicts the oldest** (regions carry a sequence number): `ForgetRegionBlocks`
  walks the guest ranges registered while that region was being filled -- 0026's records, now one per
  emitted block in emission order, empty ranges included -- and forgets each block whose entry point
  is in the region, as an invalidation does (incoming links undone through 0025's lists, its own
  slots unlinked and taken out of theirs). A location emitted again since into a newer region keeps
  that block. Then the region is retired as before: generation and epoch bumped, every other thread
  asked to halt, given back when no thread holds it (the parked-thread redirect of 0022 unchanged).
* Regions are filled and evicted in order, so once the oldest is evicted every link record and guest
  range older than the next live region's first is dead: `TrimLinkRecords` / `TrimGuestRanges` drop
  them from the front. Their indices -- in `BlockDescriptor::first_link`, the records' lists,
  `link_heads`, the page index -- are **serials** (`LinkAt(i)` is `link_records[i - link_base]`,
  likewise `RangeAt`), so nothing is renumbered; the page lists and the wide list lose a prefix each.
  The serials are 32 bits: past 3/4 of that (billions of blocks) the cache forgets every block once
  and they start again.
* With nothing free and nothing retired (a live limit of all regions), the oldest region is evicted
  on the spot; with a retired region not yet given back, the emitting thread waits for it, as before.
* `ClearCache` forgets every block, as before, and now retires the Full regions it emptied.
* Stats: `regions_evicted`, `blocks_evicted`, `blocks_reemitted` (blocks emitted at a location the
  latest eviction forgot -- what it cost in translation), `evict_ns`, `evict_max_ns`,
  `regions_live`, `regions_live_max`.

**MEASURED** (`tests/shared_cache.rs::the_cost_of_an_eviction`, ignored; Windows, release, one thread
translating 1,200,001 blocks of the engine's shape -- a load, a store, a conditional branch; a branch
-- 135 bytes each): forgetting a region holds the lock for a time set by the region, not by the live
code: **16 MiB regions, 128 MiB live: 18.4 ms per eviction (longest 19.3)**; 16 MiB with 32 MiB live
16.9 (22.2); 8 MiB with 128 MiB live 11.6 (16.9); one 128 MiB region live -- every block of a full
region forgotten, as 0022 did -- 148 ms for 917,505 blocks (that is this patch's path, not 0022's
`ForgetAllBlocks`; 0022's cost was the retranslation that followed). About 170 ns a block forgotten;
the world's blocks average 361 bytes (w30: 188,669 KiB in 535,066 blocks), about 46,000 to a 16 MiB
region.

**Detectors** (`tests/shared_cache.rs`): `a_full_region_is_not_a_flush_and_the_oldest_region_goes_first`
(a 3,001-block working set run after each of 56 cold segments of 25,000 blocks, 8 MiB regions, 32
MiB live: while regions are free a fill translates nothing again and retires nothing; each eviction
forgets at most a region's blocks; the working set is translated again 4 times over 14 evictions,
where the flush would have done it at all 17 fills; committed code at most 30 MiB; a clear gives the
full regions back), `a_block_translated_again_elsewhere_survives_its_old_region_s_eviction` (a block
invalidated and translated into a newer region, linking back into the old one, survives the old
region's eviction and its link there is undone; after more evictions an invalidation still finds
the working set's last block through the trimmed index, and only it), and
`threads_keep_their_working_set_while_another_streams_cold_code_through_the_cache` (four threads run
a 2,001-block working set ~1.5 million times while a fifth streams 1.2 million cold blocks through a
24 MiB live limit: 13 evictions, the working set translated again 5 times, every run right, every
retired region given back). The tests written for 0022's cadence (`Space::new`) keep one region live,
so that each fill still retires one.

## How a patch is carried

Patches are applied **into `vendor/dynarmic/` directly** and a `.patch` file is
committed here alongside, so `git apply --check` against a fresh clone of the
pin verifies that the tree is exactly upstream plus these patches. Touch
`vendor/PIN.txt` afterwards; it is the only thing under `vendor/` that the build
script tells Cargo to watch.

`python3 crates/dynarmic-sys/tools/verify_patches.py` does that check without a
network: the pristine tree is the one committed when the pin was vendored
(`64034d4`), every patch is applied to it in order (`git apply --check` first),
and the result must be the vendored tree **byte for byte** (in git object
space, so eol attributes and ignored build outputs cannot confuse it); the
vendored tree must also reverse-apply back to pristine. An edit made under
`vendor/` without its patch fails the first half, which is the half a
reconstruction from the tree itself could never catch.

## Known candidates, not yet applied

### 1. `hook_hint_instructions` is never plumbed into the A64 frontend

Found by the D5 spike. `A64::UserConfig::hook_hint_instructions` is read by the
A32 frontend but not the A64 one, so every `YIELD` exits the JIT regardless.
A one-line fix. Deferred to the task that needs hint handling, because changing
it changes behaviour (`YIELD` stops being an exit) and that belongs with the
code that depends on the new behaviour.

### 2. Terminals that check the cycle counter and the halt flag exclusively

All measured by `the_stoppability_matrix` in `tests/hostile.rs`, 27 cells
before 0018 (39 now).
**A configuration that stops every runaway guest does exist** — `0x0000_FFF8`,
which is `ALL_SAFE` without `BlockLinking`, `ReturnStackBuffer` or
`FastDispatch` — because it routes every terminal through `ReturnFromRunCode`
(`block_of_code.cpp:362`), the one path that checks `halt_reason`
unconditionally and then `cycles_remaining` when cycle counting is on. What
follows is what each flag buys and what it costs.

**2a. The indirect-branch handlers check nothing.**
`EmitTerminalImpl(IR::Term::PopRSBHint)` and
`EmitTerminalImpl(IR::Term::FastDispatchHint)` jump to handlers generated by
`A64EmitX64::GenTerminalHandlers` (`backend/x64/a64_emit_x64.cpp:169`) which
compute a location descriptor and transfer straight to the next block's entry
point. Neither reads `cycles_remaining` and neither reads `halt_reason`, so a
guest `BR`/`RET` loop whose target stays in the return-stack buffer or the
fast-dispatch cache cannot be stopped at all. One `BR` costs a host thread
permanently.

**`PopRSBHint`: patched (0018 on x64, 0020 on arm64). `FastDispatchHint`:
patched (0019, x64; arm64 does not implement it).** So 2a is closed.

Worked around by configuration first, not a patch:
`dynarmic_sys::optimization::INTERRUPTIBLE` cleared `ReturnStackBuffer` and
`FastDispatch` (since D35 it clears neither on x64, and only the unimplemented
`FastDispatch` on arm64), sending both terminals through `ReturnFromRunCode`, which
returns to the dispatcher, which checks both. Measured cost: **about 3.9 ns per
indirect transfer**, which is nothing for a guest with no indirect branches and
5.0x for one where half the instructions are indirect transfers (n=31, release).

**2b. `LinkBlock` checks one or the other, unless `BlockLinking` is clear.**
`EmitTerminalImpl(IR::Term::LinkBlock)` (`a64_emit_x64.cpp:612`) opens with an
early-out: with `BlockLinking` **clear** it emits `ReturnFromRunCode()` and
returns. With it set, it compares `cycles_remaining` when
`enable_cycle_counting` is set and `halt_reason` when it is not — one or the
other, never both.

So a **direct**-branch loop under `ALL_SAFE` or `INTERRUPTIBLE` honours a step
budget or a cross-thread halt, but not both; clearing `BlockLinking` gives both.
The cost is a dispatcher round trip at every block boundary, so it scales with
block length rather than with branch mix: **7.08x** on a workload with
4-instruction blocks and no indirect branches at all (0.079 -> 0.561 ms, n=31,
release), 7.11x and 7.43x on the two indirect mixes. The Task 2 re-review
measured 6.6x (0.084 -> 0.551 ms, n=31) on its own 4-instruction-per-block
workload -- the two agree within about 7%.

**Both halves of the qualification that used to follow were wrong, and Task 3
measured the thing that settles it.** D16 called 7x an upper bound on the
grounds that real code has longer blocks, and recorded that neither figure had
been measured against `libroblox.so`. `tools/branch_mix.py` now measures it:
4,225,706 control transfers in 18,156,033 words of executable sections, a mean
of **4.30 instructions per basic block** -- essentially the length these
workloads used. So 7x is not loose for this guest, and the upper-bound framing
is withdrawn. The estimate is static rather than traced, so it says which end of
the band to expect and not what a run will cost.

A runtime that does not want to pay that can instead run under `INTERRUPTIBLE`
with cycle counting on and a **short** budget, so `Run` returns on its own every
few thousand instructions and each return is a decision point. A cross-thread
halt is then honoured at the next window boundary rather than immediately.

**2c. The cycle comparison is signed.**
The same terminal emits `cmp qword[... cycles_remaining], 0` followed by `jg`.
`GetTicksRemaining` returns a `u64`, so any budget above `i64::MAX` — including
the obvious `u64::MAX` for "no limit" — compares as negative and every block
returns to the dispatcher. Correct, and roughly two orders of magnitude slower.
Covered by `a_cycle_budget_above_i64_max_reads_as_already_spent`.

A patch would add the missing checks to 2a's handlers and make 2b's terminal
check both. It is left for the task that owns the runtime's watchdog, because
what the checks should do depends on what that watchdog wants.

### 3. `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT` crashes on Windows x86-64

D12 rules that Omnidroid never holds a page that is simultaneously writable and
executable. dynarmic's code cache is committed `PAGE_EXECUTE_READWRITE`
(`backend/x64/block_of_code.cpp:280`), so that is false for the region holding
every byte of generated guest code. `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT` is the
upstream switch that would fix it — it commits `PAGE_READWRITE` and brackets
each `EmitBlock` with a `VirtualProtect` pair.

It does not work on this pin. **dynarmic's own test suite segfaults with it on**,
which is how we know it is upstream and not the Omnidroid shim:

```sh
cmake -S crates/dynarmic-sys/vendor/dynarmic -B <build> -DDYNARMIC_TESTS=ON \
      -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DBOOST_ROOT=<vendor/boost> \
      -DDYNARMIC_ENABLE_NO_EXECUTE_SUPPORT=ON
cmake --build <build> --target dynarmic_tests
<build>/tests/dynarmic_tests.exe "[a64]"     # SIGSEGV, 1 of 1 assertions failed
```

Same build with the option `OFF`: `All tests passed (202200 assertions in 123
test cases)`.

The root cause is not characterised. `BlockOfCode::BlockOfCode` calls
`EnableWriting()` before `EnsureMemoryCommitted()`, so the first
`VirtualProtect` runs against `committed_size == 0` on a `MEM_RESERVE`-only
region and its failure is discarded — a plausible starting point, not a
diagnosis.

So the D12 exception cannot be closed by configuration. Until a patch exists it
is reported instead: `od_effective_config::code_cache_w_xor_x` carries it (it
echoes the build flag — querying the actual page protection would mean
`VirtualQuery`, and Global Constraint 4 keeps OS calls in `omni-platform`), and
`the_code_cache_is_writable_and_executable_at_once` asserts it, so the day it
changes a test says so. The `w-xor-x` cargo feature exists to make the retest on
a re-pin a single flag, and the build script refuses it — with the evidence
above — rather than handing back a build that access-violates in every test.

**What this exposes, stated properly.** The code cache is a `VirtualAlloc`
region in the runtime's own address space. Under D4's identity mapping —
`fastmem_pointer = 0`, `fastmem_address_space_bits = 64`, which
`ARCHITECTURE.md` §1 makes the central bet — `EmitFastmemVAddr`
(`backend/x64/emit_x64_memory.h:165-167`) takes the `unused_top_bits == 0`
branch and returns `r13 + vaddr` with **no mask and no bounds test**. Guest
address *is* host address: that is the point of the bet, and it is why the
memory path costs nothing. It also means **the W+X code cache is guest-writable
in principle**, with nothing between a guest and it but not knowing where it is
— that is, ASLR.

W^X is therefore a mitigation the identity-mapping bet gives up, not a property
that survives because the guest is boxed in. There is no guest/host address
separation to fall back on; by construction there is one address space.

The test suite cannot see this. It runs at `fastmem_address_space_bits = 20`
with `silently_mirror_fastmem`, so every guest address is masked into a 1 MiB
arena and a wild store cannot reach anything. That is the right shape for
testing the decoder and the wrong shape for reasoning about exposure, and the
difference is exactly the configuration D4 commits production to.

### 4. `A64EmitX64` holds a 16 MiB fast-dispatch table by value, and fills it even when fast dispatch is off

**The largest single per-guest-thread cost in the runtime, and it is unconditional.**

`backend/x64/a64_emit_x64.h:59-66`:

```cpp
struct FastDispatchEntry {
    u64 location_descriptor = 0xFFFF'FFFF'FFFF'FFFFull;
    const void* code_ptr = nullptr;
};
static_assert(sizeof(FastDispatchEntry) == 0x10);
static constexpr u64 fast_dispatch_table_mask = 0xFFFFF0;
static constexpr size_t fast_dispatch_table_size = 0x100000;
std::array<FastDispatchEntry, fast_dispatch_table_size> fast_dispatch_table;
```

1,048,576 entries at 16 bytes each: **16 MiB, held by value inside `A64EmitX64`**, which is held by
value inside the `Jit::Impl`. So it is one allocation per guest thread, and because the members carry
non-static data-member initialisers, constructing it **writes every byte** — this is resident working
set from the moment the jit exists, not merely reserved address space. `ClearFastDispatchTable()`
writes it again on every cache clear.

Nothing reads it unless `FastDispatch` is on. Every use is already guarded:

- `A64EmitX64::A64EmitX64` calls `ClearFastDispatchTable()` unconditionally, and
  `EmitTerminalImpl(IR::Term::FastDispatchHint)` returns early when
  `!conf.HasOptimization(OptimizationFlag::FastDispatch)`;
- `GenTerminalHandlers` emits `terminal_handler_fast_dispatch_hint` and `fast_dispatch_table_lookup`
  behind the same `HasOptimization` test.

**Omnidroid runs with `FastDispatch` cleared.** D16 settled on `0x0000_FFF9` (`INTERRUPTIBLE`)
because `PopRSBHint` and `FastDispatchHint` check neither the cycle counter nor the halt flag, so a
guest `BR X30` branching to itself is stoppable by nothing at all while they are on. So the runtime
pays 16 MiB per guest thread, and touches it, for a table it has disabled.

**Measured** (`omni-cpu/tests/bench.rs::the_commit_charge_of_a_guest_thread`, serialized, n = 1 run
of 8 contexts, release, D2 host):

| `code_cache_size` | commit charge per guest thread |
|---|---|
| 8 MiB | 24.56 MiB |
| 32 MiB | 34.61 MiB |
| 128 MiB | 34.61 MiB |

Flat from 32 MiB upwards, and 24.56 MiB at the floor — so the per-thread cost is mostly **not** the
code cache, which is the mitigation D5's risk 2 assumes. At 32 guest threads that is about 781 MiB,
against D10's whole budget.

**The patch.** Replace the by-value array with a lazily-allocated
`std::unique_ptr<std::array<FastDispatchEntry, fast_dispatch_table_size>>`, allocated in
`GenTerminalHandlers` only when `conf.HasOptimization(OptimizationFlag::FastDispatch)`, and make
`ClearFastDispatchTable()` a no-op when the pointer is null. Contained: the guards that decide
whether it is read already exist, so the change is the allocation and the null checks, not the
control flow. Worth **16 MiB per guest thread, about 512 MiB at 32 threads**, and more in working set
than in commit charge because the constructor writes it.

**Applied as 0017 (2026-09-24)**, after the suite run this paragraph asked for -- see "Applied"
above. The paragraph that follows is the reasoning as it stood before:

**Not applied.** It changes a hot structure's indirection on the path that *is* enabled upstream, so
it needs the 202,200-assertion suite run against it with `FastDispatch` both on and off before it can
be carried — and, per this directory's rule, the pristine-tree claim given up deliberately rather
than by accident. Recorded now because it was found by measurement during Task 3 and the number is
large enough that it should not wait to be rediscovered.
