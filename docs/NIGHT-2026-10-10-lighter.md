# Night 2026-10-09/10 -- lighter and faster, for weaker PCs too (Windows; branch `perf/lighter-1010`)

Owner's goal (open-ended, until stopped): less CPU and RAM, faster in-world (judged by CPU per frame
at the 60 fps cap, and by fps on a weaker CPU), faster boot. Base: `main` dc72223 (the PS99 60 fps
night merged). Every change lands on this branch with a switch; `main` is the owner's.

Setup: Windows i7-13700F (8P+8E, 24 threads), RTX 4060, 32 GB -- but the owner's `llama-server`
(10 GB working set, 15 GB private), a MuMu VM (2.3 GB) and `RobloxPlayerBeta` are running: free
commit ~11.5 GB, so the harness now takes `-MinCommitGB` (`OMNI_AB_MIN_COMMIT_GB`, used: 8). APK
`Desktop/Roblox-2.740.931.apk` (the Delta build, as the owner asked), cookie `HeZmI_ImYu1080`, PS99
(8737899170), WARP exit TR.

## Harness additions (`tools/perf_ab.ps1`)

- `-Affinity <hex>` (`OMNI_AB_AFFINITY`): the run and every host process it spawns are held to
  those CPUs -- a weaker PC on this one. `FF0000` = the eight E-cores, `F0000` = four.
- `loaded_s` (start to `onGameLoaded`), `cores` (all hosts' CPU over the window, in cores), `warp`.
- A network gate: no boot until `www.roblox.com` answers (below, why).

## Findings before any change

**A run without the bypass looks like a crash.** At 22:2x WARP's client said "Connected" but
`cdn-cgi/trace` said `warp=off`: every TLS connection of the app was reset ("Error pre-warming TLS:
java.net.SocketException: Connection reset", `IOException initial newCall()` on every
`*.roblox.com` URL), the app never initialised, and **Delta's `libzstd-jni` worker killed it 20 s
after the library loaded** (`SIGSEGV ... fault addr 0x40`, lr `libzstd-jni+0x5f42a0`) -- the same
signature as the 2026-09-29 "startup race" (docs/research/2026-09-29-libzstd-jni-16k.md). That
worker sleeps ~20 s and then calls through a table the app's initialisation fills: anything that
holds the initialisation back past 20 s (a slow network, a slow CPU) kills the Delta build. Two runs
lost (weak4e 22:23, base 22:37). The harness now waits for roblox.com before booting.

**The setup's `svc power stayon true` starts a Java VM that always aborts** ("Failed anonymous
mmap(0x0, 67108864): Out of memory" in the shell's `app_process`, ~3,600 log lines of maps), and the
screen is not kept on by it. The native `settings put global stay_on_while_plugged_in 7` is now the
default (`OMNI_R_SVC_STAYON=1` = the old way).

## Baseline (dc72223)

| arm | CPUs | fps | all ms/frame | top / top2 ms | cores | join | loaded | private WS | system host |
|---|---|---|---|---|---|---|---|---|---|
| base (s1, 4 runs) | all 24 | 57.4-59.0 | 42.5-47.4 | 11.9-13.3 / 10.0-11.2 | 2.48-2.72 | 85-90 s | 97-104 s | 3.035-3.205 GB | 0.900-0.924 GB |
| weak8e | 8 E-cores | **33.79** | 54.55 | 20.15 / 10.33 | 1.84 | 150 s | 188 s | 3.158 GB | 0.972 GB |
| weak4e | 4 E-cores | -- | | | | (no network: see above) | | | |

**Where first translation goes** (`OMNI_JIT_TIME=5`, s1): every Java process translates the
framework again -- each small app host 145-213k blocks, ~0.8 s translate + ~1.5 s emit; the system
host 1.57M blocks by sign-in (7.3 s + 14.8 s); the game ~1M blocks by DID_LOG_IN (5.4 s + 10.2 s).
Emitting x64 costs about twice the frontend, everywhere.

On eight E-cores only 1.84 cores are busy at 33.8 fps: the frame is a cross-thread critical path
(the engine worker 20 ms of each 29.6 ms frame), not a shortage of cores.

## Ledger

| # | change | A/B | result | kept? |
|---|---|---|---|---|
| 1 | setup keeps the screen on with `settings` (native), not `svc` (a Java VM that aborts) | s1 ABBA svc/native, 2 pairs | join 90/85 vs 85/85 s, world 104/103 vs 97/99 s, WS 3.108/3.205 vs 3.076/3.035 GB: inside noise, consistent sign; the removed process failed every time | yes (way back `OMNI_R_SVC_STAYON=1`) |
| 2 | `OMNI_DEVICE_IDLE_APPS=out` (8 idle apps not in the image) | s2, 1 pair (stopped for the JIT build) | WS 3.141 vs 3.139 GB, join 90 vs 85 s, world 104 vs 105 s | not yet (no RAM effect seen; retest) |
| 3 | dynarmic **0077**: IR accessors inline, `VerificationPass` only with `OMNI_JIT_VERIFY=1` | `the_speed_of_emission` (deterministic, byte-identical code) | frontend **7.60 -> 4.85 us/block** (i5-4460), 4.34 -> 3.16 (i7, verify on/off) | yes |
| 4 | dynarmic **0078**: small fastmem fallbacks (`OMNI_JIT_SMALL_FALLBACKS`) | `tests/resident.rs`, fallback bytes | a fresh cache's resident **0.88 -> 0.13 MiB** (Windows); fallbacks 1.89 MB -> 115 KB per cache (Linux) | yes |
| 5 | 0077 + 0078 on a device: `old` = `OMNI_JIT_SMALL_FALLBACKS=0 OMNI_JIT_VERIFY=1` | s3 `s3-jit.csv`, 3 pairs ABBA-BA | **fps 57.81/58.05/58.69 -> 58.70/59.14/59.33 (3/3)**; **engine worker 13.81/14.00/12.88 -> 11.85/10.64/12.33 ms/frame (3/3)**; game JIT per boot translate 11.0 -> 8.8-9.2 s, emit ~22 s both; join/world within noise; **system host WS +15-20 MB (3/3)** -- see below | yes, with #6 |
| 6 | code aging's floor 8 -> 6 MiB (`code_trim::MIN_BYTES`) | s5 `s5-snap.csv`, `new` vs `new8` (`OMNI_CODE_TRIM_MIN_MB=8`), 2 pairs | **private WS 3.079/3.102 -> 2.991/2.993 GB (-88/-109 MB)**; **system host 0.911/0.908 -> 0.822/0.824 GB**; all ms/frame 42.8/41.2 -> 38.6/41.2; fps, join, world the same. Cause of #5's +17 MB: a cache's commit counts its prelude, ~1 MiB smaller with 0078, so small quiet services (ueventd, gatekeeperd, HALs) fell under the 8 MiB floor and kept their translations; at 6 they are trimmed again, and more with them | **yes** |
| 7 | dynarmic **0079**: Xbyak's label manager without heap nodes (tsl robin map/set, flat waiting list) | `the_speed_of_emission`, byte-identical | emit **15.7 -> 13.1 us/block (-16.5%)**, i5-4460 | yes (device check s5) |
| 8 | dynarmic **0080**: a value's host location from a checked hint, not a search | same | emit **13.1 -> 11.9 us/block (-9%)** | yes (device check s5) |
| 9 | system_server and the HALs after servicemanager is ready, not a fixed 1.5 s sleep (`OMNI_INIT_FIXED_WAIT=1` = old) | s6 `s6-boot.csv`, ABCCBA, 2 pairs; `[t]` milestones | servicemanager was ready "after 0 ms": **system_server 12.5/12.1 -> 10.4/10.4 s, boot_completed 30.8/30.8 -> 29.5/29.5, DID_LOG_IN 59.2/61.0 -> 58.6/58.6, onGameLoaded 87.3/88.0 -> 84.7/85.7** | **yes** |
| 11 | the game's live code 512 MiB (`OMNI_JIT_SHARED_CACHE_LIVE_MB=512`, default 256) | s7, 2 pairs | 2 regions retired during the start instead of ~18; onGameLoaded and private WS unchanged | no (neutral; with snapshots: s9) |
| 10 | the place's link at once after sign-in (`OMNI_R_LINK_DELAY=0`, default 3) | s6, 2 runs | Joining 71.8/63.6 vs 70.6/67.7: the sign-in-to-Joining gap swings 5-14 s run to run (matchmaking, network) | no (inconclusive; default unchanged) |

**Why the in-world worker got cheaper with faster translation:** the game keeps translating in the
measured window -- after its code-aging pass (3 min) it translates the hot code again in bursts of
30-45k blocks per 5 s (~0.5-0.65 s of JIT per 5 s, 10-13% of a core) -- and the engine worker does
much of that itself, on its own frame time.

**Where the system host's memory is** (s3 `[mem]`, ~5 min after start): 62 guest processes, guest
memory 210 MiB, translated code 326-338 MiB committed, the JIT's tables on the heap 78-95 MiB (block
map, links, fastmem sites, guest ranges: ~155 bytes per live block, beside ~380 bytes of code). The
JIT is about half of that host's working set.

## Session s5 (00:32-01:22): translation snapshots, and code aging's floor (`s5-snap.csv`)

Build fd73108 (0076-0080, floor 6 MiB). `snap` = `OMNI_JIT_SNAPSHOT` + `_LAZY=1` + `_LIB_ZONE=1` +
`_FORGET=1` (one fill run first). Milestones from the log's `[t]` clock (seconds from start):

| arm | system_server | boot_completed | DID_LOG_IN | Joining | onGameLoaded | private WS | system host |
|---|---|---|---|---|---|---|---|
| new | 11.5 / 11.4 | 30.6 / 30.4 | 58.9 / 58.4 | 72.0 / 71.4 | 86.0 / 85.5 | 2.991 / 2.993 | 0.822 / 0.824 |
| new8 | 11.4 / 11.5 | 30.5 / 30.7 | 58.6 / 59.0 | 71.2 / 73.0 | 85.6 / 86.0 | 3.079 / 3.102 | 0.911 / 0.908 |
| snap | 11.2 / 11.3 | **28.4 / 27.7** | **54.4 / 53.3** | **68.4 / 63.5** | **82.4 / 79.5** | 3.137 / 3.108 | 0.872 / 0.851 |

Snapshots: -2..-3 s to boot_completed, -4..-5 s to sign-in, -3..-6 s to the world; +115..+145 MB
private WS against `new`. Where they work and where not (`[jit-snapshot]` lines):
- native daemons, SurfaceFlinger, the small app hosts: **~98-100% of restored blocks verified** --
  the library zone puts every library at its home (SurfaceFlinger 141,931 of 143,869).
- system_server: 98% of 78,488 verified, but its snapshot is small: it was saved after code aging had
  dropped most of its translations (549k blocks translated anyway).
- **the game: 682,955 restored, 16,628 verified (2.4%)**, and the settle-time forget dropped 0 in 12
  ms -- the restored blocks were already gone from the cache (invalidated), so the game translated its
  1.7M blocks as without a snapshot. Its host's commit was +84 MiB (2,641 vs 2,557) with the same
  guest memory and JIT counters: the snapshot machinery's own memory for a game it does not help.
  Next: what invalidates the game's restored blocks (`[jit-time]` now counts invalidations).

## Session s6 (01:23-02:04): the boot's waits (`s6-boot.csv`)

| arm | system_server | boot_completed | DID_LOG_IN | Joining | onGameLoaded |
|---|---|---|---|---|---|
| fixed (`OMNI_INIT_FIXED_WAIT=1`) | 12.5 / 12.1 | 30.8 / 30.8 | 59.2 / 61.0 | 73.3 / 75.4 | 87.3 / 88.0 |
| smwait (default now) | 10.4 / 10.4 | 29.5 / 29.5 | 58.6 / 58.6 | 70.6 / 67.7 | 84.7 / 85.7 |
| nolink (+ `OMNI_R_LINK_DELAY=0`) | 10.5 / 10.4 | 29.6 / 29.2 | 58.8 / 58.5 | 71.8 / 63.6 | 85.9 / 82.6 |

The early boot, from one run's log: apexd done 4.2 s; init's `exec_start` chain (derive_sdk,
vold_prepare_subdirs, the BPF loader, ...) until 9.4 s, one fresh guest process at a time; gralloc
registered 9.4, composer 10.4 (`[t]` ticks are 1 s apart: the next build prints `[boot-ms]`);
system_server's runtime 10.4.

**Why the game's snapshot does not help (diag run):** the game's code cache keeps 256 MiB of live
code; its start translates ~1.4M blocks and retires regions all through it (18 regions, ~288 MiB,
between 54 and 88 s) -- with or without a snapshot. A restored snapshot (274 MiB) sits in the oldest
regions, so it is the first evicted, before most of it is entered. Invalidations are not the cause
(174k requests in the first 5 s dropped 41 blocks). Measured next (s7): a 512 MiB live budget.
Also: every rebuild of the host binary refuses every snapshot (host addresses are part of it).

## For the owner to decide

- **Saved devices by hard link, not copy.** A saved Roblox device is 781 MB: 237 MB of decompressed
  APEX images (the same on every device), 283 MB of the installed APK and its extracted libraries,
  253 MB of the app's caches. Every boot from it copies all of it (841 files, ~1.5 s measured) and
  every running instance holds its own copy on disk. Linking the files that never change (the APEX
  images, `base.apk`, `lib/`) would make the copy nearly instant and save ~520 MB of disk an
  instance -- but a guest write into a linked file would change the saved device itself, so it would
  need the saved files made read-only first. Not done: it changes the MCP's main path.
- **Old saved devices set aside** in `%TEMP%\omni-golden` (`...-gutted-<date>`) still take disk
  space; they are yours to delete.

## Session s7 (02:04-03:00): the game's live code budget (`s7-live.csv`)

`live512` = `OMNI_JIT_SHARED_CACHE_LIVE_MB=512` (default 256). With it the game retires **2 regions
during its start instead of ~18**; its code-aging pass at ~3 min then retires ~29 and memory comes
back to the same place. Two pairs: DID_LOG_IN 57.6 / 61.4 vs 58.2 / 60.0 s, onGameLoaded 83.7 / 89.5 vs
84.3 / 88.6, private WS 2.995 / 2.977 vs 2.965 / 3.027 GB: **neutral** on 24 threads, where the start's
re-translation is absorbed in parallel. Default stays 256 MiB; retested with snapshots (s9), where the
restored set needs the room.

**The snapshot arms of s7 are invalid** (an error of the session's design): a snapshot file is keyed
by the process, not by the cache's configuration, so `snap512` and `snap` overwrote each other's
files in one directory and each refused the other's (`app_process64 not installed (error -10)`).
A snapshot comparison needs one directory per configuration.

## Session s8 (02:51-): the working set, trimmed (`s8-trim.csv`)

`wstrim` = `OMNI_WS_TRIM=120`: every host process empties its working set every 120 s
(`K32EmptyWorkingSet`). What each kept, and what it had touched again two minutes later:

| host process | before | after the trim | 2 min later |
|---|---|---|---|
| system host | 1,294 MB | 29 MB | **187 MB** |
| game | 3,011 MB | 185 MB | **748 MB** |
| each idle app host (4) | 139-161 MB | 0 | **2 MB** |

| arm | fps | all ms/frame | private WS | system host | onGameLoaded |
|---|---|---|---|---|---|
| base | 59.03 / 59.45 | 45.40 / 40.58 | 2.982 / 2.965 GB | 0.819 / 0.819 GB | 86.7 / 85.5 s |
| wstrim | 59.47 / 59.06 | 39.82 / 43.89 | **0.735 / 0.734 GB** | **0.138 / 0.138 GB** | 83.7 / 84.8 s |

No hitch in the 5 s fps windows around the game's trim (59.92 / 59.71 / 60.11). Most of the memory
omnidroid holds resident is **cold**: the system host touches ~190 MB in two minutes, the game ~750
MB, the helper apps nearly nothing. What this frees for the machine is less than the private
working set says -- trimmed pages go to the system's compressed store (or the page file) -- so the
harness now also records available physical memory and the compressed store's working set
(`avail_gb`, `mc_gb`); the next sessions measure the real RAM freed.

**Weaker PCs on this build** (same session, one run each; `-Affinity`, the eight or four E-cores):

| | fps | engine worker ms/frame | all ms/frame | cores | boot_completed | DID_LOG_IN | onGameLoaded |
|---|---|---|---|---|---|---|---|
| 8 E-cores, baseline dc72223 (22:51) | 33.79 | 20.15 | 54.55 | 1.84 | -- | -- | 188 s |
| 8 E-cores, this build | 29.88 | 21.95 | 58.66 | 1.75 | 57.8 s | 107.8 s | **160.1 s** |
| **4 E-cores**, this build | **32.89** | 18.21 | 48.12 | 1.58 | 63.3 s | 122.1 s | **178.9 s** |

The world is reached **~25 s sooner on eight E-cores** (faster translation weighs more on a weak CPU).
**On four E-cores the Delta build now starts and plays** (~33 fps): libzstd-jni loaded at 97.8 s and
the app signed in at 122.1 s, its initialisation inside the injected worker's 20 s (the 22:23 four-core
run had no network, so it never got that far either way). In-world fps on the E-cores moves with the
live world as much as on the P-cores (29.9 on eight vs 32.9 on four, an hour apart): judging a lever
there needs interleaved pairs, as everywhere.

## Session s9 (03:3x-): snapshots with a 512 MiB live budget, one directory (`s9-snap512.csv`)

Build d9fbf71 (+ the snapshot saved before a code trim). Every arm `OMNI_JIT_SHARED_CACHE_LIVE_MB=512`;
`snap` adds snapshots (lazy, library zone, forget) in `C:\od-unified\jitsnap-s9-512`. What verifies now:

| process | restored | verified | |
|---|---|---|---|
| the game | 1,340,969 | **435,302 (32%)** | was 16,628 of 682,955 (2.4%) at 256 MiB: the budget holds it now |
| system_server | **495,587** | 65,817 (13%) | was 78,488: now saved before its trim; but most of it does not verify -- its code is mostly the framework's compiled `.odex`/`.oat`, which ART maps itself (not through `linker64`, so the library zone does not place it): next |
| native daemons, helper apps | 18-166k each | ~99.9% | |

First pair: `snap` boot_completed 27.5 s, DID_LOG_IN 53.2, onGameLoaded 81.6 vs `live` 29.6 / 57.9 / 85.9;
private WS 3.163 vs 2.966 GB (+197 MB). Snapshots stay opt-in: -4..-10 s for +100..+200 MB.
