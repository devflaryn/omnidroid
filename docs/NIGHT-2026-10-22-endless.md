# Perf ledger -- 2026-10-22, branch `perf/endless-1022` (Windows, i7, RTX 4060)

Owner's brief: keep finding ways to boot faster, use less RAM and CPU, be smaller and faster,
until stopped. Rules as before (`perf-night-rules`): side branch only, `main` untouched; a change is
kept when its measurement says so, each commit with its numbers.

## Method

- **Warm boot**: `tools/boot_bench.py --warm` (the saved warm device, timed to `[r] warm ready`
  from the log's own clock).
- **Steady state** (new, scratch `steady.ps1`): boot the saved warm device, wait 60 s past warm
  ready, then sum the private bytes of every host process of the instance and their CPU over 30 s.
- Builds go to `CARGO_TARGET_DIR=C:\odt` (`OMNIDROID_DYNARMIC_BUILD_DIR=C:\od-endless`): the
  owner's pool was open the whole time and holds `target\release`. `omnidroid aosp` runs
  `cargo test`, so a source edited during a bench is compiled into it (s2's first run) -- edits
  only between benches.
- The owner's own pool (one warm Android, idle) ran beside every measurement.

## Baseline (main a08bf02a)

| | warm ready (log) | from launch (wall) |
|---|---|---|
| saved warm device, 3 boots | 15.3 / 17.2 / 16.7 s | 16.4 / 19.0 / 18.0 s |
| new warm device (make + save + boot) | 39.1 s quiet, then 15.2 s | 58.8 s |

Where a saved boot's 15 s go (s1 log): init's boot commands to 2.1 s (apexd 0.46 s, bpfloader
0.37 s, init_user0 0.26 s); zygote preloads 17,758 classes in 1.2 s; system_server from 5.2 s
(StartServices 5.6 s: PackageManager 1.9 s of it, 170 packages from its cache in 1.1 s; app data
1.5 s); first app at ~10.9 s; boot_completed 15.0 s.

## s1 -- decompressed APEXes linked, not copied (KEPT, 40595a5e)

237 of a saved device's 248 MB were its own copy of the 21 decompressed APEXes, copied again into
every boot. The host cache is now read-only and hard-linked; `copy_device` links read-only files.
fsutil shows the cache file linked from both saved devices. Boot time unchanged (15.0-16.8 s).
**-237 MB of disk per saved device and per running device.**

## s2 -- the kiosk device's own home (KEPT, 0856b2ab)

FallbackHome (Settings) was the kiosk home: 223 MB private, per device and per pool user.
`com.omnidroid.home` (8 KB APK, `device/src/home/`), software-drawn.

| run (ABBA) | private, all hosts | warm ready | CPU (30 s, % of a core) |
|---|---|---|---|
| A FallbackHome | 1519.8 / 1468.7 / 1467.8 MB | 16.4 / 15.0 / 16.1 s | 12.9 / 8.2 / 9.2 |
| B own home, GPU-drawn | 1403.9 / 1407.8 / 1403.3 MB | 15.2 / 15.0 / 14.9 s | 9.0 / 9.5 / 9.0 |
| own home, software-drawn | 1301.1 / 1299.6 / 1294.5 / 1296.8 MB | 15.3 / 15.0 / 15.0 / 15.0 s | 8.4 / 8.9 / 8.3 / 9.2 |

**-170..-220 MB per warm Android** (home process 223 -> 72 MB), warm ready no later. The probe APK
launched on it draws (screenshot). Not run: a pool with users (the owner's pool held the machine's
one pool); each pool user's home is this app too, so the saving repeats per user.

**Correction (found in s7):** the device's `omni_lean.sh` already ended Settings every 30 s once
it was in the background (`cmd activity kill com.android.settings`). So the saving holds while the
home is in front -- an idle warm device, the pool's waiting state, where it was measured -- and is
transient once an app is in front. That loop also ends `android.process.media` (a "recent
provider" system_server keeps at PREVIOUS otherwise): ending the loop on kiosk-home devices kept it
alive (+76 MB in every run), so the loop stays.

## s3 -- translation snapshots on a warm boot (MEASURED, left to the owner)

`OMNI_JIT_SNAPSHOT=1` against the default, same build (s2's), interleaved after one fill run:

| | warm ready | private, all hosts | system host | available |
|---|---|---|---|---|
| snapshots (F1-F3) | 12.8 / 12.8 / 12.7 s | 1409 / 1431 / 1436 MB | 934-956 MB | 17.9-18.1 GB |
| default (N1-N3) | 15.0 / 14.9 / 15.0 s | 1293 / 1302 / 1293 MB | 830-837 MB | 18.1-18.2 GB |

**-2.2 s (-15%) to warm ready, +115..+140 MB private** (all of it the system host), 756 MB of
snapshot files on disk. The same trade the 2026-10-10 report left for the owner (+190 MB, the
world 10 s sooner); not switched.

## s4 -- inert overlays left out (KEPT, 1c394aca)

54 of the kiosk device's 134 packages were overlays that never apply (emulator skins for the
framework and SystemUI, cutout emulations, a font, the notes role -- all `[ ]` in `cmd overlay
list` -- telephony's, the navigation bar's on a kiosk). `OMNI_DEVICE_OVERLAYS=all` keeps them.

| run (ABBA) | warm ready | cached packages | StartPackageManagerService | all hosts private |
|---|---|---|---|---|
| A kept | 15.0 / 15.0 / 14.9 s | 171 | 2162 / 1944 / 1975 ms | 1300.9 / 1305.9 / 1307.6 MB |
| B left out | 13.7 / 13.7 / 13.7 s | 117 | 1777 / 1756 / 1769 ms | 1297.1 / 1295.1 / 1302.2 MB |

**-1.3 s to warm ready (3/3 pairs)**, system host ~-8 MB. A saved device is now a 7 MiB copy
(`[r] device saved ... 7 MiB in 319 ms`, s1's links).

Side effect found: the owner's pool runs `cargo test` in this checkout, so it built and booted
this branch's code (a `-md-kh` device appeared at 01:07; healthy, its home displayed, no crash).
From s5 on the work moves to a worktree (`C:\oe`) and this checkout is back on `main`.

## s5 -- system_server without the zygote's class preload (NOT KEPT, reverted)

`OMNI_SYSTEM_PRELOAD=0` left `/system/etc/preloaded-classes` out of the system's host process too
(apps' hosts already have none). Interleaved, same build:

| | system_server entered | boot_completed (log) | warm ready | all hosts private | machine CPU to ready+60 s |
|---|---|---|---|---|---|
| preload (A1-A3) | ~5.0-5.1 s | 13.4 / 13.4 / 13.4 s | 13.7 s x3 | 1296.9 / 1291.9 / 1291.1 MB | 170.0 / 170.1 / 170.9 core-s |
| none (B4-B6) | ~4.0-4.1 s | 13.4 / 13.4 / 13.4 s | 13.7 s x3 | 1292.7 / 1285.7 / 1293.4 MB | 164.5 / 171.5 / 169.3 core-s |

system_server starts 1 s sooner and boots no sooner: the classes it uses are loaded later instead,
and the home's start gates boot_completed. RAM and CPU inside the noise. Reverted.

What it showed: boot_completed landed on the same 13.4 s every run, and warm ready on 13.7 s --
the setup script polled `sys.boot_completed` with `sleep 1` (s6).

## s6 -- the boot script started by the host's property wait (KEPT)

The setup script's `until [ "$(getprop sys.boot_completed)" = 1 ]; do sleep 1; done` saw the boot
up to a second late. A `--then` shell whose first line is `#after <name>=<value>` now starts once
the property service (host side, woken by the change) has it; `BOOTED` begins with
`#after sys.boot_completed=1` and keeps its loop for an older runner. `OMNI_THEN_AFTER=0` turns it
off. Interleaved, same build:

| | LOCKED_BOOT_COMPLETED | `[r] boot_completed` | warm ready |
|---|---|---|---|
| polled (A1-A3) | ~13.2 / 13.0 / 13.0 s | 13.5 / 13.4 / 13.5 s | 13.8 / 13.7 / 13.8 s |
| host wait (B1-B3) | ~13.0 / 12.8 / 12.9 s | 13.2 / 13.1 / 13.2 s | 13.5 / 13.4 / 13.5 s |

**-0.3 s to warm ready (3/3)**, and every later step of an app session's script with it.

## s5 -- system_server without the zygote's class preload (NOT KEPT, reverted)

`OMNI_SYSTEM_PRELOAD=0` left `/system/etc/preloaded-classes` out of the system's host process too
(apps' hosts already have none). Interleaved, same build:

| | system_server entered | boot_completed (log) | warm ready | all hosts private | machine CPU to ready+60 s |
|---|---|---|---|---|---|
| preload (A1-A3) | ~5.0-5.1 s | 13.4 / 13.4 / 13.4 s | 13.7 s x3 | 1296.9 / 1291.9 / 1291.1 MB | 170.0 / 170.1 / 170.9 core-s |
| none (B4-B6) | ~4.0-4.1 s | 13.4 / 13.4 / 13.4 s | 13.7 s x3 | 1292.7 / 1285.7 / 1293.4 MB | 164.5 / 171.5 / 169.3 core-s |

system_server starts 1 s sooner and boots no sooner: the classes it uses are loaded later instead,
and the home's start gates boot_completed. RAM and CPU inside the noise. Reverted.

What it showed: boot_completed landed on the same 13.4 s every run, and warm ready on 13.7 s --
the setup script polled `sys.boot_completed` with `sleep 1` (s6).

## s7 -- boot receivers a kiosk device has no use for (KEPT, 4c8f81a9)

`/vendor/bin/omni_quiet.sh` (init's `omni_quiet`, root; the pool runs it for each new user)
disables 20 BOOT_COMPLETED receivers of Settings, PermissionController, AdServices' extension, the
package installer, Shell, MTP and downloads (the script names each). First try ran it from the
setup shell: "Shell cannot change component state", nothing disabled; then five receivers only:
Settings came back for its other six. Same build, interleaved:

| | app processes started | machine CPU to ready+60 s | warm ready |
|---|---|---|---|
| receivers on (A1-A3) | 14 / 14 / 14 | 169.0 / 162.1 / 165.7 core-s | 13.2 / 13.1 / 13.0 s |
| disabled (B1-B3) | 10 / 10 / 10 | 159.5 / 163.8 / 154.2 core-s | 13.0 / 13.0 / 12.9 s |

Settings, the package installer, Shell and AdServices no longer start after a boot. CPU lower in
2 of 3 pairs (median -6 core-s) -- small against the machine's noise, kept for the processes.

## s8 -- the kiosk home starts in a spare app process (KEPT)

The home (on boot's critical path: boot_completed waits for it to draw) took ~2.4 s from its start
to displayed, ~0.8 s of it a cold host process and ART. With spares on (`OMNI_APP_SPARE=1`: warm
and pool devices) the first spare now starts at the first app's start (the WebView relro maker,
~1.3 s before the home) and the home takes it; the next spare follows at once.
`OMNI_HOME_SPARE=0` turns it off. First try started the spare at the zygote's own start (pid
2000, before system_server): the home never attached and the boot waited 58 s -- not kept.

| | home displayed after | warm ready |
|---|---|---|
| cold (C1-C3) | 2.441 / 2.381 / 2.402 s | 13.3 / 13.2 / 13.2 s |
| spare (D1-D3) | 2.150 / 2.116 / 2.129 s | 13.1 / 13.0 / 12.9 s |

**-0.27 s to the home, -0.2 s to warm ready (3/3).**

## s9 -- the spare looks for its app every 50 ms, not 10 (KEPT)

A warm device keeps a spare app process waiting for its app; it looked for its "go" file every
10 ms -- a stat and a wake-up through the guest's code 100 times a second, for the device's whole
life. Now 50 ms (`OMNI_SPARE_POLL_MS` in the guest environment, e.g. through
`OMNI_APP_GUEST_ENV`): an app taken by a spare starts ~20 ms later on average.

| | all hosts' CPU, 60 s idle (% of a core) | warm ready |
|---|---|---|
| 10 ms (A1-A3) | 9.56 / 9.12 / 9.32 | 13.2 / 12.9 / 12.9 s |
| 50 ms (B1-B3) | 7.30 / 7.58 / 7.23 | 13.0 / 13.0 / 13.1 s |

**-2 points of a core idle (-22% of a warm device's idle CPU), 3/3.**

## s10 -- vsync while nothing is drawn

Found with two new counters (`[vsync] ... N frames presented`, and `OMNI_VSYNC_TRACE=1` logging
SurfaceFlinger's `setVsyncEnabled`): on an idle warm device SurfaceFlinger turned vsync on at boot
and never off -- 1800 vsyncs delivered and 0 frames presented in 30 s, each one a binder
transaction into SurfaceFlinger. `dumpsys SurfaceFlinger --vsync`: `hwVsyncState=Enabled`,
`mMoreSamplesNeeded=1`, "No Last HW vsync" -- its VsyncReactor keeps asking for samples, so it never
turns hardware vsync off (why its predictor keeps rejecting them is not found yet; its model reads
period 16.67 ms, intercept 38 ns).

`OMNI_VSYNC_IDLE_EVERY=<n>`: once no frame has been presented for 1 s, only every n-th vsync is
delivered; the first frame presented brings every one back (SurfaceFlinger times its frames with
its own predictor, not these callbacks).

| | vsyncs delivered / 30 s idle | all hosts' CPU (% of a core) | system host |
|---|---|---|---|
| every vsync (A1-A3) | 1800 / 1801 / 1801 | 7.47 / 7.59 / 7.67 | 6.64 / 6.54 / 6.59 |
| every 30th (B1-B3) | 61 / 61 / 61 | 7.35 / 7.66 / 7.52 | 6.88 / 6.59 / 6.64 |

**No difference: not kept** (the throttle reverted; the frames-presented count and
`OMNI_VSYNC_TRACE` stay, as diagnostics). A vsync callback is cheap; the system host's idle
~6.6% of a core is elsewhere -- and `OMNI_THREAD_CPU` shows system_server's own threads idle.

## Found, not acted on (yet)

- System host idle: ~2-7% of a core; `StorageManagerService` asks for `storaged` (left out) once a
  second forever -- cheap, a stub daemon would cost more than it saves.
- JIT at boot (`OMNI_JIT_TIME=5`): the system host translates 1.13 M blocks by warm ready, 3.1 s
  frontend + 7.5 s emission; each app process 120-210 k blocks, 1.1-1.9 s. Guest libraries are at
  random addresses per process (`/proc/self/maps`), so blocks are not shared between processes.
- An idle app host gives its translations back (home: 57 -> 2 MiB of code in ~1 min); idle app
  processes sit at ~72 MB private (35 MiB committed by the sweep's count).
