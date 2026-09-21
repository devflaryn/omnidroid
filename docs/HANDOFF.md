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

**1,059 passing, 0 failing, 12 ignored** (`cargo test --workspace --release`, 2026-09-21 — was
1,004 before M3 task 3 phase 3b, 959 before phase 3a, 926 before phase 2, 877 before phase 1, and
608 before `omni-bionic` existed, so those are not comparable). **The whole mutation table has been
run on the committed tree: 239/239 caught.** Clippy clean on
`--all-targets`, `cargo doc` clean, `--no-default-features` builds — and that last one is now
*verified* rather than assumed: `cargo tree -p omni-android -e normal` has no `dynarmic-sys` in it.
With `workspace = true` a member's `default-features = false` is **ignored**, so the omni-android
dependency on omni-cpu spells its path out; see the comment in that manifest.

Three committed mutation harnesses, all restoring the tree byte-for-byte and all with a pre-flight
gate that refuses to run against a modified tree:

| Harness | Rows |
|---|---|
| `tools/mutate.py` (workspace) | **239**, all caught on a full run |
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
| `omni-platform` beyond `vm`/`fault` | **`clock`, `process`, `log` and `fs` exist** (phases 3a and 3b, D22 and D23). Most of their primitives are portable `std` and are implemented once; the four that need a target-specific call — entropy, the current processor number, `pread` and `statvfs` — have a Windows backend and structural `linux.rs`/`macos.rs` naming `getrandom(2)`, `arc4random_buf(3)`, `sched_getcpu(3)`, `pread(2)` and `statvfs(3)`. **macOS has no `sched_getcpu` and no supported equivalent**, so that one is expected to stay a refusal there |
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
  Phase 3b, this session: **`omni-platform` gained `fs`, a ROOTED filesystem** — every guest path
  resolves inside one host directory the embedding supplies, and an instance with no root refuses
  every path call by name — plus bionic's `FILE*` layer in `omni-bionic` over a trait, and the 29
  file-io symbols over both. **166 of the 188** now covered (148 thunk functions + 18 data
  objects), 55 new tests, mutation **213 → 239** (26 new, 26/26 caught). Durable record is **D23**.
  Phases 3c (sockets and polling) and 3d (thread lifecycle) remain; scope for the whole task is
  **170 thunk functions + 18 data objects**.
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
| Sockets and polling, thread lifecycle | **More `omni-platform` surface** | **YES** — phases 3c, 3d |

**That blocker is now mostly cleared.** `omni-platform` was `vm` and `fault` and nothing else
until phase 3a, which added `clock`, `process` and `log` (D22); phase 3b added `fs` (D23). What is
still missing is sockets and threads — ARCHITECTURE §2 describes the crate as covering "virtual
memory, threads, files, dynamic loading, clocks, windowing", and **four** of those six are now real
rather than aspirational. The portability invariant itself is intact throughout (verified: every
`cfg(target_os)` mention outside `omni-platform` is a doc comment stating the rule, not an escape
from it).

So the last groups cannot be written without extending `omni-platform` first, and per the five-target
guidance that extension must add the **Linux and macOS signatures as honest `unsupported` returns at
the same time, naming the intended POSIX call**. Do not write speculative `mmap`/`open` bodies for
those targets — that was ruled against deliberately.

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
5. **Next:** sockets and polling (3c), then thread lifecycle and signals (3d), then the six the
   plan's `3e` row collects.

## Next action

**M3 Task 3 phase 3c: sockets and polling.** Phases 1, 2, 3a and 3b are complete and committed
(D20, D21, D22, D23). **166 of the 188 reachable imports are covered** — 148 thunk functions and
all 18 `STT_OBJECT` data objects — and **22** are left. The list is derived, not asserted: the 188
of `init-reachable-imports.txt` minus the 148 bound and the 18 placed, classified with
`tools/os_surface.py` — and the adapter's own test now asserts that remainder as a **set
difference** rather than as a total, so a substitution in it cannot pass.

| group | count | symbols |
|---|---|---|
| ~~**file-io** — phase 3b~~ | ~~29~~ | **DONE** (D23) |
| **network** — phase 3c | **8** | `socket`, `poll`, `select`, `eventfd`, `getaddrinfo`, `freeaddrinfo`, `gai_strerror`, `inet_ntop` |
| **threads-sync** — phase 3d | **8** | `pthread_create`, `pthread_join`, `pthread_detach`, `pthread_getschedparam`, `pthread_sigmask`, and **three signal symbols the plan's phase table never assigned to any phase**: `sigaction`, `sigfillset`, `raise` |
| time-clocks | 2 | `clock` (process CPU time, which needs `GetProcessTimes`-shaped surface) and `time` (one line over `clock::realtime_now`, deliberately left for whoever binds `clock` beside it) |
| no group | 4 | `mallinfo`, which returns 80 bytes indirectly through `X8` and would have to describe a libc heap `libroblox.so` does not have; `longjmp`, which is guest control flow and not an OS call at all; and the two `__gcov_*` |

**Three of those are a finding rather than a list entry.** `sigaction`, `sigfillset` and `raise` are
in the reachable 188 and appear in **no row** of the plan's phase-3 table — the classifier files them
under `threads-sync`, and the table's thread row is about lifecycle and scheduling. Signals are their
own problem (the guest's signal state is exactly what `pthread_sigmask` was excluded from
`omni-bionic` for, D19), and whoever picks up 3d should decide whether they belong there or in a
phase of their own. `longjmp` is likewise unassigned and is the odd one out in the other direction:
it needs no OS at all, only the guest's own `jmp_buf` and a way to resume the guest at a saved PC.

**The blocker is now mostly cleared.** `omni-platform` was `vm` and `fault` and nothing else
until phase 3a, which added `clock`, `process` and `log`; phase 3b added `fs`. What is still
missing is sockets and threads.

Per the five-target guidance, that extension must add the **Linux and macOS signatures as honest
`unsupported` returns at the same time, naming the intended POSIX call**. Do not write speculative
`socket`/`poll` bodies for those targets — that was ruled against deliberately, because an
unverified body misbehaves silently where a typed error fails immediately. **And read D22's other
half before copying the pattern:** a primitive that calls no OS API at all must *not* be given a
fabricated `unsupported` arm, because that is a false claim in the other direction.

**A prediction this file made about phase 3b was wrong, and the correction is the useful part.**
It said "Files will be the opposite balance, and almost all of them will need [the structural unix
half]". The opposite happened: **fifteen of the seventeen** file primitives are one portable `std`
call and are implemented once, and only `pread` and `statvfs` needed a backend. D23 records the
sharper test that produced that answer, and it is the one to apply to sockets rather than the
guess: not "does it call the OS", but **is there one `std` call that serves all five targets?**
`std::net` will answer yes for rather less of `socket`/`poll`/`select` than `std::fs` did, but the
question is the same one and the answer is worth measuring rather than assuming in either
direction.

What phases 2, 3a and 3b leave for whoever picks this up:

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
- **`fprintf` and `vfprintf` are still refusals and are now one binding away.** Both halves exist:
  `format::render` and phase 3b's `stdio`. Phase 3b deliberately left the binding out of its scope
  of 29 and corrected the refusal text, which used to say `omni-platform` had no file surface.
- **`ReentrantCall::invalidate_code` reaches one context.** `munmap` and `mprotect` discard the
  calling thread's translations; a second guest thread that had already translated the same range
  keeps its own. Closing that needs a registry of live contexts, which belongs with thread lifecycle.
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

## Blockers and risks

| Risk | State |
|---|---|
| **Test APK is cheat-injected** | `Roblox-2.738.1397.apk` is signed by "Gloop", not Roblox, with an injected Luau executor in `libzstd-jni`. `libroblox.so` itself is stock. **A stock Play-signed APK has been requested from the user and never supplied.** Design is from the stock engine only (D6). Still the correct thing to ask for |
| **16 MiB per guest thread** | A fixed array dynarmic allocates and *writes* even when its feature is disabled — and D16 says to run with it disabled. Fork patch written up in `crates/dynarmic-sys/patches/README.md`, not applied. ~512 MiB at 32 threads. Directly threatens the multi-instance memory requirement |
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
| 88 of the 188 reachable imports are implemented in `omni-bionic` | **79.** 88 was the naive any-mention grep minus one known exclusion; the stated method, run, gives 82, of which five are English words in unrelated prose. Wrong by nine, in the direction that makes the remaining work look smaller (D20) |
| `FileExt::seek_read` on Windows is `pread` | It is **not**: it moves the descriptor's own file pointer, so a `pread` built on it alone leaves the next sequential `read` at end of file with every call reporting `Ok`. MEASURED on a ten-byte file: `read(4)`, `pread(3, offset 7)`, `read(3)` gave **0 bytes** instead of `456`. The position is saved and restored now (D23) |
| `fprintf` cannot be serviced because `omni-platform` has no file surface | It has one as of phase 3b. The refusal text said this and was corrected: what is missing now is only the binding of the `printf` family onto the stream layer (D23) |
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
