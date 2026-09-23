# Windows: branch `perf-windows` (from `base-0923-night`)

The Windows machine's share of the three-machine night of 2026-09-23/24 (HANDOFF, "Three machines,
one source"): performance and memory of the real arm64-v8a Roblox APK on Windows x86-64, plus the
two fixes that in-world runs depend on. Everything below is MEASURED unless it says otherwise;
runs and scripts are in the session scratchpad (`0b762c51-…/scratchpad/runs`, `memrun.sh`,
`multi.sh`, `memmap.py`, `play.sh`).

## Commits, in order

| commit | what |
|---|---|
| `ced3e1d` | merge of `perf-world` (instruments: `OMNI_PERF*`, `omni_platform::sampler`, JIT counters, the `ExclusiveMonitor` option, the lost-update stress test), reviewed as a diff against its merge base |
| `a5e35c8` | the review's four fixes: D31 written (cited, never existed); no guest-thread panic in `perf_record`; saturating Vulkan count deltas; two string literals that had lost their `\` continuations; `tests/perf.rs` gated on `target_arch` only |
| `cdf1334` | `__vsprintf_chk` bound (bionic `fortify.cpp` order: `vsnprintf`, then `__check_buffer_access`); rows `vsprintf-` 6/6 |
| `83cfa6e` | a death that hangs the close ends as `REASON_CRASH_NATIVE`; the watchdog halts the UI thread's call; `OMNI_INJECT_DEATH`; rows `crashclose-` 3/3 |
| `c18e0eb` | vendored dynarmic patch 0002 (D32): per-thread fixed JIT cost on demand |
| `fa7e136` | `OMNI_IMPORT_CENSUS=off`, the in-world A/B switch for the census |

Mutation: `inbound-` 10/10 (step 0, never run before), `vsprintf-` 6/6, `crashclose-` 3/3, each
with the tree to itself. Patch 0002's two halves were proven by hand-applied regressions (the
harness cannot rebuild vendored C++): both detected, tree restored byte-identical (sha1).
Whole affected suites (release): everything passes except the **headless** `gameactivity` gate,
which fails on HANDOFF open item 7's two unchanged causes (Windows 1224 re-opening
`memProfStorage<pid>.json` under a live shared mapping; `eglGetDisplay` with no window).

## Step 2: why a worker death froze the game, and why the sign-in was "never kept"

* **The freeze** is a lost TaskScheduler job, not a held lock: p2's watchdog shows no thread in
  `pthread_mutex_lock`, the pools idle in `pthread_cond_wait`, and the game loop spinning 3.8e9
  `syscall` crossings. The dead worker's job never completes; the frame and the close's
  `drainRenderingJobs` wait for it. Nothing faithful heals a lost job -- on a device the fatal
  signal ended the process.
* **The close "spin"** was the watchdog's own doing: stopping the guest threads made every
  `pthread_cond_wait` return at once, and the glue's predicate loop burned 2e9 instructions.
* **The sign-in**: the directory a hung close left could not launch again. Relaunched with no exit
  record, the engine infers a crash and dies in its own report (`InferredCrash+0xc8` null, link
  `0x2383500`): 0 presents, a hung close (relaunch-a). With the `REASON_CRASH_NATIVE` a device
  records, the same directory reaches Landing and closes cleanly (relaunch-b) -- **but logged
  out**: the account session is not persisted mid-session in a form a relaunch uses. On a device
  the Java side keeps the engine's cookies (`onSetCookie` is a Sink here); that is credential
  storage, the owner's decision, **not built**. A clean close is what keeps a sign-in: sign in,
  close at Home with the X, and run from copies of that directory.
* The `InferredCrash` null member also killed a worker at +5 s on a **fresh** install once (1 of 3
  concurrent instances) -- it is not only a post-crash path. Harmless to the run; open.

## Step 4: memory (MEASURED, logged-out landing, +100 s, ~45 guest JITs)

| | commit | working set |
|---|---|---|
| `83cfa6e` | 3,157 MiB | 2,528 MiB |
| `c18e0eb` (patch 0002) | **2,105 MiB** | **1,884 MiB** |

* Per guest thread (omni-cpu's own test, n = 8): **24.56 → 4.47 MiB**. Before: a 16 MiB
  fast-dispatch table (constructed and written; FastDispatch is off, D16) and 18 MiB committed up
  front per code cache (least-used caches touched 3,200 KiB). The **32 MiB-per-thread JIT cache**
  now costs address space (1.4 GB reserved for 45 threads) and commit only for code kept: 5 MiB for
  an idle thread, up to 32 for a busy one.
* **Instances started one by one** (3 on this PC, `multi.sh`): each adds **~2.0-2.1 GB commit and
  ~1.75 GB RAM**, linearly -- no sharing beyond the library image. ~10 on 8 GB needs ~0.8 GB each.
  What remains per instance: the guest's own ~1.1 GB (its heap granules, stacks and blocks, pages
  it touched -- the 64 KiB granule rules out pager over-commit), ~0.4 GB of images, ~0.5 GB of
  translated code, duplicated per thread and per instance. The next lever is sharing translated
  code (a large dynarmic change, not started).
* Code-cache size: 128 MiB instead of 32 cuts load-phase retranslation 40% (251,904 → 150,933
  blocks retranslated over +10..+40 s) for +340 MiB commit and no earlier Landing. **Default kept
  at 32**; `OMNI_JIT_CACHE_MB` is the in-world A/B.

## Step 3: performance -- NOT YET MEASURED IN A WORLD

No in-world run was possible: the owner's sign-in window stood open 43 minutes (01:26) with no
sign-in, then was closed cleanly. On the landing: census on/off and global/value-compare monitor
make no measurable difference (the one hot thread is the game loop's `ALooper_pollOnce` spin at
~92 M guest insn/s, 4.1 M crossings/s, `mon` 0%). The benchmark leads (census contention, the
monitor at 256 slots, return prediction) need a world with many busy workers to judge, and nothing
was changed without that evidence. Protocol for the owner's next session: HANDOFF, "Windows
2026-09-24".

## Merge notes (for `port-macos` / `port-linux`)

Nothing in any `macos.rs`, `unix.rs`, `linux.rs` or their Cargo sections was touched. Changes to
shared code are additive except where marked:

* **`omni-platform::sampler`** (new module, from `perf-world`): `windows.rs` implements it;
  `unsupported.rs` is compiled for every other target and returns `SamplerError::Unsupported`
  naming the intended mechanism. A Linux/macOS sampler is a new `sampler/linux.rs` /
  `sampler/macos.rs` plus the `cfg` lines in `sampler/mod.rs`. `omni-android/tests/perf.rs` fails
  by name off Windows until one exists (it is `cfg(target_arch)` only, like its siblings).
* **`omni-cpu`**: `GuestCpu::jit_counters()` is a new trait method **with a default**;
  `JitCounters`, `stats` (monitor registry) are new. **`DynarmicOptions` gained two public fields**
  (`exclusive_monitor`, `optimizations_override`) -- a struct literal without `..Default::default()`
  will not compile; every in-tree construction uses `Default`. `DynarmicBackend::new` now reads
  `OMNI_JIT_*` switches (`with_environment`), each announced.
* **`dynarmic-sys`**: `OdMonitorLayout`, `od_monitor_layout_of`, `optimization::UNSAFE_IGNORE_GLOBAL_MONITOR`
  (shim ABI additions). **Vendored patch 0002** changes `a64_emit_x64.{h,cpp}` and
  `block_of_code.cpp` -- files of the **x64 backend only**. An arm64 host (the M1) builds
  dynarmic's arm64 backend, which this patch does not touch and which was **not examined** for the
  same per-thread costs; measure it there before assuming either way.
  `OD_FIXED_PER_JIT_BYTES` keeps its value; its meaning is now "when FastDispatch is on".
* **`omni-mem`**: `process_pager_totals()` (new).
* **`omni-android`**: `perf` module (new); `jni::ExitRecord` gained `REASON_CRASH_NATIVE`,
  `SIGABRT`, `SIGSEGV`, `SIGILL`, `IMPORTANCE_FOREGROUND` and names reason 5; `Vulkan::slot_names`;
  bionic binds `__vsprintf_chk` (bound 317, inline 302 -- the counts in `tests/bionic.rs` are pinned
  and will conflict textually with any other branch that binds a symbol); `OMNI_INJECT_DEATH` in
  `boundary.rs`. `tools/mutate.py`: rows `vsprintf-`, `crashclose-` inserted before the terminator.
* The gate (`tests/gameactivity.rs`): the close watchdog halts the UI context, a hung close behind
  a death records `REASON_CRASH_NATIVE`, `OMNI_IMPORT_CENSUS=off`. No OS names added.
