# Handoff

## MORNING REPORT (overnight run 2026-09-28, Windows only, stock APK)

APK used throughout: `omnidroid-unified/Roblox-2.738.1397.apk`, 229,466,269 bytes, sha256
`BBE00AE3...2742`, signer "Roblox Corporation", 3 dex. **Not** the 159,853,296-byte file of the
same name in the main checkout `Desktop/Omni Apps/omnidroid` (sha256 `4BCB90EE...6128`): that one
has an extra `classes4.dex`, an 18,440,296-byte `libzstd-jni` (stock: 603,960) and is re-signed
with a non-Roblox key (`META-INF/KEY.RSA`, "Gloopiest Man") -- the modified build this file warns
about. `libroblox.so` is byte-identical in both. A `Roblox-2.739.691.apk` is also still in the
unified root; `choose_apk` would pick it (higher versionCode), so always name the APK.

| phase | result | artifact |
|---|---|---|
| 1 address space | **fixed** (`d65fbe8`): 10/10 boots reach "System now ready" (72-75 s each, 0 `[mm]` refusals, 0 guest deaths). Before: 5/7 D5 logs of 09-27 reached it, 1/7 exhausted the space (49 refusals, 1,048,576 bytes free of 64 GiB) | `docs/runs/2026-09-28-phase1-c4-10boots.csv`; `tests/low_space.rs` (fails without the fix with the boot log's exact figure) |
| 2 login screen | **the logged-out login screen on the host display** (r15): ROBLOX logo, "Hesap Oluştur" (Create Account), "Giriş Yap" (Log In), Koşullar/Gizlilik over the game-art backdrop, drawn by the engine's own Vulkan (RTX 4060 via `vulkan.omni.so`) through SurfaceFlinger; the engine's `onDataModelNotification() type:APP_READY data:Landing`. Also the **signed-in Home** (r12) | `work/overnight/r15-login-landing.png` (1,531 colours, 43.8% #000000, 9.7% #f8f8f8); `r12-home-signed-in.png` (12,189 colours: Home, "HeZmI_ImYu1080", "Arkadaşlar (10)"); `r15.log` |
| 3 `--cookie` | **signed in, 4/4 runs** (r10-r13): `onDataModelNotification() type:DID_LOG_IN data:{"username":"HeZmI_ImYu1080",...,"countryCode":"TR","userId":5457009831}`; 0 HTTP 401/403 after it (r10) | `work/overnight/r10-signin-markers.txt`, `r13-markers.txt` |
| 3 `--place` | **joined** (r13, `am start -d roblox://experiences/start?placeId=8737899170 -n .../.ActivityProtocolLaunch`): `GameJoinLoadTime join_time:1.773 referral_page:DeepLink placeid:8737899170`, `onGameStarted`, **`D/rbx.jni onGameLoaded() SessionReporterState_GameLoaded placeId:8737899170`**, `ExperienceSession onGameLoaded: placeId = 8737899170`. (The HLE gate's `JOIN: ... returned 1` does not exist on this path; the engine's `GameJoinLoadTime`/`onGameLoaded` are its equivalents.) | `work/overnight/r13-markers.txt`, `r13.log` |
| 4 in place | **ran ~80 s, then Roblox ended it**: after `onGameLoaded`, frames kept presenting (727 -> 1,147 in 281 s, ~1.5/s); PS99's own in-place loading GUI drawn (BIG Games logo, 3/5 dots) under Roblox's in-experience top bar; the game's client scripts ran (r12: `✅ CLIENT \| _L took 1818ms to initialize!`). Then **"Client has been disconnected with reason: Roblox cannot be used in an emulated environment. Please run Roblox on a supported device."** (reason 305, `connectionTime 80142` ms). This is Roblox's emulator detection -- a security control; per this file's rules it is **not** worked around. The in-world screenshot and a settled world are therefore not reached | `work/overnight/r13-inplace-loading.png` (233 colours: 97.6% #f8f8f8, 1.3% #f89840 = the BIG Games logo), `r13-markers.txt` (the disconnect lines) |

| regression (06:45-06:58) | **C4 5/5** "System now ready" (64-67 s); **D5 3/3** -- the probe's blue at the centre, 79.4% of the screen #2090f0 (was 1 pass in 4 on 09-27: the splash-occlusion bug #1 is gone with the binder, socket and property fixes); 0 oneway stalls | `docs/runs/2026-09-28-regression-c4x5-d5x3.csv` |

**The wall (phase 4):** the game server disconnects this device as an emulator ~80 s after the
join. The image is Google's SDK emulator build ("Unknown Android SDK built for arm64", goldfish/
ranchu props); hiding that from Roblox would be evading its security, which the owner's rule
forbids. The owner decides whether and how to proceed (a non-emulator system image is the honest
route; nothing here spoofs one).

| 2 launch | **Roblox starts on the real AOSP stack**: `pm install` 0 (libs extracted), `LauncherAliasMain` -> `ActivitySplash`, "Displayed ... +24-25 s", `libroblox.so` loaded by its class loader, the engine's own `I/Roblox [FLog::...]` lines, crashpad up. r1: its splash on the display, then its main thread died (below). r2 (after `4f600d3`): alive, its own **"Connection error -- Unable to contact server"** dialog: no network on this path yet | `work/overnight/r1-roblox-splash.png` (97.4% #f8f8f8, 2.0% #3058f8 = the Roblox logo), `work/overnight/r2-roblox-connection-error.png` (80.9% #606060 scrim + 16.6% #f8f8f8 dialog); logs `work/overnight/r1.log`, `r2.log` (untracked) |

Fixed on the way (phase 2), each with a test that fails without it:
- `4f600d3` binder: a transaction that cannot be made is `BR_FAILED_REPLY` to the sender (the
  command consumed, the ioctl 0), not an ioctl errno -- libbinder kept its out-buffer and every
  later call of the thread failed (`tests/binder_failed.rs`; old driver: -9). And a plain file's
  descriptor crosses host processes (the WebView's `variations_seed_new`), by
  `omni_platform::fs::path_of` (`remote::tests`).
- `617f943` ART's JIT code cache: a memfd's section was never executable, so every ART process
  (system_server, SystemUI, Roblox) ran without JIT (`tests/jit_cache_map.rs`: -17 before).

- `efd0e33` (merge `aosp-net`): TCP/UDP are real host sockets (`tests/host_sockets.rs`, 37 checks
  in a guest), `pselect6`, and the kernel answers `/dev/socket/dnsproxyd` in an app's host process
  (`tests/dns_proxy.rs`: a guest `getaddrinfo` through the real bionic gets the host resolver's
  addresses). Before it, every inet `connect` was ENETUNREACH.
- merge `binder-oneway` (7 commits, a test each that fails on the old driver): a freed oneway
  buffer hands the node's next oneway to the process, not the freeing thread; only free loopers
  take process work; undelivered work is freed and its sender told; nodes named by a transaction
  are held until the receiver frees the buffer (the system_server SIGSEGV of r1: a use-after-free
  in `Parcel::unflattenBinder`, symbolized to `RefBase::incStrongRequireStrong` on a freed object).
- `2832702` no modem: the image's RIL spun on its modem's vsock (198,217 log lines per 100 s; one
  session's log reached 2.4 GB and filled C:); it is declared and not started, as on a device
  without a radio. C4 logs 4 MB -> 1.2 MB. 3/3 C4 boots after both merges: ready in 70-84 s.

- `7f1bde0` binder: BC_FREE_BUFFER freed the memory before removing the buffer's record; a
  transaction delivered into the reused address replaced the record, and SurfaceFlinger's composer
  callback node wedged ("101,312 oneway calls wait on node 85 ... the one out: none found", boot
  stalled). 5/5 C4 boots after: 0 stall lines (before: 1 of 3 C4 + r5).
- `e8e3461` mremap: ART's CMC GC moves its 512 MiB space with MREMAP_DONTUNMAP; the byte copy
  committed all of it. Idle app host process 765 -> 263 MiB (guest 636 -> 135), system_server
  2,971 -> 2,486 MiB (`OMNI_MEM_TRACE`). Two sessions (r4, r6) had been stopped by Claude Code's
  low-memory reaper before it.
- `1792321` ftruncate of a mapped file (SQLite `-shm`): `SQLITE_IOERR_SHMOPEN` killed the
  contacts provider. `b1a8f08` kill(2) of an app in its own host process ("refused to die":
  `am force-stop` never stopped Roblox, so the planted cookie was never read); fallocate(2).

### Later fixes (after r12)

- `9724db0` abstract unix sockets across host processes (the WebView zygote; ActivityManager's
  2,402 connect retries stalled system_server; `tests/xsocket.rs`, ENOENT before).
- `b1a8f08` kill(2) of an app in its own host process; fallocate(2).
- `1792321` ftruncate of a mapped file (SQLite `-shm`).
- `fa11ddc` property changes published in order (r14: system_server killed by its Watchdog after
  vold waited forever on a property; not reproduced by `tests/props_order.rs`, argued in props.rs).
- `5dd6ba1` the test device boots in the account's locale (tr-TR) so the app's locale change does
  not relaunch its game activity; `5bfcc91` 24 idle image apps disabled (memory: the reaper).
- Open: com.android.phone crash-loops without a radio (47 restarts in r12), costing CPU.

### r11/r12 (2026-09-28 02:28-03:00 guest time): signed-in Home on the display, the game joined

- **The engine draws on the host display**: `work/overnight/r12-home-signed-in.png` -- Roblox's
  signed-in Home (HeZmI_ImYu1080, Turkish UI: "Ana Sayfa", "Arkadaşlar (10)", game tiles), 12,189
  distinct colours, 66.5% #101010. The black window of r8 was system_server stalled by the WebView
  zygote's socket (`9724db0`).
- **The game joined**: r12 `GameJoinLoadTime ... join_time:1.856, referral_page:DeepLink,
  placeid:8737899170, userid:5457009831`; `! Joining game ... place 8737899170`; RakNet to
  128.116.5.33 "Handshake complete", "Connection accepted", "Replicator created"; PS99's own client
  scripts ran (`✅ CLIENT | _L took 1818ms to initialize!`, `[MiningFrontend] inMine=false`, server
  `RobloxGitHash: ef44663a...`). Markers: `work/overnight/r12-join-markers.txt`.
- **No `onGameLoaded`**: 2.3 s into the game the app's activity was relaunched (the app applied the
  account's locale tr_tr on an en-US device; ActivityNativeMain's configChanges 0xfb0 has no
  locale), its Java game session ended ("Ending game session with place ID 8737899170") and the
  UI went back to Home while the engine stayed in the server. The test device now boots in the
  account's locale (`persist.sys.locale=tr-TR`, as the owner's phone) and joins 45 s after sign-in.

### Phase 2/3 state (r8, 2026-09-28 01:31 guest time)

- With the network: `ActivityNativeMain` displayed (+6.6 s); the engine's **own Vulkan** on the RTX
  4060 through `vulkan.omni.so` ("Vulkan Device: NVIDIA GeForce RTX 4060", "swapchain images 3
  present mode 2 format 37 size 1280x720", 2,919 shaders loaded); `SingleSurfaceApp` reaches
  `stage:LuaApp`; HTTPS answered by Roblox (401 "Authentication required to access Asset" while
  signed out). **But the app's surface is black** (r8 shots 0008-0068: 92.2% #000000 = the whole
  window; status and task bars drawn) and presented frames nearly stop once the native activity
  shows (~500 in total, then ~1/min): the engine does not present. Under investigation (r9: the
  app's syscalls traced).
- **`--cookie`: signed in (r10, 02:13 guest time).** Planted into the app's own Chromium `Cookies`
  store, `am force-stop` now ends the app (`[zygote] pid 172000: signal 9`), the fresh process
  (pid 196000) calls `initializeLuaAppWithLoggedInUser` and the engine logs
  `onDataModelNotification() type:DID_LOG_IN data:{"username":"HeZmI_ImYu1080",...,
  "countryCode":"TR","userId":5457009831}`. After it: **0** HTTP 401/403 and no logout line in the
  log. Evidence: `work/overnight/r10-signin-markers.txt`, `work/overnight/r10.log` (untracked).
- `--place` (r10): the deep link `roblox://experiences/start?placeId=8737899170` was delivered to
  `ActivityProtocolLaunch` (`am start` 0), but the app was killed ~60 s later: ANR "Input
  dispatching timed out" -- its main thread waited 50 s in a binder call on a system_server stalled
  by ActivityManager retrying the WebView zygote's socket (2,402 x "Got error connecting to zygote").
  Fixed below the framework in `9724db0` (abstract unix sockets across host processes); r11 re-runs
  it.

Cause (phase 1): every guest process asks for the same low range; system_server holds it,
reserved *around* the host's pieces there, host threads' 1 MiB stacks among them. When such a
thread exited, its stack was the only free piece, and the next process's space "succeeded" with
1 MiB free. `process::reserve_space` now refuses a low space less than half free.

Markers on this path: `DID_LOG_IN`, `onGameLoaded: placeId:`, `submitStartGameTask` are the
engine's own (logcat tag `Roblox`). `JOIN: ... returned 1` and `FRAMES:` are printed only by the
HLE gate (`omni-android/tests/gameactivity.rs`) and cannot appear here; their stand-ins are the
engine's `onGameLoaded` / `NativeDM ... placeId` and the runner's `[display] N frames presented`.

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
- **C4 now (2026-09-27)**: on a fresh instance, system_server boots through PackageManager,
  SettingsProvider, WindowManager, input, Bluetooth and NetworkManagement (netd up) to
  `StartNetworkStatsService`; the last run then aborted in ClatCoordinator on bpffs pin modes and
  contexts (fixed since in `1f21f27..`: bpffs keeps modes/owners, genfs contexts from the image's
  policy). What the boot needed, all below the framework: init's `wait_for_prop`, `setprop`,
  `restart`, `init_user0` (vdc), `perform_apex_config` (linkerconfig), `load_bpf_programs`, APEX
  versioned `.rc`, `socket` (init sockets); the kernel's xattrs/restorecon, bind mounts, fork/
  exec/wait (vfork child in the parent's memory, the parent's private memory snapshot-restored),
  ownership (persisted per instance), record locks, flock, capabilities, set*id, pwrite, sendfile,
  eBPF (maps, programs, bpffs, attach/query), xtables, netlink, Unix server sockets, the boot id
  and `/dev/ashmem<boot_id>`. The device lists no sensor sub-HAL (`device/vendor/etc/sensors/
  hals.conf`). system_server is started directly: `--caps` (the zygote's set) and `--setprop
  dalvik.vm.profilesystemserver=true` (standalone jars' class loaders on first use).
  Later the same day (`5043072..f38e95e`) it passed NetworkStats, Connectivity, Audio and
  SoundTrigger to BiometricService (gatekeeperd: init's `late_start` class, now started). Filled
  on the way, each below the framework: the image's owners/modes/labels/capabilities
  (`sysroot.meta`, `tools/make_sysroot.py --meta`), inet `bind`, MAP_SHARED file mappings
  (SQLite WAL), peer credentials (SO_PEERCRED, SCM_CREDENTIALS, init's `+passcred`), `*at`
  dirfd as an int (AT_FDCWD), PI futexes (audioserver), the bootloader's `ro.boot.*` and the
  vendor build.prop (KeyMint), init restarting services (`onrestart`, `ctl.stop`), seccomp
  filters (a cBPF interpreter; minijail in the media daemons), 512 threads per process,
  tracefs (tracing off), and fork: private file views snapshotted, descriptors closed at exit,
  pids allocated onward (mksh pipelines and `$(...)`). A killed process no longer waits for
  threads a halt cannot reach (5 s). Gates: `tests/c4_system_server.rs`,
  `tests/c5_app_launch.rs` (`--ignored`, minutes; `tests/common/boot.rs`).
- **Run the boot in release** (`cargo test --release ... -- --ignored`, or a release
  `omni-linux-run`): the debug build's syscall/binder/memory paths made system_server ~5x slower
  (PackageManagerService 61 s debug, 12.7 s release; system_server start to "System now ready"
  41 s release), and its watchdog (60 s on the main thread) killed it after systemReady in debug.
  JIT on (`dalvik.vm.usejit=true`) changed nothing measurable. `OMNI_LOG_TIME=1` stamps log lines;
  `OMNI_FORK_TRACE=1` times forks (system_server's: 43 MiB kept, ~0.7 s frozen).
- **C5 in progress** (spec `docs/superpowers/specs/2026-09-27-c5-app-launch-design.md`): binder
  across host processes works (`tests/c3_remote_binder.rs`; `--binder-server`, `--pid`); the
  zygote responder (`--zygote`, `crate::zygote`) launches an app's `app_process64 ...
  android.app.ActivityThread seq=<n>` in its own host process. Probe: scratchpad `run_c5.sh`
  (probe APK `tests/fixtures/probe-app/probe.apk` in `/data/app`, `--then` runs `am start`).
- Probing: `OMNI_INIT_TRACE=1` (init's commands), `OMNI_TRACE_SERVICE=<name>` (one service's
  syscalls), `OMNI_FS_TRACE=1` (unlink/rename), `OMNI_REMOTE_TRACE=1` (cross-host binder frames).
  A full-disk C: shows as ENOMEM in every spawn (page file cannot grow): prune target/debug/deps.
- system_server command = `app_process64 -Xgc:CMC -Xhidden-api-policy:disabled /system/bin
  com.android.server.SystemServer` as uid 1000 with CLASSPATH=$SYSTEMSERVERCLASSPATH and the
  derive_classpath environment, `omni-linux-run --init early_hal,core,hal,main --hal gralloc`
  (init starts ~58 services incl. apexd, which system_server needs). It stops at
  StartDisplayManager today (waits for SurfaceFlinger).
- Probing from Git Bash: `export MSYS_NO_PATHCONV=1` or guest paths are rewritten.
- Diagnostics: `OMNI_GPU_TRACE=1` (every forwarded Vulkan command and its answer).
- **Network (2026-09-28, `aosp-net`)**: TCP/UDP sockets are host sockets (`crate::hostnet`, via
  `omni_platform::net`, policy `hostnet::set_policy`, unrestricted by default); the guest shares
  the host's ports. One watcher thread per host process turns host readiness into `crate::poll`
  wakeups (`tests/host_sockets.rs`). In a host process with no netd (an app's), the kernel answers
  `/dev/socket/dnsproxyd` itself (`crate::dnsproxy`: getaddrinfo, gethostby*, resnsend from the
  host resolver); where netd bound it, netd answers. `OMNI_NET_TRACE=1` prints host socket and
  lookup failures. Open: SCM_RIGHTS is not passed, so in the system's host process a connect by
  a process whose libnetd_client reaches netd's fwmarkd gets netd's error for the missing fd
  (apps are unaffected: fwmarkd is absent there and a failed fwmarkd connect is "no error").
- Known gaps: AHB mirrors are linear images (input-attachment usage and other-format views are
  dropped when the host refuses them linear); no sync_file fds (fences are -1: synchronous);
  `vkDestroyCommandPool` leaves guest command-buffer wrappers behind; CLOCK_MONOTONIC is shared per
  host process (not across host processes).

### D4/D5: the app on the display (2026-09-28, Windows)

`tests/d5_app_on_display.rs` (release, `--ignored`): boot, set the device up (provisioned, awake,
animations off), `pm install` the probe, `am start`, then require the probe's `frame committed`,
ActivityTaskManager's `Displayed`, and the probe's blue (0xff2196f3) at the centre of the host
display's screenshot. Passed once (161 s); 3 of 4 runs failed -- the flakiness is the open work:

1. **Starting window left on top.** WindowManager removes the splash window (its input channel is
   disposed) but its layer stays under a `window_animation` leash at z max; the removal transition
   never completes (seen with animations on and off). `BLASTSyncEngine: Sync group 0 timed-out
   because not ready` and `SurfaceSyncGroup ... Failed to receive transaction ready` precede it.
2. **Guest space exhausted early in boot** (~1 run in 3): right after init stops odsign, every
   `map_anonymous` fails with ~225 KiB free of 64 GiB. `mm.rs` now lists what holds the space at the
   first refusal (by label and call site) -- not yet caught with it.
3. **Live handle refcounts.** BC_RELEASE/BC_DECREFS are still no-ops (a live process's handles last
   as long as it does). Counting them deleted handles in use because a write stops at its first
   failed command, dropping later BC_ACQUIREs; the kernel goes on past a failed transaction. Fix
   that first, then restore the counting (commit `5e62d85` has it, with `tests/binder_release.rs`).
4. `probe.apk` logs `frame committed` (ViewTreeObserver.registerFrameCommitCallback).

Diagnostics added: `OMNI_COMPOSER_TRACE=2` (each presented frame: slot, centre and corner pixel),
`[display] N frames presented` with `OMNI_SCREENSHOT`, `[remote]` binder ioctl failures and
descriptors that cannot cross, `[binder] N oneway calls wait on node` (every 64 queued),
`[binder] pid P closed its driver: N objects released`, `OMNI_TRACE_SERVICE=<service>` for one
init service's syscalls. Crossed gralloc files stay in `%TEMP%\omni-shm-*` (clean them between
sessions).

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
