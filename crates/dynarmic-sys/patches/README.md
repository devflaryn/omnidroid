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
0001 + 0002**. The older 202,200/123 was a build that also had the A32 frontend;
it is not comparable and was not re-run.

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

### 0002 — a guest thread's fixed cost: the fast-dispatch table and the prelude commit

`0002-per-thread-fixed-cost-fast-dispatch-and-prelude-commit.patch`, D32. Two
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

## How a patch is carried

Patches are applied **into `vendor/dynarmic/` directly** and a `.patch` file is
committed here alongside, so `git apply --check` against a fresh clone of the
pin verifies that the tree is exactly upstream plus these patches. Touch
`vendor/PIN.txt` afterwards; it is the only thing under `vendor/` that the build
script tells Cargo to watch.

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

**Applied as 0002 (2026-09-24)**, after the suite run this paragraph asked for -- see "Applied"
above. The paragraph that follows is the reasoning as it stood before:

**Not applied.** It changes a hot structure's indirection on the path that *is* enabled upstream, so
it needs the 202,200-assertion suite run against it with `FastDispatch` both on and off before it can
be carried — and, per this directory's rule, the pristine-tree claim given up deliberately rather
than by accident. Recorded now because it was found by measurement during Task 3 and the number is
large enough that it should not wait to be rediscovered.
