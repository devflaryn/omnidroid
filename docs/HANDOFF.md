# Handoff

Written 2026-09-19 for a fresh session. This file is a pointer and a state snapshot, not a history.
The durable sources of truth are `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`, `docs/STATUS.md`, the M3
plan and ledger, and git history.

## Git state

| | |
|---|---|
| Current branch | **`android-abi`** (M3 work) |
| Working tree | **clean**, nothing uncommitted |
| HEAD | see `git log` — M3 task 2 complete |
| Other branches | `cpu-execution` (M2, complete), `foundation` (M0/M1, complete), `main` (behind — holds only early docs) |
| Remotes | **none configured** |
| Merge state | Nothing has been merged to `main`. Each milestone branched from the previous one. **The user has never been asked to approve a merge; do not merge without asking.** |

Branch lineage: `main` → `foundation` → `cpu-execution` → `android-abi`.

## Verification state

**608 passing, 0 failing, 12 ignored** (`cargo test --workspace --release`). Clippy clean on
`--all-targets`, `cargo doc` clean, `--no-default-features` builds — and that last one is now
*verified* rather than assumed: `cargo tree -p omni-android -e normal` has no `dynarmic-sys` in it.
With `workspace = true` a member's `default-features = false` is **ignored**, so the omni-android
dependency on omni-cpu spells its path out; see the comment in that manifest.

Three committed mutation harnesses, all restoring the tree byte-for-byte and all with a pre-flight
gate that refuses to run against a modified tree:

| Harness | Rows |
|---|---|
| `tools/mutate.py` (workspace) | **118** |
| `crates/dynarmic-sys/tools/mutate_shim.py` | 23 |
| `crates/omni-elf/tools/mutate_loader.py` | 18 |

Other committed tools, each self-checking against a known count: `tools/thunk_sweep.py`,
`tools/init_reach.py`, `tools/atomic_mix.py`, `tools/branch_mix.py`.

## Milestones

| | Status | Evidence |
|---|---|---|
| **M0** APK parsed, libraries extracted | **Reached** | All 11 arm64-v8a libraries extracted to a content-addressed 4 KB-aligned cache and mapped from it. Exactly one entry in the whole APK is directly mappable (a 1,447-byte icon) — no library is, which is why the cache exists |
| **M1** ELF loaded and relocated | **Reached** | All **568,806** relocations applied (568,272 APS2 + 534 `DT_JMPREL`) and **read back out of mapped memory**, RELRO sealed over exactly 5,205,568 bytes with a child process asserting the fault, 565 imports enumerated, 3,594 initializers collected |
| **M2** Real ARM64 Roblox code executes | **Reached** | Three real `libroblox.so` functions run with `init_array` deliberately **not** run. A base64 function returns **256 predicted values, one per byte**, predicted from RFC 4648 — and a reviewer hand-decoded all 26 words and wrote an independent interpreter to confirm. A stack-protected leaf proves D13 three ways including both failure paths |
| **M3** All 3,594 initializers | **In progress** — Task 1 of 4 complete | See below |
| M4-M8 | Not started | JNI, GameActivity, Vulkan, first frame, interactive |

## M3 progress — exact

Plan: `docs/plans/android-abi-plan.md`. Ledger: `.superpowers/sdd/android-abi-plan/progress.md`.

- **Task 1 (measure before building) — complete, reviewed, approved.** Commits `d44a516`..`672ddc2`.
- **Task 2 (the thunk boundary) — IN FLIGHT** as of this writing, dispatched from `c553177`. A
  subagent is implementing it in the session that wrote this handoff. **Subagents do not survive a
  session change**, so if you are reading this in a fresh session, check `git log` first: if Task 2
  has commits, review them; if it has none, simply re-dispatch it. Nothing is lost either way, because
  the task's requirements are fully specified in the plan and in D17.
- Task 3 (the bionic subset) — not started. Scope is fixed at **170 thunk functions + 18 data objects**.
- Task 4 (all 3,594 initializers, the M3 gate) — not started.

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

## Next action

**Start M3 Task 2, the thunk boundary.** Generate its brief with
`scripts/task-brief docs/plans/android-abi-plan.md 2` from the subagent-driven-development skill, and
dispatch an implementer. The plan's Task 2 section is the spec; D17 is the design.

Task 2 must deliver: AAPCS64 ↔ host ABI marshalling in both directions (integers and pointers in
X0-X7, floats in V0-V7, stack arguments beyond that, returns in X0/X1/V0, and **variadics**, which
several libc imports need); host → guest callbacks (a `pthread` entry, an `atexit` handler, a `qsort`
comparator); an ARM64-host path that stays expressible though untestable here; and an unbound symbol
failing with a **typed error naming the symbol and guest address**, never a crash or a silent zero.

## Blockers and risks

| Risk | State |
|---|---|
| **Test APK is cheat-injected** | `Roblox-2.738.1397.apk` is signed by "Gloop", not Roblox, with an injected Luau executor in `libzstd-jni`. `libroblox.so` itself is stock. **A stock Play-signed APK has been requested from the user and never supplied.** Design is from the stock engine only (D6). Still the correct thing to ask for |
| **16 MiB per guest thread** | A fixed array dynarmic allocates and *writes* even when its feature is disabled — and D16 says to run with it disabled. Fork patch written up in `crates/dynarmic-sys/patches/README.md`, not applied. ~512 MiB at 32 threads. Directly threatens the multi-instance memory requirement |
| **W^X does not hold for the code cache** | Recorded as an explicit exception under D12. Enabling dynarmic's no-execute option makes **upstream's own suite segfault**. Under identity mapping the cache is guest-writable in principle, gated only by ASLR |
| **`AT_HWCAP` is an open decision for M3** | Roblox's atomics are one population behind a single flag we own. Advertise LSE → 53 hard interpreter halts; decline → 106 fallback arms into a global spinlock that anti-scales 21x. Both arms measured; the choice is unmade |
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
- Per guest thread **24.5 MiB** (n=8); shrinking the code cache does **not** help.
- Roblox shape: 2.27% indirect, **4.30 instructions per basic block**, **128 exclusive-monitor sites
  against 53 LSE** (29.3% of atomic RMW). All static mixes, used as proxies.
- Texture formats: **neither ETC2 nor ASTC** on the dev GPU; BC1/BC3/BC7 yes. Runtime transcoding is
  mandatory from M6.

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
| `malloc` is the host allocator, so the guest heap is the host heap | `libroblox.so` imports **no allocator at all**; the seam is guest `mmap` through the demand pager |
| D5's warm figure is an entry ceiling excluding the exit | It was always entry-**and**-exit. This error was the controller's, and it propagated into a report and three code comments |
| Thunk design ratio is 7.6x | **3x** loader-shaped. 7.6x holds the PLT stub out |
| A 19-24 ns PLT-stub effect | Re-measured at 10.7 ns and inside an unexplained bimodality. Observation kept, figure dropped |

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

---

# START HERE

Read in this order:

1. **`docs/HANDOFF.md`** — this file.
2. **`docs/STATUS.md`** — the honest capability matrix. Distinguishes verified from planned; nothing is
   claimed for Linux or macOS.
3. **`docs/ARCHITECTURE.md`** §§1, 5, 6 — the identity-mapping bet (§1), the Android compatibility
   layer M3 is building (§5), and ARM64 execution (§6).
4. **`docs/DECISIONS.md`** — **D17 first** (it is M3's design), then D4, D10, D13, D16. Read the
   amendments and corrections inline; several entries supersede their own earlier text.
5. **`docs/plans/android-abi-plan.md`** — the M3 plan. Read the Global Constraints, then Task 2.
6. **`.superpowers/sdd/android-abi-plan/progress.md`** — the ledger: Task 1's rulings, the open
   question, and the preflight conflict scan.
7. **`.superpowers/sdd/android-abi-plan/task-1-report.md`** and **`task-1-review.md`** — only if Task
   2 needs the measurement detail behind D17.

**First concrete action:** check `git log --oneline c553177..HEAD` for Task 2 commits.

- **If there are none**, generate the Task 2 brief with the subagent-driven-development skill's
  `scripts/task-brief docs/plans/android-abi-plan.md 2` and dispatch one implementer. The brief already
  exists at `.superpowers/sdd/android-abi-plan/task-2-brief.md` if the workspace survived.
- **If there are commits**, Task 2 got partway in the previous session. Read its report if present,
  then review what landed before continuing — do not assume it is complete just because commits exist. Carry into its dispatch: D17's in-loop dispatch decision, that the dispatcher-side MXCSR
guard already exists and must not be moved or removed, and that an unbound symbol must fail with a
typed error naming the symbol and guest address.

Do not start Task 3 in parallel — it consumes Task 2's boundary API directly.
