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
| `c6e7c0d` | a guest's raw `SVC #0` answered by the syscall emulation (kernel convention: `-errno` in `x0`, `errno` untouched; `openat` → `open`); `atol`. The two deaths of the owner's first join of 606849621. Rows `svc-` 5/5, `atol-` 2/2 |
| `56be27a` | docs: the first in-world join; the multi-instance measurement re-run with a data directory each |
| `e2eba84` | the app's cookie store (`jni::cookies`) -- a sign-in survives a restart, as on a phone. Rows `cookie-` 6/6 |
| `208c4c8` | translated code is invalidated only for ranges that were executable: -75% re-translation on the landing. Rows `inval-` 5/5 |
| `bae4b92` | D31 decided: guest exclusives default to value-compare, not the global monitor. Row `monitor-A1` 1/1 |
| `05efab2` | vendored dynarmic patch 0003 (D33): a return-stack-buffer hit checks the budget and the halt flag, so `INTERRUPTIBLE` keeps the RSB -- 132 → 24 ns per call and return at 262,144 blocks. Row `rsb-A1` 1/1 |

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
  storage, the owner's decision. **Later measured: a clean close does NOT keep it either** -- the
  engine's session cookie only ever lived on the Java side. The owner then decided the runtime may
  keep the app's own cookies: `jni::cookies` (see "The sign-in" below).
* The `InferredCrash` null member also killed a worker at +5 s on a **fresh** install once (1 of 3
  concurrent instances) -- it is not only a post-crash path. Harmless to the run; open.

## The sign-in: the app's cookie store (the owner's decision, 2026-09-24)

DECODED from `classes2.dex`: the engine pushes its cookies to Java through a handler it is given
by `JNICookieProtocol.updateOnSetCookieHandler` (called from `NativeHelper.Q` via `jk.k0.w` ->
`CookieProtocol.<init>`); the Java side stores each in WebView's `CookieManager`; at the next start
`bh.x0.W0` -> `S0` hands them back with `nativeSetMultipleCookies("https://www.roblox.com", …)`.
None of that existed here. Now: `jni::cookies` (RFC 6265 storage/retrieval, persisted at
`app_webview/omnidroid-cookies` in the app's data directory, written on every change, values never
logged), the handler's `onSetCookie` and the static `CookieProtocol.setCookie` answered into it,
and both script rows added (`updateOnSetCookieHandler` is the sequence's one instance native; its
`thiz` is never read -- `0x230a9f4`). MEASURED: a logged-out landing run stored the engine's two
cookies (`RBXEventTrackerV2`, `GuestData`) and the next launch of the same directory loaded both
and handed them back. Rows `cookie-`.

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
* **Instances started one by one** (3 on this PC, `multi.sh`, a separate fresh data directory
  each): each adds **~2.0-2.2 GB commit and ~1.6 GB RAM**, linearly -- no sharing beyond the
  library image. ~10 on 8 GB needs ~0.8 GB each. (A first attempt ran all three on ONE data
  directory -- a script quoting bug, `…\Omnidroid$D` -- and is discarded; the same bug put the
  memory A/B pair and the retranslation runs on one kept, logged-out directory, run back to back,
  which leaves those comparisons like for like but not "fresh". In the re-run one instance lost its
  game thread at +5 s to a `MemoryFault` at `0x21db02c`, the libc++ hash-map code of HANDOFF item 8
  -- a second sighting of that one, 1 in ~15 runs tonight -- so the figure is instances 2 and 3.)
  What remains per instance: the guest's own ~1.1 GB (its heap granules, stacks and blocks, pages
  it touched -- the 64 KiB granule rules out pager over-commit), ~0.4 GB of images, ~0.5 GB of
  translated code, duplicated per thread and per instance. The next lever is sharing translated
  code (a large dynarmic change, not started).
* Code-cache size: 128 MiB instead of 32 cuts load-phase retranslation 40% (251,904 → 150,933
  blocks retranslated over +10..+40 s) for +340 MiB commit and no earlier Landing. **Default kept
  at 32**; `OMNI_JIT_CACHE_MB` is the in-world A/B.

## Step 3: performance

**One in-world profile exists** (606849621, n = 1, idle camera, ~12 fps, 2026-09-24, signed in).
Three causes were found in it and changed; **none of the three changes was measured in a world**,
because the owner's APK then began failing Roblox's integrity check ("missing or corrupted files")
and the owner asked for no more real-game runs until it is updated. What was measured instead:

| change | the in-world evidence for it | measured after (not in a world) |
|---|---|---|
| `208c4c8`: invalidate only executable ranges | ~240 code invalidations/s reached every thread; 64-entry queues overflowed into whole-cache wipes; threads 9-33% in the translator | landing: re-translated blocks 387,688 → 96,833 (-75%), invalidations applied -87% |
| `bae4b92`: value-compare exclusives (D31) | busiest workers 3-8.5% of samples at the global monitor | benchmark 131 → 12.8 ns per guest atomic; landing passes |
| patch 0003: the RSB kept (D33) | (from the benchmark, not the profile) every `RET` was a dispatcher lookup | benchmark 132.0 → 24.1 ns per call and return at 262,144 blocks |

Still open from the profile: the game loop's `ALooper_pollOnce` + mutex spin (thread g5), and the
~12 fps itself. The owner's next session should re-take the same profile on the updated APK
(`OMNI_PERF=1`, the same place) to measure the three together.

### Before the profile (kept for the record)

The first sign-in window stood open 43 minutes (to 01:26) with no sign-in and was closed cleanly.
The second was signed in at ~02:41 and the owner joined 606849621 straight away: the world never
finished loading (0-1 presents per 5 s for the 80 s from the join to the deaths) before a worker
died on the raw `svc` (+1570 s) and another on `atol` (+1595 s) and it froze -- both fixed in
`c6e7c0d`. So no in-world frame-rate figure exists yet. On the landing: census on/off and global/value-compare monitor
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
  **Vendored patch 0003** changes `a64_emit_x64.cpp` (x64 backend only) and, **not additively**,
  the value of `optimization::INTERRUPTIBLE`: `0xFFF9` → `0xFFFB` (it keeps `ReturnStackBuffer`).
  An **arm64 host** builds dynarmic's arm64 backend, which this patch does not touch and whose
  return-stack-buffer handling was **not examined**: run `the_stoppability_matrix` there -- its
  `return` and `indirect-call` cells fail if that backend's handler does not check -- and keep
  clearing `RETURN_STACK_BUFFER` on that target until they pass.
* **`omni-cpu`** (later): `DynarmicOptions::default()` selects `ExclusiveMonitor::ValueCompare`
  (D31); `OMNI_JIT_EXCLUSIVE_MONITOR=global` restores the old default.
* **`omni-mem`** (later): `GuestSpace::any_executable(at, len)` (new, additive). `bionic::guestmem`
  now invalidates only ranges that were executable before `munmap`/`mprotect`/`madvise`, and
  `mmap` only for executable mappings.
* **`omni-mem`**: `process_pager_totals()` (new).
* **`omni-android`**: `perf` module (new); `jni::ExitRecord` gained `REASON_CRASH_NATIVE`,
  `SIGABRT`, `SIGSEGV`, `SIGILL`, `IMPORTANCE_FOREGROUND` and names reason 5; `Vulkan::slot_names`;
  bionic binds `__vsprintf_chk` (bound 317, inline 302 -- the counts in `tests/bionic.rs` are pinned
  and will conflict textually with any other branch that binds a symbol); `OMNI_INJECT_DEATH` in
  `boundary.rs`; the run loop now services `ExitReason::UnsupportedInstruction` with encoding
  `0xD4000001` (`SVC #0`) instead of returning it -- a backend that reports a guest SVC some other
  way needs the same arm; bound 318 / inline 303 after `atol`. `tools/mutate.py`: rows `vsprintf-`,
  `crashclose-`, `svc-`, `atol-` inserted before the terminator.
* **`omni-android::jni`**: new `cookies` module; `Answer::OnSetCookie` and
  `Answer::CookieProtocolSetCookie` (new enum variants -- an exhaustive `match` on `Answer`
  elsewhere needs them); `Jni::set_cookie_store`, `cookie_header`, `cookie_count`;
  `script::ScriptArg::CookiesFor`, `script::guest_argument`; two new `SEQUENCE` rows (20 -> 22) and
  `JNICookieProtocol` in `SCRIPT_CLASSES`. The `java/lang/String.onSetCookie` sink is gone.
  Pure Rust, no OS calls: it ports as is.
* The gate (`tests/gameactivity.rs`): `COOKIE_STORE` handed to `Jni::set_cookie_store`; the close watchdog halts the UI context, a hung close behind
  a death records `REASON_CRASH_NATIVE`, `OMNI_IMPORT_CENSUS=off`. No OS names added.
