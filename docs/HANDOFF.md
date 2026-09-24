# Handoff

Written 2026-09-19, current as of **2026-09-22, late M6**. This file is a pointer and a state
snapshot, not a history. The durable sources of truth are `docs/ARCHITECTURE.md`,
`docs/DECISIONS.md`, `docs/STATUS.md`, **`docs/VERIFICATION.md`**, the M3 plan and ledger, and git
history.

## Read `docs/VERIFICATION.md` before writing a test you intend to rely on

New, and the least reconstructible thing here. **Thirteen documented ways verification has failed
in this project**, each a real incident with its measurement, plus the six process rules they
produced. **Rules 2, 3 and 4 were all amended on 2026-09-22 because all three were broken that
day** -- twice by committing or building against a tree a mutation harness was holding.
Not general advice — every entry produced a *green suite that proved less than it claimed*, and most
were written by whoever wrote the code.

The two that recur: **a count cannot see a substitution** (assert membership, not totals), and **the
regression test for a previous defect is where the next gap hides** — both High findings of the
adapter review were exactly that.

## Where we are on the startup contract

`docs/research/jni-surface.md` §8 is the spine of everything from here: **26 ordered steps** from
`dlopen` to "the engine asks for a rendering surface", nearly all VERIFIED against the real binary.

| steps | milestone | state |
|---|---|---|
| 1-5 | M0-M3 | **done** — ending with all 3,594 initializers |
| 6-12 | M4 | **done** — `JNI_OnLoad` returns `0x00010006` on the real engine; 19 of 21 scripted downcalls |
| **13** | **M5 — reached** | `initializeNativeCode` returns a `NativeCode *` on the real engine. Record: **D29** |
| 14 | **M5 — reached** | the game thread runs `android_app_entry`, and §8 row 14's cond-wait completes |
| 15 | **reached** | the game thread runs `android_main` → `NativeEngine::GameLoop()` |
| 16-20 | **M6 — reached** | all seven lifecycle/surface natives return, and the engine logs `APP_CMD_INIT_WINDOW: hasWindow = true` through `APP_CMD_CONTENT_RECT_CHANGED`. `ALooper_pollOnce(-1)` now waits instead of refusing, on a measured wake source |
| **21** | **M6 — half reached** | `nativeInitClientSettings` **returns 0 and the flags load** (`Flag::areFlagsLoaded` = 1); the engine writes its flag cache, brings up Mimalloc and reports its build. `nativePostClientSettingsLoadedInitialization3` then blocks in `pthread_cond_wait` — **the live blocker**. Opt-in: `OMNI_M6_ROWS_21_22=1` |
| 22-24 | M6 | `nativeGameGlobalInit`, the app start, the surface update — unreached |
| 25 | **M7 — the host half exists** | `omni-platform::window` and `omni-gfx` are real: a resizable Win32 window and a Vulkan swapchain presenting **pixel-verified** frames. What is still unbuilt is the *guest-facing* half — EGL (17 hard-linked symbols, **0 bound**) and Vulkan via `dlopen` (**0 `vk*` imports**), and nothing yet connects `ANativeWindow` geometry to the host window |
| 26 | M7/M8 | input, first frame, interactive |

**The architectural fact that shapes M5 and M6:** the Java side is the **initiator**. `libroblox.so`
will not reach a first frame by itself — the host must *drive* it with the sequence ART would
normally perform. It is an **orchestration problem, not an interpretation problem**, and D7 (no JVM,
no ART, no dex interpreter) is unaffected.

**M6's scope collapsed** (D27): the APK ships **ETC1 only** — 38 containers, 813,802 blocks, **zero**
in any ETC2-only mode, and **zero** ASTC/EAC/PVRTC bytes anywhere. `crates/omni-texture` decodes it
with zero dependencies. An ASTC decoder would have been entirely dead code.

## Running more than one agent at once

M4 and the texture work ran in parallel successfully. What made it safe:

* **Disjoint crates.** One owned `omni-android`/`omni-bionic`/`omni-platform`, the other
  `omni-gfx`/`omni-texture`/`tools`. Both may touch `docs/`.
* **`tools/mutate.py` is shared and the harness is EXCLUSIVE.** It mutates the working tree in place,
  so two runs cannot overlap and no one may build or commit during one. Each agent runs only its own
  rows with `--only`; **the controller runs the full table once, after both finish.**
* **`--only` pre-flights only the rows it selects**, so staleness elsewhere stays invisible. A large
  feature *will* stale rows anchored on code it moves — six were staled once, and an id collision
  once. Pre-flight the whole table (without running it) after any large change.
* Stage explicit paths, never `-A`; commit in pieces. An agent hit a usage limit mid-task in this
  session and lost nothing **only because it had committed thirteen times**.

## Git state

| | |
|---|---|
| Current branch | **`bionic-threads`** (M3 task 3 work) |
| Working tree | 2026-09-23 late night: the four fixes are committed; branch `base-0923-night` holds the rest -- see START HERE (`.claude/` is untracked scratch) |
| HEAD | see `git log` |
| Other branches | `android-abi` (M3 tasks 1-2), `bionic-pure` (the pure libc/libm subset), `cpu-execution` (M2), `foundation` (M0/M1), `main` (behind — holds only early docs) |
| Remotes | **none configured** |
| Merge state | Nothing has been merged to `main`. Each milestone branched from the previous one. **The user has never been asked to approve a merge; do not merge without asking.** |

Branch lineage: `main` → `foundation` → `cpu-execution` → `android-abi` → `bionic-pure` →
`bionic-threads`. It is a single linear chain, so `bionic-threads` contains everything.

**Provenance warning.** `bionic-pure`, `bionic-threads` and
`docs/research/os-surface-inventory.md` + `tools/os_surface.py` were produced by **GLM 5.3 Flash**,
a much weaker model; its commits are marked `Produced-By: GLM 5.3 Flash`. That work is under review;
see "GLM work — review state" below. The `bionic-threads` session was cut off mid-turn, so the tree
was checked for a live mutation before anything was run: **it was clean**, and `cargo test
--workspace --release` passed from HEAD.

## Verification state

> **Superseded — the current figures are in the `Verification state` section under `# START HERE`
> at the end of this file.** Kept here for the shape of the history. As of 2026-09-22 it is
> **1,404 passing / 0 failing / 17 ignored** across 114 targets, and `tools/mutate.py` holds **446**
> rows with gate 1 clean; the full table has **not** been run in one pass since it was 393.

**1,328 passing, 0 failing, 17 ignored** (`cargo test --workspace --release`, after M5). The two
gates — `tests/jni_startup.rs` and `tests/gameactivity.rs` — are four of them, and each takes about
50 s because it loads 109 MB and runs 3,594 initializers before it starts.

Mutation: `tools/mutate.py` held **393** rows at M5 and **the whole table was run on the committed
tree: 393/393 caught**, with both pre-flight gates passing. Two rows missed on the first full run
and **neither was a gap in the code**: both dropped entries from an array without changing its
declared length, so neither compiled, and the harness reported `did not compile, twice` rather
than `caught` — entry 8's retry gate doing its job. Re-anchored on the whole const; `--only
confine` is 8/8.

**Read the `N/M caught` line, not the exit code**, if you pipe the harness through `tail`: the
pipeline reports `tail`'s status and the harness's own non-zero exit is masked.

The M4-era figures, kept for the shape of the history:

**1134 passing, 0 failing, 13 ignored** (`cargo test --workspace --release`, 2026-09-21 — was
1,093 before M3 task 3 phases 3d+3e, 1,059 before phase 3c, 1,004 before phase 3b, 959 before
phase 3a, 926 before phase 2, 877 before phase 1, and 608 before `omni-bionic` existed, so those
are not comparable). The thirteenth ignored test is phase 3c's per-guest-thread memory
measurement, which is `#[ignore]`d because `process_commit_charge` is process-global. **The whole
mutation table has been run on the committed tree: 284/284 caught**, with both pre-flight gates
passing (284/284 patterns match exactly once; 11/11 commands pass on the unmutated tree). Clippy
clean on `--all-targets`, `cargo doc` clean, `--no-default-features` builds — and that last one is now
*verified* rather than assumed: `cargo tree -p omni-android -e normal` has no `dynarmic-sys` in it.
With `workspace = true` a member's `default-features = false` is **ignored**, so the omni-android
dependency on omni-cpu spells its path out; see the comment in that manifest.

Three committed mutation harnesses, all restoring the tree byte-for-byte and all with a pre-flight
gate that refuses to run against a modified tree:

| Harness | Rows |
|---|---|
| `tools/mutate.py` (workspace) | **284**, all caught on a full run |
| `crates/dynarmic-sys/tools/mutate_shim.py` | 23 |
| `crates/omni-elf/tools/mutate_loader.py` | 18 |

Other committed tools, each self-checking against a known count: `tools/thunk_sweep.py`,
`tools/init_reach.py`, `tools/atomic_mix.py`, `tools/branch_mix.py`.

**`tools/mutate.py` gained two gates in phase 3a, both because it needed them.** It now refuses to
run when two rows share an id (six new rows collided with the fault handler's `plat-A1`..`plat-A4`
and nothing complained), and it runs **every distinct command once on the unmutated tree** before
mutating anything. That second gate closes the worst hole a mutation harness can have: a command
that already fails reports every row using it as `caught`, because "the suite failed" is the whole
of what caught means. It did exactly that for eight rows before it was fixed — see D22.

## The five-target requirement — read this before writing any code

The project must support **Windows x86-64, Linux x86-64, Linux ARM64, macOS ARM64, macOS x86-64**.
Everything built so far runs and is tested on **Windows x86-64 only**, and the other four are
**deliberately not claimed** anywhere. That asymmetry is the single easiest thing to erode by accident,
because every task so far has been able to ignore it and still pass.

Two rules make the difference, and both have already been enforced against real attempts to break them:

**1. Keep the door open, and say when you nearly closed it.** On ARM64 hosts there is **no translator
at all** — the loader maps guest code executable and calls it, so a guest pointer, a guest register and
a guest call are all native. Any abstraction that assumes translation silently makes those two hosts
unreachable. This has genuinely nearly happened:

- The `GuestCpu` trait was reviewed specifically for it, by sketching how a native backend would
  implement every method. The subtle one went the *other* way: dynarmic truncates guest PC to 56 bits,
  and encoding that in the shared address type would have bound the **native** path to a *translator's*
  limit. It reports a queryable address width instead.
- D18 records **four places the thunk boundary nearly foreclosed it, two of which already had** — and
  notes that AArch64 variadic rules differ by platform (Apple arm64 puts *all* variadic arguments on
  the stack; Windows on ARM64 puts variadic floating point in integer registers). Getting that wrong
  is a silent wrong-answer class, not a compile error.

**2. Never claim a platform works.** Non-Windows paths return typed *unsupported* errors that name the
POSIX call they intend to make (`omni-platform/src/fault/unsupported.rs`, `vm/error.rs`). One refuses
an operation **even though the correct Unix implementation is a no-op**, which is the right direction
to err. A hypothetical Linux build degrades to a reported "no guest paging" state with the runtime
invariant auto-disarmed, rather than being silently wrong.

**What is structurally ready versus what is genuinely untested:**

| | |
|---|---|
| OS surface confined to `omni-platform` | Verified across every crate — no `cfg(target_os)` or OS crate escapes it. `omni-android` depends on `omni-platform` directly as of phase 3a, which is the seam working, not a breach |
| `omni-platform` beyond `vm`/`fault` | **`clock`, `process`, `log` and `fs` exist** (phases 3a, 3b and 3d/3e; D22, D23, D25). Most of their primitives are portable `std` and are implemented once; the five that need a target-specific call — entropy, the current processor number, **process CPU time**, `pread` and `statvfs` — have a Windows backend and structural `linux.rs`/`macos.rs` naming `getrandom(2)`, `arc4random_buf(3)`, `sched_getcpu(3)`, `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`, `pread(2)` and `statvfs(3)`. **macOS has no `sched_getcpu` and no supported equivalent**, so that one is expected to stay a refusal there. **There is no socket seam, and D25 records why one was not needed** |
| `omni-cpu` builds with **no C++ toolchain at all** (`--no-default-features`) | Guarded by a CI job. That guard was itself found blind once and fixed — read its comment before touching it |
| ARM64-native CPU path | **Expressible, untested, not claimed.** No trait method requires emitting a byte |
| ARM64-native thunk veneer | Designed (four instructions, 16-byte slots sized for it), untested |
| Linux / macOS virtual memory, faults, JIT arena | **Not implemented.** The JIT arena's dual-mapping is Windows-specific in mechanics; Linux has `memfd_create` + two `mmap`s, macOS has `MAP_JIT` with `pthread_jit_write_protect_np` which behaves differently and needs its own measurement |
| The file seam's **confinement** on Linux | Implemented on the portable `std` half and never run there — **and it is the one place a unix implementation would be strictly better rather than a translation.** `openat(2)` with `O_NOFOLLOW` per component, from a descriptor held open on the root, closes the symlink race the shared design admits. `fs/linux.rs` says so where it will be read |
| Graphics | Vulkan verified on this host only (native resizable window, real triangle). D3D12/Metal are a renderer-trait seam that does not exist yet — graphics starts at M6 |
| One Windows-only *gap* worth knowing | `unmap` is whole-view-only on Windows and must be emulated; Linux does not have this restriction, so that emulation is Windows-specific complexity, not shared design |

**Practical guidance:** when a task adds a platform primitive, add the Linux and macOS signatures as
honest `unsupported` returns at the same time, naming the intended syscall. It costs minutes, it keeps
the seam shaped correctly, and it is how the non-Windows bring-up later becomes a fill-in rather than a
redesign. Do **not** write speculative `mmap` bodies — that was ruled against deliberately, because an
unverified body can misbehave silently where a typed error fails immediately and visibly.

**And the other half of that rule, established in phase 3a: not every primitive calls an OS API, and
one that does not must NOT be given a fabricated `unsupported` arm.** `clock`, `log`, `pid` and
`cpu_count` are `Instant`, `SystemTime`, `thread::sleep`, `stderr`, `process::id` and
`available_parallelism` — portable standard library, correct on all five targets, and already what
`omni-cpu`'s `CNTPCT_EL0` is built on (D5 amendment 4). Writing a `cfg(unix)` arm that returned
`Unsupported` for one of those would be a **false claim in the other direction**: it would assert
that a clock this process can read cannot be read, and it would make the non-Windows bring-up
harder. The rule is *never claim a platform works*; `std` working on Linux is not a claim of ours.
What stays unclaimed is what has been **run**, which is Windows x86-64 only.

## Milestones

| | Status | Evidence |
|---|---|---|
| **M0** APK parsed, libraries extracted | **Reached** | All 11 arm64-v8a libraries extracted to a content-addressed 4 KB-aligned cache and mapped from it. Exactly one entry in the whole APK is directly mappable (a 1,447-byte icon) — no library is, which is why the cache exists |
| **M1** ELF loaded and relocated | **Reached** | All **568,806** relocations applied (568,272 APS2 + 534 `DT_JMPREL`) and **read back out of mapped memory**, RELRO sealed over exactly 5,205,568 bytes with a child process asserting the fault, 565 imports enumerated, 3,594 initializers collected |
| **M2** Real ARM64 Roblox code executes | **Reached** | Three real `libroblox.so` functions run with `init_array` deliberately **not** run. A base64 function returns **256 predicted values, one per byte**, predicted from RFC 4648 — and a reviewer hand-decoded all 26 words and wrote an independent interpreter to confirm. A stack-protected leaf proves D13 three ways including both failure paths |
| **M3** All 3,594 initializers | **Reached** | Every entry runs in order, asserted on the recorded `(index, address)` sequence and on state the initializers wrote. **91,581,468 guest instructions**; all 188 statically-reachable imports accounted for |
| **M4** `JNI_OnLoad` succeeds | **Reached** | Returns **`0x00010006`** on the real engine, both `JavaVM` slots exercised, **0** lookups nothing declares, and **19 of 21** of §8's scripted steps 7-12 return. D28 |
| **M5** `initializeNativeCode` succeeds | **Reached** | Returns a non-zero `NativeCode *`; §5.2's offsets read back one assertion each; the game thread ran `android_app_entry`; §8 row 14's cond-wait completed; **20 of 21** scripted downcalls; **0** JNI misses. D29 |
| M6-M8 | Not started | Vulkan, first frame, interactive |

## M3 progress — exact

Plan: `docs/plans/android-abi-plan.md`. Ledger: `.superpowers/sdd/android-abi-plan/progress.md`.

- **Task 1 (measure before building) — complete, reviewed, approved.** Commits `d44a516`..`672ddc2`.
- **Task 2 (the thunk boundary) — implemented, commits `04186ed`..`e245bfe`. REVIEWED 2026-09-20;
  three defects to fix before Task 3** (`.superpowers/sdd/android-abi-plan/task-2-review.md`).
  Spec ✅ per the implementer; durable record is **D18**. It built the region, AAPCS64 marshalling both
  ways, the variadic rules and a guest `va_list` walk, checked guest memory, the symbol table, and
  host→guest re-entry. **No symbol is implemented** — all 565 slots are `Unbound` and name themselves
  when called. Tests went 496 → 608, mutation **89 → 118** rows (29 new), all caught.
- **Task 3 (the bionic subset) — phases 1, 2 and 3a complete.** Phase 1, commits `fd3b6f4`..`b9cdf69`:
  the adapter, 86 of the 188 reachable imports bound (81 serviced, 5 refused by name), the `printf`
  family, 37 tests against real translated ARM64 code, mutation **137 → 153**. Durable record is
  **D20**. Phase 2, commits `9878e79`..`4545ef5`: the `dl*` family, the guest-memory group and all 18 data
  objects — **114 of the 188** now covered (96 thunk functions + 18 data objects), 30 new tests,
  mutation **155 → 175** (20 new, 20/20 caught after one MISS was closed). Durable record is
  **D21**. Phase 3a, commits `21b712d`..`a27bb00`: **`omni-platform` grew past `vm` and `fault` for
  the first time** (`clock`, `process`, `log`), plus `gmtime_r`'s calendar arithmetic in
  `omni-bionic` and the 23 clock / process-environment / logging symbols over them — **137 of the
  188** now covered (119 thunk functions + 18 data objects), 16 new adapter tests and 9 new
  `omni-bionic` ones, mutation **175 → 212** (37 new, 37/37 caught). Durable record is **D22**.
  Phase 3b: **`omni-platform` gained `fs`, a ROOTED filesystem** — every guest path
  resolves inside one host directory the embedding supplies, and an instance with no root refuses
  every path call by name — plus bionic's `FILE*` layer in `omni-bionic` over a trait, and the 29
  file-io symbols over both. **166 of the 188** now covered (148 thunk functions + 18 data
  objects), 55 new tests, mutation **213 → 239** (26 new, 26/26 caught). Durable record is **D23**.
  Phase 3c, this session: **the runtime can create a guest thread**, which is what makes the
  16 MiB-per-thread blocker real rather than theoretical. `pthread_create`, `join`, `detach` and
  `getschedparam`, plus the four signal symbols — one implemented exactly and three refused by
  name. **174 of the 188** now covered (156 thunk functions + 18 data objects), mutation
  **239 → 255** (16 new). Durable record is **D24**. **`omni-platform` did not have to grow for
  it**, which the plan predicted it would. Phases **3d and 3e, this session — the last import
  phase**: the eight network symbols and the six nothing else claimed. **All 188 reachable imports
  are now accounted for** — 143 answered, 22 refused by name, 3 reporting a guest termination, 18
  data objects and **2 deliberately absent**. `omni-platform` gained exactly one primitive
  (process CPU time) and **no socket seam at all**; mutation **255 → 284** (26 new, 26/26 caught).
  Durable record is **D25**.
- **Task 4 (all 3,594 initializers, the M3 gate) — not started, and no import work blocks it.**

### What Task 1 established

Full record in **D17**. The three things that matter for Task 2:

1. **Dispatch inside the run loop, per symbol** — ≈33 ns/call against 80-105 ns for exiting to Rust,
   a **3x** gap. Keep the exit path only for unresolved imports and anything needing a guest callback.
   Verified not to weaken D16's runaway-guest defence.
2. **The MXCSR guard is mandatory and lives in the dispatcher.** An inline handler otherwise runs with
   the guest's floating-point control word, so host `exp`/`log`/`powf`/`sincosf` would compute under
   guest rounding and denormal settings — silently. Costs 1.1 ns. Already implemented; do not remove
   it, and do not move it into individual handlers.
3. **Scope: 170 thunk functions + 18 `STT_OBJECT` data symbols**, from 188 statically-reachable
   imports of 565. A **lower bound** — 17,698 unresolvable indirect call sites, and a 2,670,684-byte
   region with no unwind info hides one initializer entry point worth exactly 67 of the 188.

## GLM work — review state

Four pieces of work arrived unreviewed; three from GLM 5.3 Flash. **All four are now reviewed.**
Nothing was accepted because it reported itself complete.

| Piece | State |
|---|---|
| `android-abi` M3 Task 2 (Claude) | **Reviewed.** F1/F2/F3 to fix before Task 3 — see `task-2-review.md` |
| `bionic-pure` (GLM) — 83 pure libc/libm functions | **Verified by mutation.** 11 rows, 11/11 caught. errno values were right but **untested** — that gap is closed. The bionic byte-difference compare convention is pinned, including the glibc over-correction |
| `bionic-threads` (GLM) — pthread/sync/TLS | **Complete for its scope, and reviewed.** See the coverage row below. One real defect found and fixed (`sem_post` consumed the waiter flag; **1.0104 s** stall measured); three timing flakes fixed; one plausible stub deleted |
| `os-surface-inventory.md` + `tools/os_surface.py` (GLM) | **Reviewed, and it reproduces.** `python tools/os_surface.py --check` gives the report's table exactly (sums to 565), census `FUNC=539 NOTYPE=3 OBJECT=23`, exit 0; its `data-object` count of 23 independently matches D17's figure from a different tool. One sentence corrected — §1.1 claimed both `__gcov_*` symbols were `process-env` while the tool leaves them `unclear`, which §1.3 and §5.2 already said |

**`omni-bionic`'s coverage, measured against the reachable import list.** Of the **51** thread /
synchronisation / TLS symbols the 3,594 initializers statically reach:

| | |
|---|---|
| Implemented here | **42** |
| Explicitly excluded | **1** — `pthread_sigmask`, which needs the guest's real signal state |
| Not here, and cannot be | **8** — `pthread_create`, `join`, `detach`, `exit`, `getattr_np`, and the three sched-param functions |

The eight are a coherent group, not a ragged edge: each needs host → guest re-entry or the operating
system, and the crate has zero dependencies and no OS access by design. **They belong to the
adapter.** `lib.rs` now states this so the layer is not read as half-finished.

Two counting notes, because this project has had four wrong numbers reach its record. A first pass
said 9 missing and 42 present and **both were wrong**: `pthread_cond_timedwait` *is* implemented —
it is `cond::wait_end` with `timeout: Some(..)`, and only the C symbol is unspelled — and
`pthread_sigmask` was counted present because a grep matched the comment that **excludes** it.

**Confirmed defects found in GLM's work so far**, all now fixed:

1. **`sem_post` consumed the waiter flag other waiters still needed.** `post` computed
   `(word & !WAITERS) + 1` under a comment reading "keep flag state" — the opposite of what it does;
   `wait`/`trywait` cleared it too. The first post consumed the flag, so a second post skipped its
   futex wake. **Measured: 1.0104 s** for a posted token to reach a blocked waiter. The suite could
   not see it — every `sem_wait` loops on a bounded slice, so a lost wake always *eventually* healed.
   Exercising, not detecting.
2. **The `rwlock` and `sem` handed the futex a placeholder `expected` of `0`**, which the word can
   never be at that point — the rwlock word is `WRITER` or a reader count, and a sem waiter has just
   set its waiter flag. `mutex` and `once` pass real words; only these four sites did not. The value
   check is the whole point of a futex: one that performs it would answer `WouldBlock` to every such
   waiter and the `continue` would busy-spin. Invisible because the crate's mock ignores `expected`
   by design **and the new adapter's futex parks unconditionally for that stated reason** — the
   placeholder had propagated into an accommodation. Fixed; rows `bionic-A11`/`B2`.
3. **`rwlock` healed a lost wake with 1,000 ms slices.** `sem`'s were cut to 50 ms when its
   waiter-flag bug was fixed; `rwlock`'s were missed. MEASURED at 19,200 acquisitions per version
   (8 threads × 400 × 6 runs): **44 stalls >100 ms (0.23%), worst 2.0169 s** against **2 (0.010%),
   worst 119.7 ms**. The fix *bounds* the stall; eliminating it needs a futex that honours
   `expected`, which item 2 enables and the adapter does not yet do.

   **A first measurement of this was wrong, recorded so the method is not repeated.** One run showed
   1.0115 s and a patched run 1.8 µs, which looked like a 500,000× win. It was not: the pristine
   build also measures 1.5–3.5 µs in most runs, because the stall is a rare race, and eight runs per
   version separated them not at all. Both regression tests are therefore **structural** — a
   recording futex, and a bound on the constant — because a latency test for a 0.23% race would be
   flaky in both directions.
4. **Three timing flakes**, two root causes: six wall-clock assertions with zero headroom (a 150 ms
   wait measured returning at 149.9569 ms), and a probabilistic `observed_max > 1` guard on a
   writer-preferring rwlock (~12% failure, 4 in 33 runs). Both fixed and mutation-verified.

**Open question on GLM's layout constants.** `layouts.rs` claims `PTHREAD_MUTEX_T = 40` on
"NDK header arithmetic + a VERIFIED 40-byte gap". No NDK is on this machine, so the header arithmetic
was not actually performed. The gap evidence is real but not by itself conclusive — the observed
`android_app` map has other unaccounted gaps. It is **corroborated independently** by `pthread_cond_t`
= 48 landing exactly on `msgread` at `+0x120` (`0xf0 + 48`), which is a second exact hit, so 40/48/56
are probably right. Note GLM's own test comment cites the struct as 256 bytes when the research file
says `operator new size 0x180 = 384`. `SEM_T = 4` is likely an under-declaration (bionic's LP64
`sem_t` carries `int __reserved[3]`), which is the **safe** direction: nothing writes more than 4
bytes. No write in the crate exceeds a declared size — `pthread_attr_init` and the rwlock initialisers
are the only bulk zeroes, both at 56 bytes, and 56 is confirmed by field-by-field arithmetic.

## The straddling-access defect — FIXED (`97e12b7`)

**Any guest access that straddles a commit-granule boundary in a lazily-committed mapping is refused
as `NotMapped`, even when both granules are committed and both belong to the same mapping.**

MEASURED on a 1 MiB `CommitPolicy::Lazy` mapping with both granules committed:

| access | result |
|---|---|
| 8 bytes wholly inside granule 0 | `Ok` |
| 16 bytes straddling the boundary | `Err(BadPointer { refusal: NotMapped })` |

Cause: omni-mem's entry map splits the entry at each granule it commits and **never coalesces
adjacent committed granules** (`region_at` reports 65,536 after committing two neighbours, not
131,072), and `admits_region` refuses any access whose end passes the entry's end. So this is
`admit`/`admits_region`, not a string-walk problem: it applies to `read_u64`, `read_bytes`,
`write_bytes` and every other boundary access.

Reachable wherever the guest heap lives — guest `mmap` through the demand pager, committing granule
by granule — for any string or struct that happens to cross a 64 KiB boundary. It would surface as
rare, position-dependent failures deep into Task 3's 170 handlers and Task 4's 3,594 initializers.

Invisible to every existing suite because **every test mapping is `CommitPolicy::Eager`**, and an
eager mapping is one entry that is never split. Any fix must add a lazy fixture.

This was found while investigating Task 2's F2, which claimed the opposite — that `cstr` could scan
*out* of the committed granule. That claim is **disproved**: `admit().end` is the split entry's end,
so the walk was already bounded by committed memory. Do not resurrect it.

**Fixed in `97e12b7`.** `admit` now walks the entries an access touches, requiring each to be
contiguous, to belong to the **same mapping**, and to permit the access. `admits_region` is untouched
and is still the single copy of rules 1-3 — it is called once per entry instead of once per access.
`Admitted.end` is the end of the last entry needed, which for an access inside one entry is exactly
what it was, and `fully_committed` is the AND over those entries, so `omni-cpu`'s fetch cache keeps
its contract. Rows `access-A1` (restores the defect) and `access-B1` (the over-correction: the walk
running out of its mapping), both caught. The over-correction row is guarded by a test that places
two mappings at adjacent addresses: contiguous addresses are not the same mapping.

## Task 3's real shape — measured before dispatching it

Task 3 is "the bionic subset", scoped at **170 thunk functions + 18 data objects** from the 188
statically-reachable imports. Measuring what already exists changes how it should be split.

**CORRECTED — it is 79, not 88.** The method was stated but not run: 88 is the naive any-mention
grep (89) minus the one known excluding comment. Running the stated method gives 82, and five of
those are ordinary English words matching unrelated prose (`abort`, `access`, `clock`, `read`,
`time`), leaving 77; two more are implemented under a name that does not spell the C symbol
(`strerror`, `__vsnprintf_chk`), giving **79**. The error is nine symbols and it makes the remaining
work look smaller. Full table and method in **D20**.

The 100 do **not** form one job. They split by what they depend on:

| Group | Needs | Blocked? |
|---|---|---|
| ~~Wiring the 79 that exist onto the boundary~~ | **DONE** — phase 1, D20 | — |
| ~~`printf` family~~ | **DONE** — three implemented, five refused by name (D20) | — |
| ~~Guest memory (`mmap`, `munmap`, `mprotect`, `madvise`, `mlock`)~~ | **DONE** — phase 2, D21 | — |
| ~~`dl*` (`dl_iterate_phdr`, `dlopen`, `dlsym`, `dlclose`, `dlerror`)~~ | **DONE** — phase 2, D21 | — |
| ~~The **18** data symbols~~ — not ~19, and the list that stood here named two symbols the initializers never reach | **DONE** — phase 2, D21 | — |
| ~~Clocks, process info, logging~~ | **DONE** — phase 3a, D22. `omni-platform` gained `clock`, `process` and `log` | — |
| ~~Files and directories~~ | **DONE** — phase 3b, D23. `omni-platform` gained `fs`; the confinement policy is in `fs::path` | — |
| ~~Thread lifecycle and signals~~ | **DONE** — phase 3c, D24. **No `omni-platform` surface was needed**: `std::thread`, `omni-mem` and `omni-cpu` between them have all of it | — |
| ~~Sockets and polling~~ | **DONE** — phases 3d/3e, D25, and it needed **no** `omni-platform` surface either: the descriptor space `poll` and `select` observe is closed, and `socket` and `eventfd` refuse by name | — |
| ~~The six nothing else claimed~~ | **DONE** — phase 3e, D25. `omni-platform` gained one primitive, process CPU time, for `clock()` | — |

**That blocker is cleared, and the last three phases each cleared it by needing less than
predicted.** `omni-platform` was `vm` and `fault` and nothing else until phase 3a, which added
`clock`, `process` and `log` (D22); phase 3b added `fs` (D23); phase 3c needed **nothing** (D24);
phases 3d/3e added **one primitive** and no socket seam at all (D25). ARCHITECTURE §2 describes
the crate as covering "virtual memory, threads, files, dynamic loading, clocks, windowing", and
**four** of those six are now real rather than aspirational — threads and windowing are not, and
threads turned out not to need the crate. The portability invariant itself is intact throughout
(verified again this session: every `cfg(target_os)` mention outside `omni-platform` is a doc
comment stating the rule or a Windows-only test gate, not an escape from it).

The five-target rule still applies to whatever grows the crate next: add the **Linux and macOS
signatures as honest `unsupported` returns at the same time, naming the intended POSIX call**, and
do not write speculative `mmap`/`open` bodies for those targets. And D22's other half, which three
phases running have now been the case for: a primitive that calls **no** OS API must not be given
a fabricated `unsupported` arm either.

**Suggested order**, unblocked work first so the seam is proved end to end before the OS surface grows:

1. ~~The adapter skeleton + the ones that already exist + the `printf` family.~~ **Done** (D20): 86
   of the 188 bound, 81 serviced and 5 refused by name; 37 new tests; 16 new mutation rows, 16/16
   caught. The seam is proved end to end against real translated ARM64 code.
2. ~~The data symbols, `dl*`, and the guest-memory group.~~ **Done** (D21): 114 of the 188
   covered, 30 new tests, 20 new mutation rows, 20/20 caught.
3. ~~Extend `omni-platform` with clocks, process information and a log sink, then bind them.~~
   **Done** (D22): 137 of the 188 covered, 25 new tests, 37 new mutation rows, 37/37 caught.
4. ~~Files and directories.~~ **Done** (D23): 166 of the 188 covered, 55 new tests, 26 new
   mutation rows, 26/26 caught.
5. ~~Thread lifecycle and signals.~~ **Done** (D24): 174 of the 188 covered, 16 new mutation
   rows. It needed no new `omni-platform` surface at all.
6. ~~Sockets and polling, then the six the plan's `3e` row collects.~~ **Done** (D25): **all 188
   accounted for**, 41 new tests, 26 new mutation rows, 26/26 caught. One new platform primitive
   and no socket seam.

## M4 is reached — what it left for M5 (§8 step 13, `initializeNativeCode`)

M4 delivered `jni-surface.md` §8 steps 6 through 12 (D28). The gate is
`cargo test -p omni-android --release --test jni_startup`, which loads the real library, runs all
3,594 initializers, calls `JNI_OnLoad`, and drives the scripted Java-side sequence.

**What is VERIFIED, on the real `libroblox.so`, n = 1 run each:**

| | |
|---|---|
| `JNI_OnLoad` at `base + 0x2173ff4` | returns **`0x00010006`** |
| the two `JavaVM` slots | `GetEnv` 11, `AttachCurrentThread` 1. The thread began **detached**, so the engine's own scoped-attach helper took the `JNI_EDETACHED` branch and attached with the name `"Main"` out of `JavaVMAttachArgs` |
| step 6a/6b | 23 `FindClass`, 46 `GetStaticMethodID`, 8 `GetFieldID`, 20 `NewGlobalRef`, 18 `ExceptionCheck`, **0 lookups nothing declares** |
| §8 steps 7-12 | **19 of 21** downcalls return, step 12 included |
| whole run | 22 upcalls into Java, **113 distinct imported symbols** called |
| the JNI census over the whole run | 45 `GetStringUTFChars` each paired with a `ReleaseStringUTFChars`, **0 buffers left pinned** |

### The two scripted downcalls that do not return, exactly

1. **`nativeInitFastLog` needs `strftime`**, and `omni-bionic` does not have it. Not a binding gap:
   there is no implementation to bind. It is a full C library function with a format language, and
   a partial one is the plausible-stub shape — an unimplemented specifier produces wrong *text*,
   which nothing downstream can tell from right text.
2. **`nativeSetPlatformHeadersWithIdfa` hands `strchr` a pointer to memory that is mapped nowhere.**
   MEASURED: `region_at` on it returns `None`, and it is in neither the JNI arena nor the pinned
   pool, so it is not a pointer this layer handed out. Whether it is a guest-heap pointer the
   engine's own allocator released, or something this layer got wrong further back, is **not
   established**. The gate prints the pointer and what is mapped there on every failed step, which
   is the instrumentation the next person needs.

### What step 13 needs that does not exist yet

§8 rows 13, 13a and 14 are explicit, and none of this is built:

* **`ALooper_forThread` / `ALooper_acquire` / `ALooper_addFd` / `ALooper_prepare`.** §8.1's fourth
  failure mode is that `ALooper_forThread()` returning null makes `initializeNativeCode` return
  **`0`** and Java-side startup fail *silently*. A looper has to exist on the calling thread
  **before** the call, and a second one on the game thread the glue spawns.
* **`AAssetManager_fromJava`**, and a Java `AssetManager` object for it to accept.
  `AConfiguration_new` / `_fromAssetManager` / `_getLanguage` / `_getCountry` follow on the game
  thread.
* **`ANativeWindow`** and a Java `Surface` that `ANativeWindow_fromSurface` accepts — that is step
  17, but the type has to exist before step 13's glue can store one.
* **`pipe` and `fcntl(F_SETFL, O_NONBLOCK)`**, twice. Neither is bound; neither is among the 188.
* **`__system_property_get("ro.build.version.sdk")`** is bound and answers, but the host has to
  *set* the property or the SDK version field is empty.
* **The glue blocks on `pthread_cond_wait` until the game thread signals `app->running`** (§8 row
  14). Guest threads exist (D24) and run in short budget windows, so this is expected to work —
  but §8.1's fifth failure mode is that a deadlock here is indistinguishable from a hang.
  **Instrument it**: `Boundary::last_call` is the one thing that identifies a guest parked inside
  a handler, and the M3 gate's `OMNI_INIT_WATCHDOG` is the pattern.

### What M4 leaves in place for it

* **`RegisterNatives` is implemented.** `GameActivity_register` binds 24 natives from a
  `JNINativeMethod[24]` at `.data.rel.ro 0x062dc1c8` — 24 bytes per entry, which Section F
  VERIFIES against the real table. Every binding lands in `Jni::registrations`, and a method the
  registry does not declare is **recorded, not refused**, with its function pointer kept.
* **The Tier 0 classes step 13 aborts without are declared**: `GameActivity`'s five methods,
  `Insets`' four fields, `WindowInsetsCompat$Type`'s nine static masks (distinct bits, asserted),
  `Configuration`'s 18 fields plus `getLocales()`, `ActivityThread`, `ClassLoader`, `String`.
  `getWindowInsets` and `getWaterfallInsets` return a real `Insets` instance.
* **`Jni::misses` is the measurement to read first** if step 13 aborts. A Tier 0 miss is a
  `CHECK_NOT_NULL` three thousand instructions before the abort, and the list has it by name.
* **A contradiction with §3.1, unresolved.** §3.1 calls `Configuration`'s fields "**18 int
  fields**" and lists `fontScale` among them. `fontScale` is a `public float` on every Android
  release, and Section D shows the analysis could not resolve **any** of these descriptors
  (`<unresolved>`), so "18 int" is the analyst's summary and not a measurement. It is declared `F`
  here. If the engine asks for it as `I` the lookup misses **and the miss is recorded with the
  descriptor it asked for**, which is the measurement that settles it.
* **`tools/gen_dex_surface.py` regenerates `src/jni/surface.rs`** and reproduces the committed file
  byte for byte. Re-run it if the APK changes; it also answers "what members does class X have"
  for any dex class, which is how `DeviceParams`, `DeviceStaticParams`, `PlatformParams` and
  `InitParams` got their real field lists.

### Two host-side facts the script depends on, which a new harness must reproduce

* **The confinement root needs an Android directory tree.** The engine canonicalises
  `/data/data/com.roblox.client/{cache,files,shared_prefs}`, `/data/app/com.roblox.client`,
  `/data/app/android` and `/storage/emulated/0/Android/data/com.roblox.client`, and throws
  `boost::filesystem::canonical: No such file or directory` on any that is absent. The gate's
  `Scratch::DIRECTORIES` is the list.
* **`nativeSetAssetPath` takes a directory, not the apk file.** MEASURED: passing
  `/data/app/com.roblox.client/base.apk` made the engine throw `'…/base.apk' is not a directory`.
  It then canonicalises `dirname(assetPath)/android` as well.

### One thing that happened once and has not reproduced

An early run of the gate exited with `STATUS_ACCESS_VIOLATION` **after both tests reported `ok`**,
on a run in which four scripted steps failed. It has not reproduced since the step count rose to
19, and nothing was changed that would explain it. It is recorded rather than claimed fixed:
teardown of a guest with live guest threads is the obvious suspect, and the M3 gate's note about
the instance never being released is the other end of the same thread.

## M3 Task 4, kept for the record — it is done, and M4 has been done on top of it

**M3 Task 4: run all 3,594 initializers — the M3 gate.** Every import phase of Task 3 is complete
and committed (D20, D21, D22, D23, D24, D25), and **all 188 statically-reachable imports are
accounted for**:

| outcome | count |
|---|---:|
| answered | **143** |
| refused by name | **22** |
| a guest termination, reported (`abort`, `__stack_chk_fail`, `_exit`) | **3** |
| `STT_OBJECT` data objects | **18** |
| deliberately **absent** (`__gcov_dump`, `__gcov_flush`) | **2** |

That split is asserted by **calling** each symbol rather than by counting a table
(`the_final_split_of_the_reachable_set_is_what_the_record_claims`), and the remainder — reachable
minus bound minus data — is asserted as a **set difference** that must equal exactly the two
absent symbols. A substitution in it cannot pass.

**`__gcov_dump` and `__gcov_flush` are the one place this layer's answer is "nothing".** They are
`WEAK NOTYPE`, no Android libc supplies them, and `libroblox.so` tests each GOT slot for null
before calling — VERIFIED by decoding the single site that references them, where the call a stub
would have answered is followed **four bytes later by `BL abort`**. `BoundaryBuilder::declare_absent`
leaves a *weak* reference unresolved and a *strong* one named; D25 has the instruction listing.

### What Task 4 needs from this layer that does not exist yet

Read this before starting: these are the gaps the last import phase could see, in the order they
are likely to matter.

1. **The `AT_HWCAP` decision is still open and `getauxval(AT_HWCAP)` refuses under it.** An
   instance starts `HwcapPolicy::Undecided` and the refusal carries both measurements. If the
   3,594 initializers read `AT_HWCAP` — and a C++ runtime doing atomics feature detection is
   exactly the shape that does — **Task 4 stops there on the first initializer that asks**, and
   the fix is a decision, not code. Both arms are measured (advertise LSE → 53 hard interpreter
   halts; decline → 106 fallback arms into a global spinlock that anti-scales 21x). Decide it
   before the run rather than during it.
2. **An instance needs a filesystem root and a thread host before it can do anything.** Neither
   has a default, deliberately (D23, D24). A Task 4 harness that forgets either gets a refusal
   naming the method that supplies it, which is the intended failure but is worth knowing in
   advance.
3. **`fprintf` and `vfprintf` are still refusals and are still one binding away.** Both halves
   exist — `format::render` and phase 3b's `stdio` — and the engine logging through `fprintf(stderr,
   ..)` during static initialisation is entirely plausible. This is the single most likely refusal
   to stop the run, and it is the cheapest to close.
4. **`sysconf` refuses every name, including the two this layer could answer.** Task 4 will report
   which `_SC_*` numbers the engine actually passes, which is the measurement that turns this into
   four lines (D22).
5. **`setjmp` is not bound**, so a guest that reaches it through the address-taken edge the
   reachability scan found gets `Unbound`. `longjmp` refuses for the same reason among others.
   Neither is in the 188's direct-call closure, so neither is expected — but they are the pair
   most likely to appear from an indirect call the static scan could not follow.
6. **A guest thread does not run its `pthread_key` destructors when it exits** (D24). It does not
   affect the gate — the initializers run on a thread that does not exit — and it is a leak per
   thread exit afterwards.
7. **`poll`'s always-ready rule is correct only while the descriptor space stays closed** (D25).
   If Task 4 or a later milestone binds `socket` or `eventfd` for real,
   `the_descriptor_space_poll_answers_over_is_closed` fails, and that is the signal to give `poll`
   a real readiness source rather than to update the test.
8. **The boundary can call guest code only from inside a `ReentrantCall`.** Task 4 has to run
   3,594 guest functions *from the host*, which is the same capability `pthread_create` needed and
   got as `ReentrantCall::boundary()`. Check whether `Boundary::run` on its own is enough for an
   initializer array, or whether the gate needs a host-initiated entry point that does not start
   from a thunk crossing — D24 records that this API does not exist for `pthread_key` destructors,
   and the initializer run is the other caller that would want it.

The three ASSUMED guest layouts (`FILE` = 152, `dl_phdr_info` = 64, `struct tm` = 56) and the
three phase-3b ones (`stat` = 128, `statvfs` = 112, `dirent` = 280) are unchanged; **there is
still no NDK on this machine**. `sizeof(sigset_t) = 8` (D24) and `sizeof(struct addrinfo) = 48`
(D25, recorded for whoever binds `getaddrinfo`) join them.

**Two things about the last three phases are worth carrying into any estimate.** Each predicted
more `omni-platform` surface than it needed — files, threads and now sockets — so the OS-surface
half of a plan row has been wrong three times in the same direction. And the most valuable finding
of this phase came from **decoding the guest's own instructions** rather than from reading a
header: it is what turned `__gcov_dump` from "leave it Unbound" into "resolve it to nothing", and
it is the technique to reach for when a symbol's right answer depends on what the guest does with
it.

### What phases 2, 3a and 3b leave for whoever picks this up:

- **`sizeof(FILE) = 152` is still derived, not verified** (`bionic::data::FILE_BYTES`). There is
  still no NDK on this machine. **Phase 3b did not need one and did not discharge the obligation;
  it narrowed it** (D23): a `FILE *` is a *key* into a host-side stream table, the bytes at one are
  written once to zero and never read, so a wrong number cannot produce a wrong *answer* — only a
  wrong *address*, which is not in the table and refuses by name. The obligation now belongs to
  anything that makes a `FILE` field observable: `ferror`, `clearerr`, `fseek` and `setvbuf` are
  the four that would, and **none is among the 188**. `sizeof(struct dl_phdr_info) = 64` has the
  same provenance and a stronger safety argument, written out in D21.
- **Three more ASSUMED guest layouts join them**, all from phase 3b and all *transparent* to the
  guest, which is the opposite case from `FILE` and is why each is asserted from real guest code
  against a value the test chose: `struct stat` **128** bytes (Linux UAPI
  `asm-generic/stat.h`), `struct statvfs` **112** and `struct dirent` **280** (bionic LP64
  headers). `statvfs` has the strongest argument of the three — every field is eight bytes on
  LP64, so the layout is forced once the order is right.
- **`mkdir`'s mode is read and not applied** on Windows, which has no POSIX permission bits. Stated
  in the open rather than refused, because refusing every `mkdir` stops the engine creating any
  directory and because the confinement boundary is the **root**, not a directory inside it.
- **`fprintf` and `vfprintf` are still refusals and are still one binding away.** Both halves
  exist: `format::render` and phase 3b's `stdio`. Phase 3b deliberately left the binding out of
  its scope of 29 and corrected the refusal text, which used to say `omni-platform` had no file
  surface. Phases 3d/3e did not bind it either, because it is not among the fourteen they owned --
  but it is item 3 in the list above, and it is the refusal most likely to stop Task 4.
- ~~**`ReentrantCall::invalidate_code` reaches one context.**~~ **Closed as far as it can be, in
  phase 3c (D24).** There is a registry of live contexts now: an `munmap` or `mprotect` applies to
  the calling context synchronously and is **queued for every other live one**, which applies it at
  the top of its next run segment. What is left is stated rather than implied — a guest thread that
  neither crosses the exit path nor returns from `cpu.run` keeps a stale translation until it does,
  which for a created guest thread is bounded by one step window and under `RunLimit::Unlimited` is
  not bounded at all. `Boundary::code_invalidations` is a detector for it, not a watch.
- **A guest thread does not run its `pthread_key` destructors when it exits**, and bionic does.
  Closing it needs a host → guest call from *outside* a thunk crossing, which is a boundary API
  that does not exist. It does not affect M3's gate — the 3,594 initializers run on a thread that
  does not exit, and `pthread_exit` is not among the 188 — but a guest that frees per-thread state
  from a key destructor leaks it once per thread exit (D24).
- **`mlock`, `MADV_DONTNEED`, `MAP_FIXED` and file-backed `mmap` are refusals, not gaps.** Each names
  the guarantee it could not meet. D21 records why `-1`/`ENOMEM` was rejected for `mlock` in
  particular: it is the most believable wrong answer in the group.
- The two open items phase 1 left are **unchanged**: `omni-bionic`'s `rwlock` still passes a
  placeholder `expected` of `0` to `futex.wait`, so the adapter's futex still ignores `expected`;
  and `strerror`'s message table still holds ten errno codes, so `ETIMEDOUT` and most of `errno.rs`
  answer `Unknown error`.
- **`sysconf` refuses every name, including the two this layer could answer** (the page size and the
  processor count), because bionic's `_SC_*` numbering is its own and there is no NDK here to check
  it against. D22 has the argument. Four confirmed constants turn it into four lines, and running
  task 4 will report which values the engine actually passes.
- **`gai_strerror`'s fifteen messages are ASSUMED** from bionic's `ai_errlist`, transcribed with
  no NDK to check them against (D25). A wrong message is a wrong *diagnostic* and nothing branches
  on the text, which is the whole of why it was acceptable to write them down.
- **`sizeof(struct addrinfo) = 48` is recorded and unused**, for whoever binds `getaddrinfo`:
  bionic orders `ai_canonname` before `ai_addr` where glibc does the reverse, so a glibc-derived
  layout puts the canonical name where the address belongs (D25).
- **`sizeof(struct tm) = 56` joins `FILE_BYTES` and `dl_phdr_info`** as derived-not-verified, stated
  in `omni_bionic::time::TM_BYTES`. It has the strongest safety argument of the three: every field
  is an `int` except the last two, so the layout is forced once the field order is right.
- **`omni-platform::clock::sleep` is bound by Windows' ~15.6 ms timer tick**, so a guest
  `usleep(1000)` sleeps for roughly 15 ms. Recorded, not worked around; raising it costs either a
  process-wide `timeBeginPeriod` or a high-resolution waitable timer per sleeping thread, and
  neither has been measured.

Carry into the dispatch as before: D17's in-loop dispatch decision, that the dispatcher-side MXCSR
guard already exists and **must not be moved or removed**, that an unbound symbol must fail with a
typed error naming the symbol and guest address, and **F9** — anything that reaches `GuestSpace` or
calls guest code is `bind_reentrant`, and nothing in the types says so.

After phase 3 comes M3's gate: all 3,594 static initializers complete, **verified by reading back
state they actually wrote**, not by a counter reaching 3,594.

## What Task 2's reviewer was asked to check (kept for the record)

The implementer reported these against its own work; they were the highest-value things to verify.

- **Two defects it found late, both now pinned.** The driver handed the caller's `RunLimit` to *every*
  exit-path crossing, so a guest crossing N times got **N times its allowance** — found because two
  mutation rows **hung instead of failing**. The fix then mis-reported any terminal exit landing on the
  budget's last instruction as `StepLimitReached`, **including `MemoryFault`**, which is not resumable
  where a step limit is — so a caller would have resumed a faulting guest. Check both fixes are complete.
- **13 of the reachable 188 are variadic** — 9 true (`fprintf`, `fscanf`, `snprintf`, `sscanf`,
  `syslog`, `open`, `prctl`, `syscall`, `__android_log_print`) plus 4 `va_list` forms. `printf` is
  **not** reachable and `__open_2` is **not** variadic. Also: **`mallinfo` returns in `X8`**, which the
  brief's "returns in X0/X1/V0" omitted — check nothing else needs an indirect result register.
- **An inline handler holding no CPU is a *type* property, not a rule**, which is how re-entrancy is
  prevented. Verify a handler genuinely cannot obtain two live mutable CPU references.
- **One guard is labelled a watch, not a detector** — the caller-saved half of the callback state
  restore has no test that can distinguish it. That labelling is correct practice here; confirm the
  label rather than asking for a test that cannot exist.
- The **ARM64-host path is expressible but untested and not claimed to work.**

Round trip re-measured after the mechanism changed: **26.7-31.0 ns against 81-101 ns**
(n=31/cell/process, 45 processes). D17's 3x holds; its bimodality open question is untouched.

## The adapter review — findings still open

Phases 1/2/3a/3b had their **numbers** verified (counts, mutation totals, derivations all reproduce)
but had never had an adversarial code read. That review ran 2026-09-21; full text in
`.superpowers/sdd/android-abi-plan/task-3-review.md` (**git-ignored scratch — this table is the
durable copy**).

Its shape is worth knowing: **the implementations were largely sound; the evidence for them was
weaker than the records claimed.** Fourteen statements in D20-D23 were false or overstated.

**Closed:**

| | |
|---|---|
| **H1** — confinement rules 5 and 6 had never executed here | Fixed `b75ff45`. The test early-returned because this host cannot create a symlink (**MEASURED: `WinError 1314`**). It now falls back to a directory junction and **fails loudly rather than skipping**; rows `fs-A7`/`fs-A8` added, both caught |
| **H2** — the `gmtime` regression test asserted nothing for six of its ten inputs | Fixed `b75ff45`. Its `Err` arm was empty. No assertion *could* catch the wrap — in release it is correct by accident — so the detector is row `time-A7`, run in debug |
| **M2** — the arena test asserted its own definition | Fixed by phase 3c: replaced with one that walks the bases out of the accessors, verified as a detector |

**Open, none a blocker for Task 4:**

| | |
|---|---|
| **M1** | `read`/`pread`/`__write_chk` consume from the descriptor **before** validating the guest destination, so a half-mapped buffer keeps some bytes behind a reported failure. D22 got this right for `arc4random_buf` and wrote the rule down; phase 3b did not carry it across |
| **M3** | The log ring bounds **record count, not bytes**. 256 records × a ~1 MiB message ≈ 0.8 GiB per instance with `log_dropped()` still reporting 0 |
| **M4** | `__android_log_print` refuses on at least **eight** guest-reachable paths, six undiscussed — a >1 MiB `%s` aborts the whole run, the exact outcome its module header says it exists to prevent. Real `liblog` truncates |
| **M5** | A `fopen` mode string over 16 bytes is **silently truncated** where the code's own doc says `EINVAL`, so `"rbbbbbbbbbbbbbb+"` loses its `+` and yields a read-only stream |
| **M6** | `WINDOWS_DEVICES` omits `COM0`/`LPT0` and the superscript `COM¹/²/³` forms. **No exploit constructible today** — the canonicalised root is a verbatim `\?\` path — but `path.rs` claims the rules are host-independent, and this one now is not |
| **M7** | `process::cpu_count` answers `1` on failure, the substitute-an-answer pattern `random_bytes` forbids eleven lines below. Unreachable from any guest path today |
| **L1-L7, N1-N3** | `__system_property_get`'s 0 indistinguishable from unconfigured; a hostile-test predicate that admits any fabricated success; `write_tm`'s atomicity documented as asserted in a suite that cannot reach it; `gmtime_r`/`nanosleep` errnos set but never asserted; `fgets` one host `read(2)` per byte; `openlog`'s facility dropped; `fclose` does not flush `stdout` |

**Five of the fourteen false claims are corrected** (`branch-free` → loop-free in two places, the
`linux.rs`/`macos.rs` "differ in shape" that are identical re-exports, the duplicated `EOVERFLOW`,
and `data.rs`'s "none is implemented" naming four functions phase 3b implemented). The rest are
recorded in the review file.

**The lesson worth carrying.** Both High findings were *tests written as the fix for a previously
found defect* — the place nobody re-audits, because a test that closed a bug is assumed to work.
And one flaky test was found **inflating a `wcslen` mutation's catch list**, which means a flake does
not only cost a red run: it can make a row look detected when nothing detected it.

## Blockers and risks

| Risk | State |
|---|---|
| **Test APK is cheat-injected** | `Roblox-2.738.1397.apk` is signed by "Gloop", not Roblox, with an injected Luau executor in `libzstd-jni`. `libroblox.so` itself is stock. **A stock Play-signed APK has been requested from the user and never supplied.** Design is from the stock engine only (D6). Still the correct thing to ask for |
| **16 MiB per guest thread** | A fixed array dynarmic allocates and *writes* even when its feature is disabled — and D16 says to run with it disabled. Fork patch written up in `crates/dynarmic-sys/patches/README.md`, **still not applied**: D5 pins the vendored tree byte-for-byte unmodified, so applying it is a decision to record rather than a side effect of needing the memory. ~512 MiB at 32 threads. **Live as of phase 3c**, which is the first phase in which a guest can create a thread at all — and measured through that path: **24.76-24.84 MiB per concurrent guest thread**, of which **98.6% comes back when the thread exits** (n = 4 runs of 8 threads, D24). So the constraint is on *concurrent* threads, not on threads ever created, and `ThreadHost::with_limit` is what an embedding bounds it with |
| **W^X does not hold for the code cache** | Recorded as an explicit exception under D12. Enabling dynarmic's no-execute option makes **upstream's own suite segfault**. Under identity mapping the cache is guest-writable in principle, gated only by ASLR |
| **`AT_HWCAP` is an open decision for M3** | Roblox's atomics are one population behind a single flag we own. Advertise LSE → 53 hard interpreter halts; decline → 106 fallback arms into a global spinlock that anti-scales 21x. Both arms measured; **the choice is still unmade, and phase 3a made that structural**: `bionic::HwcapPolicy` has no `Default`, an instance starts `Undecided`, and under it `getauxval(AT_HWCAP)` *refuses by name with both measurements in the message*. A host that has decided calls `Bionic::set_hwcap_policy`. Defaulting to "decline" was considered and rejected — it reads as the safe arm and is the one that costs 21x, and nothing would have recorded that a choice had been made (D22). Mutation row `procenv-A1` injects exactly that |
| No Vulkan validation layers installed | Will matter from M6. A real use-after-free once crashed the driver with no diagnostic |
| Linux/macOS untested | Deliberately unclaimed everywhere. Non-Windows paths return typed "unsupported" errors naming their intended POSIX call |

## Decisions not to re-litigate without new evidence

All in `docs/DECISIONS.md` with cost-if-wrong recorded. The load-bearing ones:

- **D4 — guest VA == host VA (identity mapping).** Verified; `fastmem_pointer=0` with 64 address bits
  emits one instruction. Losing it costs **30-49x**. Asserted at startup because the 36-bit default
  degrades *silently while still producing correct results*.
- **D5 — dynarmic adopted as a pinned fork** (`yuzu-mirror/dynarmic@9d45823`; upstream 404s). Vendored
  tree verified byte-for-byte unmodified; upstream's 202,200 assertions pass on it.
- **D7 — no JVM, no ART, no dex interpreter.** Every mechanism that would force dex execution was
  searched for and is absent.
- **D10/D11 — memory model.** `MEM_DECOMMIT` is the *only* primitive that returns commit charge;
  `MEM_RESET` frees nothing. Reservation is free. Libraries map from a shared 4 KB-aligned cache.
- **D12 — JIT memory is dual-mapped**, with the code-cache exception noted above.
- **D13 — `TPIDR_EL0` must be programmed per guest thread before any guest code runs.**
- **D16 — runaway guests.** Which flag set stops which guest shape, and that a watchdog must be built
  from **short budget windows**, not a cross-thread halt.
- **D17 — the thunk boundary.** In-loop dispatch, the dispatcher-side MXCSR guard, the scope.

## Measured figures still valid

Every figure below is in `STATUS.md` or `DECISIONS.md` with its sample size. **The rule is: a measured
quantity appears once, with its n, and everything else links to it** — three drifted duplicates have
already appeared in this project's own documents, every one added by summarising rather than measuring.

- `libroblox.so` loaded: **~16.7 MiB commit per instance**, **~104 MiB file-backed and shared**; peak
  equals steady, so windowed relocation produces no spike. Load 11.8 ms.
- **Three concurrent instances: ~50 MiB total**, ~312 MiB mapped. Each instance's *marginal* cost is
  asserted separately, so sharing cannot be first-instance-only nor decay with count.
- Guest address space: a 4 GiB reservation costs **0.000 MiB** of commit. Grow to 1 GiB written
  through, then release → back to 0.
- Commit granule **64 KiB** (4 KiB measured *worse* than the VEH fault D10 rejected).
- JIT arena: dual-mapped, 162 ns/cycle against 2259 for protection flipping.
- Thunk: **≈33 ns/call** in-loop, 80-105 exiting to Rust, 3x.
- Cold translation on real code **0.516 Mguest-insn/s** (n=11); warm **156.7** (n=31).
- Per guest thread **24.76-24.84 MiB** (n = 4 runs of 8 threads, measured through a real guest
  `pthread_create` in phase 3c), and **98.6% of it comes back when the thread exits** — the
  residual is 0.35-0.40 MiB per thread. Shrinking the code cache does **not** help: 24.56, 34.61
  and 34.61 MiB/thread at 8, 32 and 128 MiB of cache (n=8 contexts, one layer down).
- Roblox shape: 2.27% indirect, **4.30 instructions per basic block**, **128 exclusive-monitor sites
  against 53 LSE** (29.3% of atomic RMW). All static mixes, used as proxies.
- Texture formats: **neither ETC2 nor ASTC** on the dev GPU; BC1/BC3/BC7 yes. Runtime transcoding is
  mandatory from M6 — and **the format it must handle is ETC1, not ETC2 and not ASTC**. Measured
  before the decoder was written (`tools/texture_census.py --check`, D27,
  `docs/research/texture-formats.md`): **38** compressed containers in the APK, every one
  `GL_ETC1_RGB8_OES`; **813,802** 4x4 blocks walked and **zero** in any ETC2-only mode; **zero**
  bytes of ASTC, ETC2, EAC, PVRTC or KTX2 anywhere; everything else block-compressed is
  DXT1/DXT3/DXT5 or `R8_UNORM`, which the host samples natively. The engine negotiates streamed
  assets in `dxt`/`etc`/`etc2`/`uncompressed` and `"astc"` does not occur in `libroblox.so` at all.
  `crates/omni-texture` decodes ETC1 to RGBA8 at **39.0 ns/block** — the whole baked set in a
  **median 31.72 ms** (n=11 runs, release, single-threaded), producing 52,079,224 B.

## Corrected or withdrawn — do not resurrect

Each of these was recorded, then disproved by someone other than its author. Several were the
*convenient* version.

| Withdrawn | Replaced by |
|---|---|
| `DT_ANDROID_RELA` = `0x6000000F` | `0x60000011`. The wrong tag finds **zero** packed relocations |
| The 22 symbolic relocations are `ABS32` | **`ABS64`** (257). Writing 4 bytes where 8 are needed |
| `p_align` is 4 KiB | **16 KiB** (`0x4000`) on every `PT_LOAD` |
| 15,646 exclusive-monitor sites, so the LSE gap is moot | **128 vs 53**; the miscount folded in acquire/release. LSE is 28-29% of RMW sites and the gap is **live**. Two successive miscounts both erred *toward* making the risk look smaller |
| D15's invisibility gap is 164x | 128x asserted, 315-4,096x observed (n=45). The original figure was mostly the test's own heap |
| `size/512` page tables applies to section views | **Retracted as unverified**, not softened. D10 measured fully-touched anonymous commit |
| D16's 7x is an upper bound because real code has longer blocks | Roblox's blocks are **4.30 instructions** — the benchmark's length. 7x is the expected cost |
| Losing identity mapping costs 13.2x | **30-49x** (n=31). 13.2x measured a bare stub and is a floor |
| 88 of the 188 reachable imports are implemented in `omni-bionic` | **79.** 88 was the naive any-mention grep minus one known exclusion; the stated method, run, gives 82, of which five are English words in unrelated prose. Wrong by nine, in the direction that makes the remaining work look smaller (D20) |
| `FileExt::seek_read` on Windows is `pread` | It is **not**: it moves the descriptor's own file pointer, so a `pread` built on it alone leaves the next sequential `read` at end of file with every call reporting `Ok`. MEASURED on a ten-byte file: `read(4)`, `pread(3, offset 7)`, `read(3)` gave **0 bytes** instead of `456`. The position is saved and restored now (D23) |
| `fprintf` cannot be serviced because `omni-platform` has no file surface | It has one as of phase 3b. The refusal text said this and was corrected: what is missing now is only the binding of the `printf` family onto the stream layer (D23) |
| D23: `the_arena_fits_in_one_commit_granule` "pins the total against the granule **and the four tables against the order the accessors assume**" | Only the first half was true. The second assertion restated `ARENA_BYTES`'s own definition character for character and **could not fail**, so nothing checked that the four accessors agreed with the layout — dropping `POOL_BYTES` from `files_base()` left the whole suite green while `fopen` handed out `FILE` objects on top of the pool. Found by an independent review of phase 3c; replaced by a test that walks the bases **out of the accessors** (D24). The second time here a *total* stayed consistent while its *membership* did not |
| The plan's table: `omni-platform` must grow "**Sockets and polling** — socket, poll/select, getaddrinfo" | It did not have to. `poll` and `select` need **no OS call at all**: the descriptor space they observe is closed — the only bound symbols producing a descriptor are `open`, `__open_2` and `opendir`, and `socket` and `eventfd` refuse — so every descriptor is a regular file, a directory or a standard stream, and POSIX fixes the answer for all of them. **Third phase running whose five-target prediction over-estimated the OS surface** (D25) |
| HANDOFF: "`std::net` will answer yes for rather less of `socket`/`poll`/`select` than `std::fs` did" | The question did not arise: there is no `std::net` in any of it. The *third* answer to D23's test — not "one `std` call" and not "a backend", but **no OS call** (D25) |
| `Ipv6Addr`'s `Display` formats an address the way bionic's `inet_ntop` does | It does **not**, and a differential run of 200,000 addresses found the one class where it differs: Rust deliberately stopped printing the deprecated IPv4-compatible form in dotted notation, so it writes `::77:0` where BIND — which bionic ships essentially unchanged — writes `::0.119.0.0`. 43 disagreements in 200,000, all one class. The algorithm is written out and the differential test is committed with that class asserted from the other side (D25) |
| The APK's compressed textures are the 26 `.ktx` files | **38.** The twelve `.tex` files are KTX1 containers too — they are the skybox — so an extension-driven census misses every skybox face. `apk-analysis.md` §8.2's file-type table is not wrong; scoping a decoder from it would be. The census classifies by leading bytes now (D27) |
| The APK's ETC payload decodes to 52,083,328 B of RGBA8 | **52,079,224 B.** The first figure counts whole *blocks*; the 2x2 and 1x1 mip level of each of the 38 textures still occupies a full 4x4 block, so 27 padded texels x 38 files = 1,026 = 4,104 bytes. Found by the Rust sweep disagreeing with the Python census, which is the only reason two implementations exist (D27) |
| Leaving `__gcov_dump`/`__gcov_flush` `Unbound` is the right answer for a weak import | **Neither `Unbound` nor bound is right: they must resolve to nothing.** The guest tests each GOT slot for null before calling, and the call a stub would have answered is followed four bytes later by `BL abort`. VERIFIED by decoding the one site that references them (D25) |
| The plan's table: `omni-platform` must grow "**Threads** — spawn, join, detach, attributes, scheduling" | It did not have to. Phase 3c is `std::thread`, an `omni-mem` mapping for the stack and an `omni-cpu` context — **no new platform primitive**, and therefore no `unsupported` arm to write (D24). Second phase running whose five-target prediction over-estimated the OS surface |
| HANDOFF: phase **3c** is sockets and polling, **3d** is thread lifecycle | The plan's own phase-3 table has `3c` as threads + signals and `3d` as network, and the table is what was followed. Corrected here rather than quietly, because a reader with the old numbering will otherwise think a phase was skipped |
| `malloc` is the host allocator, so the guest heap is the host heap | `libroblox.so` imports **no allocator at all**; the seam is guest `mmap` through the demand pager |
| The reachable `STT_OBJECT` set includes `timezone` and `tzname` | It does **not**, and it does include `AMEDIAFORMAT_KEY_STRIDE` and `AMEDIAFORMAT_KEY_WIDTH`, which the list omitted. The count of **18** was right in both versions, which is why every count-based assertion passed; the membership was wrong by two in each direction. Derived from the real `.dynsym` now (D21) |
| `__sF` is reached as `__sF + addend`, which is why `declare_data` needs a size | Each of the eighteen data imports has exactly **one** relocation, `R_AARCH64_GLOB_DAT`, addend **zero**. The conclusion stands — `&__sF[2]` is arithmetic the guest does at run time — but the evidence given for it was not true of this binary (D21) |
| D5's warm figure is an entry ceiling excluding the exit | It was always entry-**and**-exit. This error was the controller's, and it propagated into a report and three code comments |
| Thunk design ratio is 7.6x | **3x** loader-shaped. 7.6x holds the PLT stub out |
| A 19-24 ns PLT-stub effect | Re-measured at 10.7 ns and inside an unexplained bimodality. Observation kept, figure dropped |
| `omni-platform` had 11 tests before phase 3a (commit `21b712d`'s message) | **7** — two in `vm::windows`, five in `fault::windows`. The 16 after was counted; the 11 before was remembered. Recorded in D22 rather than left in a commit message, because it is the shortest possible example of how a wrong number gets in |

**One open question, recorded not resolved:** an entry-and-exit measures ~41 ns, yet a per-call exit
*is* one entry and one exit and measures 81-103 ns. A bimodal entry path was directly observed during
review but did not reproduce when instrumented. Changes no decision. `tools/thunk_sweep.py` prints both
positions every run.

## Unfinished state that can be ignored

- An orphaned background shell (`bjpfg50m4`) from a reviewer that finished hours ago. Its scratch file
  `/c/od-rev-build.bat` is already deleted. Harmless.
- `.superpowers/` is git-ignored scratch. The `foundation-plan` and `cpu-execution-plan` workspaces
  were deleted after their milestones closed, by design — git and `docs/` hold the record.
- `crates/dynarmic-sys/vendor/` is 61 MB of unmodified upstream pin. Exclude it from review diffs;
  its immutability has been verified against a fresh clone in both directions.
- Old scratch trees under the session temp directory (`dynarmic-spike`, `gfxspike`, `memprobe`,
  `apk/`) are throwaway. Their findings are in `docs/research/`.

## Working agreements that have earned their place

These are in the M3 plan as Global Constraints. The three worth knowing before writing a brief:

1. **Hostile input is the expected case and the implementer tests it**, because four of five foundation
   tasks shipped a hostile-input defect their own passing suites could not see. A panic or abort
   reachable from untrusted input is Critical; an abort cannot be contained by any caller.
2. **Mutation-verify in both directions.** A fix that goes *too far* can pass every correctness test
   while destroying a property the design depends on — this has been caught three times. And
   distinguish *exercising* code from *detecting* a bug: a counter that rises under load but stays at
   zero under the injected fault is a watch, not a detector, and must be labelled as one.
3. **A figure enters `DECISIONS.md` only after someone other than its author reproduces it.** That rule
   has caught a wrong number four times, most clearly when the wrong version was the one that closed
   an open question.
4. **The whole-workspace suite runs `--release` and the mutation harness runs `debug`, and the
   difference is load-bearing.** Arithmetic overflow panics in debug and wraps in release, so an
   overflow reachable from guest input can pass `cargo test --workspace --release` — including the
   test written for it, if the wrapped value still satisfies the assertion. That is exactly what
   happened to `gmtime(i64::MIN)` in phase 3a (D22). When a new module does arithmetic on a
   guest-supplied number, run its tests in **both** profiles.

5. **`--only` pre-flights only what it selects, so a change to a file another row keys on is
   invisible until the full run.** M4 made two rows stale — one whose statement the milestone had
   made obsolete, one whose pattern a new function duplicated — and `--only jni` reported 17/17
   throughout. The check that found them runs no command and mutates nothing: import `mutate.py`,
   and for every row count `as_written(old, text)` in `read_exactly(path)`. It takes a second, it
   is safe to run while another agent holds the harness, and it is what a task that edits a file
   with existing rows owes.

---

# START HERE

Read in this order:

1. **`docs/HANDOFF.md`** — this file.
2. **`docs/VERIFICATION.md`** — **twenty-one** documented ways verification has failed *in this
   project*, each a real incident, plus the seven process rules they produced. Entries 15 and 16
   cost the most: a diagnostic that was switched **off** read exactly like a system that had
   stopped, and three guest threads died unremarked behind a green gate. Entries 17-19 arrived with
   the first presented frames and are about **which thing a result was about** -- a verdict printed
   before the process finished, a test binary built from another worktree, a capture that could
   not see a swapchain. Entries 20-21 came with input: a unit test that built its own input
   instead of the caller's, and taps aimed from one layout landing on another. Read it before
   writing a test you intend to rely on, and before believing a number.
3. **`docs/STATUS.md`** — the honest capability matrix. Nothing is claimed for Linux or macOS.
4. **`docs/research/jni-surface.md` §8** — the 26-step startup contract, and **§8.1**, which ranks
   the failure modes in the order you will meet them.
5. **`docs/DECISIONS.md`** — **D29** (M5 and the NDK surface) and **D28** (M4) first, then D27
   (textures — M6's scope), D26 (`AT_HWCAP`), D24, D23, D19, D16, D13, D7.
6. **`docs/ARCHITECTURE.md`** §§1, 2, 5, 6.
7. **`docs/briefs/`** -- two ready-to-launch subagent briefs for the current frontier
   (`webview2-seam.md`, `performance.md`).

## 2026-09-24 Windows (`perf-windows`): the close, the relaunch, memory on demand -- read this first

Full record, merge notes and every figure: **`docs/ports/windows.md`**. In short:

* **Done and committed** (`ced3e1d`..`fa7e136`): `perf-world` reviewed and merged (4 fixes);
  `__vsprintf_chk` (`vsprintf-` 6/6); a death that hangs the close now ends at once as
  `REASON_CRASH_NATIVE` and the next launch of that directory runs (`crashclose-` 3/3, verified live
  with `OMNI_INJECT_DEATH`); vendored dynarmic **patch 0002** (D32) -- a guest thread's fixed JIT cost
  on demand: landing commit 3,157 → 2,105 MiB, working set 2,528 → 1,884 MiB, 24.56 → 4.47 MiB per
  guest thread; `OMNI_IMPORT_CENSUS=off` for the census A/B. `inbound-` 10/10 (step 0).
* **The sign-in**: a clean close keeps it; a crash does not (the engine persists the account session
  on the way to the background; on a device the Java side also keeps its cookies, a Sink here --
  credential storage, the owner's call, not built). **Sign in once, close at Home with the X, and
  run everything from copies of that directory.**
* **Performance was NOT measured in a world**: the first sign-in window stood 43 minutes unused; at
  the second the owner joined 606849621 directly and the world died loading (raw `svc`, then
  `atol` -- both fixed in `c6e7c0d`). Nothing on the performance leads was changed without
  in-world evidence; the landing shows none of them (one hot thread; `docs/ports/windows.md`).

### The in-world protocol (needs the owner for the sign-in and the join; ~5 min per run)

GoodbyeDPI (not a VPN) broke teleports on 09-23, so measure in **place 606849621**, which is joined
directly (no teleport). Play binary: `../omnidroid-play` at `c6e7c0d`, already built. Script:
`<scratchpad>/play.sh <data-dir-name> <log-name> [ENV=...]` (session until the window is closed).

1. Master: `play.sh data-master-0924 master` -- the owner signs in (Quick Sign-in), waits for
   Home, closes with the X. Check the log ends `SessionHistory "IB"`.
2. Each measurement: copy the master to a fresh directory, `play.sh <copy> w<N> OMNI_PERF=5 [switch]`;
   the owner joins 606849621, leaves the camera idle ~3 minutes, closes with the X. Read presents
   per 5 s (FRAMES), `[SlowBenchmark]`/`[SlowModule]`, and the `PERF` blocks (per thread: jit / mon
   / dyn / hnd shares, crossings, translation). n >= 2 per arm.
3. Arms, one switch each against the default: `OMNI_IMPORT_CENSUS=off`,
   `OMNI_JIT_EXCLUSIVE_MONITOR=value` (D31 decides on this), `OMNI_JIT_CACHE_MB=128` (load-phase
   retranslation: -40% on the landing). Change the default only where a world shows the difference.

### Still open, new tonight

1. **In-world performance** (above) -- the goal's first item, unstarted for want of a session.
2. **The `InferredCrash` reporter** (`+0xc8` null at `0x2383500`) kills one worker at +5 s after any
   crash, and once on a fresh install; harmless to the run. Who sets it on a device: not decoded
   (its setter is not among the handle getter's twelve callers).
3. ~~Raw `svc #0`~~ **done in `c6e7c0d`** (with `atol`): the owner's first join of 606849621 died on
   both. The integrity check's `openat("/proc/self/maps")` now gets `-ENOENT` (no `/proc` here);
   what the engine does with that answer is not yet observed.
4. **The headless gate's 1224**: Linux lets `O_TRUNC` shrink a file under a live shared mapping;
   Windows refuses. A faithful fix is a logical EOF in the file seam.
5. **Memory for ~10 instances**: ~2.0 GB commit per landing instance, linear. Next lever: sharing
   translated code between threads (and instances).

## 2026-09-23 late night: keyboard and mouse, the first freeze fixed, three machines

**The owner's verdict after joining a world twice tonight: "painfully slow, 2-3 fps, nowhere near
playable"; the aim is 120 fps, ARM64 only.** Performance is still the first item. Everything below
is MEASURED from this session's runs (scratchpad of session 50cf03b2: `play/p1.log`, `play/p2.log`,
`suite-*.log`, `input-agent/`) unless it says otherwise.

### Where the source is

On `bionic-threads`, in order: `1f946d9` sincos, `23530d1` capacity (256 threads / 512 streams /
JNIEnv per thread), `c6187b8` vkCmdCopyImageToBuffer, `caab859` the gate's 16 GiB space with an
8 GiB commit ceiling, `23dd867` the two briefs (`docs/briefs/performance.md` rewritten for the
world, `docs/briefs/input.md`). Each was mutation-verified with the tree to itself: sincos- 3/3,
capacity- 5/5 (the rows the last handoff said existed did not -- they were written tonight),
readback- 4/4, adapter-B3 re-anchored 1/1.

**Branch `base-0923-night`** (one commit on `23dd867`, **the common base all three machines start
from**) adds, NOT yet on `bionic-threads` because it is not fully verified by this project's rules:
the input subagent's keyboard and mouse (its rows mouse- 22/22 and kbd- 3/3 caught in its own
worktree), the `listen`/`accept` + `NetworkUtils` freeze fix (rows `inbound-` A1-A9, B1 written and
pre-flighted, **not yet run**), and this handoff. With it, `cargo test -p omni-platform -p
omni-android -p omni-gfx --release --no-fail-fast` passes every target (bionic 242/242 after the
descriptor inventory learned `accept`; the headless gameactivity test passed this time). What
remains before it goes onto `bionic-threads`: run `python tools/mutate.py --only inbound-` with
the tree to yourself, then split it into its two commits (input, inbound) or merge it as one.

**Branch `perf-world` (`9b3e5a2`) is UNREVIEWED work in progress** of the performance subagent,
based on `23dd867`: instruments (`OMNI_PERF*`, `omni_platform::sampler`, translation counters),
an `ExclusiveMonitor` option, a lost-update stress test. Default behaviour unchanged. Not merged.

### What the two sessions showed

* **p1** (`23dd867`, `OMNI_HARDWARE_KEYBOARD=1`): signed in, lobby (+338 s), teleport, main world
  (+466 s) at 0-10 presents per 5 s. 20 asset downloads timed out at 60 s with partial bytes (p1
  only; play17 had 0). Keys reached `nativePassKeyEvent` in the world (Tab, D held, Esc). **At
  +635 s the game froze for good**: the engine's MicroProfiler web server (thread start link
  `0x61f1580`; blocking socket, `0.0.0.0:1338`, `listen(fd, 8)` result ignored, then
  `accept(fd, NULL, NULL)` in a loop) died on unbound `listen`, and a TaskScheduler worker died on
  JNI `NetworkUtils.getPublicIPv4Addresseses`. FIXED in `base-0923-night` (below).
* **p2** (keyboard/mouse + the freeze fix, `OMNI_KEYBOARD_MOUSE=1`): sign-in, Home and the menus
  by mouse (2,048 moves, 50 buttons, 52 wheel notches, all returned). **Pet Simulator 99's
  teleport to its main world failed 4 of 4 with `NoResponse`** from the world server
  (`128.116.55.33`, `128.116.51.33`; the lobby on the same IPs connected in ~1 s) on ProtonVPN --
  p1 the same night connected (in 33 s). Believed network (the VPN exit), not proven. Then the
  owner joined place 606849621: **a TaskScheduler worker died at +760 s on unbound
  `__vsprintf_chk`, the game froze, the server dropped the client (reason 266)** -- the next fix.
* **Every freeze so far is one worker death with everything behind it**, and the close then hangs:
  `onSurfaceDestroyedNative` spins its whole 2e9-instruction budget, `onStop` never runs, and
  **the sign-in is not kept** (twice tonight). Until that is fixed each session needs a new
  sign-in. Frontier item 5 of the previous section (name who holds each lock) is now urgent.

### Built tonight (on `base-0923-night`)

* **Keyboard and mouse** (`jni/mouse.rs`, `jni/keys.rs`, the window seam; decode in the module
  docs): a mouse's press/move/release go to `vk.e.onTouch`'s mouse branch, hover/scroll/button
  events to `vk.e$e.onGenericMotion`, captured motion to `vk.e$d.onCapturedPointer`;
  `nativePassMouseMove(x, y, dx, dy)` in dp, `nativePassMouseButton` at the last move's position
  with `getActionButton() - 1`, `nativePassMouseWheel` (vertical axis only). The engine locks the
  pointer only for `LockCenter` (shift-lock, first person), and learns a keyboard and mouse exist
  from events, not from `Configuration`. Window seam: `Wheel`, raw `PointerMotion` (`WM_INPUT`),
  pointer capture (hide, pin, raw input; released on focus loss), `wait(timeout)`. Held keys and
  buttons are released on focus loss. **`OMNI_KEYBOARD_MOUSE=1`** turns it on (`tools/play.ps1`
  defaults to it; `-Phone` for the old touch configuration; the gate's default stays the phone).
  Verified on the landing screen (a mouse click on Sign In -> `APP_READY(Login)`) and the menus in
  p2; **never yet verified in a world** (WASD walking, right-drag camera, wheel zoom, shift-lock).
* **`listen`/`accept`** in the net seam and bionic, with an inbound policy rule
  (`NetPolicy::allow_listen`, `check_listen` asked with the bound address; loopback under
  `allow_loopback`; `unrestricted` admits it). A blocking `accept` waits for its connection without
  the 60 s cap -- sliced, ended by the teardown flag, bounded by `SO_RCVTIMEO` -- because
  refusing after a minute would kill a correct thread. The accepted socket is blocking and not
  close-on-exec (Linux semantics; Winsock would inherit). `accept4` stays unbound (nothing calls it).
* **`NetworkUtils.getPublicIPv4Addresseses`**: the Java body transcribed
  (`jni::classes::public_ipv4_addresses`: skip loopback, skip anything with ':', append
  `addr + " : "`) over `omni_platform::net::interface_addresses()` (GetAdaptersAddresses), defined
  by the gate at startup.

### Still open, in order

1. **Performance** -- see `docs/briefs/performance.md` and branch `perf-world`. Benchmarks so far
   (not the world): the global exclusive monitor serializes all guest atomics (~2 M/s process-wide
   with 8 threads) and **`23530d1` made each atomic ~2x slower** by sizing it for 256 threads
   (57 -> 131 ns); a value-compare monitor is 12.8 ns and passes a lost-update stress test.
   Indirect returns go through dynarmic's C++ block lookup: 125 ns per call+return at 262k blocks
   vs 25 with return prediction (which needs a vendored patch to stay budget-stoppable -- a
   decision record). The gate's per-symbol import census stays on all session: 238-280 ns per
   crossing with 8 threads vs 35-46 off. Nothing measured in a world yet.
2. **`__vsprintf_chk`** (the p2 death). Then whatever the next join reaches.
3. **A worker death freezes the game and hangs the close** (above).
4. **Raw `svc #0`** kills a TaskScheduler worker on every place load (+340 s and +490 s in p1): the
   site (`0x32462e0`) is inside heavily obfuscated code (flattened control flow, computed syscall
   number in `x8`). The faithful fix is generic -- route a guest's raw SVC through the same Linux
   syscall emulation the `syscall()` import uses -- and answer truthfully.
5. **Server kick, reason 304, "Roblox has detected missing or corrupted files"** (play17, +914 s,
   ~225 s into the world). The test APK is re-signed ("Gloop") and modified; a stock APK is the
   discriminating test. Not worked on: an integrity verdict is not something to work around.
6. **The PS99 teleport `NoResponse`** under ProtonVPN (p2). Try another exit first.
7. **The headless gate** (`--test gameactivity` without `OMNI_GFX_WINDOW_TESTS`) failed 3 of 3 here
   at `a5443f3` and after: `open` of `LocalStorage/memProfStorage<pid>.json` refused with Windows
   1224 (a re-open truncating a file with a live shared mapping; Linux allows it), and
   `eglGetDisplay` (headless answers `dlopen("libvulkan.so")` NULL, the engine falls back to GLES,
   EGL is unbound -- a device with neither does not exist, so this is a gate design question).
   The input subagent's reruns passed it -- timing-dependent; treat as live.
8. **16 mutation rows are stale at `a5443f3`** (android-mem-A1, boundary-A11/A12, adapter-A5,
   guestmem-A3/A10, fs-A3, fallocate-B1, mmapfile-B2, threads-B2, dl-A5, pipe-B3, looper-B2,
   confine-A3/A4, fmod-B6): re-anchor them before the next full-table run.

### Decisions the owner was given tonight

* **Login.** The owner asked for the old "bootstrap" that plants a `.ROBLOSECURITY` cookie into the
  app. **Declined, and not to be built**: this project does not handle account credentials. The
  supported path is the app's own persistence -- the owner signs in once, the window is closed
  with its X so `onStop` runs, and later runs start from a *copy* of that data directory. That
  works only once item 3 is fixed.
* **Swapping in a newer Roblox APK is not drop-in.** The gate names the APK file and asserts
  exactly 3,594 initializers; the Java side's behaviour this layer transcribes was decoded from
  this version's obfuscated dex (`vk.e`, `vk.g`... change every release); a newer engine will
  reach unbound imports and JNI members. Exported native entry points tend to be stable. Each
  update is a porting job, and Roblox forces updates. The next APK should be stock and a single
  arm64-v8a APK (the Play Store ships splits, which this runtime does not read).

### Three machines, one source (from `base-0923-night`)

The owner is continuing on three machines at once, each with its own Claude session and prompt:
**Windows** (this PC) for performance; **macOS** (`berat@192.168.0.24`, Apple M1, 16 GB RAM,
**~12 GB disk free**, macOS 26.5; Rust present, no cmake/Homebrew/Vulkan) to port the runtime;
**Linux** (`berat@192.168.0.38`, Ubuntu 26.04, Intel i5-4460, 7 GB RAM, **NVIDIA Quadro 4000 --
Fermi, no Vulkan driver exists for it**, so Mesa lavapipe; no toolchain installed). A copy of this
folder (source, `.git` with every branch, and the APK; no `target/`) is on each Desktop as
`omnidroid`. Each
works on its own branch from `base-0923-night` -- `perf-windows`, `port-macos`, `port-linux` --
and the three are merged into one source afterwards; the ownership rules that keep that merge
mechanical are in each machine's prompt.

**The owner's cross-platform requirement, for all three:** memory and CPU strictly on demand --
no fixed reservation the way a VM takes one, no reliance on a page file. The scenario: boot peaks
near 4 GB and settles near 800 MB; a machine with 8 GB and a nearly full disk must run ~10
instances if they are started one at a time. What the runtime releases after boot (decommit /
`madvise(MADV_DONTNEED)`), shares between instances (the library's file-backed pages already are:
~104 MiB), and never commits speculatively (D10) decides that; the guest's own heap decides the
rest, and every figure needs a measurement.

## 2026-09-23 night: A GAME WORLD LOADS AND RENDERS -- the section before this one

**Where it stands.** The owner signed in (Quick Sign-in), pressed Play on Pet Simulator 99 and
the whole join ran on this layer: RakNet connected ("Connection accepted"), the lobby place loaded
(`onGameLoaded: placeId:8737899170`), the experience teleported to its main world (place
140403681187145), that server accepted, the main world loaded, and **the render thread drew it**:
2-8 presents per 5 s, and the owner's clicks and drags reached the engine (play17, the scratchpad
of session a4b3c2d9). No guest thread died on the whole path except one (below).

**The owner's verdict: "way too slow -- I can barely move my camera"**, and **keyboard and mouse
controls do not reach the game** (WASD, Tab, mouse camera). Those are the two top items of the
frontier below. **The owner cannot switch away from the arm64-v8a APK** (the APK here ships only
`lib/arm64-v8a`): the x86-64 Android build Sober uses is not an option, so performance work is
ARM64-through-dynarmic only.

### Fixed and committed this session (each mutation-verified, row prefix in brackets)

`9e70608` fcntl F_GETFD/F_SETFD + FD_CLOEXEC recorded per descriptor [cloexec-, 12/12];
`9cf6e10` SO_LINGER and SO_BROADCAST, Linux's every-socket semantics over Winsock's refusals
[linger-, 7/7]; `7d17623` the unimplemented-option test moved to SO_OOBINLINE (it broke because
`9cf6e10` was committed after running only filtered tests -- VERIFICATION entry 5 again); `0adabca`
path-MTU discovery by mode over Windows' `IP_MTU_DISCOVER` (RakNet: PROBE then DONT on one
socket; Windows refuses to mix `IP_DONTFRAGMENT` with it) and `getauxval(AT_SECURE)` = 0
[pmtu-, atsecure-, 7/7]. Earlier the same day: `ebe115e`, `5d9699d`, `9cc04c3`, `f0cd023`,
`b438c31`, `96f3c8d`, `dd5e634`, `fdb2f88` (see the join list further down).

### UNCOMMITTED in the tree -- tested, NOT yet mutation-verified; finish these first

The affected suites pass in full, run 2026-09-23 night with all four in the tree:
`omni-android --test bionic` 241, `--lib` 340, `--test vulkan_present` 27 (9 ignored),
`omni-bionic --test libm_tests` 22. The mutation rows are
written in `tools/mutate.py` and **have not been run**. Run each prefix with the tree to itself,
then commit each fix separately with explicit paths (bionic.rs and mutate.py hold hunks of
several fixes -- stage by hunk or strip-and-restore, as `0adabca` was):

1. **`sincos`** (`omni-bionic/src/libm.rs`, `handlers.rs`, `libm_tests.rs`, bionic.rs inventory
   314 bound / 299 inline + a BEYOND_THE_PREDICTION entry). The in-game worker pool (link
   `0x4fbcbc4`) died on it. Rows `sincos-` (3).
2. **Capacity for a loaded world** (`bionic/mod.rs`, `addrinfo.rs`, `jni/mod.rs`, bionic.rs +
   jni tests): `MAX_GUEST_THREADS` 64 -> 256 (pthread_create EAGAIN -> RBXCRASH "thread
   constructor failed: Unknown error 11"), `MAX_GUEST_FILES` 16 -> 512 (hundreds of asset-cache
   `errno=24` while ~60 descriptors were open), `MAX_JNI_THREADS` = `MAX_GUEST_THREADS` (a worker
   pool, link `0x601021c`, died on the 64-env cap), the arena now 5 commit granules
   (`ARENA_GRANULES`, the eager-commit cost stated). Rows `capacity-` (5).
3. **`vkCmdCopyImageToBuffer`** (`vulkan/draw.rs`, `vulkan/host.rs`, `vulkan/mod.rs`,
   `omni-gfx/src/host.rs`, `tests/vulkan_present.rs`): the render thread died on it on the main
   world's first frame. **No mutation row yet** -- add one (e.g. swap image and buffer) and run it.
4. **The gate** (`tests/gameactivity.rs`): a 16 GiB guest space with an 8 GiB commit ceiling
   (`GUEST_SPACE_BYTES`, `GUEST_MAX_COMMITTED`; `MemTotal` follows) -- the world hit the 4 GiB /
   3.5 GiB defaults (mimalloc's 1 GiB regions ENOMEM, low-memory warnings); and the FRAMES line now
   prints "N descriptors open". Verified by the play runs, not by a row.

### How the owner's builds were made (and must keep being made)

* **Never build while the owner is in a session.** MEASURED: a cargo build during play exhausted
  the host's commit (Windows error 1455, "paging file too small"), and two guest threads died in
  the JIT (a C++ exception escaped `Jit::Run`). The host: 31.8 GB RAM, 51.8 GB commit limit, and
  ~31 GB of it held by the owner's browsers and apps.
* The play binary comes from the worktree **`../omnidroid-play`**, currently detached at `7d17623`
  **with the uncommitted fixes applied as patches** (pmtu/atsecure -- now `0adabca` -- sincos, the
  gate memory, capacity, the JNI cap, the copy). After committing, `git -C ../omnidroid-play
  checkout -f --detach <new HEAD>` and rebuild: `OMNIDROID_DYNARMIC_BUILD_DIR='C:\odp-build'
  CARGO_TARGET_DIR=<worktree>/target cargo test -p omni-android --release --test gameactivity
  --no-run`; run the exe from the worktree's `crates/omni-android` with `OMNI_DATA_DIR=<fresh dir>
  OMNI_M6_ROWS_21_22=1 OMNI_GFX_WINDOW_TESTS=1 OMNI_SESSION_SECONDS=315360000 <exe> --nocapture
  --test-threads=1 initialize_native_code_returns_a_native_code_and_the_game_thread_starts`. A
  running session locks the exe (LNK1104): close it first.
* **Use a fresh `OMNI_DATA_DIR` each launch** (data-session-0923k, -l, -m were the last): a
  force-quit data dir hits frontier item 4's inferred-crash death on the next launch.
* **Network.** Roblox is ISP-blocked here. The owner used **GoodbyeDPI** (not a VPN) until the
  last runs: under it both teleports' transport handshakes got NoResponse and `tr.rbxcdn.com`
  never resolved. On a **real VPN** the same teleport connected in 1.4 s. Ask which is on before
  debugging a network failure.
* **Watch the log with a filter that flushes** (`awk '{...; fflush()}'`, not `cut`, which
  buffers -- a death went unreported for minutes because of it).

### The frontier, in order

1. **Performance, ARM64 only** -- the owner's first ask ("I want it smooth"). In the world:
   2-8 presents per 5 s. While the world loads: ~0 for 60+ s (in play13 the Lua main thread ran
   heavy module loads -- `[SlowModule] PetItem 2815ms` -- while the render job presented nothing).
   Measure first (`OMNI_WAIT_TRACE=<s>`, the omni-cpu code-fetch counters, `docs/briefs/performance.md`),
   in the world, not on the menus: where a frame's time goes (translation, JIT execution, the
   Vulkan forwarding, waits). Known levers already measured: the code cache (`fdb2f88`), the 1 ms
   timer; the landing's idle 1 fps is still undecoded ("The frontier -- CURRENT", item 6).
2. **Keyboard and mouse into the game, the way a device with a hardware keyboard gets them** --
   not hardcoded actions. Roblox's Android client handles `KeyEvent`s itself (WASD, Space, Tab,
   Esc...) and mouse `MotionEvent`s (source MOUSE: right-drag camera, wheel `AXIS_VSCROLL`).
   Decode GameActivity's key path (`onKeyDown`/`onKeyUp` natives reading a `KeyEvent` over JNI:
   keyCode, action, metaState, source, repeatCount, scanCode, unicodeChar...) and the mouse
   motion path, then translate the host window's keys and mouse (omni-platform's window seam)
   into them. Touch already works (`nativePassInput`).
3. **One death left on the join**: at +335 s of play17, guest thread 2 (link `0x284d168`) stopped
   on `UnsupportedInstruction` -- a raw `svc #0` at link `0x32462e0`. This layer has no path for a
   guest's own supervisor calls (`omni-cpu/src/dynarmic/callbacks.rs` stops the thread there by
   design); libroblox has 138 such sites. The game kept running without that thread. Not
   investigated further in this session.
4. Smaller, measured: the descriptor table is 1024 where Android gives an app 32768 (tied to the
   poll backend's 1024-socket `select`, `MAX_OPEN_FILES <= MAX_POLL_SOCKETS`); `MAX_LOOPERS` is
   16; `getViewportDisplaySize: Failed to find class 'DeviceUtils'` is benign (the class is in no
   dex of this APK, so a device fails the same way); a private address
   (`10.110.101.222:5052`) times out in every session and is harmless.

## Where the runtime actually is today

The gate, and the switches it takes (every stimulus switch is opt-in and says so in the log):

```text
OMNI_M6_ROWS_21_22=1 OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-android --release --test gameactivity -- --nocapture --test-threads=1
  OMNI_SESSION_SECONDS=120   the session after the rows (default 20 s)
  OMNI_RESIZE_PROBE=1        resize the real window to 960x540 at 40% and back at 70%
  OMNI_INPUT_PROBE=1         one synthetic press-drag-release at the centre, once the surface is alive
  OMNI_LATE_INPUT=<s>,..     a synthetic 240 px drag at each second of the session
  OMNI_LATE_TAP=<s>@<x>,<y>;..   a synthetic tap at a window pixel (press one of the engine's buttons)
  OMNI_LATE_TEXT=<s>@<text>;..   synthetic typing (<enter> is the Enter key) -- never a credential
  OMNI_HARDWARE_KEYBOARD=1   declare a QWERTY keyboard before step 13; keys then go to nativePassKeyEvent
  OMNI_PROFILE=1             where every guest thread's time goes (in guest code / which handler)
  OMNI_CLIENT_APP_SETTINGS=<json>  Roblox's own ClientAppSettings.json, for turning on an engine log
  OMNI_DATA_DIR=<dir>        keep the app's storage between runs (a signed-in session included)
  OMNI_WEBVIEW_PROBE=<s>     SYNTHETIC: open a page through the engine's bus that calls the app's web-view bridge
```

**2026-09-23: a person signed in, reached Home, and pressed Play.** `tools\play.ps1` is how the
person runs it (until the window is closed, since `b66b943`; storage kept in `%LOCALAPPDATA%\Omnidroid\data`,
`-Fresh` for a clean install). What the signed-in sessions (play2-play7 in that session's
scratchpad) found, in order, and where each stands:

* **Sign-in works through Quick Sign-in** (DID_LOG_IN, then Home). The first two accounts were
  **moderated by Roblox** (every signed-in call `403 "User is moderated"`), which no runtime change
  can alter; a fresh account worked. **Password sign-in gets a captcha** (`"Challenge is required to
  authorize the request"` -> `ChallengeHybridWebView` -> `Load generic challenge failed` 70 s later),
  because this runtime has **no web view** -- the frontier's item 2.
* **Signing in killed six guest threads**, all fixed and committed (`8ac8b64`):
  `Context.getSharedPreferences` (the render thread -- the window froze), writable `MAP_SHARED` file
  mappings (three threads; now real host file views), `recvmsg`, `sysconf(_SC_OPEN_MAX)`.
* **The first game join** (the loading screen showed) failed with Roblox's "Http error 529": the
  descriptor ceiling was 64 and `socket()` answered `EMFILE`, so the request to
  `gamejoin.roblox.com/v1/join-game` never got a socket. Fixed (`bc3fe21`: 1024 descriptors, a
  1024-socket Windows `select` set), with `JavaVM::DetachCurrentThread` (two FMOD threads died on it
  holding a lock, and everything behind the lock hung). A QUIC thread's `raise(SIGTRAP)` in that
  run is believed to follow from the `EMFILE`; unverified.
* **`vkCmdResolveImage`** killed the render thread on the landing screen of the next client
  (`a27de7c`, fixed). The client after that ran 400 s with no death and closed cleanly.
* **The second join (2026-09-23 evening, password sign-in with no captcha asked) reached the game
  page, Play, and a black window**: `GUEST THREAD DIED at +340s: thread 2 (started at link
  0x284d168): ... pthread_condattr_init ... nothing in the compatibility layer implements it`, and
  no frame after it. It was not slowness. Fixed (`dd5e634`): `pthread_condattr_init`, `_setclock`,
  `_destroy` bound -- the primitives were in `omni_bionic::cond`, tested, never wired (entry 16).
* **Then one death per press of Play (2026-09-23 night), each fixed, each the next thing the join
  reached**, every one on an engine TaskScheduler worker (start routine link `0x284d168`):
  `pthread_attr_setschedparam` and `pthread_setschedparam` (`ebe115e`); `gethostname` (`5d9699d`,
  "localhost", as AOSP's `init.rc` sets it); `SystemThemeProtocol.getSystemTheme` (`9cc04c3`, from
  `uiMode`); `getaddrinfo` with an empty service (`f0cd023`, `EAI_SERVICE`) and with a null node
  (`b438c31`, bionic's `explore_null`: the bind or loopback address); `fcntl(F_GETFD)`
  (`9e70608`, `FD_CLOEXEC` recorded per descriptor -- all 17 of the engine's `fcntl` call sites
  are DECODED to commands 1-4, so `fcntl` is done). Found reading waits on the way: the futex now
  compares its word as `FUTEX_WAIT` does (`96f3c8d`), a lost-wake class.
  **Watch next:** Windows IPv6 sockets default to `IPV6_V6ONLY` 1 where Android's is 0; the null
  node answer lists `::` first, so an engine that binds `[::]` and sends to an IPv4-mapped address
  would fail here and work on a device. Not seen yet; not changed without a measurement.
* **Unbound imports the join may meet next** -- 206 of `libroblox.so`'s 565 imports appeared as no
  string anywhere in `omni-android/src` (a crude audit, 2026-09-23). Most are GL/EGL (the engine
  takes Vulkan) and `AMediaCodec_*`/`AMediaFormat_*` (video). The libc ones a join could plausibly
  reach, less the two since bound: `pthread_key_delete`,
  `pthread_exit`, `sendmmsg`, `writev`, `getpeername`, `getnameinfo`, `socketpair`, `pread64`,
  `lseek`, `readlink`, `realpath`, `mremap`, `uname`, `tzset`/`localtime`, `vsscanf`, `strspn`,
  `strncat`, `tolower`, `atol`, `bsearch`, `ldiv`, `difftime`, `sincos`, `erff`, `powl`,
  `__memmove_chk`, `__strcpy_chk`, `__vsprintf_chk`, `__read_chk`, `__android_log_write`,
  `sigaltstack`, `signal`. Only `strspn`, `strncat` and `bsearch` have `omni-bionic` primitives.
* **Play binaries while other work is in the tree:** build them from a clean worktree of the
  commit with a private target (`git worktree add --detach ../omnidroid-play <commit>`, hard-link
  the APK in, `OMNIDROID_DYNARMIC_BUILD_DIR=C:\odp-build` because the worktree's path is too long
  for MSVC, `CARGO_TARGET_DIR=<worktree>/target`), and run the exe from the worktree's
  `crates/omni-android`. That keeps a subagent's half-finished edits out of the owner's session.
  `tools\play.ps1` now runs until the window is closed by default (`b66b943`).
* **The network:** Roblox is DNS-blocked here without the owner's VPN (frontier item 1).
* **Audio works** (`b25559d`): `libaaudio.so` over WASAPI; FMOD's init no longer fails with 51.
* **The frame rate is the person's main complaint** ("unstable and unusable"): 1-7 fps. The menus
  are much faster since `fdb2f88` (a 32 MiB JIT code cache per guest thread: 13 to 60 fps while
  dragging; the person: "noticeably faster on the menus"). See the frontier's item 3.
* The gate prints **`GUEST THREAD DIED at +Ns`** the moment a thread dies (it used to say so only
  at teardown, and a dead render thread read as a frozen window). **PowerShell wraps redirected
  output at 120 columns** in `play.ps1` logs, so a grep pattern can be split across two lines.
* Earlier data directories the person used are set aside, not deleted:
  `%LOCALAPPDATA%\Omnidroid\data-moderated-account-*`, `data-frozen-session-*`,
  `data-join-attempt-*`, `data-resolve-freeze-*`.

It **passes** with a real window on an RTX 4060 (gate114): seven tests, no guest thread killed,
teardown with none left running -- **and it closes the app the way a device does**: focus lost,
`onPause`, the surface destroyed, `onStop`, `onTrimMemory(UI_HIDDEN)`, and the process lifecycle
events the app's own observer turns into `setInactive`/`setHidden` (below). The render thread's
`APP_CMD_TERM_WINDOW` teardown runs to the end -- pipeline cache saved (`vkGetPipelineCacheData`),
then swapchain, surface, device and instance destroyed, each only after its children -- and a
close watchdog names every thread and releases the glue if a close ever hangs again. **The close
is asserted**: every call returned, and the engine's own session record says the app went to the
background (`SessionHistory` ending in `B`). The memory the guest is told it has is the commit
ceiling this runtime enforces, 3.5 GiB (it was 2 GiB, which put the engine in a low tier: a
16-48 MB texture budget, now 512 MB).

**A kept install launches again** (gate110-112: a first launch and two more of one
`OMNI_DATA_DIR`, 7/7 each). Two things a device hands the engine were missing. How the last run
ended: the gate records a run it closed as `REASON_USER_REQUESTED` in the kept root, and step 11
hands it to `nativeSetAppPreviousExitReasons` as the `ApplicationExitInfoCpp` list the Java side
builds. And the process lifecycle: `RobloxApplication.onCreate` registers a
`JNIAppLifecycleNativeAdapter` with `ProcessLifecycleOwner` (`ON_RESUME` -> `setActive`,
`ON_PAUSE` -> `setInactive` 700 ms after the pause, `ON_STOP` -> `setHidden`); without them the
session record stayed `I` and the next launch died in the engine's inferred-crash report
(gate109).

**Twenty minutes on a kept install** (gate115, its fourth launch): 7/7, no guest thread killed,
1,419 presents with no break -- the settled landing at a steady 57 a minute -- both resizes
delivered, at 8 and 14 minutes, and the device's close at the end.

What a run reaches, every time:

* **The engine's landing screen, drawn by its own renderer, presented to the host window** -- the
  Roblox logo, Create Account / Sign In, the thumbnail collage (`landing_first.png` in the session
  scratchpad was the first). Frames continue for the whole session.
* **Frame rate is the engine's own choice, measured, not a stall here.** On the settled landing it
  presents about 1 frame/s; bursts to 30-59 fps while it loads or animates (gate80: 290-295 per 5 s
  for ~25 s); input and resizes raise it (a late drag took a 5-per-5 s window to 18-20). Submits
  equal presents -- the render loop itself runs at that rate. Why the settled rate is 1 Hz rather
  than 60 is **not decoded**; `SessionIdleController` and `FrameRateManager` are the candidates.
* **The app lifecycle is the device's**: startup, a brief `Home`, every authenticated call 401,
  `DID_LOG_OUT`, `restartLuaApp`, the logged-out `Landing` again.
* **Input the engine acts on.** A tap on Sign In navigates to its `Login` screen (`APP_READY(Login)`),
  on Create Account to `SinglePageSignUp`, on Quick Sign-in to its Quick Sign-in dialog.
* **Typing reaches a focused `TextBox`** (gate88): tapping the username field makes the engine ask
  for the keyboard (`gameActivity_showKeyboard`); typed text reaches it through `jni::text` --
  `RbxKeyboard`, run by the host; Enter reaches `nativeReturnPressedFromOnScreenKeyboard` and the
  engine moves focus to the password box itself; the engine's Sign In button enables. The host
  window's `WM_CHAR` is `WindowEvent::Text`. **Typed text is never logged** (lengths only).
* **Quick Sign-in works up to the owner's part** (gate89): the engine fetches a real cross-device
  session and shows its QR code and a six-letter code.
* **The window survives a resize** and frames continue on both sides (`OMNI_RESIZE_PROBE`). Without a
  resize the landing layout sits 30 px higher than with one (Sign In at y≈367 rather than 397); a tap
  aimed from one layout misses in the other.
* **Networking is real**: TLS to the real endpoints, QUIC both ways.

## The frontier -- CURRENT

1. **The person's next test: enter a game.** Every death the signed-in sessions found is fixed.
   Relaunch `tools\play.ps1` (`-Fresh` while item 4 stands), sign in (Quick Sign-in, or password +
   the captcha in the new web view), open a game, Play, and watch the log for `GUEST THREAD DIED`.
   Everything after the join -- the RCC connection over UDP, the 3D renderer, physics -- has never
   run here; expect refusals, each naming itself.
   **MEASURED 2026-09-23: without the VPN this machine's network cannot reach Roblox at all.** The
   system resolver answers `195.175.254.2` for `clientsettingscdn.roblox.com` and
   `www.roblox.com` (a public resolver answers CloudFront's `65.9.9.68`), and that address presents
   a certificate Windows rejects (`SEC_E_UNTRUSTED_ROOT`). The engine's first fetch then fails
   `HttpError: TlsVerificationFail`, `getFlags: success = false`, no frame is ever drawn, and the
   gate's close assertion fails on a `SessionHistory` of `I` (wv1 in that session's scratchpad). A
   gate failure with those lines is the network, not the runtime. The VPN is how the owner reaches
   Roblox from here; the runtime does not work around a network's block.
2. **The web view -- BUILT (`b74f62d`, `9cdef80`); not yet met a real captcha.** The whole path,
   DECODED, is in `crates/omni-android/src/jni/webview.rs`'s module docs. In short:
   * **Where it is built:** `MainGameActivity.B2`'s UI runnable (`jk.c1`) forces the lazy `fh.c`
     and `jk.a0`; `jk.a0`'s constructor is `new WebViewProtocol(jk.a0)`:
     `setRequestHandlerRaw("WebView", "isAvailable")`, `doSubscribeRaw` for `WebView.openWindow`,
     `.mutateWindow`, `.closeWindow`, then `initializeAndroidWebViewProtocol` (installs the engine's
     `AndroidWebViewProtocol`); then `fh.c.c()` binds `BrowserService.OpenBrowserWindow`,
     `.CloseBrowserWindow`, `.SendCommand` in `MemStorage`. The gate does all of it before step 12,
     with a window (`WEBVIEW:` lines), and pumps it on the UI thread every session turn.
   * **The page's bridge:** `cl.d.d` adds `cl.d$c` as **`__globalRobloxAndroidBridge__`**, one
     method, **`executeRoblox(String)`**; in `ri.a` (the web view `jk.a0.g` puts up) the string goes
     raw to `signalJavascriptCallback`, which the engine publishes as `WebView`/
     `handleJavascriptCallback` (`0x2bac648`). The host defines the object with the seam's
     `init_script` and forwards through `window.chrome.webview.postMessage`.
   * **The user agent** is the app's (`el.i.c`: `... ROBLOX Android App 2.738.1397 Phone Hybrid()
     ...`), built from the facts the engine is given; Roblox's pages read it to use the bridge.
   * **MEASURED:** every install call returns; `OMNI_WEBVIEW_PROBE=<s>` (opt-in, SYNTHETIC)
     publishes `openWindow` on the engine's own bus with a page that calls the bridge: the page
     opened, `executeRoblox` reached the engine (`[FLog::WebView] Sending command: {...} to
     WebViewService`), `closeWindow` closed it and `handleWindowClose` was published (wv4). Unit
     tests 13/13; `tools/mutate.py --only webview-` 11/11.
   * **Assumed, and what would falsify it:** the Java flag `EnableAndroidWebViewService4` (whether
     `ri.a` gets the listener that reaches `signalJavascriptCallback`) is taken as **on** -- its
     compiled default is off and a device reads Roblox's settings service, which this host does not
     fetch; the engine forces its own two web-view flags on (`0x2bd58a0`). If a captcha's result
     never arrives and the engine is waiting on `MemStorage` `BrowserService.JavaScriptCallback`,
     this is wrong.
   * **Not done, each said in the code:** a page calling the bridge **from an iframe** -- the init
     script runs there, but WebView2 routes an iframe's `postMessage` to `ICoreWebView2Frame2`,
     which the seam does not subscribe (measured; about five COM pieces to add); the telemetry call
     `MessageBus$b.run` makes; `BrowserService.OpenBrowserWindow` (the Java side throws on it in this
     version) and `.SendCommand` (not decoded) refuse by name; cookies, deep links and back
     navigation inside the page; `NativeGLJavaInterface.getWebViewUserAgent()V` is still a `Sink`
     (on a device it asks a `WebViewUserAgentGetter`; not decoded).
3. **Performance -- 1 to 7 frames per second, the person's main complaint.** Measured state and a
   ready brief in **`docs/briefs/performance.md`**. In short: CPU is not saturated (2.3-2.4 cores);
   one core is the game loop spinning on `ALooper_pollOnce(0)` + a mutex (DECODED at `0x2bcd648`;
   it draws nothing); the render thread waits 81% of its time on a condition variable; the 1 ms
   timer resolution (`1634316`) is **not yet measured**. Measure what each frame waits on before
   changing anything. **Since then (`fdb2f88`):** the TaskScheduler workers were re-translating
   ~290,000 guest instructions a second because dynarmic evacuates a thread's whole code cache when
   under 1 MiB is free; a 32 MiB cache per guest thread took dragging from 13 to 60 fps (the gate's
   `CODE_CACHE_BYTES`, `OMNI_JIT_CACHE_MB` overrides). The **settled** landing still draws 1 fps
   idle and 5-6 dragging, with the workers ~96% idle -- not translation, not decoded; the engine's
   own throttle is the suspect (item 6). `OMNI_WAIT_TRACE` and the omni-cpu code-fetch counters
   (`ce8a2d8`) are the instruments.
4. **A kept directory after an unclean end** (console closed, Ctrl+C, a hang the watchdog ended):
   no exit record, a session record saying `I`, and the next launch takes the engine's inferred-crash
   report and a worker dies on a null member (MemoryFault reading 0 at link `0x2383500`, gate109,
   gate124). DECODED so far: the member is `+0xc8` of the engine's `InferredCrash` object
   (constructor `0x2269244` zeroes it; handle getter `0x22690c4`, twelve callers, holder
   `0x6a6b880`); the report runs only while the fast flag
   `PerformanceControlCrashMetricAlgorithmType2` (`0x6ed98e0`) is non-zero; the call comes from a
   listener loop at `0x2251590`. What sets it on a device is **not decoded**. Also MEASURED: a
   sign-in from a session killed before `onStop` is **not kept** (the next launch logged out) --
   the engine persists it on the way to the background. Until fixed, `play.ps1 -Fresh`.
5. **A thread that dies holding a lock hangs everything behind it**, including the close (the
   watchdog then shows threads INSIDE `pthread_mutex_lock` at `0x2b53a78`). The cure has always been
   the death's own cause, but the watchdog should say who holds each lock: `Bionic::mutex_owner`
   exists and is not printed.
6. **The idle 1 Hz** and the engine's frame-pacing log (`[FLog::ApplicationFrameRate]`): no FLog
   channel has been turned on yet -- `OMNI_CLIENT_APP_SETTINGS` with 12, 1030, "1030" and 65535 all
   printed nothing (gate115-116). The log site's check is DECODED (`0x61c96a4`: the flag's low byte
   >= 6 and a bit of `0xfc00`); how a settings value becomes those bits is not.
7. **`tools/mutate.py --only aaudio-`: 6/6 caught** (2026-09-23, the tree to itself).
8. **The one unexplained corruption**: gate42's MemoryFault in a libc++ `unordered_map` rehash at
   link `0x21db208` -- 32 bytes of `0xFF`, seen once in ~20 runs, not since. Treat it as live.

## What this session built, so it is not rebuilt

JNI: `android.os.Build` (all string fields, from the device properties by `Build.java`'s rule),
`Build$VERSION`, `Debug`; `GetStringUTFLength`, `NewByteArray`, `SetByteArrayRegion`; a bounded log
of class and member lookups (`Jni::lookups`, printed at teardown); `Answer::Construct` (a data
class constructor that keeps its arguments), `ShowKeyboard`/`HideKeyboard` and the keyboard request
queue; `jni::text` (`RbxKeyboard`). bionic: `sem_*` bound; `sem_wait` answers `EINTR` and a
contended mutex or cond relock refuses when the instance shuts down (both used to loop in the host
past the join); `setpriority`; JNI slots released with their thread; `/proc/meminfo` and `statm`.
Platform: `WindowEvent::Text` from `WM_CHAR`; `host_manufacturer`; `Filesystem::guest_path_of` (a
refused writable file mapping names its file). Gate: the switches above; every thread still running
at a failed teardown is named with its start routine and where it is.

Then, for the second launch: `jni::ExitRecord`, `Jni::set_previous_exits`/`previous_exit_reasons`
(the `ApplicationExitInfoCpp` list), `Jni::new_list`/`new_object_with`, `java.util.List` answered
from a host-built `ArrayList`; `script::process_lifecycle` (`ProcessEvent`); in the gate, the exit
records in `data/system/` of the kept root and the asserted close (`close_failure`).

Then, 2026-09-23 afternoon (commits `b25559d`..`1159291`): **audio** -- `omni_platform::audio`
(WASAPI, hand-written COM vtables) and `omni_android::aaudio` (`libaaudio.so` as FMOD dlsyms it; the
data callback on a guest thread started through the guest's own `pthread_create`);
`FMOD.supportsLowLatency` (false: the feature is not declared); `Context.getSharedPreferences` and
its editor (`Jni::shared_preferences`); writable `MAP_SHARED` file mappings as real host views, and
`msync`; `recvmsg`; `sysconf(_SC_OPEN_MAX)`; 1024 descriptors and a 1024-socket `select` set;
`JavaVM::DetachCurrentThread`; `vkCmdResolveImage`; `omni_platform::clock::TimerResolution`; in the
gate, audio bound with the window, live `GUEST THREAD DIED` lines and 1 ms timers.

**Decoded and deliberately not sent**, so nobody re-derives them:
* `JNIActivityLifecycleCallbacks` (registered unconditionally in `RobloxApplication.onCreate`, so a
  device does call it for `MainGameActivity`): its seven `Post*`/`SaveInstanceState` natives are a
  bare `ret`, and the other twelve feed one handler (`0x21f15a4`) that timestamps each transition --
  startup telemetry. Sending it would be faithful and changes nothing a run has shown to matter.
* `nativePassCurrentDisplayRefreshRate`/`nativePassSupportedRefreshRates`: only
  `MainScreenController` (`fi.r0`) calls them, and only `ActivityNativeMain` creates one --
  **not** `MainGameActivity`, so a device on the GameActivity path never sends them either; the
  engine's `PerformanceControlDisplaySupportedRefreshRates` `Empty` is that path's truth. (A
  window-seam refresh-rate query was written and removed for this reason; the patch is in the
  session scratchpad.)

## After the first frame -- the goal is not a frame

Frames continue, input is acted on, text is typed. What is left of the goal: **the game** (the
owner's sign-in, then joining one), then **optimisation** (measure with `OMNI_PROFILE` first -- this
project has twice been wrong about what a spin was costing), then **survival** (a session of
minutes, in a game, and teardown).

# History -- the sections below describe earlier frontiers, kept for the record

## The immediate blocker: the engine will not take the surface until the flags have arrived

Row 21's **second** downcall, `nativePostClientSettingsLoadedInitialization3`, does not return,
and the previous session's account of why was wrong in every particular. What it actually is:

```text
[FLog::NativeDM] nativeActivity_onSurfaceChanged: state:2.
[FLog::NativeDM] nativeActivity_onSurfaceChanged: ... Flags-Not-Received. Return.
```

The engine **drops the window** and returns, having done nothing with it. Nothing re-delivers a
dropped surface, so the renderer is never asked for, and from outside that is indistinguishable
from a graphics problem: the game loop spins in `ALooper_pollOnce` (MEASURED 138,974,961 calls in
180 s), the flags-loaded call blocks, and a worker burns a core on `sched_yield`.

The gate now drives **§8 row 21's first downcall before rows 17-20**, because that is the order
the engine asks for. On a device the client-settings fetch (`fi.e$f`) runs from `onCreate`, long
before the SurfaceView's `surfaceCreated`; §8's table lists 21 after 20 because that is the order
the *dex* names them in, and the ordering between those two rows was never independently verified.
With the flags first, `[FLog::NativeDM] initialize: state:1. areFlagsLoaded:**true**`.

The gate is the byte at `DataModel + 0x289`, read at guest `0x02bd307c`
(`ldrb w8, [x19, #0x289]; tbz w8, #0 -> bail`) and written at `0x02bd3be4`, inside
`[FLog::NativeDM] continueAfterFlagsLoaded_:` at `0x02bd3b58`. So the surface is accepted only
after `continueAfterFlagsLoaded_` runs, and that runs only after the settings fetch resolves one
way or the other.

### Three guest threads were dying, and nothing said so

`nativePostClientSettingsLoadedInitialization3` waits on a one-shot `pthread_cond_wait` at
`0x02320118` (no predicate, EINTR retry only), reached through
`getFlags("ClientAppSettings")` at `0x02bd564c`. The **guest backtrace** at the park — a frame
walk added this session, `omni_bionic::unwind::frames` — is

```text
0x2320170  0x3d387e4  0x5ff4678  0x4eca058  0x4ecb060  0x4ecaf30  0x2bd5650  ...
```

and `0x3d387d0` is a proper `while (!done) wait()` loop on a byte at `obj + 0x28`. Something has
to complete that future. Three of the guest's own worker threads had been **killed by this
layer**, and every one of the gate's assertions is about the thread it is standing on, so nothing
printed a word (`VERIFICATION.md` entry 16):

| thread | what killed it | fixed by |
|---|---|---|
| a log worker | `__android_log_print` refused a NUL-terminated line "with no NUL in the first 96 bytes" | `omni_mem::scan_reach` |
| `0x2173df8` | `pthread_getattr_np` unbound | bound |
| `0x2b53aa0` | `pthread_mutex_trylock` unbound — `omni_bionic::mutex::trylock` already existed and was already tested, and had never been wired to a symbol | bound |

The 96 was not a property of the string. `GuestMem::cstr` bounded its walk with
`admit(address, 1, Read)`, which reports the end of the **first entry** — and a lazy commit carves
one mapping into a run of entries, one per OS placeholder, never coalesced. 96 was the distance to
the next granule. `omni_mem::scan_reach` is the question a scan actually asks: *how far may I read
before I must stop*, bounded by the mapping and not by the granule.

With that fixed the engine gets much further and names its own next step:

```text
[FLog::ClientRunInfo] The base url is https://www.roblox.com
[FLog::Output] settingsUrl: https://clientsettingscdn.roblox.com/v2/settings/application/android
```

### The pure-binding-gap pattern, which has now happened five times in one session

`pthread_mutex_trylock`, `pthread_attr_getstack`, `__strcat_chk` and `strcspn` were each **already
written and already unit-tested** in `omni-bionic`, and each had simply never been wired to a
symbol in `handlers.rs`. Every one was found the same way: a guest worker thread died on it and
`Bionic::guest_thread_failures()` named it.

That is not four coincidences. The primitives were written against **the import list**, and the
wiring was done against **what the run had reached** — so every primitive whose symbol the run had
not yet reached stayed unbound, and stayed invisible, until a thread walked into it.

**Do not respond to this by binding everything available.** `strspn` is `strcspn`'s own
`span_walk` with one test flipped, binding it would cost nothing, and it is deliberately still
unbound: "the primitive is already written" is not the same claim as "the guest needs it", and the
rule against implementing an API because its name exists is the rule that has kept this layer
honest. What makes waiting safe is the assertion — a guest thread that dies now fails the gate by
name on the first run.

`inet_pton` is the one that broke the pattern: the guest reached it and nothing in `omni-bionic`
parses addresses at all, so it is real work rather than wiring.

### The host window is connected, and it was demonstrated rather than argued

`Ndk::set_window_source` takes an `Arc<dyn WindowSource>` and `ANativeWindow_getWidth`/`_getHeight`
ask it on **every call** — a pull, not a push, because `Window::client_size` asks the OS each time
and a pushed geometry is stale after every resize the *display* does rather than the user. The
constant path (`set_window_geometry`) is untouched and still what most tests use.

MEASURED, on this machine, through a real thunk from translated ARM64 code:

```text
live window: device "NVIDIA GeForce RTX 4060", validation false,
guest (1024, 576) -> (736, 414), swapchain Some((736, 414)) over 2 generations,
6 frames presented, 8 samples
```

The same `ANativeWindow *` followed a real `set_client_size`, the swapchain was recreated once,
frames were presented on both sides, and `window_geometry()` was asserted `None` throughout so the
constant path provably was not what answered. A minimised window gives a genuine 0x0 client area,
`swapchain_extent() == None`, and a refusal that names the source — there is no device analogue for
a zero-sized surface, so it is refused rather than invented.

**This is still not the engine's frames.** It is Omnidroid's renderer presenting into Omnidroid's
window with the guest reading the right numbers. The engine's own drawing still has nowhere to go:
0 of 17 `egl*` symbols bound, `dlopen("libvulkan.so")` still refused, and which of the two paths
Roblox takes has still never been observed.

### The chain after the string fix, in the order the guest walked it

Each of these was reached only because the one before it was answered, and each was found by the
thread-failure assertion rather than by looking:

```text
__android_log_print (the 96-byte scan)  ->  pthread_getattr_np  ->  pthread_attr_getstack
  ->  pthread_mutex_trylock  ->  strcspn  ->  __strcat_chk  ->  sysconf(_SC_PHYS_PAGES)
  ->  inet_pton  ->  getaddrinfo  ->  mallinfo  ->  socket(AF_INET, SOCK_DGRAM, 0)
```

`sysconf(_SC_PHYS_PAGES)` is answered from `Bionic::set_memory_budget`, the same seam `sysinfo`
takes, so a guest that asks both ways cannot be told two different things — and never from the
host's RAM, which is what it used to be refused for.

`mallinfo` now writes ten zeroed `size_t` fields. Its refusal had conceded, in its own text, that
zeroes would be *arithmetically true* of a heap nothing has allocated from, and `libroblox.so`
imports no allocator at all (D17), so nothing ever has. The worry underneath the refusal was that
a human reading a log would take "0 bytes allocated" for "uses no memory"; that is a reason for a
comment, not for failing a call the engine needs. Reporting this process's commit charge as the
arena is still rejected, and still for the original reason: a real number, from the right process,
describing the wrong allocator.

**And then the network seam was built, and the chain ran out the other side.** As of the guest
network surface landing:

```text
nativeInitClientSettings RETURNED 0, engine flags byte 1
[FLog::NativeDM] initialize: areFlagsLoaded:true  ->  bootstrapTheApp_
onSurfaceCreated / Changed / Start / Resume / FocusChanged / ContentRect / WindowInsets all returned
settingsUrl: https://clientsettingscdn.roblox.com/v2/settings/application/android
nativePostClientSettingsLoadedInitialization3 RETURNED 1      <- it used to block here
engine in its main loop: 123 M ALooper_pollOnce, 12 guest threads
socket() = fd 12  ->  setsockopt(fd 12, SOL_SOCKET, 9)  ->  refused  ->  worker thread died
```

**§8 row 21 is closed.** The call that consumed three sessions returns, the engine is running its
own main loop, and the runtime is carrying twelve guest threads. `SO_KEEPALIVE` (option 9) was the
next name, and it is now answered on both sides of the seam — `std::net` has no spelling for it, so
it is backend work, and only the boolean is carried because the *timing* knobs
(`TCP_KEEPIDLE`/`TCP_KEEPINTVL`/`TCP_KEEPCNT` against Windows' `SIO_KEEPALIVE_VALS`) have no
portable form at all.

### The empty-resolver diagnostic existed, bought one step, and is gone

`OMNI_EMPTY_RESOLVER=1` answered every `getaddrinfo` with `EAI_NONAME`, under the permission D30
records, so that the graphics path downstream could be observed while the real seam was built.
**It did not reach rendering, and that was measured rather than assumed**: the engine does not give
up when resolution fails — it opens a UDP socket and resolves for itself. So it bought one step
rather than the four that were hoped for, and there was never a shortcut to first rendering that
did not go through sockets.

`Bionic::use_the_empty_resolver_for_diagnosis`, its field, its `getaddrinfo` branch and the gate's
env block were **deleted** the moment `getaddrinfo` resolved for real, which is what its own doc
comment and D30 both said had to happen. `grep -rn "empty_resolver\|OMNI_EMPTY_RESOLVER" crates/
tools/` is empty. This paragraph is the record that it existed, because `VERIFICATION.md` entry 14
is about a deliberate failure that outlived its purpose and started reading like a measurement.

### Hypotheses that were measured and are dead — do not repeat them

* **It was never a deadlock.** The census read `FROZEN` because it had been switched *off*, not
  because nothing was moving; `Boundary::crossings().exits`, which is not census-gated, read
  26,631,317 -> 53,276,895 -> 79,473,983 over forty seconds. See `VERIFICATION.md` entry 15.
* **The two indefinitely-parked raw futex waiters are not it.** Both are ordinary idle workers;
  `AddressFutex::near_misses()` is empty, so no wake has ever landed beside one.
* **The `sched_yield` spinner is downstream, not upstream.** It waits at `0x2173f8c` for the
  singleton at `0x7275550`, published by `0x2174ad4` under a `__cxa_guard`, called only from
  `0x217402c` — code that has not run yet. It burns a core and it is not the cause.
* Hints, `STLR`, callee-saved registers across nested guest calls, lost `pthread_create`s and the
  processor count were all eliminated in earlier sessions and stay eliminated.


## Network: the constraint the goal will meet

Flags now load with **no network at all**, because the host supplies the settings document that
`nativeInitClientSettings` parses — which is what the Java side does on a device too. The
`AF_INET6` call M6 reaches is a capability probe and is answered.

**Everything past that is a real network question, and the engine has now named the first URL.**
With the string-scan defect fixed, `nativePostClientSettingsLoadedInitialization3` reaches its
settings fetch and logs

```text
[FLog::Output] settingsUrl: https://clientsettingscdn.roblox.com/v2/settings/application/android
```

The engine's HTTP is its **own**, over raw BSD sockets: `libroblox.so` imports `socket`,
`connect`, `getaddrinfo`, `sendmsg`/`recvmsg`, `epoll_*`, `select` and `poll` directly, with ten
direct `getaddrinfo` call sites and eight `connect` ones. So the fetch will resolve a name before
it opens anything, and `getaddrinfo` is the **first** thing it meets. That is the fork, and it is
a narrower one than `socket` would be:

* **`getaddrinfo` currently refuses by name**, which kills the calling guest thread — and a killed
  worker leaves the future nobody else can complete, which is precisely the hang this session
  spent itself on. A refusal here is not a clear failure; it is a silent deadlock.
* **The engine has a first-class failure arm** and logs it:
  `[FLog::NativeDM] ... getFlags: success = false.` at `0x04b5223`, and that path runs on to
  `continueAfterFlagsLoaded_`, which sets the byte the surface is gated on. A device in airplane
  mode takes exactly that branch and still renders its own UI.
* Answering `EAI_NONAME` allocates nothing, hands back no descriptor, opens no seam, and is
  **true**: this runtime resolves no names. The recorded objection — "a caller believes it asked a
  resolver" — is weaker than it reads, because the caller *did* ask this runtime's resolver.

**That decision has since been made, and it went the other way: see D30.** Global Constraint 8 is
**withdrawn**. The project owner's instruction is that playable Roblox is the higher-priority
requirement, that networking is allowed and required, and that no earlier "no runtime networking"
rule may be preserved where it prevents login, settings fetches, game joining or normal operation.
So the socket seam in `omni-platform` is not a decision to be made any more, it is work to be done,
and D30 records what constrains it: the smallest surface a run has actually reached, sockets in the
descriptor table `fs` already owns, isolation per instance, and portability unrelaxed.

A temporary `EAI_NONAME` is permitted **only** as a diagnostic to reach first rendering, and D30
says why that permission is dangerous: a deliberate failure that gets you past a gate is
indistinguishable a week later from an implementation that works.

## The graphics path: decoded, not guessed

`libroblox.so` **bootstraps Vulkan itself**, and the sequence is at guest `0x02595160`:

```text
0x2595170: adrp/add x0, "libvulkan.so.1" ; mov w1, #2 (RTLD_NOW) ; bl dlopen
0x2595180: cbnz x0, got_it                ; and if that failed,
0x2595184: adrp/add x0, "libvulkan.so"    ; mov w1, #2           ; bl dlopen
0x2595194: cbz  x0, give_up
0x2595198: adrp/add x1, "vkGetInstanceProcAddr" ; bl dlsym   -> global 0x6d3ca8
0x25951b8: mov x0, xzr ; x1 = "vkCreateInstance"              ; blr x8
0x25951c8:              x1 = "vkEnumerateInstanceExtensionProperties"
```

That is the standard loader bootstrap: fetch `vkGetInstanceProcAddr` by name, then call it with a
**null instance** for the global entry points. There are **zero `vk*` symbols in the import
table** — the whole API arrives through those two calls, which is why §8 row 25 describes Vulkan as
arriving "by `dlopen`" and lists no symbols for it.

`libEGL.so` and `libGLESv2.so` are `DT_NEEDED`, so both paths are in the binary and **which one the
engine uses has never been observed at runtime.** That observation is what the first Vulkan stage
is for: answer the `dlopen`/`dlsym`/`vkGetInstanceProcAddr` *mechanism*, record every name the
engine asks for, and let the first actual call refuse by name. One run then yields the ordered list
of entry points and the first one that matters, instead of one name per three-minute run.

**Why Vulkan is the tractable side, and it is not a preference.** Identity mapping (D4) means a
guest pointer *is* a host pointer, and Vulkan's structures are fixed by the specification and
identical on every LP64 target — so most of the forwarding is genuine trampolining: read the
AAPCS64 arguments, call the host function, write the result. The parts that are *not* trampolining
are small and known in advance: callbacks the guest supplies (allocator, debug messenger) have to
become guest calls back across the boundary, and **`vkCreateAndroidSurfaceKHR` has no host
counterpart at all** — the guest will hand it an `ANativeWindow *`, and this layer has to turn that
into a Win32 surface for the real window `ndk::HostWindowSource` now backs it with. GLES has no
equivalent shortcut: it would have to be implemented, not forwarded.

### Vulkan stage 1 is built: the loader opens and records, and implements nothing

`crates/omni-android/src/vulkan/` answers the *mechanism* and not the API. `dlopen("libvulkan.so")`
and `"libvulkan.so.1"` issue a handle **only if `vkGetInstanceProcAddr` is bound in that boundary**,
so an embedding that never calls `Vulkan::bind_into` gets the old NULL byte for byte.
`vkGetInstanceProcAddr(NULL, name)` answers the specification's five global commands with a stable
thunk each, answers NULL for anything else with a null instance — which the "Command Function
Pointers" table *specifies*, so it is not a guess — and refuses a non-null instance, because this
layer has issued none. Every thunk it hands out **refuses when called**, naming the Vulkan function
and quoting `x0`–`x7`. There is no list of Vulkan names in the file: 64 anonymous slots handed out
in ask-order, and a 65th distinct name is a refusal rather than a NULL, because "the pool is full"
must not be spelled like "this implementation lacks that function".

The census is per-instance, bounded at 512, and **ungated** — unlike `Boundary::census`, which sits
on a 33 ns path. This one is entered tens of times per process, so a flag would buy nothing and
would make an empty census ambiguous, which is entry 15 exactly.

### Stage 2a is built: the guest holds a real `VkInstance` on this machine's driver

```text
the driver reported 20 instance extension(s); the guest was shown:
    VK_KHR_surface (specVersion 25)
    VK_KHR_android_surface (specVersion 6)
vkCreateInstance returned VkResult 0
the guest's VkInstance handle is 0x1b752e78000 -> HostInstance(#0)
that instance's first physical device is: NVIDIA GeForce RTX 4060
  extension-name substitutions: 3 recorded (0 dropped)
    [0] "VK_KHR_win32_surface" advertised to the guest as "VK_KHR_android_surface" (from 0x…010c)
    [2] "VK_KHR_android_surface" sent to the driver as "VK_KHR_win32_surface"     (from 0x…01b4)
  pAllocator: NULL in all 1 vkCreateInstance call(s)
```

**The device name is the evidence, not the `VkResult`.** `VK_SUCCESS` proves nothing — a stub
returns it — and an NVIDIA string read back out of the instance the *guest* holds cannot be
fabricated.

The seam is `trait VulkanHost` in `omni-android`, implemented by `omni-gfx`, so `omni-gfx` stays a
**dev**-dependency and `cargo tree -p omni-android -e normal` still contains no `ash` and no
`libloading`. Three properties of its shape are load-bearing:

* **`has_instance_proc` returns `bool`, never a pointer.** The guest branches to whatever
  `vkGetInstanceProcAddr` gives it, so a host code address reaching translated ARM64 is a jump into
  x86-64 with an AAPCS64 frame. No type in the file can carry a host function pointer, so it cannot
  happen by mistake.
* **`HostInstance` is a token the host mints**; the driver's dispatchable pointer never leaves
  `omni-gfx`, and two indirections separate the guest from it.
* **`DriverAnswer` separates "the seam could not ask" from "the driver said no."**
  `VK_ERROR_INCOMPATIBLE_DRIVER` is an answer the engine branches on, so it is forwarded verbatim;
  collapsing the two would mean either refusing a legitimate decline or inventing a `VkResult`.

`VK_KHR_android_surface` is advertised where the driver offers `VK_KHR_win32_surface`, and the
guest's enabled list is rewritten back the other way. **Both directions are logged** (`vulkan::
rewrite`, bounded at 256, printed unconditionally including "no extension name was substituted in
this run"), because Global Constraint 1 is about exactly this and a silent rename is the defect it
names. The test checks it three ways — the guest reads the Android name, the guest does *not* read
the Win32 name, and the log names both spellings and the direction — so a list that came out right
by coincidence fails the third.

**`pAllocator` has not been observed for Roblox**, because the gate does not reach graphics yet.
What exists is the *instrument*: `allocator_calls()` is charged on the handler's first line, and
`report()` prints "vkCreateInstance was never entered, so nothing was observed" rather than
presenting zero-of-zero as evidence.

Known before stage 3: **`MAX_PROC_SLOTS = 64` is now reachable for real.** Stage 1 could only issue
five; a live instance means the driver answers for most of the instance-level set, and a renderer
resolving surface + swapchain + debug utils will be in the dozens. Raise it before stage 3, not
after.

### Stage 3 is built: a real surface on the real window, and a real device chosen

```text
the window's client area is 1024x576
vkCreateAndroidSurfaceKHR -> VkResult 0; the guest's VkSurfaceKHR handle is 0x2919292e0c0
vkEnumeratePhysicalDevices reported 1 device(s):
    0x2919292e040 -> "NVIDIA GeForce RTX 4060" (VkPhysicalDeviceType 2)
chosen: queue family 0 of 16 queue(s); memory: 5 type(s) across 3 heap(s)
    surface: currentExtent 1024x576, minImageCount 2; 7 format(s); present modes [2, 3, 1, 0, ...]
    263 device extension(s); the guest asked for 189 and got VkResult 5 -- VK_INCOMPLETE, as it must
vkCreateDevice     -> VkResult 0; the guest's VkDevice handle is 0x2919292e100
vkGetDeviceQueue   -> the guest's VkQueue handle is 0x2919292e140
vkGetDeviceProcAddr("vkCreateSwapchainKHR") -> guest thunk 0x29192926e70
```

Every number there was read out of **guest** memory after a real driver wrote it, from assembled
ARM64 branching through guest thunks. The `VK_INCOMPLETE` is not contrived: this driver reports 263
extensions = 68,380 bytes, which does not fit the 64 KB guest arena, so the guest asks for the 189
that fit and is told so. That is the truncating half of the two-count idiom exercised against a
real driver, which no test double can establish.

`MAX_PROC_SLOTS` went 64 -> **640**, measured rather than chosen: every entry point the engine can
ask for must exist as a NUL-terminated string in the binary, because `pName` is the only place the
API is named, and `libroblox.so` contains **592** distinct `vk[A-Z]...` strings.

**A third rewrite site appeared, and only because it was run.**
`vkGetInstanceProcAddr(instance, "vkCreateAndroidSurfaceKHR")` asked the driver, an NVIDIA driver
has never heard of that command, and the layer answered its NULL -- one call after telling the
engine `VK_KHR_android_surface` exists. That is a worse state than not advertising it. It is now
answered on this layer's authority, **conditional on the driver having the host's own surface entry
point**, so a host with no WSI still produces NULL and records nothing.

### Stage 4 is built: a frame presented from guest code, with its pixels asserted

```text
vkCreateSwapchainKHR    -> VkResult 0; the guest's VkSwapchainKHR is 0x2cf0e56e240
vkGetSwapchainImagesKHR -> 2 image(s)
vkAcquireNextImageKHR   -> VkResult 0, imageIndex 0
vkQueueSubmit / vkWaitForFences / vkQueuePresentKHR -> VkResult 0
read back 1024x576 from the presented image (VkFormat 44):
    cleared to [0.2, 0.6, 0.8, 1.0], which a UNORM stores as [51, 153, 204, 255]
    centre pixel  = [51, 153, 204, 255]
    corner pixels = [51, 153, 204, 255], [51, 153, 204, 255]
```

Sixty-three calls, all through guest thunks from assembled ARM64. The clear colour has four
distinct channels, none 0 or 255 in RGB, so a channel-order mistake or a buffer of zeros cannot
pass. **It is not a Roblox frame** -- it is this project's own test driving the guest path end to
end, and what it establishes is that the path exists and carries pixels.

**The read-back had to be built, and the reason is worth keeping.** `omni-gfx` does *not* read the
framebuffer back: `tests/renderer_live.rs`'s header records that `PrintWindow(PW_RENDERFULLCONTENT)`
returns solid black for a flip-model client area on this host. So the host copies the swapchain
image instead -- idle, `PRESENT_SRC_KHR` -> `TRANSFER_SRC_OPTIMAL`, `vkCmdCopyImageToBuffer`,
transition back. **It reads the pixels handed to the presentation engine, not a capture of the
monitor**, and that gap is the compositor's. The guest asks for `TRANSFER_SRC` itself; the host
never widens usage behind it.

**Two swapchains on one window is invalid and nothing here would report it.** There are no
validation layers on this machine, and the graphics spike crashed `nvoglv64.dll` silently doing
exactly that. `omni_gfx::claim` is a process-wide window -> owner table: `Renderer::new` and
`create_swapchain` both take a claim, and the conflict is a refusal **naming the other owner**
before any driver call. `oldSwapchain` **transfers** the claim rather than taking a fresh one, so
recreation never leaves the window unowned and a failed driver call puts the claim back.

`VK_ERROR_OUT_OF_DATE_KHR` and `VK_SUBOPTIMAL_KHR` reach the guest as the raw `i32`. `Acquired`
and `Presented` are deliberately **not** `DriverAnswer`, which would have to call suboptimal a
failure, and both use ash's *raw* entry points because its wrappers flatten suboptimal to a `bool`.
Nothing recreates a swapchain on the guest's behalf. Measured: NVIDIA's **first acquire after a
resize still returns `VK_SUCCESS`** and reports at present, which is conforming, and is why the
resize test drives whole frames rather than one acquire.

**`REGISTRY_BYTES` is 3648 of the 4096-byte data area** every embedding passes to
`BoundaryBuilder::new`. A first draft came to 8,640 and made every Vulkan test fail at `bind_into`
with `RegionFull`. Raising a bound past 4096 now means raising the data area in **every** embedding.

### The `vkMapMemory` answer: neither option, and the premise was wrong

**Recommendation: do not teach `omni-mem` about foreign ranges, and do not bounce. Make the memory
not foreign** -- allocate it out of `GuestSpace` and import it with `VK_EXT_external_memory_host`.

Measured on this machine:

```text
VK_EXT_external_memory_host is present (of 263 device extensions)
minImportedHostPointerAlignment = 4096            (= GuestSpace::page_size())
vkGetMemoryHostPointerPropertiesEXT(ordinary committed host memory) -> SUCCESS
  memoryTypeBits = 0xc  -> types 2 and 3, both HOST_VISIBLE | HOST_COHERENT
vkAllocateMemory(VkImportMemoryHostPointerInfoEXT) -> OK
vkMapMemory -> 0x1cdb4f89000, and the pointer imported was 0x1cdb4f89000 -- SAME
```

So `vkMapMemory` returns an address **already inside `GuestSpace`**, `admit` admits it with **zero
changes to `omni-mem`**, and `HOST_COHERENT` stays genuinely coherent because there is only one
copy of the bytes.

**Bouncing is not merely wrong here, it is impossible.** This driver's memory types:

```text
[0] (none)                                        heap 1
[1] DEVICE_LOCAL                                  heap 0
[2] HOST_VISIBLE | HOST_COHERENT                  heap 1
[3] HOST_VISIBLE | HOST_COHERENT | HOST_CACHED    heap 1
[4] DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT   heap 2
```

**Every** `HOST_VISIBLE` type is also `HOST_COHERENT`, so the engine is never required to call
`vkFlushMappedMemoryRanges` and a bounce buffer has no flush point at all. There is no memory type
on this GPU where the explicit-flush variant would even be legal.

What the import route costs, so it is weighed rather than discovered: it is an **extension**, not
guaranteed by the specification; the importable set (`0xc`) is a *subset* of the host-visible set,
so `vkGetPhysicalDeviceMemoryProperties` must mask out the types this layer cannot back -- **a
Global Constraint 1 rewrite needing its own entry in `Vulkan::rewrites`**, because an engine
choosing from a list this layer edited is not choosing from the driver's list; device-local,
non-host-visible allocations need no import and stay ordinary forwards, split at `vkAllocateMemory`
on the type index the guest already supplies; and guest-space commit charge now covers texture
uploads, so `GuestSpaceConfig::max_committed` (D15) becomes a streaming ceiling to set deliberately.

### Stage 2: what is trampolining and what is not

**Genuinely trampolining.** D4 identity mapping makes a validated guest pointer a host pointer, and
Vulkan's structures are fixed by the specification in fixed-width types — `long` never appears, and
`size_t` is 8 bytes on both aarch64 LP64 and x86-64 Windows LLP64 — so a
`const VkInstanceCreateInfo *` can go to the host driver unchanged after an `admit` check. Returns
are `void`, `VkResult` (i32) or a 64-bit handle, all of which `Ret` covers, and the arguments past
eight that `vkCmdWaitEvents` needs spill to the AAPCS64 overflow area `ImportCall::args()` already
walks.

**Not trampolining, hardest first:**

1. **`vkMapMemory` — ANSWERED, and the premise as written here was wrong.** See "the `vkMapMemory`
   answer" above. Neither of the two options this paragraph offered is the recommendation, and the
   sentence "`admit` refuses it and the guest cannot touch what it was given" is **false**: `admit`
   governs this layer's own shims, not the guest's loads and stores (D4 amendment 1).
2. **`vkCreateAndroidSurfaceKHR` has no host counterpart.** On Win32 it is
   `vkCreateWin32SurfaceKHR`, so the shim reads `VkAndroidSurfaceCreateInfoKHR.window`, resolves
   that `ANativeWindow *` to the host window behind it, and calls the Win32 path —
   `ndk::HostWindowSource` publishes only width and height today and must also carry
   `omni_platform::window::RawWindow`. Paired with it: the instance must be created with
   `VK_KHR_win32_surface` where the guest asked for `VK_KHR_android_surface`, and
   `vkEnumerateInstanceExtensionProperties` must **advertise** the Android extension or the engine
   will never try. Both are substitutions, and a silent rename is the defect class Global
   Constraint 1 exists for — each needs its own recorded rewrite log.
3. **Callbacks.** `VkAllocationCallbacks *pAllocator` and any debug messenger are guest function
   pointers a host driver cannot branch into, and the driver may call `pfnAllocation` on its own
   worker thread where there is no guest CPU context at all. **Refuse a non-null `pAllocator` and
   measure whether Roblox passes one** — most engines pass NULL — and build a re-entry trampoline
   only if it does. Any shim that can re-enter the guest must be `bind_reentrant`; `ImportCall`
   structurally cannot.
4. **Handles.** `VkDevice`/`VkQueue`/`VkCommandBuffer` are dispatchable host pointers the driver
   dereferences, so a wild one from the guest is a host crash reachable from guest data (Global
   Constraint 11). Each needs a registry like `ndk::handles::Slots`, never a cast.
5. **`vkGetInstanceProcAddr`/`vkGetDeviceProcAddr` must never return a host function pointer.** The
   guest branches to what it is given.

**Do not make `omni-gfx` a normal dependency of `omni-android`.** It would push `ash` and
`libloading` into every build of the adapter on all five targets, and `omni-gfx`'s own manifest
records `libloading` as a tolerated exception *inside that crate*. Define a `VulkanHost` trait in
`omni-android` and have `omni-gfx` implement it — the seam shape `AssetSource`, `WindowSource`,
`ThreadHost` and `HwcapPolicy` all already have, for this exact reason.

## Be clear-eyed about how far this is from a playable game

**That earlier warning was right and is kept, with its numbers corrected.** It said reaching §8 row
25 of 26 "sounds like 96%. It is not" — row 25 is where the engine *asks for* graphics, and it is
the door rather than the room. That is still the right way to think, and the list it gave has
largely been worked through:

* `crates/omni-gfx` is real, and it is no longer only the host half: `omni-android` drives it
  through the `VulkanHost` seam, and `ANativeWindow` geometry comes from a real resizable window
  rather than a constant.
* **Zero `egl*` symbols are still bound, and that is now a decision rather than a gap.** The engine
  bootstraps Vulkan itself through `dlopen`/`dlsym` (decoded at guest `0x02595160`), so the EGL
  path has never been needed. `libEGL.so`/`libGLESv2.so` remain `DT_NEEDED` and **which path the
  engine takes at runtime has still never been observed** — the Vulkan census will say, the first
  run that gets that far.
* `crates/omni-texture` (735 lines, ETC1 only per D27) still has **no consumer**. It becomes one
  the moment the engine uploads a texture.
* **No Vulkan validation layers are installed on this machine.** That is still true, it still
  hurts, and it has already cost one silent `nvoglv64.dll` crash. `omni_gfx::claim` exists because
  of it.

Honest position, weighted by effort rather than by checklist length: the structurally hard parts —
no VM, no JVM, no dex interpreter, a JIT with identity mapping, demand-paged guest memory, a
startup handshake that took three sessions — are behind. What remains is **broad rather than deep**:
the Vulkan surface an engine drives to produce a frame, which is mechanical (SPIR-V needs no
translation, the structs are byte-identical, the census names every entry point in call order) and
large.

The two named blockers above are the whole critical path. Neither is research.


## What this session changed

Four commits. In the order they matter rather than the order they happened:

**The hang was never a deadlock, and two instruments now make that class visible.** The census read
`FROZEN` because it had been switched *off* — `Boundary::census` keeps its counts when the flag
clears, so "off" and "stalled" are the same reading — while the un-gated `crossings().exits` showed
1.3 M imports a second. What was actually wrong: **three guest worker threads had been killed by
this layer and nothing said so**, because every assertion in the gate is about the thread it is
standing on. One held the future the blocked call was waiting for. Added:
`omni_bionic::unwind::frames` (a guest frame-pointer walk, so a parked thread names its caller
rather than a helper with ten call sites) and a gate assertion that fails on any guest thread this
layer kills. The assertion caught a **fourth** death on its first run, in the gate's *default* path,
happening on every ordinary run of this suite. `VERIFICATION.md` 15 and 16.

**`omni_mem::scan_reach`.** `GuestMem::cstr` bounded its walk with `admit(addr, 1, Read)`, which
reports the end of the first *entry* — and a lazy commit carves one mapping into a run of entries.
An ordinary NUL-terminated log line was refused "with no NUL in the first 96 bytes"; 96 was the
distance to the next granule.

**Networking, D30.** The owner withdrew Global Constraint 8. `omni-platform::net` is the only place
a socket call is made, sockets joined the descriptor table `fs` already owned, and the blanket
refusal was replaced by a policy an embedding sets. The guest's own OpenSSL now completes a real
TLS session.

**`Filesystem::open_misses`, and the certificate bundle.** An `ENOENT` is the quietest failure this
runtime can produce — `open` answers it correctly, the guest handles it correctly, and the
consequence lands elsewhere. Recording every missing path named
`/data/data/com.roblox.client/files/exe/cacert.pem` in one run. The APK ships it; the Java side
places it on a device; D7 says the Java side is defined rather than executed, so the host does it.
`HttpError: Unknown` became `HTTP 400`.

**Vulkan, stages 1–4.** The loader (the engine bootstraps Vulkan itself; decoded at `0x02595160`),
a real `VkInstance`, a real surface on the real window, a real device and queue, and a presented
frame with its pixels asserted. The `VulkanHost` seam keeps `omni-gfx` a dev-dependency, so no
`ash` or `libloading` reaches the adapter's normal graph on any target.

**D4 amendment 1**, which corrects this project's own documents: identity fastmem means `admit`
does **not** govern the guest's own loads and stores. Omnidroid is a compatibility layer, not a
sandbox.

**Nine symbols this session were already implemented and already unit-tested in `omni-bionic`, and
had simply never been wired to a symbol.** The primitives were written against the *import list*;
the wiring was done against *what the run had reached*. Expect more of these, and expect the gate to
name each one.


## Verification state

Run the suite yourself before trusting a number here; the point of this section is the *shape*, not
the totals. As of the last full run: `cargo test --workspace --release` green, clippy clean at
`-D warnings` across all targets, `cargo doc` adds no warning in any file this session touched, and
`cargo build --workspace --no-default-features` is clean.

The live graphics and window tests **fail rather than skip** when run without
`OMNI_GFX_WINDOW_TESTS=1`, which is `VERIFICATION.md` entry 4 applied deliberately: a skipped
fixture is a failure, so the gate for a test that needs a GPU panics rather than passing quietly.

**`tools/mutate.py` is the largest outstanding debt and it keeps growing**, because every session
adds behaviour faster than rows. It now holds roughly 500 rows. What is owed:

* **The whole table has never been run in one pass.** Per-prefix runs have been.
* **Five rows are stale in files nobody is editing** — listed under "Still open" below. Entry 8's
  remedy is a whole-table pre-flight, which is cheap and finds them all at once.
* **`--only sorder` and `--only jmid` must be re-run on an exclusive tree.**
* Rows are owed for this session's newest behaviour: the Vulkan handle registries (a forged
  non-dispatchable handle naming *another object* is the silent failure this guards), the
  extension-name rewrite log, the `counted::enumerate` two-count idiom, and the swapchain claim.


## Still open, carried forward

**Closed since this list was written**, so nobody re-opens them: `nativeSetPlatformHeadersWithIdfa`
returns (it was `strchr` with no terminator arm, and 21 of 21 scripted downcalls now return); the
`ALooper_pollOnce(-1)` question; §8 rows 17-22; the "two idle workers" deadlock, which was never a
deadlock (see D4 amendment 1 and `VERIFICATION.md` entries 15 and 16).

**Live, and each one is a real finding rather than a chore:**

* **Five mutation rows are stale in files nobody is editing**: `android-mem-A1` (`src/mem.rs`),
  `boundary-A11`, `boundary-A12` (`src/boundary.rs`), `dl-A5` (`bionic/dl.rs`, staled by the Vulkan
  work), `looper-B2` (`ndk/looper.rs`, **committed** staleness -- its pattern matches zero times in
  `HEAD` too). `VERIFICATION.md` entry 8's remedy is a whole-table pre-flight; run one.
* **The 498-row mutation table has never been run in one pass.** Per-prefix runs have been.
* **`sockcfg-A3` is a kept, measured MISS**: Winsock refuses a zero keep-alive figure itself with
  an error this seam maps to `EINVAL`, so no test on this host can separate the check from the
  host's own refusal. The doc comment says what would make it load-bearing.
* **One flake, recorded rather than chased**: one run in three ended with a guest thread taking a
  `MemoryFault` at teardown -- a futex-woken thread reading memory already unmapped.
* **`getpeername`, `strspn`, and the rest of the socket group are deliberately unbound.** Each has
  a working primitive and no run has reached it. That is the rule, not an oversight.
* **`GfxVulkanHost` has no `vkDestroyInstance`/`vkDestroySurfaceKHR`/`vkDestroyDevice`**, so those
  three accumulate. Stage 4's seven handle families do support removal.
* **`Vulkan::set_host` is last-writer-wins across five handle families.** A replaced host leaves
  tokens the new host never issued; each resolves to a refusal by name. Documented, not fixed.
* **`/proc/meminfo` and `/proc/self/statm` are opened and missing.** The telemetry thread logs and
  continues; not blocking. If ever answered, they must come from `Bionic::memory_budget()` and
  `omni_mem::process_commit_charge()`, never invented.
* **`sched_yield` runs at ~20 M per run.** Unchanged for several milestones; a guest spin loop.
* **`nativeSetPlatformHeadersWithIdfa`'s headers may be why the settings request is a 400.**
  Unverified -- it returns, but what it set has never been read back.
* **W1b** — `%.Ne/E/g/G/a/A` with a precision above 64 KiB still refuses by name. `format_exp`
  carries the mantissa through `10u64.pow(precision.min(15))`, so past fifteen places this engine's
  digits are *already* not `vsnprintf`'s; under a budget those wrong digits would become visible.
  Closing it means making the float engine exact past fifteen digits.
* **S1 and T3** — a `%s` argument or a log tag longer than `STRING_LIMIT` refuses where a device
  copies what fits. Needs a *bounded* `cstr` in `crates/omni-android/src/mem.rs`.
* **`trunc-A1` is proposed but deliberately not in the table**: it reverts the budget at the
  `__android_log_print` call site and its only possible detector lives in `tests/bionic.rs`. A row
  whose command cannot catch it reports a MISS that reads as a missing test.
* **`android/app/Application.getResources` is `Answer::Unanswered`** while `android/content/Context`
  answers it properly. Nothing on the measured path reaches it; if anything ever does, it refuses.
* **bionic's own `strftime` behaviour is unverified** — no NDK and no bionic source on this machine.
  If bionic accepts `%k`/`%l`/`%s`, this refuses six calls bionic answers.
* **`AT_HWCAP` is still undecided** (D26). `sched_yield` was called **22,387,975 times** in one run
  — the decline path's fallback spinning. Revisit under real thread load.
* **L1-L7, N1-N3** minor review findings remain.

## Do not re-litigate without new evidence

* **D7** — no JVM, no ART, no dex interpreter. The Java side is *defined*, not executed. Five
  milestones have been reached on that bet.
* **D19** — `omni-bionic` has zero dependencies and no OS access. Its `GuestMemory` has only `read`
  and `write`; it therefore *cannot* validate a mapping, which is why buffer admission lives in the
  adapter. A defaulted trait probe returning `Ok(())` is a plausible stub for every implementer.
* **D27** — the APK is **ETC1 only**. Zero ETC2-only blocks, zero ASTC/EAC/PVRTC bytes anywhere.
* **Only Windows x86-64 is developed and tested.** `omni-platform` is the only crate that may
  contain `cfg(target_os)` or an OS crate. Never claim a non-Windows target works.
