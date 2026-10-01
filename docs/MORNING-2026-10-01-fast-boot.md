# Morning report 2026-10-01: any APK in seconds on a warm device (branch `perf/fast-boot`)

Goal (owner's overnight brief): an LLM agent behind the MCP server builds APKs and tests them one
after another; from its install/start call to the APK's first screen must take **seconds**.

**Where it landed.** One warm Android per host, booted and idle, takes APK after APK through a
control channel, and keeps a spare app process ready: a rebuilt APK is on screen **8.3 s** after
`start_instance`, another app **9.9 s**, the same APK again **2.1 s**, Roblox **26.6 s**. From nothing
running, the first APK is on screen in **70-80 s** (the device boots from its saved copy in ~45-60 s).
On `main` every new APK meant a new device: **~190 s**. All figures: i7-13700F, Windows, release, no
cookie; beside the owner's MuMu emulator (13 GB) and, for the last hours, another Claude session's
job (20+ Python workers, 30-54% CPU, commit and disk down to nothing twice).

## Before / after

Seconds. "Before" = `main` @`cfd32ed` (a worktree at `C:\od-base`), three runs, medians from the
log's `[t]` marks (Roblox APK, no cookie). "After" = this branch through the real `omni-mcp.exe`
over stdio (`tools/mcp_demo.py`, the whole call as the agent sees it), and `tools/boot_bench.py`
for the boots.

| | before (`main`) | after (`perf/fast-boot`) |
|---|---|---|
| **cold boot**: Android up (`boot_completed`), new device | 125.4 (124.4-136.9) | 74.3 (a new warm device) |
| **cold boot**: Android up, saved device | 124.3 (112.3-131.0) | **45.5** (45.3-49.0 on a quiet host; ready for an app +0.8 s) |
| **cold path**: nothing running -> APK's first screen (MCP `start_instance`) | 189.6 app on screen (a new device per APK; ~193 wall) | **70.4** / 79.7 (two runs; device 49-60, install 8-11, start 9-13) |
| **warm device, a new APK**: install + launch (MCP) | no warm path: a new device, 189.6 | **9.9** probe B (A uninstalled, B installed 2.7, on screen 7.2) |
| **same version, different bytes**: reinstall + launch | no path (a saved device is keyed by APK name+size: same size = the old app booted) | **8.3** (reinstalled 2.1, on screen 6.2) |
| same APK again | -- | **2.1** (reused: decided on the host in 1 ms) |
| a higher versionCode / another signature at a lower one | -- | 7.7 / 11.3 (uninstalled + installed 4.0) |
| **Roblox on screen** (its splash, then its sign-in screen) | 162.8 saved device / 189.6 new | **26.6** on the warm device (installed 7.4, on screen 19.1) |
| `stop_instance` (force-stop + clear, device stays warm) | shuts the device down | 2.4-2.6 |

"After" is the last demo run (`demo-spare`, spare targeted): the calls 10-15 s apart, as an agent's
builds would be -- a spare is ready ~10 s after the previous one was taken. Without the spare (the
run before it, same flow): 11.5 rebuilt A, 12.2 B, 26.0 Roblox.

## The demo (the real `omni-mcp.exe` over stdio, one warm device)

The brief's demo -- APK A, a rebuilt A (same version), then B, each call timed -- plus the other
variants and Roblox, from nothing running. Every call's answer is in
`docs/runs/2026-10-01-fast-boot-mcp-demo.jsonl`; the screenshots taken after each start show the
app's own colour (A blue `#2196f3`, rebuilt A green, B red, Roblox's splash).

| call | seconds | what the server said |
|---|---|---|
| `start_instance a.apk` (nothing running) | 79.7 | device `booted` 60.4, `installed` 10.5, on screen 8.8 (from a spare; Android's TotalTime 6.1) |
| `start_instance a.apk` again | 2.1 | `reused` (same SHA-256), already on screen |
| `start_instance a2.apk` (same versionCode 1 "1.0", other bytes) | **8.3** | `reinstalled` 2.1, on screen 6.2 |
| `start_instance a-v2.apk` (versionCode 2) | 7.7 | `reinstalled` 1.6, on screen 6.1 |
| `start_instance a-key2.apk` (another key, versionCode 1 < 2) | 11.3 | `reinstalled-after-uninstall` 4.0, on screen 7.3 |
| `start_instance b.apk` (another package) | **9.9** | A uninstalled, B `installed` 2.7, on screen 7.2 |
| `stop_instance` | 2.4 | force-stopped, data cleared; device warm |
| `start_instance Roblox-2.740.931.apk` | 26.6 | B uninstalled, `installed` 7.4, on screen 19.1 |

## What landed (commits on `perf/fast-boot`, oldest first)

1. `aa603bd` **omni-apk: the launcher Activity from the APK's own manifest** (`LaunchInfo`: package,
   versionCode, first enabled MAIN/LAUNCHER activity or alias; any APK, no versionName needed).
2. `8bad15b` **a control channel into a running device**: `omni-linux-run --control <dir>` runs
   `<id>.cmd` as the shell user (or `#uid=<n>`), output in `<id>.out`, status in `<id>.rc`, a
   heartbeat in `<dir>/alive`. Every harness boot has one at `<instance>.ctl`. Round trip 0.43 s.
3. `da599df` **the warm device**: `omnidroid aosp --warm` (r_roblox `OMNI_R_WARM=1`) boots the kiosk
   device with no APK, saves it once (`omni-golden/base-kiosk-<locale>-v2`), boots copies after;
   `data/local/tmp/warm-ready` when it can take an app. `OMNI_APP_ENV_FILE`: per-app switches read
   at each app start (A/B launch against launch on one device).
4. `4f1d3cc`, `27eeb85`, `aa79d2f` **omni-mcp on the warm device**: without `cookie`,
   `start_instance`/`install_apk` find the host's one warm device (or boot it under a lock -- never a
   second), decide by SHA-256 against the `base.apk` the device holds (read on the host), reuse /
   reinstall (`pm install -r -d -g`; uninstall first on another signature or a refused downgrade) /
   uninstall the previous test app, start the launcher Activity and answer when `am start -W` says
   it is displayed. New tools `shell`, `uninstall_apk`, `stop_app`, `device_status`, `stop_device`.
   `screenshot` waits for a frame written after the call. Fixed on the way: `<dir>.ctl` matched the
   `<prefix>*` lookups (warm device *and* standby) -- only `<prefix><digits>` is an instance now.
5. `dfe519f` **/dev/loop-control**: apexd waited 20.0 s for it at every boot; servicemanager now at
   +1.5 s instead of +22.3 s.
6. `d07faca` **the boot without its waits, apps without the preload**: odsign left out (5 s waiting
   for keystore2, then failing; its done-properties set by init); a static RRO in the device's
   vendor overlay sets `config_checkWallpaperAtBoot=false` (the 30 s BOOT_TIMEOUT is gone -- a
   static overlay is applied at boot here, only fabricated ones were lost); no dexopt at install
   (artd cannot chown the oat dir here, every dexopt failed anyway: a cold-artd reinstall 7.4 ->
   1.6 s); app host processes skip the zygote class preload (probe first frame -1.8 s median of 8
   ABBA pairs, 7 faster; Roblox -2.1/-3.7 s).
7. `5b9887c` **revert: the boot animation stays** -- leaving it out lost 3 of 3 ABBA pairs
   (ready 52.3/49.6/69.8 s without, 46.3/46.1/49.9 s with).
8. `deb82be` **the host's descriptor limit** (omni-linux-run raises RLIMIT_NOFILE to the hard limit):
   on the Linux box pinned to 2 CPUs the system's host process ran out of its 1024 descriptors as
   init started its services ("Too many open files": system_suspend and 15 others never started,
   the Watchdog ended system_server after 306 s). And the **spare app process** (below).
9. `b803fa6` **the warm device keeps a spare app process, preloaded** (`crate::zygote`,
   `device/src/spare`): an app host process started ahead of need -- host process, ART, binder and
   the zygote's preload done -- waiting in `com.omnidroid.spare.Spare` under a reserved pid. The next
   app ActivityManager asks for is answered with that pid; the spare's uid is rebound
   (`remote::rebind_uid`), it reads the app's arguments from a file and runs
   `WrapperInit.wrapperInit`. Probe first frame **6.5 s vs 10.9 s** (4/4 ABBA pairs; 7/7 counting the
   pairs before it preloaded), Roblox 21.6 vs 24.5 s (2/2; it goes on to its sign-in screen). A new
   spare 5 s after one is taken; one waiting holds ~200-350 MB. Warm device only (`OMNI_APP_SPARE=0`
   turns it off); app sessions do not keep one.
10. `b2f1f95` **the spare is for an installed app**: the image's own apps (FallbackHome relaunched when
    a reinstall ended the probe, the package installer, the WebView's service) had taken the spare a
    second before the app asked; now only a package in the device's `/data/app` gets it.
11. tools: `boot_bench.py` (a session timed from its `[t]` marks, `--warm`, `--env` levers,
   `--repo` another checkout), `mcp_demo.py` (the real server over stdio, each call timed),
   `device_ctl.py` (adb shell for the warm device), `warm_ab.py` (ABBA on one live device),
   `make_test_apks.sh` (the five test APKs), `nb_run.py` (a notebook's cells as a script).

**The same demo on Linux** (the box's i5-4460, 4 cores, Mesa llvmpipe, `OMNI_MCP_WARM_DIR=~/omni-warm`,
final branch `b2f1f95`): `start_instance a.apk` from nothing 277.7 s (the box's first warm device: a
new base device made, saved and booted again, 242.5 s), the rebuilt A **9.6 s** (reinstalled 2.7, on
screen 7.0), B **11.6 s** (A uninstalled, B installed 4.0, on screen 7.6; screenshot red), `stop`
3.2 s, `stop_device`. A later first call there boots the saved base device (~110-140 s on this box).

## Levers: measured, kept, reverted

| lever | result | kept? |
|---|---|---|
| 1 warm device + live install | above | yes |
| 2 wallpaper wait (static RRO) | -30 s (FallbackHome drawn -> boot_completed 30 s -> 2-4 s) | yes |
| 2 `/dev/loop-control` | -20 s before servicemanager | yes |
| 2 odsign left out | -5 s | yes |
| 2 no boot animation | slower in 3/3 pairs | **reverted** |
| 2 fewer services/apps | nothing more left out tonight (the wins above were waits, not work) | -- |
| 3 no dexopt at install | -5.8 s when artd was cold, -0.1 s warm; same app (dexopt failed anyway) | yes |
| 3 skip verification | nothing to skip: no package verifier on this image | -- |
| 4 no class preload in app processes | -1.8 s probe (8 pairs), -2.1/-3.7 s Roblox | yes |
| 4 spare app process (preloaded) | -4.7 s probe first frame (4/4 pairs), -2.9 s Roblox (2/2) | yes (warm device) |
| 4 real zygote fork (COW) | not possible here: every guest process of a host process shares one address space, and Windows has no fork; the spare gets most of it | -- |
| 5 persistent dynarmic cache | quantified: an app launch runs on ~1 core, main thread **~50% in dynarmic itself** (`dyn`: translating, lookups), 2-11% in translated code | write-up |
| 6 checkpoint/restore | `docs/research/2026-10-01-checkpoint-restore.md`: CRIU on headless Linux (llvmpipe) is the promising spike; Windows keeps the warm device | write-up |

## Gates

- `cargo test --release -p omni-linux --no-fail-fast` (Windows): 95 test binaries green; 2 fail --
  `d3g_gl_fallback` (the host GPU's GL pbuffer) and `mm::four_threads_wait_and_wake_on_a_futex_word_in_a_strict_split_page`
  -- and **both fail the same way on `main`** (`C:\od-base`, run tonight: d3g 2 failures there, the
  futex test 3 of 3).
- `omni-apk` (+ the new `launch_info`), `omni-mcp` (23 tests), `omnidroid`: green, except
  `omni-apk`'s `real_apk`, which fails on `main` too (the repo's `Roblox-2.738.1397.apk` is 159 MB,
  the tests expect the 229 MB stock file).
- The boot itself: the warm device, the MCP flow and Roblox's sign-in screen on Windows (above); the
  notebook flow on Linux (below).

## What's open

- **To use it**: nothing to configure -- `start_instance {apk}` without a cookie boots the warm
  device if none is up. To have it booting as soon as an agent session connects (so the first APK
  finds it ready), add `OMNI_MCP_WARM=1` to the omnidroid MCP server's environment (Claude Code:
  `~/.claude.json` -> `mcpServers.omnidroid.env`; pi: `omniMcp.env`) -- not changed tonight. Also give
  it `OMNIDROID_DYNARMIC_BUILD_DIR=C:\od-unified`, or its first `omnidroid aosp` rebuilds dynarmic
  (the Claude Code entry has no env today).

- **App start is still the clock** (probe ~5-6 s from a spare; Roblox ~20 s): bindApplication,
  onCreate and the first frame (HWUI/EGL init), on one thread that spends about half its time in
  dynarmic translating. Next: a **translation cache shared across app host processes** (they all
  translate the same libart/framework code); a spare that is already past HWUI's init.
- The spare's timing: it is taken by the next app of any kind (at boot, system apps took several),
  and is ready ~10-15 s after the previous one was taken; back-to-back starts closer than that get
  a spare still starting (no worse than without one).
- Each control-channel command spawns a guest `sh` (0.43 s); a persistent shell would take ~1 s off
  a start (install + start = 2-3 commands).
- The warm device's idle cost: ~7 host processes (network stack, permission controller, media...);
  not measured tonight.
- `real_apk` tests in omni-apk fail on this checkout's `Roblox-2.738.1397.apk` (159 MB, the tests
  expect the 229 MB stock file): pre-existing, not touched.
- **Linux, the Colab flow** -- done, every cell (`notebooks/omnidroid_boot_bench.ipynb` via
  `tools/nb_run.py`; the box's i5-4460 pinned to 2 CPUs with `taskset -c 0,1`, Mesa llvmpipe;
  commit `deb82be`; result `~/cache/bench/20260930-2331-i5-4460-2cpu-llvmpipe-fastboot.json` on the
  box): new device `boot_completed` **138.0 s**, app on screen **266.3 s** (the handoff's previous
  figures for this shape: ~218 / ~389 s); saved device median **128.0 / 205.6 s** (was ~179 / ~265),
  peak memory 5.3 GiB. The first attempt failed on the descriptor limit (fixed, item 8).

## Reproduce

```powershell
# Windows, PowerShell, repo root; nothing else booted (one device per host)
$env:OMNIDROID_DYNARMIC_BUILD_DIR = "C:\od-unified"
cargo build --release -p omnidroid -p omni-mcp -p omni-linux
cargo test --release -p omni-linux --test r_roblox --no-run
bash tools/make_test_apks.sh                       # target/test-apks: a a2 a-v2 a-key2 b
$A = "$PWD\target\test-apks"
python tools/mcp_demo.py "start:$A\a.apk" "start:$A\a.apk" "start:$A\a2.apk" "start:$A\a-v2.apk" `
  "start:$A\a-key2.apk" "start:$A\b.apk" "shot:b.png" "stop" "start:$HOME\Desktop\Roblox-2.740.931.apk" "stop"
python tools/device_ctl.py "pm list packages -3"   # the warm device, adb-shell style
python tools/device_ctl.py "touch /data/local/tmp/stop"   # shut it down
python tools/boot_bench.py --label w --warm        # a warm device's boot, from its [t] marks
python tools/boot_bench.py --label base --repo C:\od-base --dyn-dir C:\od-base-dyn --apk $HOME\Desktop\Roblox-2.740.931.apk --forget-saved
python tools/warm_ab.py launch --apk $A\a.apk --package com.omnidroid.probe --activity com.omnidroid.probe.MainActivity --pairs 4 --b-env OMNI_APP_PRELOAD=1
```

Claude Code picks the new tools up from the rebuilt `target/release/omni-mcp.exe` at its next
start (the running sessions hold the old binary, renamed `omni-mcp.old-*.exe`).
