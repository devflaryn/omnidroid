# Handoff

Written 2026-09-19 for a fresh session. This file is a pointer and a state snapshot, not a history.
The durable sources of truth are `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`, `docs/STATUS.md`, the M3
plan and ledger, and git history.

## Git state

| | |
|---|---|
| Current branch | **`bionic-threads`** (M3 task 3 work) |
| Working tree | **clean**, nothing uncommitted (`.freebuff/` is untracked scratch) |
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

**864 passing, 0 failing, 12 ignored** (`cargo test --workspace --release`, 2026-09-20 — was 608
before `omni-bionic` existed, so the two are not comparable). Clippy clean on
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
| OS surface confined to `omni-platform` | Verified across all six crates — no `cfg(target_os)` or OS crate escapes it |
| `omni-cpu` builds with **no C++ toolchain at all** (`--no-default-features`) | Guarded by a CI job. That guard was itself found blind once and fixed — read its comment before touching it |
| ARM64-native CPU path | **Expressible, untested, not claimed.** No trait method requires emitting a byte |
| ARM64-native thunk veneer | Designed (four instructions, 16-byte slots sized for it), untested |
| Linux / macOS virtual memory, faults, JIT arena | **Not implemented.** The JIT arena's dual-mapping is Windows-specific in mechanics; Linux has `memfd_create` + two `mmap`s, macOS has `MAP_JIT` with `pthread_jit_write_protect_np` which behaves differently and needs its own measurement |
| Graphics | Vulkan verified on this host only (native resizable window, real triangle). D3D12/Metal are a renderer-trait seam that does not exist yet — graphics starts at M6 |
| One Windows-only *gap* worth knowing | `unmap` is whole-view-only on Windows and must be emulated; Linux does not have this restriction, so that emulation is Windows-specific complexity, not shared design |

**Practical guidance:** when a task adds a platform primitive, add the Linux and macOS signatures as
honest `unsupported` returns at the same time, naming the intended syscall. It costs minutes, it keeps
the seam shaped correctly, and it is how the non-Windows bring-up later becomes a fill-in rather than a
redesign. Do **not** write speculative `mmap` bodies — that was ruled against deliberately, because an
unverified body can misbehave silently where a typed error fails immediately and visibly.

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
- **Task 2 (the thunk boundary) — implemented, commits `04186ed`..`e245bfe`. REVIEWED 2026-09-20;
  three defects to fix before Task 3** (`.superpowers/sdd/android-abi-plan/task-2-review.md`).
  Spec ✅ per the implementer; durable record is **D18**. It built the region, AAPCS64 marshalling both
  ways, the variadic rules and a guest `va_list` walk, checked guest memory, the symbol table, and
  host→guest re-entry. **No symbol is implemented** — all 565 slots are `Unbound` and name themselves
  when called. Tests went 496 → 608, mutation **89 → 118** rows (29 new), all caught.
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

## GLM work — review state

Four pieces of work arrived unreviewed; three from GLM 5.3 Flash. Review is in progress and its
findings are recorded as they land. **Nothing here has been accepted because it reports itself
complete.**

| Piece | State |
|---|---|
| `android-abi` M3 Task 2 (Claude) | **Reviewed.** F1/F2/F3 to fix before Task 3 — see `task-2-review.md` |
| `bionic-pure` (GLM) — 83 pure libc/libm functions | **Partially verified.** errno constants and the bionic byte-difference compare convention check out; the `tools/mutate.py` table has **zero** rows for `omni-bionic` |
| `bionic-threads` (GLM) — pthread/sync/TLS | **Partially verified.** One real defect found and fixed (`sem_post` consumed the waiter flag; 1.0104 s stall measured); three timing flakes fixed |
| `os-surface-inventory.md` + `tools/os_surface.py` (GLM) | **Not yet reviewed.** |

**Confirmed defects found in GLM's work so far**, both now fixed:

1. **`sem_post` consumed the waiter flag other waiters still needed.** `post` computed
   `(word & !WAITERS) + 1` under a comment reading "keep flag state" — the opposite of what it does;
   `wait`/`trywait` cleared it too. The first post consumed the flag, so a second post skipped its
   futex wake. **Measured: 1.0104 s** for a posted token to reach a blocked waiter. The suite could
   not see it — every `sem_wait` loops on a bounded slice, so a lost wake always *eventually* healed.
   Exercising, not detecting.
2. **Three timing flakes**, two root causes: six wall-clock assertions with zero headroom (a 150 ms
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

## The straddling-access defect — fix this before Task 3

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

## Next action

**Fix the straddling-access defect above, then start Task 3 (the bionic subset).** F1, F5 and F3 are
already fixed, with hostile tests and mutation rows (`varargs-A5`..`A8`, `abi-A6`, `boundary-A12`,
6/6 caught). F4 and F6-F10 from the review remain open and are not blockers.

Then: decide whether `omni-bionic` folds into `omni-android` (ARCHITECTURE §2 puts bionic there; the
separate crate was only for isolation during review), write the adapter binding the bionic layer to
the thunk boundary, and run M3's gate — all 3,594 static initializers complete, **verified by reading
back state they actually wrote**, not by a counter reaching 3,594.

**A mutation harness for `omni-bionic` is still owed.** `tools/mutate.py` has 118 rows and **none**
of them touch the 12,543 lines of bionic code; every other crate has one. GLM did hand spot-checks
(commit `26b3a1c`) but left nothing durable.

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

**First concrete action: review Task 2.** It is implemented and unreviewed, which is the one gap in
the chain — every other completed task in this project was reviewed, and reviews have caught a Critical
or a wrong figure in almost every one, including a use-after-free and four wrong numbers that had
already been written down as fact.

Build the package as `git diff -U10 c553177..e245bfe -- crates/ tools/ ':!crates/dynarmic-sys/vendor'`
and dispatch a reviewer against `.superpowers/sdd/android-abi-plan/task-2-brief.md`,
`task-2-report.md`, and D18. Ask it specifically to check: the AAPCS64 **variadic** rules (they differ
from the fixed-argument rules, and 13 of the reachable 188 are variadic); the guest `va_list` walk
against hostile input; that an inline handler genuinely cannot re-enter the guest, which the
implementer made a *type* property rather than a rule; and the two late defects it found in its own
code (see below) for whether their fixes are complete.

Then Task 3 — the bionic subset, 170 functions + 18 data objects. Do not start it before Task 2's
review closes; it consumes that boundary directly. Carry into its dispatch: D17's in-loop dispatch decision, that the dispatcher-side MXCSR
guard already exists and must not be moved or removed, and that an unbound symbol must fail with a
typed error naming the symbol and guest address.

Do not start Task 3 in parallel — it consumes Task 2's boundary API directly.
