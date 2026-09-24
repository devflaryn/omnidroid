# Patches carried against the pin

D5 adopted dynarmic as "a pinned fork we carry patches against". This directory
is where those patches are recorded.

**The vendored tree is upstream `9d4582339990d4eae53f1dc7160686920fc2075c`
plus the patches listed under "Applied".** Until patch 0001 it was pristine,
which kept "the 202,200 upstream assertions pass on our pin" a claim about
upstream rather than about us; that figure has **not** been re-measured with
0001 applied, and should be before it is repeated.

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

All measured by `the_stoppability_matrix` in `tests/hostile.rs`, 27 cells.
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

Worked around by configuration, not a patch:
`dynarmic_sys::optimization::INTERRUPTIBLE` clears `ReturnStackBuffer` and
`FastDispatch`, sending both terminals through `ReturnFromRunCode`, which
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

**Not applied.** It changes a hot structure's indirection on the path that *is* enabled upstream, so
it needs the 202,200-assertion suite run against it with `FastDispatch` both on and off before it can
be carried — and, per this directory's rule, the pristine-tree claim given up deliberately rather
than by accident. Recorded now because it was found by measurement during Task 3 and the number is
large enough that it should not wait to be rediscovered.
