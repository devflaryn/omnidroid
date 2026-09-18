# Plan: ARM64 execution (milestone M2)

Spec: `docs/ARCHITECTURE.md` §6. Rationale: `docs/DECISIONS.md` D2, D4, D5, D12, D13.
Builds on the completed foundation (M0, M1) on branch `foundation`.

Scope: reach **M2** — a real function from `libroblox.so` executes through the CPU backend and
returns a correct result. Deliberately excludes the thunk boundary for imported symbols (that is M3's
problem, since `init_array` is what first calls into libc) and excludes graphics entirely.

## Global Constraints

These bind every task. They carry forward from the foundation plan because every one of them was
earned; constraints 11 and 12 in particular caught defects in four of five foundation tasks.

1. **No fabricated implementations and no placeholder success.** A function that cannot do its job
   returns an error. No `todo!()`/`unimplemented!()` on a path a test claims to exercise.
2. **Tests use the real APK.** `Roblox-2.738.1397.apk` is the fixture. Tests must **skip gracefully
   rather than fail** when it is absent, since it is git-ignored.
3. **Exact values are mandatory** where given. A test asserting a rounded version of a specified
   value is a defect.
4. **Platform code is confined.** OS APIs and `#[cfg(target_os)]` live only in `omni-platform`.
   Depending on `omni-platform` from another crate is correct and expected; adding `windows-sys`,
   `libc` or a raw syscall outside it is not. **New for this milestone:** C/C++ toolchain invocation
   belongs in one place — a build script in `omni-cpu` — not scattered.
5. **`unsafe` is localized and justified.** Every `unsafe` block carries a comment stating the
   invariant that makes it sound. This milestone crosses an FFI boundary into C++ and then into
   generated machine code, so this constraint is doing more work than usual: say what the foreign
   side guarantees, not just what ours does.
6. **Commit charge is the scarce resource** (D10). Never commit speculatively. Only `MEM_DECOMMIT`
   and `MEM_RELEASE` return commit charge. **New for this milestone:** D5 measured dynarmic
   committing **20-35 MiB per guest thread** with unshared code caches, and D15 recorded that a
   pagefile-backed section is invisible to `process_commit_charge`. Both mean per-thread CPU state
   must be measured and reported, not assumed small.
7. **Errors are typed and diagnostic.** `thiserror`, naming the failing value. An unsupported guest
   instruction must report *which* instruction at *which* guest address.
8. **No network access at runtime.** Vendored sources are fetched at build time only, pinned.
9. **Stable Rust.** No nightly. Warnings from our own crates are a defect.
10. **Document discoveries.** If implementation shows the research wrong, say so in the report.
    Six such corrections came out of the foundation, two of which would have caused silent corruption.
11. **Hostile input is the expected case, and you test it yourself.** D6: our own test APK is
    adversarially modified. Construct and run hostile inputs before reporting done, and put the
    results in your report. A panic or abort reachable from untrusted input is **Critical**; an abort
    cannot be contained by any caller. Remember: **a bound is only as trustworthy as its
    least-validated input**, and **saturating arithmetic on a limit turns hostile input into a larger
    permission**. **New for this milestone:** guest code is the ultimate untrusted input. It will
    jump to unmapped addresses, execute garbage, and recurse without bound. None of that may take
    down the host process.
12. **A test that cannot fail is worse than no test.** Verify by mutation that reverting a fix makes
    its test fail, and check **both directions** — a fix that goes too far can pass every correctness
    test while destroying a property the design depends on. Two committed harnesses exist
    (`tools/mutate.py`, `crates/omni-elf/tools/mutate_loader.py`); extend them or add a sibling.

---

## Task 1: `omni-cpu` skeleton, and harden the code arena with a real execution test

The final foundation review named this the single most valuable next step, because `CodeArena` is the
only component with an unsound edge, no test that runs a single generated instruction, and a cost
`process_commit_charge` cannot see — and it is what M2 touches first.

### The `GuestCpu` trait

Define in `omni-cpu`, with no backend yet:
- Create a CPU context for a guest thread, given the guest address space.
- Run from a guest address until an exit condition.
- An exit reason enum: returned normally, hit a thunk address, executed an unsupported instruction
  (naming it and its address), faulted on a memory access (naming the address), hit a breakpoint,
  exhausted a step budget.
- Read and write guest registers: X0-X30, SP, PC, NZCV, V0-V31, and **`TPIDR_EL0`** (D13 makes this
  mandatory, not optional).
- Invalidate translated code for an address range.
- Report per-context memory cost, since D5 measured 20-35 MiB per thread.

The trait must be implementable both by a translating backend on x86-64 hosts and by direct native
execution on ARM64 hosts (D5, `ARCHITECTURE.md` §6) — so nothing in its shape may assume translation.
State in your report where that assumption nearly crept in.

### Arena hardening and the execution test

`omni-mem`'s `CodeArena` already has arena identity and typed errors from the foundation's final fix
wave. What it lacks is a test that **executes generated code**. Add one:

- Emit real x86-64 bytes through the write view, execute through the execute view, assert the result.
- Do it across **two arenas** simultaneously, since per-thread code caches make several arenas M2's
  expected shape.
- Exercise seal → patch → re-execute, which is what a JIT actually does when it invalidates.
- Assert `CommitBudget` throughout, because the arena's cost is invisible to `process_commit_charge`
  (D15) and this is the only place that gap gets instrumented.
- Gate on `target_arch = "x86_64"`; on other hosts the test must skip visibly, not silently.

### Tests
Beyond the above: a foreign `CodeBlock` is rejected; a block outliving its arena cannot be written
through; W+X remains unrepresentable in the API; and a child process storing through the execute
pointer still faults.

### Report
The trait as it finally shaped up and what forced each method; the measured arena cost for a realistic
amount of generated code; and anywhere the trait's shape leaked an assumption about translation.

---

## Task 2: Build dynarmic, and bind it from Rust

D5 adopted dynarmic as a **pinned fork**: `yuzu-mirror/dynarmic@9d45823` (v6.7.0, ISC/0BSD), because
`merryhime/dynarmic` 404s. This task makes that build reproducible and callable.

### Build

The spike established what is needed, so do not rediscover it:
- **Boost is an undeclared dependency** (icl and variant). The prior-art survey's dependency list
  (fmt, mcl, xbyak, zydis, robin-map) was incomplete.
- **`-DCMAKE_POLICY_VERSION_MINIMUM=3.5`** is required, because robin-map still declares
  `VERSION 3.1` and CMake 4.x rejects it.
- **Short build paths**, or MSVC fails with `C1083`.
- `cl.exe` is not on `PATH`; the `cc`/`cmake` crates locate MSVC themselves.
- It built in **49 s** with `-j24` and passed **all 202,200** of its own test assertions.

Vendor the source pinned to that revision (submodule or vendored tree — your call, justify it), and
drive the build from a build script in `omni-cpu`. The build must not require network access at test
time, and must fail with a clear message naming the missing tool if the C++ toolchain is absent,
rather than a wall of CMake output.

### The C shim and Rust binding

dynarmic is C++ with virtual-callback interfaces, which do not bind to Rust directly. The spike
demonstrated a working shim: **18 `extern "C"` entry points plus 17 callback pointers**, reaching
**5,566 Mguest-insn/s** with guest code writing straight into a Rust `Vec<u64>`. Build the equivalent
properly: a narrow `extern "C"` surface over exactly what `GuestCpu` needs, no more.

Keep the shim's surface minimal and documented. Every callback that can be invoked from generated
guest code while Rust state is borrowed is a soundness hazard — say in the report how you prevent
re-entrancy problems.

### Tests
Execute hand-encoded A64 instructions and verify register results: integer ALU with shifted operands,
load/store including pair and register-offset forms, a loop, `BL`+`RET`, NEON, floating point, and an
atomic. Encode by hand or with a small committed helper; do not depend on an ARM assembler existing.
State the encodings in the test so they can be checked by eye.

### Report
What the shim surface ended up being and why each entry point exists; build time and any friction
beyond the four known items; and whether the 202,200 upstream assertions still pass on our pin.

---

## Task 3: Identity mapping, and the bionic thread pointer

Two settings decide whether this runtime is fast or unusable, and one decides whether it runs at all.

### Identity mapping (D4, measured)

`fastmem_pointer = Some(0)` with `fastmem_address_space_bits = 64` emits `mov reg, [r13 + vaddr]`
with `r13 = 0` — a single instruction, base folded into the SIB byte. Verified executing at host VA
`0x7F00_0000_0000` with **zero** slow-path callbacks, and measuring **13.2x** faster than routing
memory through callbacks (5,207 versus 396 Mguest-insn/s).

**Assert this configuration at startup and fail loudly if it is not in effect.** The default
`fastmem_address_space_bits` is **36**, and a high guest VA silently degrades to the callback path
while still producing correct results — a 13x performance loss that no functional test can see. This
assertion is the entire defence against that.

Also record: guest PC is truncated to a sign-extended **56 bits**. Harmless for Windows and Android
user-space addresses, but real.

### Guest fault handling

dynarmic's Windows fault handling is frame-based SEH scoped to its code cache, so an Omnidroid
**vectored** exception handler runs first — verified in the spike (`veh_hits=1`, dynarmic's slow path
never entered). Omnidroid keeps ownership of guest demand paging, which D10 requires. Wire that up and
test that a guest access to an unmapped address produces a clean, typed exit rather than a host crash.

### The bionic thread pointer (D13 — mandatory)

`libroblox.so` contains **1,282 `MRS TPIDR_EL0`** instructions, of which **1,276 read `[Xt, #0x28]`**
— bionic's `TLS_SLOT_STACK_GUARD` (slot 5). Every stack-protected function reads the thread pointer
*directly*, and this happens **before `JNI_OnLoad` and before the first static initializer**. If
`TPIDR_EL0` does not point at a valid bionic-layout TLS block with a stack guard at +0x28, the very
first stack-protected function crashes, with a symptom that looks like a loader bug.

So: allocate a bionic-layout TLS block per guest thread, populate at minimum slot 5, set `TPIDR_EL0`,
and only then run guest code. This applies to **every** guest thread, not just the first. dynarmic
supports `TPIDR_EL0`/`TPIDRRO_EL0` fully (D5), so the register is real and writable.

### Tests
Identity mapping verified by executing at a high guest VA and asserting zero callback-path entries;
the startup assertion verified by constructing the wrong configuration and confirming it is refused;
an unmapped guest access producing a typed exit; and `TPIDR_EL0` readable from guest code with the
stack guard at the right offset.

### Report
The measured callback-path entry count for a memory-heavy loop (it should be zero); what the startup
assertion checks and how you proved it fires; and the per-thread cost of the TLS block.

---

## Task 4: Execute a real function from `libroblox.so` — milestone M2

Join Tasks 1-3 to the foundation's loader and run genuine Roblox code.

### Responsibilities
- Load `libroblox.so` through the existing loader (M1), which already maps, relocates and seals it.
- **Do not run `init_array`** — those 3,594 initializers call imported symbols, which needs the thunk
  boundary, and that is M3.
- Choose a **leaf function** that calls nothing external, set up a guest thread with its TLS block and
  stack, execute it, and verify the result.
- Finding a suitable leaf function in a stripped 109 MB binary is part of the task. Use the
  `.eh_frame_hdr` function boundaries (the JNI analysis recovered **245,117 exact function starts**
  this way) and scan for a function whose body has no `BL`/`BLR` and no relocation-bearing loads.
  Commit the selection tool, and state in your report how you verified the function is genuinely a
  leaf and what it computes.
- Report translation throughput and cold-translation time for the code actually executed, against
  D5's measured 0.15-0.31 Mguest-insn/s cold and ~2.0x native steady state on memory-heavy code.

### Tests — the M2 gate
- A real function from `libroblox.so` executes and returns a **correct, independently-predicted
  result**. A test asserting only that execution completed is vacuous: predict the value from the
  instruction semantics, not from what the run produced.
- Guest code reads `TPIDR_EL0` and finds the stack guard.
- Execution is repeatable and leak-free: run many times, assert commit charge returns to baseline.
- An unsupported instruction produces a typed exit naming it, not a crash.
- Guest code jumping to an unmapped address produces a typed exit.
- Per-thread CPU memory cost is measured and **asserted against a ceiling**, since D5 warns of
  20-35 MiB per thread and D15 warns that part of it is invisible to `process_commit_charge`.

### Report
The function chosen and why it qualifies; its predicted versus actual result; measured cold and warm
throughput; per-thread memory cost; and anything about dynarmic's behaviour on real Roblox code that
differs from the spike's synthetic benchmarks.
