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

## s11 -- the home's spare started when system_server starts (KEPT)

s8's spare started at the first app's start, ~1.3 s before the home: not enough for its preload.
It now starts when system_server sets `sys.system_server.start_count` (~5 s before the home), the
first-app start kept as a fallback; `OMNI_HOME_SPARE_EARLY=0`: as s8.

| | home displayed after | `[r] boot_completed` | warm ready |
|---|---|---|---|
| s8's (E1-E3) | 2.216 / 2.238 / 2.238 s | 12.8 / 12.9 / 12.9 s | 13.1 / 13.2 / 13.1 s |
| early (F1-F3) | 1.015 / 1.018 / 1.007 s | 11.9 / 11.8 / 11.8 s | 12.2 / 12.1 / 12.1 s |

**-1.0 s to warm ready (3/3).**

## s12 -- init skips what fails or does nothing here (KEPT)

`apexd-bootstrap` (init's first `exec_start`) failed at every boot -- no loop-control or
device-mapper, "/apex/sharedlibs: Read-only file system", exit 1 -- having set and written
nothing: now a service the device leaves out (`OMNI_KEEP_SERVICES=apexd-bootstrap` runs it). The
root module scripts (`omni_root_pfd`, `omni_root_service`) are not started on a device the host
staged no root for (they exited at once).

| | init's boot commands | system_server entered | warm ready |
|---|---|---|---|
| bootstrap apexd run (A1-A3) | 2083 / 2105 / 2083 ms | ~5.0-5.1 s | 12.0 / 12.2 / 12.3 s |
| skipped (B1-B3) | 1929 / 1959 / 1961 ms | ~4.1-5.0 s | 12.0 / 12.1 / 12.1 s |

**-140 ms of init (3/3)**; warm ready within noise.

## s13 -- /linkerconfig made once per image (KEPT)

Both of init's `perform_apex_config` ran the same `linkerconfig --target /linkerconfig` (~200 ms
each, critical path) though its inputs are fixed by the image here, and a saved device keeps the
output. It is now made once and stamped (`/linkerconfig/.omni-key`: the manifest, what the device
leaves out, the APEX list, the device's own files); a matching stamp skips it.
`OMNI_LINKERCONFIG_KEEP=0` runs it every time.

| | linkerconfig | init's boot commands | warm ready |
|---|---|---|---|
| every time (A1-A3) | 249+217 / 284+200 / 262+211 ms | 1935 / 1961 / 1942 ms | 12.2 / 12.2 / 12.1 s |
| once (B1-B3) | kept / kept / kept | 1537 / 1532 / 1555 ms | 11.8 / 12.7 / 11.9 s |

**-410 ms of init (3/3)**; warm ready 2 of 3 pairs sooner (B2 an outlier).

## s14 -- the BPF loader beside the boot (NOT KEPT, reverted)

`exec_start bpfloader` (360 ms) run on a thread of its own, joined before `class_start` (netd waits
for `bpf.progs_loaded` itself):

| | init's boot commands | `[r] boot_completed` | warm ready |
|---|---|---|---|
| waited for (A1-A3) | 1520 / 1530 / 1530 ms | 11.3 / 11.3 / 11.3 s | 11.7 / 11.6 / 11.6 s |
| beside (B1-B3) | 1175 / 1178 / 1171 ms | 11.3 / 11.4 / 11.4 s | 11.6 / 11.7 / 11.7 s |

Init 350 ms shorter, the boot not: the critical path is past init now. Where it is (`[boot-ms]`,
s14's logs): system_server's program starts at 1.75 s, ART is up at ~3.0 s, its class preload
1.1 s, system_server's services 5.2 s (PackageManager 1.8 s), the home (in its spare) +0.2 s,
then the user's unlock ~1.1 s to boot_completed at 11.2 s.

## s15 -- translation snapshots again, 4 minutes after ready (MEASURED, owner's call)

With s1-s13 in, `OMNI_JIT_SNAPSHOT=1` against the default, 240 s after warm ready (after code
aging's first pass), interleaved after one fill run:

| | warm ready | all hosts private | system host | idle CPU (30 s, % of a core) |
|---|---|---|---|---|
| snapshots (F1-F3) | 10.7 / 10.6 / 10.6 s | 1357.7 / 1342.9 / 1336.1 MB | 967.6 / 950.9 / 941.0 MB | 5.68 / 4.48 / 4.79 |
| default (N1-N3) | 11.2 / 11.2 / 11.2 s | 1222.3 / 1221.8 / 1219.8 MB | 841.2 / 841.0 / 839.3 MB | 7.12 / 8.64 / 7.03 |

The boot gain is smaller now (-0.5 s; s3's -2.2 s was before the home and init fixes); the RAM cost
stays after aging (+100..+126 MB, system host); new: **idle CPU -2.5 points of a core** with them.

## s16 -- binder handle references counted: the per-launch leak closed (KEPT)

Found from the GPU memory: the system host's write-combined memory was 42 MiB on an idle device,
~76 after one app launch, 124 after eight, 183 after a dozen -- and stayed after the app ended.
`OMNI_AHB_TRACE=1` (new): the host-visible mirrors of gralloc buffers SurfaceFlinger's ANGLE
imports went 7 -> 21 live over 8 launches (+2 a launch). `dumpsys SurfaceFlinger --list`: every
launch left its task, activity, window (with the app's last buffers) and leash layers offscreen
-- 76 layers after three launches. Not GC (forced, no change), not the window presenter's import
cache (`OMNI_PRESENT_KEEP_FRAMES=2`, no change), not blur (`ro.surface_flinger.supports_background_blur=0`,
no change). Cause: the binder broker ignored `BC_ACQUIRE`/`BC_RELEASE`/`BC_INCREFS`/`BC_DECREFS`
from live processes -- "a live process's handles last as long as it does" -- so system_server,
which never ends, never let go of a layer handle, and SurfaceFlinger never destroyed a layer.

Now each handle's references are counted per process; a handle whose counts reach zero, with no
transaction in flight naming its object, is deleted (re-checked when the object is unpinned), and
the object released by its owner when no one else refers to it. Never handle 0, never the host's
own handles. The counting commands never fail (libbinder resends a write from where a failed
transaction stopped it). `OMNI_BINDER_REFCOUNT=0` is the old way. Tests: two binder tests whose
raw clients used a handle without acquiring it now acquire it as libbinder does; a new one,
`binder_handle_release`, has a live process let a handle go and its owner hear the release.
Checked live: 25 launches on one device (layers flat at 52, 4 mirrors, no new error -- the one
`DeadObjectException` is WindowManager telling a force-stopped app, in the old way's runs too),
Roblox (Delta) to the same screen as without.

After 8 launches of the probe app on one device (force-stopped after each), 30 s later, same build:

| | SurfaceFlinger layers | live mirrors | system host commit | its GPU memory | its private bytes |
|---|---|---|---|---|---|
| not counted (A1, A2) | 116 / 116 | 20 / 20 | 817 / 812 MiB | 127 / 125 MiB | 939 / 934 MB |
| counted (B0-B2) | 60 / 60 / 60 | 4 / 4 / 4 | 748 / 754 / 752 MiB | 68 / 69 / 70 MiB | 870 / 872 / 873 MB |

**-60..-65 MiB after 8 launches, and no growth after**: 25 launches ended at 743 MiB, 52 layers.
Launch times unchanged (1.2-1.5 s TotalTime either way).

## The saved devices gutted again (fixed, 302e04b5)

Mid-run every device stopped coming up: `/data/apex/sessions`, `/data/tombstones`, `/data/local`
were gone from every saved device in `%TEMP%\omni-golden` -- Windows' empty-folder cleanup (C: at
~20 GB free), as on 2026-10-03; `.omni-init-dirs` said they were made, so nothing made them, and
apexd aborted. The marker now lists them and each boot re-makes what is gone (`init_dirs_healed`).
The owner's pool's saved devices were among the gutted ones: set aside as `.gutted-20261022`
(main still trusts the marker -- this commit is one to take first).

## s17 -- no notification assistant (KEPT)

After boot an idle warm device keeps app processes for the media provider, the network stack, the
home, the spare -- and `android.ext.services`, bound by NotificationManager for its notification
assistant (ranking, smart replies) and nothing else (`dumpsys activity services`). `omni_quiet.sh`
now disallows the assistant for the user it quiets (`cmd notification disallow_assistant <comp>
<user>`; the pool's new users too); WARM_SETUP 5, DEVICE_SETUP 4. Measured live first: the process
ended within seconds of the command.

| 240 s after warm ready | all hosts private | warm ready |
|---|---|---|
| with the assistant (s15/sl runs N1-N3, same build but this) | 1220.9 / 1214.8 / 1219.8 MB | 11.2-11.3 s |
| without (B0-B2) | 1138.4 / 1137.3 / 1135.9 MB | 11.7 / 11.4 / 11.3 s |

**-80 MB per warm Android** (and per pool user), the process gone.

Also measured: an idle system host with the threads named (`omni-linux-<tid>` guest threads,
helpers by name) 90-150 s after ready is 1.7% of a core -- the steady script's 60-90 s window still
holds post-boot work. Its largest idle consumers: the zygote's connection threads, the working-set
trimmer, the control channel's poll, omni_lean's `cmd` every 30 s -- each 1-2 ms a second.

## s18 -- no boot animation; the window's GPU presenter only for an app's frames (KEPT, f13abe10)

`OMNI_PRESENT_ZERO=0` against the default showed the system host 118 MB lighter on an idle warm
device (708 against 826-829 MB, two pairs). `=ready` -- devices still exporting share images, the
lever off -- was as light as `=0` (705-707 MB, two triples), so it is not the exports (three
1280x720 images, 11 MB of device memory: `OMNI_SHARE_TRACE=1`) but what the lever turns on: the
display window's Vulkan presenter, made at the window's start and kept.

The presenter is now made once the composer has a frame of share images and no sink. That alone
saved nothing (L/E pairs 1212-1217 MB both): the boot animation draws through ANGLE's Vulkan, its
frames are share images, and the presenter was made for it -- and stayed. So the boot animation is
off too (`debug.sf.nobootanimation=1` in omnidroid's property overlay; on 2026-10-01, at ~46 s
boots, ready was later without it, not now):

| 60 s after warm ready, 4 ABBA pairs | all hosts private | system host | warm ready | boot CPU (live hosts) |
|---|---|---|---|---|
| boot animation (B1-B4) | 1214.7 / 1211.3 / 1215.0 / 1213.3 MB | 823.6-829.2 MB | 11.4 / 12.4 / 11.3 / 11.3 s | 68.0 / 64.2 / 67.9 / 64.9 s |
| none (N1-N4) | 1080.5 / 1078.7 / 1080.3 / 1077.0 MB | 691.1-695.7 MB | 11.5 / 11.3 / 11.4 / 11.2 s | 64.5 / 66.4 / 66.1 / 62.2 s |

**-134 MB per idle warm device**, ready the same. The probe app's launch makes the presenter and
is shown from its share images as before. `OMNI_PRESENT_EAGER=1` / `OMNI_R_SETPROPS=
debug.sf.nobootanimation=0` restore either half. (The `E/SystemServiceRegistry` stack under
`bootAnimationComplete` -- no USB manager -- is in the runs with the animation too.)

## s19 -- the presenter dropped once the app is gone (KEPT, c7443ff5)

The composer says frame by frame whether its frame had share images; the window drops its
presenter 10 s after the last one (`[window] present_gpu off: GDI (no app frames to show)`) and
makes it again for the next app. Probe app launched twice and stopped, 30 s later, two ABBA pairs:
the system host 712 / 725 MB against 845 / 845 MB kept (`OMNI_PRESENT_EAGER=1`); launches the same.

## s20 -- the idle loops wait without starting a process (KEPT, f13cb32f)

`OMNI_MEM_TRACE=30` on an idle device: 47 guest processes in the system host, two of them `sleep`
with ~6 MiB of translated code each. `omni_autogrant.sh` polled `/data/app` with `sleep 2` forever
-- a guest process every 2 s, its linker, libc and toybox translated afresh -- and `omni_lean.sh`
looped on `sleep 30`. Both now wait with `read -t <s> -u3` on a `cat |&` coprocess that never
writes (one process for the session; omni-linux has no mkfifo). `persist.omni.tick=sleep` is the
old way.

First try also ended omni_lean's loop on the kiosk device (its home is not Settings): that kept
android.process.media (+75 MB) cached after boot, as on 2026-10-1x -- the loop's `cmd activity kill`
is what has ActivityManager apply the cached-process limit. The loop stays, waiting the new way.

| 60-120 s after ready, 4 ABBA pairs | idle CPU, all hosts | system host CPU | private, all hosts | warm ready |
|---|---|---|---|---|
| `sleep` (S1-S4) | 7.34 / 9.21 / 8.45 / 8.01 % | 6.35-8.05 % | 998-1008 MB | 11.6-12.7 s |
| tick (T1-T4) | 2.74 / 2.71 / 2.39 / 2.76 % | 1.61-2.19 % | 988-991 MB | 11.5-11.9 s |

**Idle CPU -5.3 points of a core per warm device** (two thirds of what it used), -13 MB.

## s21 -- statsd and mediametrics left out (NOT KEPT)

`OMNI_LEAVE_OUT_SERVICES=statsd,mediametrics` (a measuring lever, kept): warm ready 27.5-27.7 s
against 11.5-11.8 s (something waits ~16 s for statsd), for -5 MB. Reverted.

## The owners' log emptied itself (fixed, 935b9bc4)

The boot's system_server timings showed `AppDataPrepare` 1.5 s and 237 lines of installd's "Failed
to prepare /data/data/<pkg>: Function not implemented" -- the errno stale: libcutils said "Expected
path /data/data/android with owner 1000:1000 but found 0:0", and PackageManager then destroyed and
remade each system app's data at every boot. The saved device (v5, made 09:02) had 98 owner lines
of the v4's 1,465, none for /data/data: every host process compacted the log at load with a
truncating write, and one starting meanwhile wrote back what little it had read. Compacted now only
by the boot's first process; WARM_SETUP 6 / DEVICE_SETUP 5. Boots from the new saved device: no
failures, AppDataPrepare 716-735 ms, warm ready 11.1-11.4 s (the v5 ones 11.5-11.8 s).

## s22 -- the app without the lab's CPU backend: omnidroid.exe 7.8 -> 2.9 MB (KEPT, 8e829458)

PDB section contributions: dynarmic was 3.65 of omnidroid.exe's 7.79 MB -- the app's MCP server ran
the lab's tools in-process (`omni_mcp::Server`). The lab now runs in `omni-mcp --lab` (stdio, started
at the first lab call, its sessions kept), the app lists the lab's tools from their table and
forwards their calls: **omnidroid.exe 2.91 MB (-63%)**. omni-linux-run.exe (12.4 MB): omni-linux
7.56 MB, dynarmic 3.71 MB, the rest small; `panic = "abort"` not tried (dynarmic's callbacks catch
panics to record them).

User creation, measured on a warm device: `pm create-user` 1.6-1.7 s, the first `start-user`
2.9-4.0 s (RoleService grants default roles through a PermissionController process of the user,
2.7 s); a start after the app's `install-existing` 1.07 s (the grant 66 ms) -- the pool makes and
starts a user ahead, so nothing to take there.

## Found, not acted on (yet)

- **Idle code-cache invalidations**: `[jit-time]` counts 2,000-9,000 invalidations / 30 s in the
  system host at idle (+0 blocks). Not the guest's instruction-cache operations: a trace of
  `cb_icache_op` (`OMNI_IC_TRACE`, tried and removed) saw none at idle -- they are the backend's
  one-word plants and unplants (thunks, sentinels), each one thread's JIT, 4 bytes: cheap.
- **Snapshots lower idle CPU**: 3.8-4.8% of a core against 6.9-7.2% without (s15 and a rerun with
  the 256 MiB budget, `OMNI_JIT_SHARED_CACHE_LIVE_MB=256`, which keeps the same idle CPU for ~-25..+90
  MB). Why is not found; the boot gain now converges only after two filled runs (10.5 s against
  11.2-11.3 s), +30..+130 MB.
- **An app's first frame**: HWUI's first frame of the probe app is ~740 ms ("Davey!"), most of it
  ANGLE bringing up Vulkan on the host GPU in the app's process. A spare could initialise EGL ahead
  -- but a waiting spare holding a GPU stack is ~90 MiB more on every warm device (the home's
  GPU-drawn 165 MiB against software-drawn 72 MiB), so not done.
- **Install** of the 243 MB Delta APK on a warm device: 1.5 s (`pm install -r -g`); first launch
  of Roblox on it 4.5 s (TotalTime, with a spare).
- **The Delta APKs on the Desktop** (Delta-updated 2.743, Delta-rebuild) both stop at Android's
  "All files access" page for themselves: they ask for MANAGE_EXTERNAL_STORAGE without declaring
  it, so autogrant skips it and AppOps refuses even `cmd appops set` (stays `default`). The same on
  `main` -- no in-game check was possible with them.
- **Binary size**: omni-linux-run.exe 12.4 MB (omni-linux 7.5 MB with the device's embedded files,
  dynarmic 3.7 MB); fat LTO 12.2 MB (-1.6%), not worth its build time; the image decoders (~0.5 MB)
  serve clipboard pastes.

- System host idle: ~2-7% of a core; `StorageManagerService` asks for `storaged` (left out) once a
  second forever -- cheap, a stub daemon would cost more than it saves.
- JIT at boot (`OMNI_JIT_TIME=5`): the system host translates 1.13 M blocks by warm ready, 3.1 s
  frontend + 7.5 s emission; each app process 120-210 k blocks, 1.1-1.9 s. Guest libraries are at
  random addresses per process (`/proc/self/maps`), so blocks are not shared between processes.
- An idle app host gives its translations back (home: 57 -> 2 MiB of code in ~1 min); idle app
  processes sit at ~72 MB private (35 MiB committed by the sweep's count).
