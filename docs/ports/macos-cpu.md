# macOS port — the CPU workstream (dynarmic's arm64 backend, `omni-cpu`)

Host: Apple M1, 16 GB, macOS 26.5, arm64, 16 KiB pages. Branch `mac-cpu`. The orchestrator folds this
file into `docs/ports/macos.md`.

## State

`cargo test -p dynarmic-sys --release --no-fail-fast` and `cargo test -p omni-cpu --release
--no-fail-fast` both exit 0 on the arm64 host, **the M2 gate against the real `libroblox.so`
included** (base64 alphabet, timeval arithmetic, thread pointer and stack guard, typed exits,
per-slice invariant armed and never tripped, per-thread cost). `omni-android`'s `initializers` gate
runs all 3,594 `init_array` entries in order with `degraded_slices == 0` and the exact 92,431 image
pointers (its non-pointer-word count follows guest-space placement; see "Open").

## What was wrong, and why (root causes)

Eight defects in the pin's **arm64 backend**, each a carried patch (`crates/dynarmic-sys/patches/`,
README entries 0002-0008; `tools/verify_patches.py` checks the tree is pristine + patches, byte for
byte). None touches the x64 backend or dynarmic's exception handler.

| Patch | Defect | Consequence on arm64 | Found by |
|---|---|---|---|
| 0002 | `Interpret` terminal was `ASSERT_FALSE` | the **first undecodable word** (231 decoder entries, every LSE atomic) terminated the process | `hostile.rs` fuzzer, trial 2 |
| 0003 | 18 scalar saturating opcodes unimplemented | `SQADD`/`UQADD`/`SQSUB`/`UQSUB` scalar, `SQDMULH` scalar: abort | reading + tests |
| 0004 | 19 FP16 opcodes unimplemented; **`EmitTwoOpFallbackWithoutRegAlloc` saved `~(1ull << Qresult.index())`** (the result's GPR twin) | FP16 arithmetic: abort (fuzzer trial 1,002, `FMSUB H`); `FRINT* V.8H`: silently **wrong** result (`[0, 0]` measured) | fuzzer, tests |
| 0005 | `SM4AccessSubstitutionBox` unimplemented | `SM4E`/`SM4EKEY`: abort | reading + test |
| 0006 | `VectorMaxU64`/`VectorMinU64` unimplemented | `CMHS`/`CMHI` D and `.2D` (the IR builds 64-bit unsigned compares from max/min): abort | fuzzer, trial 158,510 of 300,000 -- missed by a read-only reachability study |
| 0007 | `fastmem_exclusive_access` **ignored** | every `LDXR`/`STXR` pair took 2 callbacks; `omni-cpu`'s per-slice invariant stopped the guest (`omni-android` initializers: 136 callback entries at `init_array[2]`) | `a64_exec::atomic_load_exclusive_store_exclusive` |
| 0008 | `EmitA64CheckMemoryAbort` loaded the **u32** halt word with a **64-bit** `LDAR` | alignment fault in JIT code on every fastmem miss with `check_halt_on_memory_access`: **every guest access to unmapped memory aborted the process** (`omni-cpu` `faults.rs`, `omni-android` `bionic.rs`) | reproduced with dynarmic's handler alone |

Unreachable from A64, left as `ASSERT_FALSE` with the evidence in `patches/README.md`:
`FPHalfToFixedS16/U16`, `FPVectorToSigned/UnsignedFixed16`, `VectorEqual128`, `VectorMaxS64`,
`VectorMinS64`, `VectorSignExtend64`, `Vector{Signed,Unsigned}Multiply{16,32}`, and the
`RoundingMode::ToOdd` arms of both `EmitToFixed` helpers. The decisive evidence that nothing else
reachable remains is enumeration rather than reading: `tests/decoder_sweep.rs` executes every one of
the 874 decoder entries 48 times (one-off: 2,048 times, 1.79 M words per memory path) plus 100,000
random pairs behind a constant (one-off: 3,000,000), both memory paths, restarting past any death to
list them all -- **0 deaths, 0 wedges**. The random fuzzer, 2,000,000 trials per path: clean.

Two `omni-cpu` issues that were host facts, not backend defects:

* **`OD_FIXED_PER_JIT_BYTES`** named the x64 emitter's 16 MiB fast-dispatch table; the arm64
  backend has none (its `FastDispatchHint` is a dispatcher return with a `TODO`), so `cost()`
  claimed 16.004 MiB against 8.010 MiB measured and stopped being a floor. Per-arch now: 0 on
  aarch64, checked against the vendored source.
* **Guest-space placement.** macOS puts a default 4 GiB reservation at `0x3_0000_0000`, below the
  64 GiB every identity test requires (`assert_high_addresses`). The test harness re-asks with 2^36
  alignment only when the default is low.

## FPCR in host callbacks

The x86-64 finding that the guest's `MXCSR` is live inside host callbacks has an exact arm64 twin.
dynarmic's arm64 prelude installs the guest's `FPCR` and restores the host's only in
`return_from_run_code`; the call trampolines switch nothing. The dispatcher's guard therefore
switches `FPCR` on aarch64 (`omni-cpu` `dynarmic::fpcr`, re-exported as `mxcsr`), and
`thunk.rs::the_dispatcher_puts_the_host_mxcsr_under_a_handler_and_the_guest_s_back` asserts it with
`FPCR.FZ`. Patch 0002's interpreter fallback does the same switch inside generated code, as x64's
`Interpret` terminal does with `MXCSR`.

## Stoppability on arm64 (D16) -- MEASURED, 36 cells, each in its own process

`0xFFFF` = `ALL_SAFE`, `0xFFF9` = `INTERRUPTIBLE` (what `omni-cpu` runs), `0xFFF8` = no block linking.
Stopped (S) or wedged (W) by: a finite budget / a cross-thread halt with counting off / a halt with
an unexpiring budget.

| Shape | `0xFFFF` | `0xFFF9` | `0xFFF8` |
|---|---|---|---|
| direct, empty (`B .`) | S / S / **W** | S / S / **W** | S / S / S |
| direct, with body | S / S / **W** | S / S / **W** | S / S / S |
| indirect (`BR X30`) | S / S / S | S / S / S | S / S / S |
| **return (`RET` via the RSB)** -- new shape | **W / W / W** | S / S / S | S / S / S |

Differences from x64: the `BR` loop is stoppable everywhere, because arm64's `FastDispatchHint` is
not implemented (a plain dispatcher return); the `RET`-through-RSB loop is unstoppable under
`0xFFFF` by any mechanism, because arm64's `PopRSBHint` is implemented and checks nothing (as
x64's). **D16's operational conclusion holds on arm64 unchanged**: under `0xFFF9` with cycle
counting every shape stops at its budget, and only direct-branch loops ignore a halt while the
budget has not expired -- build the watchdog from budget windows. No patch was needed for
`omni-cpu`'s guarantees. `hostile.rs::the_stoppability_matrix` asserts the x64 table on x86_64
(unchanged) and this one on aarch64.

## x18

The arm64 allocator's `GPR_ORDER` is `{19..23, 9..15, 0..8}`; the fixed registers are 24-28 and the
scratch registers 16, 17, 30; no emitter names `X18`/`W18`. `tests/x18.rs` checks that against the
source and measures a guest `X18` across 2,000 forced host sleeps and a 200 M-iteration loop.
**MEASURED, as a one-off mutation:** with 18 put first in `GPR_ORDER`, the 200 M-iteration loop
never ended (killed after > 600 s) while the sleep test still passed -- consistent with the kernel
clearing host x18 under a running thread and the loop's value in it being lost; the register
values were not captured. The test now bounds the loop with a budget so that is a failure, not a
hang.

## TPIDR_EL0 / TPIDRRO_EL0

The guest's are memory slots (`A64GetTPIDR`/`SetTPIDR`/`GetTPIDRRO` go through the config
pointers). `tests/thread_pointer_host.rs`: the host's `TPIDRRO_EL0` (macOS TLS) is identical before,
inside a callback, and after a run in which the guest writes `TPIDR_EL0` and reads both; host
`thread_local!` intact; guest `MSR TPIDRRO_EL0` (UNDEFINED at EL0) goes to the interpreter fallback.
The host's `TPIDR_EL0` holds the CPU number on macOS and changes on migration (4100 -> 4102
measured), so it is asserted only never to hold a guest value. `omni-cpu`'s `thread_pointer.rs`
(7 tests) passes.

## W^X on this host (D12 exception)

* The arm64 code cache is oaknut's `CodeBlock`: `mmap(RWX, MAP_JIT)`, and every write is bracketed
  by `pthread_jit_write_protect_np(0/1)`, which switches **the calling thread's** view.
* MEASURED (`tests/wx.rs`, child processes): a write to the cache from the jit's thread between
  runs **faults**; a write from inside a callback in the middle of guest execution **faults**.
  Readable at the same address (positive control). A death counts only if the child printed that
  it was about to write and never that the write succeeded (see `mac-cpu-A17` below for why).
* MEASURED (`tools/map_jit_probe.c`): another thread with its own write window open **can** write a
  `MAP_JIT` page while this thread executes it. So W^X holds per thread, not per page: a guest
  thread can never write the cache, but dynarmic emitting for another jit (or its Mach handler
  thread relinking after a fault) holds every `MAP_JIT` page in the process writable for that
  window.
* `od_jit_effective_config().code_cache_w_xor_x` reports `code_cache::W_XOR_X_PER_THREAD` (2) on
  Apple arm64 instead of echoing a build flag that said 0; the x64 value and its test are unchanged.
* D12-style emit+execute cycle on `MAP_JIT` (open window, write, close, `sys_icache_invalidate`,
  execute, check): **median 192.4 ns** (min 183.2, max 207.4), n = 31 samples x 200,000 cycles,
  0 mismatches in 6,200,000. Against D12's Windows figures: 162 ns dual-mapped, 2,259 ns
  `VirtualProtect`.
* Per-jit memory: 8.010 MiB of `phys_footprint` per jit at creation with an 8 MiB cache (n = 8
  threads, 1 measurement), i.e. the `MAP_JIT` cache is charged in full when created on this host.

## Merge notes (every edit to code that also compiles on Windows)

Each is additive and leaves Windows x86-64 behaviour byte-for-byte unchanged.

1. `crates/omni-cpu/Cargo.toml`: the `dynarmic-sys` target section is
   `cfg(any(target_arch = "x86_64", target_arch = "aarch64"))`.
2. `crates/omni-cpu/src/lib.rs`: `pub mod dynarmic` for `any(x86_64, aarch64)`.
3. `crates/omni-cpu/src/dynarmic/mod.rs`: `mxcsr` is `#[cfg(target_arch = "x86_64")]` (body
   unchanged); a `#[cfg(target_arch = "aarch64")] mod fpcr` is re-exported as `mxcsr` there.
4. `crates/omni-cpu/tests/*.rs`: crate-level cfgs name both architectures; `thunk.rs`'s MXCSR helpers
   are x86_64-gated with FPCR twins and a cfg'd `HOST_FLUSH_BITS` (same value on x86_64).
5. `crates/omni-cpu/tests/harness/mod.rs` + `roblox.rs`: `high_guest_space()` returns the default
   space untouched when it is already above 64 GiB (Windows) and re-asks with 2^36 alignment only
   when it is not.
6. `crates/dynarmic-sys/src/lib.rs`: `OD_FIXED_PER_JIT_BYTES` is per-arch (x86_64 value unchanged);
   new `code_cache` constants; `od_jit_last_svc_return_address` declared for aarch64 only.
7. `crates/dynarmic-sys/shim/od_dynarmic.{h,cpp}`: `OD_CODE_CACHE_*` constants (0 and 1 as before);
   the Apple-arm64 branch of `code_cache_w_xor_x`; `last_svc_return` and
   `od_jit_last_svc_return_address` exist only under `__aarch64__`. No struct layout changed; ABI
   version unchanged.
8. `crates/dynarmic-sys/tests/`: `a64_exec::the_code_cache_is_writable_and_executable_at_once` is
   `cfg(x86_64)` (unchanged); `hostile.rs`'s matrix asserts the old 27 cells on x86_64 and the
   arm64 table on aarch64 (a `Return` shape exists but is only in the arm64 table); harness options
   and hooks added with defaults that reproduce the old behaviour; new test files are
   architecture-neutral except `wx.rs` and `thread_pointer_host.rs` (aarch64 only).
9. `crates/dynarmic-sys/vendor/`: patches 0002-0008 touch only `backend/arm64/` (not compiled for
   x86_64 targets) plus an `#include` of `backend/x64/exclusive_monitor_friend.h` from arm64 code.
   `vendor/PIN.txt` lists them.
10. `tools/mutate_mac/cpu.py`: rows; `crates/dynarmic-sys/tools/{verify_patches.py,map_jit_probe.c}`.

## Mutation results

`python3 tools/mutate.py --only mac-cpu-` (rows in `tools/mutate_mac/cpu.py`; vendored rows touch
`vendor/PIN.txt` to force the rebuild): **24/24 caught** -- 23/24 on the first run, and the miss was
a real test defect, fixed and re-run:

* `mac-cpu-A17` (never re-protect the code cache) was **NOT CAUGHT**: the W^X child died, but
  earlier -- a thread left in its write window cannot execute the cache either -- and "the child
  died" was all `tests/wx.rs` asked. The child now prints a marker immediately before its write and
  the parent requires the marker and the absence of a successful write; re-run: caught by both
  `wx.rs` tests. (On Apple silicon no mutation can make one thread write and execute the cache at
  once; the test's power is against a host where the cache is plain RWX.)
* Caught by a crash rather than an assertion (the harness names no test): A1 (Interpret asserts),
  A14 (128-bit fault entry leaves the stack displaced), A15 (0008 reverted), B3 (RSB no-op: the
  matrix child's own assertion).
* B2 (the inline store-exclusive without the monitor lock) is caught only by the mixed
  inline+callback contention test; B1 only by the negative lane added to the FMLS test.

After the run the tree was byte-identical (`git diff --exit-code crates tools`) and
`vendor/PIN.txt` was touched again so the next build could not keep the last row's mutation.

## Performance (arm64), release, MEASURED with the machine shared (load average 4.5-9)

`cargo test -p dynarmic-sys --release --test bench -- --ignored` (n = 31 per configuration):

| Workload (100,000 iterations) | `ALL_SAFE` | `INTERRUPTIBLE` | no block linking |
|---|---|---|---|
| no indirect branches | 0.082 ms, 4,873 M insn/s | 0.085 ms (1.03x) | 0.389 ms (4.75x) |
| 2 indirect in 12 | 0.505 ms | 0.798 ms (1.58x) | 1.553 ms (3.07x) |
| 2 indirect in 4 | 0.540 ms | 0.798 ms (1.48x) | 1.471 ms (2.73x) |

Per indirect transfer under `INTERRUPTIBLE`: 1.29-1.46 ns (x64: 3.9 ns). Cold translation: 2,000
blocks / 8,000 instructions in 13.627 ms median (n = 31), 0.587 M insn/s, 6.8 us/block.

`cargo test -p omni-cpu --release --test bench -- --ignored` (n = 31 unless stated):

* D4: identity 64-bit fastmem 1.267 ms (3,947 M insn/s, 0 callbacks) against dynarmic's default 36
  bits 42.131 ms (119 M insn/s, 64,000,000 callbacks): **33.26x**.
* Per-thread charge (`phys_footprint`, n = 1 run): 8.014 MiB/thread created, 8.029 translated, 8
  threads; against the cache size (4 contexts each): 8 MiB -> 8.023, 32 -> 32.027, 128 -> 128.074
  MiB/thread. On this host the per-thread cost **is** the code cache, charged in full at creation
  (`MAP_JIT`), so the cache size is the knob.
* Sliced run loop: 1.263 ms for all three limits (1.000x). `check_halt_on_memory_access`: 1.319x
  memory-heavy, 1.220x register-bound. Per-slice invariant: 1.0002x (one counter read 0.956 ns).

`omni-cpu` `roblox.rs` (real `libroblox.so`, 870 leaf functions, 8,679 instructions per pass): cold
15.103 ms (n = 11, 0.575 M insn/s), warm 0.053 ms (n = 31, 165.1 M insn/s), 60.4 ns per warm call;
0 callback-path entries.

`omni-cpu` `thunk.rs` (n = 31): design B (inline) 14.71 ns/call, A (exit) 30.35; with the 8-argument
marshal through a real PLT stub B 34.12, A 44.63; one `od_jit_run` entry+exit 30.6 ns, first and last
in the process alike (not bimodal here). The FPCR guard: 1.57 ns when the words match, **26.01 ns**
when they differ (`MSR FPCR` is expensive on M1; x64's `LDMXCSR` pair is cheaper).

## Open

* ~~`omni-android` initializers: 131,992 non-pointer words against a floor of 133,000.~~ **Settled by
  the orchestrator, not the CPU:** the x64 backend under Rosetta 2 on this Mac gives the same
  131,991, and the count follows where the guest space is placed (131,991 for any base from 4 GiB
  to `0xff_0000_0000`, 133,136 at 2^40, 133,269 at Windows' `0x1b7_0000_0000`). The floor is
  placement-aware on `port-macos` (07fd9f2).
* ABI hazard, unmeasured: the pin's arm64 memory/exclusive trampolines pass `u8`/`u16` guest values
  to `UserCallbacks` without extending them to 32 bits, which Apple's ABI requires of the caller.
  The new trampolines in 0007 take `u64` and narrow in C++; the upstream ones were left alone.
* `emit_arm64_a32.cpp` carries 0008's 64-bit `LDAR` too; A32 is not built here.
* FP16 goes through `FP::` soft routines (as on x64), not the host's FEAT_FP16; correct and
  host-independent, and slower than the hardware would be.
