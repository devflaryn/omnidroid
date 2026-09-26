# Performance in a game world: measurements and levers

Measured 2026-09-23 on the 2.738.1397 `libroblox.so` (byte-identical to the stock APK's, so the
figures stand). MEASURED = printed by a run or benchmark; INFERRED = read from code, not run.

## Starting point (one session, n = 1)

Presents per 5 s from the gate's `FRAMES` lines: world loading ~9 (1.7 fps), first two minutes in
the world ~5 (1 fps), later 55 at 1280x720 (11 fps) and 91 maximized at 2560x1369 (18 fps), menus
~274 (55 fps). The world got faster over time and the larger window did not slow it: the bound is
not resolution.

## H1: the exclusive monitor

INFERRED from dynarmic's x64 emitter: under the **global** monitor every guest `LDXR`/`LDAXR`
takes one process-wide spin lock, and every `STXR`/`STLXR` takes it again and clears other
processors' reservations with a scan unrolled inline over every slot the monitor was sized for
(`EmitExclusiveTestAndClear`). The gate sizes it from `MAX_GUEST_THREADS` = 256
(`omni_android::bionic`), so each store-exclusive ran 255 compares.

`libroblox.so` has 587 exclusive instructions in `.text` (293 load sites), all in LLVM
atomic-expansion shapes: mostly `ldxr`/`subs` (125, decrement) and `ldxr`/`add` (98, increment),
plus 14 compare-and-swap, 10 16-byte pair forms and a few exchanges and outline-atomic fallbacks.
`libroblox.so` is the only guest image the runtime loads.

MEASURED, `omni-cpu/tests/bench.rs::the_cost_of_a_guest_atomic_increment` (release, median of 7,
`LDAXR`/`ADD`/`STLXR`/`CBNZ` loop), ns of wall time per increment:

| Monitor | 1 thread | 8 threads, private words | 8 threads, one word |
|---|---|---|---|
| global, 64 slots | 57.3 | 366.7 | 417.2 |
| global, 256 slots | 131.4 | 511.1 | 836.2 |
| value-compare | **12.8** | **2.4** | 46.1 |

Under the global monitor eight threads on private words are serialized (~2 M increments/s for the
whole process); value-compare scales. Correctness (`omni-cpu/tests/exclusive.rs`): 16 threads x
20,000 exclusive increments of a 64-bit word and a 16-byte pair give exact totals under both
monitors (plain `LDR`/`ADD`/`STR` lost 399,762 of 3,200,000). The one semantic difference: a
reservation held across another thread's increment-then-decrement (ABA) fails under global and
succeeds under value-compare.

**Outcome (D31):** `DynarmicOptions::default()` uses `ExclusiveMonitor::ValueCompare`;
`OMNI_JIT_EXCLUSIVE_MONITOR=global` switches back.

## H4: `check_halt_on_memory_access`

MEASURED (`bench.rs::the_cost_of_stopping_at_the_faulting_instruction`, n = 31): 1.001x on a
memory-heavy loop, 0.950x on a register-bound one. Not pursued.

## H5: the import census

MEASURED (`omni-android/tests/perf.rs::the_cost_of_an_import_crossing_with_the_census_on_and_off`):
with the census charging a shared per-symbol counter, a crossing cost 518-614 ns with eight
threads on one symbol, against 48-50 ns with the census off. The census now writes only the
crossing thread's own record (`Boundary::start_census`); that test is the check.

## Instruments

- `omni_cpu::JitCounters` per context: words fetched for translation, block starts, retranslated
  block starts (only with `OMNI_JIT_RETRANSLATION`), icache ops, invalidations.
- `omni_cpu::stats`: exclusive-monitor addresses, retranslation tracking.
- `omni_platform::sampler`: suspend / read context / resume of this process's threads.
- `omni_android::perf`: interval reporter, `OMNI_PERF=<s>`, `OMNI_PERF_SAMPLE=<hz>`,
  `OMNI_PERF_DUMP=<file>`, `OMNI_PERF_WAITS=1`; tested in `tests/perf.rs`.
- Switches read by `DynarmicOptions::with_environment`, each announced as `JIT SWITCH:`:
  `OMNI_JIT_EXCLUSIVE_MONITOR=global|value`, `OMNI_JIT_OPTIMIZATIONS=<hex mask>`,
  `OMNI_JIT_CHECK_HALT_ON_MEMORY=0|1`, `OMNI_JIT_RETRANSLATION=1`.

## Landing screen profile (n = 1, 100 Hz sampler, 1.6-5% of a core)

The GameActivity game loop thread ran at ~90% CPU with 2.3 M boundary crossings/s, 64-73% of its
time in handler code (mutex lock/unlock, `ALooper_pollOnce`). Workers translated 40-70 k guest
instructions/s during Lua start-up, with 15-20% of their samples in `EmitX64::GetBasicBlock` (the
block lookup every indirect branch takes when `interruptible`). Monitor share 0-3%. Threads spent
20-98% of samples on efficiency cores.
