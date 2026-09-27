# Handoff

Current as of **2026-09-27**, branch `unified`. This file is the state and the next steps only.
Design is in `ARCHITECTURE.md`, reasons in `DECISIONS.md`, capabilities in `STATUS.md`, per-host
detail in `ports/<os>.md`, the current goal in `briefs/goal-performance.md`. Read
`VERIFICATION.md` before writing a test you will rely on; it also holds the Global Constraints
that code comments cite by number.

## Source, branches, machines

- **`unified` is the source.** It contains every other line of work (`perf-windows`,
  `port-macos`, `port-linux`, `input-kbm`, `perf-world`, `main`). The one unmerged branch with
  pending work is `arm64-clear-audit` (patch 0023, see "Open").
- **`perf-windows` is superseded, not a merge target.** Its two commits since (`799aca4`,
  `c566be4`, 2026-09-26; `unified..perf-windows` = 2, `perf-windows..unified` = 306) add nothing
  `unified` lacks. `c566be4` (a mapped file shortened as Linux does, by a logical end of file)
  duplicates `struct Logical` in `omni-platform`'s `fs/windows.rs` and bionic's
  `a_file_a_shared_mapping_holds_is_reopened_truncating_as_linux_does` (`memProfStorage`).
  `799aca4` (raw `close`/`read`/`mprotect`) is superseded by `sysroute::ROUTES` (57, 63, 226
  among them, each with a per-argument kernel-to-import map), which `service_raw_syscall`
  dispatches through `call_routed` on the exit path: the two defects of its draft (arguments
  read as zero; a re-entrant `mprotect` unreachable inline) cannot arise here.
- `main` is behind. **Never merge to `main` or push to GitHub (`origin`) without the owner.**
- Remotes `mac` and `linux` are the other machines' checkouts. The `mac` remote URL still says
  `192.168.0.24`; the Mac was moved to **`192.168.0.37`** (macOS 27.0) on 2026-09-25.

| host | checkout | notes |
|---|---|---|
| Windows (this PC, RTX 4060, 24 threads, 31.8 GB) | `C:\Users\berat\Desktop\Omni Apps\omnidroid-unified` | build with `OMNIDROID_DYNARMIC_BUILD_DIR=C:\od-unified` (MAX_PATH). Other `Omni Apps\omnidroid*` worktrees and `C:\od*` build dirs belong to other work. |
| macOS (Apple M1, 16 GB) | `~/Desktop/omnidroid-unified` | non-interactive ssh: `. ~/.cargo/env` first. `screencapture`/`osascript` do not work over ssh. |
| Linux (i5-4460, **7 GB**, Quadro 4000 = Fermi, no Vulkan) | `berat@192.168.0.38:~/Desktop/omnidroid-unified` | one build or one app at a time; GLES through nouveau NVC0; `DISPLAY=:0`, start with `setsid nohup`. Never start an Xvfb that opens the nouveau node (it wedged the GPU once). |

Keep the three at one commit: commit on Windows, `git bundle create <f> <old>..unified`, `scp`,
`git pull --ff-only <f> unified` on the others. Live runs on one host at a time; builds may run in
parallel. Remove a finished agent's worktree `target/` (a full disk stopped a build on 09-25).

## The APK

Since 2026-09-26 the fixture is the **stock** `Roblox-2.738.1397.apk` (identity in `STATUS.md`,
contents in `research/apk-analysis.md`). The builds used before it were modified by third parties
and are gone from this checkout: the old 2.738.1397 (trojanised `libzstd-jni`, injected
`classes4.dex`) and 2.739.691 (another executor in `libzstd-jni`). Copies of them still exist in
other worktrees (`omnidroid-play`, `C:\odw\*`, `.claude/worktrees/*`): do not use them.

- The APK is chosen at run time: `--apk`, else `OMNI_APK`, else the APK in the repo root with
  the highest `versionCode` (`omni_apk::choose_apk`; `omnidroid which` prints the choice).
- Only `lib/arm64-v8a` is loaded; the APK's `armeabi-v7a` and `x86_64` sets are ignored by design.
- **Anything decoded on 2.739.691** (a different `libroblox.so`, 3,610 initializers) carries that
  build's link addresses. Code that locates things by decoding the engine at run time (e.g.
  `jni::cursor`'s lock word) is version-independent; comments and docs that quote 2.739.691
  addresses are not valid for the stock binary until re-measured.

## Running

```text
cargo build --release -p omnidroid
cargo test -p omni-android --release --test gameactivity --no-run
target/release/omnidroid play [--cookie <file|name>] [--place <id>] [--minutes N] [--fresh] [--phone]
target/release/omnidroid login            # sign in in Chromium once; keeps the cookie by name
powershell -File tools\play.ps1 [-Cookie ..] [-Place ..] [-Minutes N] [-Fresh] [-Phone]
```

`play` runs the gate test `initialize_native_code_returns_a_native_code_and_the_game_thread_starts`
as the session; the gate failing because a guest thread died is a real defect. Storage is per
account (`<app-data>/../accounts/<name>`); a session's cookies are kept by the app's own cookie
store, as on a phone.

- **End every run cleanly** (the window's X, or `--minutes`). A killed run is judged a crash at
  the next launch of that storage; after one, use `--fresh` or a new storage.
- **Never build on Windows while the owner plays**: a build exhausted the commit limit once and
  killed two guest threads.
- **Network:** Roblox is ISP-blocked; each host uses a bypass (WARP / SplitWire / VPN). The
  accounts are Turkish and Roblox ends a session whose cookie arrives through another country's
  exit: check `loc=TR` (`https://www.cloudflare.com/cdn-cgi/trace`) first. GoodbyeDPI breaks
  UDP/QUIC and teleports. A `TlsVerificationFail` / `195.175.254.2` answer means no bypass is on.
- **Credentials:** never print, log or copy cookie values or passwords; never work around
  Roblox security (captchas, the country check). A dead cookie: ask the owner.
- Measure in **Pet Simulator 99, place 8737899170** (joined directly, no teleport).

Log markers: `DID_LOG_IN`; `JOIN: ... returned 1`; `submitStartGameTask`; `onGameLoaded:
placeId:...` (in-world timing starts after it plus ~2 min settle); `FRAMES: +Ns ..., T presents
(+X in the last 5s)` (fps = X/5); `GUEST THREAD DIED` (always a bug); the close's
`SessionHistory "IAB"`/`"IB"` (clean).

### Switches

Read once at start; each announces itself in the log.

| switch | effect |
|---|---|
| `OMNI_KEYBOARD_MOUSE=1` | host keyboard and mouse as a device's (play's default; `--phone` = touch) |
| `OMNI_PERF=<s>`, `OMNI_PERF_SAMPLE=0`, `OMNI_PERF_WAITS`, `OMNI_PERF_DUMP` | per-thread PERF blocks every s seconds (jit/monitor/dispatch/handler shares, translation); sampler off; waits; symbol dump |
| `OMNI_MEM_REPORT=1` / `=<s>,..` / `=every:<s>` | `MEMREPORT`: memory by owner, guest and host (Windows, Linux; macOS guest side only) |
| `OMNI_FPS_CAP=<fps>` | pace frames from underneath (every frame still presented) |
| `OMNI_GRAPHICS_QUALITY=1..10` | the game's own saved quality (edits an existing `GlobalBasicSettings_13.xml`) |
| `OMNI_AUDIO=off` | no AAudio; FMOD's NOSOUND path |
| `OMNI_GUEST_MEMORY_MB`, `OMNI_GUEST_CPUS` | the device's RAM (D36) and CPU count (D37; default ≤ 8) |
| `OMNI_LOOPER_IDLE_US` | the game loop's idle wait (default 1000) |
| `OMNI_JIT_SHARED_CACHE=0/1`, `..._MB`, `..._LIVE_MB`, `..._REGION_MB` | D38 shared translation cache (default on for x64, off on arm64) and its sizes |
| `OMNI_JIT_CACHE_MB`, `OMNI_JIT_EXCLUSIVE_MONITOR=global`, `OMNI_JIT_OPTIMIZATIONS` | per-thread cache size; the old monitor (D31); dynarmic optimization mask |
| `OMNI_PAUSE_IN_BACKGROUND=1`, `OMNI_FOLLOW_FOCUS=1` | Android's pause-in-background (default: keep playing, as desktop Roblox) |
| `OMNI_WINDOW_SIZE=<w>x<h>` | initial window size |
| `OMNI_JOIN_PLACE`, `OMNI_JOIN_DELAY`, `OMNI_DEEPLINK` | join a place (the app's own join URL) |
| `OMNI_GUEST_ENV=K=V,..` | extra guest environment (e.g. `MIMALLOC_PURGE_DELAY`) |
| `OMNI_FILE_TRACE`, `OMNI_WAIT_TRACE`, `OMNI_PROFILE`, `OMNI_IMPORT_CENSUS=off`, `OMNI_GLES_TIMING` | diagnostics |
| `OMNI_CLIENT_APP_SETTINGS=<json>` | the engine's own ClientAppSettings (measurement only) |
| gate/test: `OMNI_GFX_WINDOW_TESTS=1`, `OMNI_M6_ROWS_21_22=1`, `OMNI_SESSION_SECONDS`, `OMNI_DATA_DIR`, `OMNI_LATE_{TAP,TEXT,INPUT,DRAG,KEYS,WHEEL}`, `OMNI_RESIZE_PROBE`, `OMNI_INPUT_PROBE`, `OMNI_INJECT_DEATH` | set by `play`, or synthetic stimuli for unattended runs |

## The real-AOSP path (`omni-linux`, sub-projects C and D) -- the current goal

Goal: an installed APK's launcher Activity starts through the real AOSP stack and renders to a
screenshot. Milestones D2 -> D3 (a: GPU, b: composer + SurfaceFlinger, c: system_server to
SystemReady) -> C4 -> C5 -> D4 -> D5; specs in `docs/superpowers/specs/2026-09-27-*`, plans in
`docs/superpowers/plans/`. Done: D2 (gralloc 5), D3a (guest Vulkan + ANGLE on the host GPU) on
Windows and Linux (see STATUS).

- **D3b done on Windows** (`tests/d3b_display.rs`, the bootanimation screenshot). Linux open:
  RenderEngine faults in ANGLE on lavapipe (STATUS).
- **D3c/C4 now**: system_server passes StartDisplayManager, stops in PackageManagerService: (1)
  apexd activates no APEX (it cannot mkdir/mount `/apex/<name>@<v>` -- the plan: present the
  sysroot's already-extracted APEXes as mounted through `/proc/mounts` + `/sys/block/loopN/loop/
  backing_file`, which apexd's `PopulateFromMounts` reads, so the real apexd reports them active);
  (2) installd SIGSEGVs at start (null deref); (3) the audio HAL (goldfish) is waited for.
  Probe script shape: `omni-linux-run --init early_hal,core,hal,main --hal gralloc --hal composer`
  with OMNI_SCREENSHOT=<png>.
- system_server command = `app_process64 -Xgc:CMC -Xhidden-api-policy:disabled /system/bin
  com.android.server.SystemServer` as uid 1000 with CLASSPATH=$SYSTEMSERVERCLASSPATH and the
  derive_classpath environment, `omni-linux-run --init early_hal,core,hal,main --hal gralloc`
  (init starts ~58 services incl. apexd, which system_server needs). It stops at
  StartDisplayManager today (waits for SurfaceFlinger).
- Probing from Git Bash: `export MSYS_NO_PATHCONV=1` or guest paths are rewritten.
- Diagnostics: `OMNI_GPU_TRACE=1` (every forwarded Vulkan command and its answer).
- Known gaps: AHB mirrors are linear images (input-attachment usage and other-format views are
  dropped when the host refuses them linear); no sync_file fds (fences are -1: synchronous);
  `vkDestroyCommandPool` leaves guest command-buffer wrappers behind; CLOCK_MONOTONIC is shared per
  host process (not across host processes).

## Where it stands

- The whole startup contract (`research/jni-surface.md` §8, 26 steps) runs on the real engine:
  sign-in (Quick Sign-in, password, or a kept cookie), Home, joining a game, the world rendered
  and played with keyboard and mouse, audio, the web view, a device-style close and relaunch.
- **2026-09-26, stock APK**, logged out, `--fresh`, 4 minutes, Windows: all 3,594 initializers,
  Landing at ~+10 s, 0 guest threads lost, clean close (`IAB`), gate passed. Not yet run in a
  world or signed in on the stock APK.
- In-world figures per host (all on the modified 2.739.691 build, 2026-09-25) are in `STATUS.md`.
  Windows sits at the engine's own 60 Hz pacing (decoded: a phone on these flags does the same);
  Linux is limited by its Fermi GPU, not the GLES layer.
- **Multi-instance.** The owner's two products: (A) 3-4 instances, high quality; (B) 30-35
  instances at lowest quality, capped. A is in reach. A B instance (`OMNI_FPS_CAP=10
  OMNI_GRAPHICS_QUALITY=1 OMNI_AUDIO=off OMNI_GUEST_MEMORY_MB=3072`, minimised) measured
  **~2.5 GiB and ~0.6 cores** (H2); the rest is mostly the engine's own heap (1.25-1.6 GiB of live
  data). Remaining levers, unmeasured: engine read-only file `mmap`s as shared views, a 16 KiB
  commit granule for the heap, `onTrimMemory` after the join, a persistent translation cache.

## Test state

2026-09-26, Windows, stock APK: `cargo test --workspace --release` **2,409 passed, 0 failed, 110
ignored** (233 binaries). Switching to the stock APK re-pinned `omni-apk/tests/real_apk.rs` (the
container), `omni-elf/tests/all_libraries.rs` and `loader_hostile.rs` (the genuine `libzstd-jni`:
641-symbol import union, `DT_HASH` only, BTI/PAC, 16 KiB-aligned), `tools/texture_census.py` (entry
count; the texture set is unchanged) and one dex decode in `tests/gameactivity.rs` (an obfuscated
name). Not run on the Mac or Linux since the switch.

- `dynarmic-sys --test shared_cache`'s `threads_stay_right_while_another_rewrites_and_invalidates_
  their_code` failed once in a full run and passed 5/5 alone: it waits up to 60 s for a region to
  retire and takes 40-50 s unloaded, so a busy host can hit the cap. A timing-bound test to fix.
- 44 mutation rows no longer match their pattern exactly once (34 in `tools/mutate.py`, 6 in
  `tools/lnx_rows`, 4 in `mutate_shim.py`), all already stale at `374df41`; re-anchor them before
  the next full-table run.

## Open, in order

1. **Re-validate on the stock APK in a world** (needs the owner's sign-in): a 30-minute PS99 run
   per host; watch for Roblox's "missing or corrupted files" kick (seen only on the modified
   builds); re-measure what was decoded on 2.739.691 wherever it is still used.
2. **macOS freezes** (m7, m9, m11): a control transfer into stale translated code after a
   mid-run cache clear on the arm64 JIT. Patch 0023 (`arm64-clear-audit`) awaits verification:
   the unpatched build must fail its new test, the `mac-cpu-C` rows, the arm64 suites. m12 with
   `OMNI_JIT_EXCLUSIVE_MONITOR=global` ran 30 min clean. Also owed on the Mac: the RSB-off run.
3. **Intermittent heap corruption on Windows** ("-1 pointer": OpenSSL `impls`, a `shared_ptr`
   control block, w23's flag-registry node): ~2 in 16 runs before the `MADV_FREE` fix
   (`e6b7769`); not seen since, not proven gone.
4. **Live checks pending**: the window-change freeze fix (w26), the JNI-audit fixes (w31's "unlock
   chat"), D38 amendment 3 on Windows at 256 MiB (w36 confirmed the default). Open on the menus
   path: Linking `openURL` ("Continue" in the age-check modal does nothing), voice's
   `WebRtcAudioManager`.
5. **A `poll` over sockets parks 60 s per slice and can hold teardown** (was in progress on
   09-25; unverified whether fixed).
6. **`/proc/<own pid>/cmdline` is not answered** (measured on `perf-windows`, 2026-09-26; not
   fixed). With the logical end of file in place, the engine's "Evaluating deferred inferred
   crashes" opens it; nothing answers that path (`bionic/procfs.rs` has `meminfo`,
   `self/statm`, `self/maps`), so the engine takes its own `memProfStorage<pid>.json` for a dead
   session's record -- re-opens it `O_TRUNC`, `fallocate`s, unlinks it -- and the gate's close
   assertion failed with `SessionHistory None`. The windowed gate on `unified` is green today,
   so the path is latent, not absent. The answer is **not** a `/proc` file invented to satisfy
   the check: measure first what the engine reads there and what a device answers.
7. Multi-instance B: the unmeasured levers above; which game and RAM the B machine has is the
   owner's answer.
8. Re-anchor the 44 stale mutation rows (above), then run the whole table once; it has never run
   whole in one pass (VERIFICATION entry 8).

## Working rules that have paid for themselves

- `tools/mutate.py` mutates the tree in place: it is exclusive (no build, commit or second run
  meanwhile). Stage explicit paths, never `-A`; commit in small pieces.
- Parallel agents own disjoint crates and their own worktree + `target` + dynarmic build dir.
- When vendored dynarmic C++ changes, touch `crates/dynarmic-sys/vendor/PIN.txt` before building.
- A measured figure lives in one doc with its n; everything else links to it.
- The five-target rule: never claim a platform works that has not been run there; a new platform
  primitive gets honest `unsupported` arms elsewhere (see `ports/`).
