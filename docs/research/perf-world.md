# Performance in a game world: instruments, measurements, levers

Working record of the world-performance work (`docs/briefs/performance.md`). Every figure carries its
n and its method; **MEASURED** means a run or a benchmark printed it, **INFERRED** means it follows
from code that was read and has not been run.

## What the logs said before anything was built (play17, n = 1 session)

Presents per 5 s, from the gate's `FRAMES` lines:

| phase | median | mean | ≈ fps |
|---|---|---|---|
| world loading (+260..+330 s) | 0 | 8.7 | 1.7 |
| world, first two minutes (+340..+460 s) | 4 | 5.2 | 1.0 |
| world +465..+665 s (1280x720) | 54 | 55.4 | 11.1 |
| world +685..+910 s (maximized, 2560x1369) | 98.5 | 91.4 | 18.3 |
| menus after the kick | 287 | 274 | 55 |

The world got faster the longer it ran, and the larger window did not slow it: whatever bounds it is
not resolution. The owner's own session (p1, binary 23dd867) was slower still in the world (0-10
presents per 5 s from load to the freeze at +635 s).

The engine's own timings of the same Lua module loads (`[SlowBenchmark]`, lobby / world):

| run | monitor slots | Types | Items | PetItem | GUILoader |
|---|---|---|---|---|---|
| play13 | 64 | 4214 / – | 4596 / – | 2815 / – | 2465 / – |
| play14 | 64 | 4051 / – | 4376 / – | 2691 / – | 2323 / – |
| play16 | 256 | 4706 / 4690 | 4871 / 4829 | 3222 / 3198 | 2735 / 2249 |
| play17 | 256 | 4883 / 5335 | 5172 / 5820 | 3322 / 3669 | 2629 / 2766 |
| p1 | 256 | 5140 / 6811 | 5327 / 7246 | 3374 / 4458 | 2794 / 2656 |

The 64-slot runs are 12-20% faster on the lobby loads. **Suggestive, not evidence**: n = 2 against
n = 3, and other things changed between the binaries (fixes, the network path).

## H1 -- the exclusive monitor

### What the code does (read, dynarmic `9d45823`)

Under dynarmic's global monitor every guest `LDXR`/`LDAXR` takes one process-wide spin lock
(`EmitExclusiveLock`), and every `STXR`/`STLXR` takes it again and then clears every *other*
processor's reservation of the same address with a scan **emitted inline and unrolled over every
slot the monitor was sized for** (`EmitExclusiveTestAndClear`, `emit_x64_memory.h`) -- a
`mov r64, imm64; cmp [r], vaddr; jne; mov [r], tmp` per slot. The runtime sizes the monitor from
`DynarmicOptions::max_threads`, which the gate sets to `MAX_GUEST_THREADS`, raised from 64 to 256 by
`23530d1`. So since that commit every guest store-exclusive has executed 255 compares, whether or not
255 threads exist, under a lock every other guest thread's atomics also take.

`libroblox.so` has **587** exclusive instructions in `.text` (293 load sites; `excl_shapes.py` in
the session scratchpad), every one in one of the shapes LLVM's atomic expansion emits:

| shape (load, then up to the store) | sites |
|---|---|
| `ldxr` / `subs` (decrement) | 125 |
| `ldxr` / `add` (increment) | 98 |
| `ldxp` / nothing (16-byte atomic load) | 8 |
| `ldaxr` / `add` | 7 |
| `ldxr`, `ldaxr` / nothing (exchange) | 9 |
| `ldaxr` / `orr`, `bic`, `and` (outline-atomic fallbacks) | 9 |
| `ldxr`, `ldaxr`, `ldxrb`, `ldxrh`, `ldaxrb`, `ldaxrh` / `cmp; b.ne` (compare-and-swap) | 14 |
| `ldxrh`, `ldaxrh`, `ldaxrb` / `add` | 5 |
| `ldaxp`, `ldxp` / `cmp; ccmp; b.ne` (16-byte CAS) | 2 |
| `ldxrb` / nothing, `ldaxrb` / `orr` | 2 |
| `ldxr` / `sub` | 3 |
| `ldxr` / `ldr; ldr; sub; sub; cmp; b.hs` (`0x62707ac`, a `compare_exchange_weak` on a timestamp) | 1 |
| `ldxr` / `cmp; csinc` (`0x6270890`, a conditional increment) | 1 |

`libroblox.so` is the only guest image the runtime loads.

### MEASURED: what one guest atomic increment costs

`omni-cpu/tests/bench.rs::the_cost_of_a_guest_atomic_increment`, release, median of 7, an
`LDAXR`/`ADD`/`STLXR`/`CBNZ` loop (the shape of a C++ `fetch_add`), ns per increment of wall time:

| monitor | 1 thread, private word | 8 threads, private words | 8 threads, one shared word |
|---|---|---|---|
| global, 1 slot | 47.2 | – | – |
| global, 64 slots | 57.3 | 366.7 | 417.2 |
| global, 256 slots (**the runtime since `23530d1`**) | 131.4 | 511.1 | 836.2 |
| value-compare, 256 threads | **12.8** | **2.4** | 46.1 |

Eight threads on eight *private* words under the global monitor are serialized: 511 ns of wall time
per increment is about 2 million increments a second **for the whole process**. Value-compare
scales (2.4 ns wall, about 19 ns per thread).

### Correctness of value-compare (`tests/exclusive.rs`, MEASURED)

* 16 threads x 20,000 iterations, each an exclusive increment of one shared 64-bit word and of one
  shared 16-byte pair (`LDAXP`/`STLXP`): exact totals under both arms; 1,171,551 store-exclusives
  failed and were retried under the global monitor, 87,657 under value-compare -- so the threads
  contended, which the test asserts.
* The same harness with plain `LDR`/`ADD`/`STR` lost updates on the first attempt (399,762 of
  3,200,000): the exact count is a detector.
* The one difference, pinned: a reservation held across another thread's exclusive increment and
  decrement (ABA) fails under the global monitor and succeeds under value-compare; both fail when the
  value really changed; both succeed with no interference.

## H4 -- `GetSetElimination` lost to `check_halt_on_memory_access`

MEASURED (`the_cost_of_stopping_at_the_faulting_instruction`, n = 31): 1.001x on a memory-heavy loop
and 0.950x on a register-bound one. On single-block loops the loss costs nothing measurable. Not
pursued unless the world's samples say otherwise.

## Instruments built

* `omni_cpu::JitCounters` per context (always on): instruction words fetched for translation, block
  starts, retranslated block starts (only under `OMNI_JIT_RETRANSLATION`), icache ops, invalidations.
* `omni_cpu::stats`: the exclusive monitors' addresses, for recognising monitor code.
* `omni_platform::sampler`: suspend / read context / resume of this process's own threads, memory
  kind, module list, process counters, processor efficiency classes. Validated in
  `sampler::tests` against a thread spinning in a known function.
* `omni_android::perf` (`OMNI_PERF=<s>`, `OMNI_PERF_SAMPLE=<hz>`, `OMNI_PERF_DUMP=<file>`,
  `OMNI_PERF_WAITS=1`): an interval reporter that starts itself. Validated in `tests/perf.rs`.
* Measurement switches in `DynarmicOptions::with_environment`: `OMNI_JIT_EXCLUSIVE_MONITOR`,
  `OMNI_JIT_OPTIMIZATIONS`, `OMNI_JIT_CHECK_HALT_ON_MEMORY`, `OMNI_JIT_RETRANSLATION`. Each prints
  `JIT SWITCH:` when set.
* Offline: `perf_report.py` (session scratchpad) symbolizes a dump through dbghelp.

## The landing screen, first instrumented run (landing1, n = 1)

Sampler at 100 Hz, 1.6-5.0% of one core. Busy threads: the GameActivity game loop (`g5`, 90% CPU,
2.3 M crossings/s, 64-73% of its time in handler code -- mutex lock/unlock and `ALooper_pollOnce`);
workers translating 40-70 k guest instructions a second while the Lua app starts, with 15-20% of
their executable-image samples in `EmitX64::GetBasicBlock` (the block lookup every indirect branch
takes under `INTERRUPTIBLE`). Monitor share 0-3%. Threads were on the efficiency cores for 20-98% of
their samples. The landing then presented 300 per 5 s. Asset downloads timed out on this run too
(`cdnHttpErrorCode: Timedout`, `assetdelivery.roblox.com`).
