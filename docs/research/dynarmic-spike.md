# dynarmic spike (2026-09-18)

The pre-adoption spike behind D5: upstream dynarmic built and run on the Windows host
(`host-environment.md`) with synthetic, hand-encoded AArch64. No Roblox code was run here; D5's
amendments measure real Roblox code. The "now" notes say what the vendored tree and omni-cpu do
today.

## Q1. Build

- `merryhime/dynarmic` (and several forks) 404. Pinned mirror: `yuzu-mirror/dynarmic`
  `9d4582339990d4eae53f1dc7160686920fc2075c`, 6.7.0, ISC/0BSD. Externals are git subtrees.
- Undeclared dependency: Boost headers (`boost::icl` for invalidation ranges, `boost::variant` for IR
  terminals). Now vendored as a 1,786-file subset (`crates/dynarmic-sys/vendor/PIN.txt`).
- CMake 4.x needs `-DCMAKE_POLICY_VERSION_MINIMUM=3.5` (robin-map declares 3.1).
- MSVC fails with `C1083` on long object paths; keep build paths short.
- Clean Release build 49 s at `-j24`; upstream suite: 202,200 assertions in 123 cases pass.

## Q2. Correctness

37 of 37 hand-encoded checks passed: integer ALU with shifted operands and NZCV, all load/store
sizes and pairs, loops, `BL`/`RET`, NEON, scalar FP, `LDXR`/`STXR`.

## Q3. Guest VA == host VA (identity fastmem)

- `fastmem_pointer` is `std::optional<uintptr_t>`; `Some(0)` enables fastmem with base 0. The base
  lives in `r13`, folded into the SIB byte: a guest `LDR` is one `mov r, [r13 + rax]`. Cost: one
  reserved GPR, zero instructions.
- `fastmem_address_space_bits` defaults to **36**. With a 47-bit address every access silently goes
  to the callback path (~13x slower), no error. Set it to 64. omni-cpu does, and asserts the
  effective config (`crates/omni-cpu/src/context.rs`, `od_jit_effective_config`).
- Guest PC is a sign-extended 56-bit value (`a64_location_descriptor.h`): no guest code above 2^55.
- The `page_table` path is a flat array (128 MiB at 36 bits); unusable for a 64-bit guest.
- Windows faults: dynarmic registers a frame-based handler (`RtlAddFunctionTable`) scoped to its code
  cache. A vectored handler installed first runs before it, so demand paging can commit pages
  without dynarmic demoting the instruction (`recompile_on_fastmem_failure` is sticky per
  instruction).

## Q4. Performance (steady state, one thread)

| loop | Mguest-insn/s | vs native |
|---|---|---|
| integer ALU, register-bound | 603 | ~33x slower |
| memory, identity fastmem | 5,207 | ~2.0x |
| memory, page table (36-bit) | 3,480 | ~3.0x |
| memory, callbacks only | 396 | ~26x |
| NEON + scalar FP | 1,626 | ~2.2x |

Register-bound code is slow because the allocator is per basic block (guest registers round-trip
through `JitState` every instruction) and NZCV goes through memory with `lahf`/`sahf`.

Independent jits scale: 729 Mguest-insn/s on 1 thread, 4,425 on 8, ~5,200 at 16 (P-core count).

Cold translation, 20,000 synthetic blocks: 0.15-0.31 Mguest-insn/s (3-7 us per guest instruction),
12-30 host bytes per guest instruction. D5 amendment 2 measured ~0.5 Mguest-insn/s on real Roblox
leaves.

## Q5. Threading

- One `Jit` per guest thread, each with a private code cache: translations were duplicated per
  thread. Now: one shared cache per address space on x64 (`shared-jit-cache.md`, D38).
- Per-jit committed floor at construction: ~20 MiB (16 MiB prelude commit + 2 MiB constant pool),
  34.5 MiB at a 32 MiB cache. Now: patch 0017 cuts the per-thread fixed cost.
- `ExclusiveMonitor` needs a fixed processor count up front; its global spinlock made one contended
  word fall from 3.4 to 0.16 M ops/s going 1 -> 16 threads (correct, anti-scaling). Now:
  `fastmem_exclusive_access` inline exclusives and value-compare (D31, patches 0007, 0021).

## Q6. Coverage

Decoder table: 643 enabled `INST` entries, 231 commented out. Unsupported encodings surface as
`InterpreterFallback` or `ExceptionRaised`, never a crash. A trap round trip costs ~87 ns (SVC round
trip 69 ns).

| feature | spike result | now |
|---|---|---|
| `TPIDR_EL0`/`TPIDRRO_EL0` | supported (caller-supplied `u64*`) | used |
| `CNTPCT_EL0`, `CNTFRQ`, `CTR`, `DCZID`, `FPCR`, `FPSR` | supported | `CNTPCT` is a wall clock (D5 amendment 4) |
| `CNTVCT_EL0` | fallback | patch 0001 |
| `MIDR_EL1`, `ID_AA64*` | fallback | |
| LSE `CAS`/`LDADD`/`SWP`, `LDAPR` | fallback (commented out in `a64.inc`) | not implemented; `AT_HWCAP` answer is a decision (D26, `HwcapPolicy`) |
| PAC hint forms, `BTI` | no-op `HINT` | |
| PAC register forms, FP16 arithmetic, BF16, i8mm, `FJCVTZS` | fallback / unallocated | FP16 on arm64 host: patch 0004 |
| crypto (AES, SHA1/256, PMULL, CRC32), NEON breadth incl. `SDOT` | supported | |
| `DC ZVA`, `IC IVAU` | supported with hooks | |
| unaligned access under fastmem | works | |

`hook_hint_instructions` is not passed to the A64 frontend, so `YIELD`/`WFE` always exit the JIT.
Still unpatched: `patches/README.md` "Known candidates" 1.

## Q7. Self-modifying code

No automatic SMC detection; `InvalidateCacheRange` is required (82 us with 20,000 live blocks) and
is per jit. There is no eviction: under 1 MiB free the whole cache is thrown away. Now: omni-android
routes invalidations (`boundary.rs`), and the shared cache retires regions oldest first (patch 0028).

## Q8. Rust FFI

An `extern "C"` shim plus a Rust driver worked with MSVC `/MD` (`msvcprt`). Identity fastmem ran
at 5,566 Mguest-insn/s with no FFI calls; callbacks into Rust 826 Mguest-insn/s. This became
`crates/dynarmic-sys` (`shim/od_dynarmic.h`).

## Q9. Verdict

Adopt dynarmic as a pinned fork and treat the x64 backend as replaceable (D5). A custom JIT at
parity was estimated at ~45,000 lines (A64 frontend 13,395, x64 backend 23,023, IR/regalloc 8,120).
