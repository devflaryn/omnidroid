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
| 12 | working-set trims: periodic `OMNI_WS_TRIM=120`, idle `OMNI_WS_TRIM_IDLE=30` | s8 + s10, 2 pairs each | **available memory +1.9 GB** (9.2 -> 11.1, twice), private WS 3.0 -> 0.63-0.73 GB; fps/CPU/vsync unchanged | **yes, default** |
| 13 | init's class_start side by side | s10, 2 runs | class_start 2.37 -> 0.42 s; system_server 10.3 -> 8.4/9.2 s; boot -1.0 s, sign-in -1.4 s | **yes, default** |
| 11 | the game's live code 512 MiB (`OMNI_JIT_SHARED_CACHE_LIVE_MB=512`, default 256) | s7, 2 pairs | 2 regions retired during the start instead of ~18; onGameLoaded and private WS unchanged | no (neutral; with snapshots: s9) |
| 10 | the place's link at once after sign-in (`OMNI_R_LINK_DELAY=0`, default 3) | s6, 2 runs | Joining 71.8/63.6 vs 70.6/67.7: the sign-in-to-Joining gap swings 5-14 s run to run (matchmaking, network) | no (inconclusive; default unchanged) |
| 14 | **one `Sysroot` per host process** (`OMNI_SYSROOT_SHARED=0` = one per open); object sizes checked once per pin (a marker in `objects/`) | Linux i5-4460, `omni-linux-run` with services; 535 opens counted in one PS99 session's log | a spawn after the first **70 -> 3 ms**; a host process's first open 27.7 -> 20.6 ms; init's setprop/wait_for_prop no longer open it | yes (5f524ca); device A/B s14 |
| 15 | dynarmic **0084**: Xbyak writes a byte in place, grows out of line | `the_speed_of_emission`, now `OD_PROD=1` (the device's switches), byte-identical | emit **11.06 -> 10.51 us/block (-5%)**, 3/3 | yes |
| 16 | dynarmic **0086**: `Inst::GetArg` inline; `SetArg`'s type check with `OMNI_JIT_VERIFY=1` | same | frontend **4.80 -> 4.62 (-4%)**, emit **10.54 -> 10.26 (-3%)**, 4/4 | yes |
| 17 | dynarmic **0087**: opcode return types and the passes' predicates from tables | same | frontend **4.61 -> 4.28 (-7%)**, emit **10.26 -> 10.02 (-2%)**, 4/4 | yes |
| 18 | dynarmic **0081-0083** together (get/set across a width change, spills to free callee-saved registers, zero-extension aliasing), on by default since s10's build | s11 `s11-codegen.csv`, ABBA at full CPU and on 8 E-cores (`FF0000`); `cg0` = `OMNI_JIT_GETSET_WIDTH=0 OMNI_JIT_SPILL_REGS=0 OMNI_JIT_ZEXT_TRUST=0` | **8 E-cores: all threads 59.07/60.00 -> 53.10/52.89 ms/frame (-11%, 2/2)**, top thread 23.16/22.75 -> 21.33/22.58, fps 28.60/29.70 vs 31.65/28.74 (mixed); full CPU: all 42.98/39.50 -> 41.52/39.49, fps 59.4 both | **yes, default** |
| 19 | dynarmic **0089** shared labels from a per-thread free list; **0090** a block's codegen census added once; **0091** get/set elimination counts barriers and asks the opcode table; **0092** a free, empty first register candidate taken at once; **0093** `Value`'s questions inline; **0094** no perf-map name per block | `the_speed_of_emission` (`OD_PROD=1`), byte-identical, 3-4 interleaved pairs each | emit 10.13 -> 9.79 (-3.3%), 9.85 -> 9.74 (-1.2%), frontend 4.27 -> 3.97 (-7%), emit 9.67 -> 9.50 (-1.8%), frontend -4% / emit -2.5%, emit 9.28 -> 8.76 (-5.6%, Linux; Windows only the formatting) | yes |
| 20 | dynarmic **0095** a snapshot installs faster (buffered reads, flat arrays, one commit a region, tables reserved); **0096** fastmem sites in order not sorted again; **0097** fields read in place | new `the_speed_of_snapshot_install` (libart corpus), 3 pairs each; snapshot test (Linux, first cache freed) | install 783 -> 562 -> 490 -> 476 ns a block (**-39%** together); 0096 also emit -1.7% | yes |
| 21 | dynarmic **0098** `IC IVAU` batched until `ISB` (`OMNI_JIT_IC_BATCH=1`) | new `omni-cpu/tests/icache_batch.rs` (4 correctness tests + cost) | a clear-cache loop **218 -> 15.4 ns a line**; device A/B: s22 | opt-in |
| 22 | snapshots: re-saved only when 2% is new (`OMNI_JIT_SNAPSHOT_RESAVE_PCT`); the directory NTFS-compressed (`OMNI_JIT_SNAPSHOT_COMPRESS=0`: not) | s12's log: 337 saves, 4.4 GB written in one run; files compress to ~1/3 | s23 measures both | yes (snapshots are opt-in) |
| - | a lock-free filter before the TBI sites' mutex; the register allocator's state kept per thread; `SelectARegister` over bitmasks | emission benchmark (a site noted) | 8.79 vs 8.79; noise; +3.5% | no, dropped |
| - | (0085) the allocator's per-block state kept per thread; (0088) `SelectARegister`'s partitions replayed over bitmasks (same choice) | same | 10.61 -> 10.52 (noise); 10.12 -> 10.47 (**+3.5%**, slower) | no, dropped |

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
- **`r_roblox`'s plant-first says `[r] cookie planted: failed` on every run** (every log since
  plant-first became the default, 2026-10-09 22:51; `ok` on 10-04 was the old dance). The `--then`
  shell runs as uid 2000 (`omni-linux-run`, `Process::spawn_as(config, 2000)`), and `chown_node`
  lets only root change an owner, so `chown -R $uid:$uid app_webview` fails. The store is copied
  first, so the app reads it and signs in anyway (`DID_LOG_IN` in every run tonight), but the files
  stay the shell's. `omnidroid aosp`'s `warm::plant` has the same command; not changed here.

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

| arm | boot_completed | DID_LOG_IN | Joining | onGameLoaded | private WS |
|---|---|---|---|---|---|
| live (no snapshots) | 29.6 / 29.7 | 57.9 / 57.8 | 71.3 / 68.8 | 85.9 / 84.9 | 2.966 / 2.988 GB |
| snap, 1st after the fill | 27.5 | 53.2 | 65.3 | 81.6 | 3.163 GB |
| **snap, 2nd** | **27.4** | **44.6** | **54.7** | **68.6** | 3.133 GB |

**The game's snapshot gets better each run** (a third run: DID_LOG_IN 52.2, onGameLoaded 79.6 -- so -4..-16 s to the world over three runs, -5 s typical): its second run verified **907,313 of 1,300,594** restored
blocks (70%; the first 32%), and the world was reached **~16 s sooner** (68.6 vs 84.9 s), sign-in 13 s
sooner. Cost: +145 MB private WS, 2.2 GB of snapshot files (the game's 558 MB, system_server's 235
MB), and every rebuild of the host binary starts them again. system_server still verifies 8% (45k of
553k): its code trim at ~60 s clears its cache wholesale (the forget step found nothing left), and
most of what it restored before then did not match -- s12 names where. Not a default yet: the RAM and
disk are yours to weigh against 16 s; `OMNI_JIT_SNAPSHOT=<dir>` with `_LAZY=1 _LIB_ZONE=1 _FORGET=1`
and `OMNI_JIT_SHARED_CACHE_LIVE_MB=512` is the measured set.

## Session s10 (04:13-05:10): the trims' real RAM, and init's class_start side by side (`s10-trim2.csv`)

Build with 0081-0083 (a stability run of them too: no crash in 8 boots). `avail_gb` is the machine's
available physical memory, `mc_gb` the system's compressed store.

| arm | fps | all ms/frame | private WS | system host | **available** | compressed store | system_server | boot_completed | DID_LOG_IN | onGameLoaded |
|---|---|---|---|---|---|---|---|---|---|---|
| base | 59.46 / 59.51 | 38.92 / 38.12 | 3.022 / 2.954 | 0.809 / 0.813 | 9.206 / 9.268 | 0.661 / 0.691 | 10.3 / 10.3 | 28.5 / 28.9 | 56.9 / 57.0 | 87.6 / 84.5 |
| idle (`OMNI_WS_TRIM_IDLE=30`) | 59.51 / 59.39 | 37.60 / 41.64 | 2.765 / 2.774 | 0.821 / 0.815 | 9.312 / 9.300 | 0.662 / 0.696 | 10.4 / 10.3 | 29.4 / 28.9 | 58.2 / 56.9 | 86.3 / 84.0 |
| **trim** (`OMNI_WS_TRIM=120`) | 59.49 / 59.57 | 37.50 / 39.05 | **0.727 / 0.632** | 0.139 / 0.127 | **11.134 / 11.084** | 1.679 / 1.664 | 10.4 / 10.3 | 28.7 / 28.7 | 56.8 / 56.7 | 81.8 / 86.3 |
| **par** (`OMNI_INIT_PARALLEL=1`) | 59.39 / 59.37 | 40.67 / 40.88 | 2.984 / 3.001 | 0.821 / 0.823 | 9.288 / 9.144 | 0.682 / 0.651 | **8.4 / 9.2** | **27.6 / 27.4** | **55.7 / 55.5** | 85.8 / 81.6 |

- **The periodic trim frees ~1.9 GB of the machine's memory** (available 9.2 -> 11.1 GB, twice),
  of which ~1.0 GB lands compressed in the system's store: omnidroid's real footprint drops by
  1.2-1.9 GB of ~3 GB. fps, CPU a frame and the vsync pacer's worst lateness a 30 s period (0.9-5.7
  ms against 1.0-7.7 on base) unchanged.
- The idle trim: Android's five idle helper apps 205-266 MB -> 0-12 MB each; private WS -257 / -180
  MB, available +106 / +32 MB: real but small next to the periodic trim.
- class_start side by side: 2,371 -> 416-419 ms; system_server's runtime 10.3 -> 8.4/9.2 s.
- `[init] boot commands 7.5-7.7 s` -- of which `exec_start` ~2 s; the rest is timed by the next build.

**All three are the defaults now** (c9eab23); `OMNI_WS_TRIM=0`, `OMNI_WS_TRIM_IDLE=0`,
`OMNI_INIT_PARALLEL=0` turn them off. **Note for later A/Bs:** with the trims on, `wspriv_gb` measures
what is touched between trims; RAM comparisons now go by `avail_gb`, or set `OMNI_WS_TRIM=0`.

## Between s10 and s14 (05:10-): spawns, and the JIT's own speed on the i5-4460

**Every spawn opened the sysroot again.** `OMNI_SPAWN_TIME=1` on the Linux box: `[spawn]
/system/bin/toybox: 66.0 ms (sysroot 61.7, ...)` -- `Sysroot::open` hashed the 0.5 MB manifest and
0.4 MB meta, looked at each of ~5,000 objects, read the device overlay and built its maps, for every
guest process, and again for each `setprop`/`wait_for_prop` init ran (`properties()`), 535 times in
one PS99 session (`[bootimage] pid` lines). Without `OMNI_GPU` set it also probed the host's Vulkan
(40 ms; the device runs set it). Now one per host process: `[spawn] servicemanager: 3.1 ms (sysroot
0.0, ...)`. Each guest process also kept its own copy of the maps (~2-3 MB of heap; 62 in the system
host) and its own `Backing` per library; they share both now. The size check of the objects runs
once per sysroot pin (`objects/.sizes-checked-<manifest sha>`).

**Emission, profiled with the device's switches** (`OD_PROD=1`, a SIGPROF sampler on the Linux box):
flat -- libc 10% (allocation, copying), `SelectARegister` 4.7%, `Xbyak::CodeArray::db` 4.3% (a call
per emitted byte), `A64GetSetElimination` 3.5%, `ValueLocation` 2.8%, `SetArg`/`GetArg` 2.7/2.5%, small
out-of-line IR predicates ~4% together. 0084/0086/0087 took the call-per-byte, the argument accessors
and the predicates: **frontend 4.80 -> 4.28 us/block (-11%), emit 11.06 -> 10.02 (-9%)** on the
i5-4460, the same bytes throughout (`compare_emit_dumps.py`: 11 blocks differ between any two runs of
one build, a jump displacement 0x2000 apart; the same 11 with each patch).

Why it matters: a fresh helper app's host translates 145-213k blocks (~1.7 s of CPU), the system
host ~1.4 M in its first 20 s, the game ~9 s + ~22 s of translate + emit a boot -- on a 4-core
machine most of that is on the boot's critical path.

## Session s12 (05:54-06:07): snapshots, diagnosed (`s12-why.csv`, the s10 build)

`OMNI_JIT_SNAPSHOT=<new dir> _LAZY=1 _LIB_ZONE=1 _FORGET=1 _WHY=1`, live 512 MiB: a run that fills
the directory, then one that restores from it.

| arm | fps | all ms | private commit | private WS | system host | available | system_server | boot_completed | DID_LOG_IN | Joining | **onGameLoaded** |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fill | 59.30 | 37.97 | 4.232 | 3.204 | 0.874 | 8.922 | 10.2 | 29.3 | 57.9 | 70.4 | 84.5 |
| **restored** | 59.11 | 41.14 | **4.165** | 3.131 | 0.830 | **9.086** | 10.2 | **26.6** | **44.4** | **52.6** | **66.7** |

- **18 s sooner in the world**, and no RAM cost any more (lazy pages + forgetting what was never
  entered): commit -67 MB, available +164 MB against the filling run. (s9's +145 MB was without them.)
- Nearly everything restored verifies now -- the library zone holds: the small daemons 99.9-100%,
  app processes 99.7-99.8% (165k blocks), the game 1.39 M restored, 1.27 M verified (91%); what it
  never entered by the time it settled (31k) is mostly anonymous memory (22.6k) and libroblox (5.6k).
- Installing a snapshot is on the process's critical path: the game's **1.3 s**, system_server's
  0.4 s -- **0095** makes it 28% cheaper.
- Cost: **2.2 GB of disk** (158 files; the game's 562 MB, system_server's 232 MB). One directory per
  configuration; every rebuild invalidates them (the code shape), the next save rewrites them.
- Linux note: `a_snapshot_of_real_code_runs_it_the_same` fails on `main` too there (load -10, the
  code shape, when a second cache is made beside the first); devices (Windows) restore fine.

**For the owner:** with the RAM cost gone, snapshots are 18 s off the way to the world for 2.2 GB of
disk. Not made a default here (the disk, and the owner's tools run this checkout).

## Session s13 (06:07-): the build with everything since s10, new defaults against them off (`s13-defaults.csv`)

Build: one `Sysroot` per host process (5f524ca), dynarmic 0084-0094, trims and parallel
class_start on. `off` = `OMNI_WS_TRIM=0 OMNI_WS_TRIM_IDLE=0 OMNI_INIT_PARALLEL=0` (the sysroot change
and the JIT patches stay).

| arm | fps | all ms | private commit | private WS | system host | available | system_server | boot_completed | DID_LOG_IN | Joining | onGameLoaded |
|---|---|---|---|---|---|---|---|---|---|---|---|
| s11 (old build) | 59.2-59.4 | 39.5-41.5 | 4.035 | 3.02 | 0.81 | 9.23 | 10.3 | 28.6 | 56.8 | 70.8-71.1 | 83.9-84.8 |
| new | 59.47 / 59.21 | 41.52 / 41.08 | 3.871 / 3.860 | 0.733 / 0.699 | 0.132 / 0.135 | 11.095 / 11.215 | **4.1 / 4.1** | **21.2 / 21.3** | **47.7 / 47.6** | 57.7 / 59.9 | **71.8 / 72.0** |
| off | 59.47 / 59.56 | 40.54 / 37.85 | 3.864 / 3.830 | 2.845 / 2.837 | 0.658 / 0.649 | 9.449 / 9.486 | **4.0 / 4.1** | **21.3 / 21.3** | **47.0 / 47.0** | 60.0 / 57.0 | **73.1 / 72.9** |

- **system_server 10.3 -> 4.0 s; boot_completed 28.6 -> 21.2; onGameLoaded ~85 -> 72-73 s.** init's
  boot commands **7.7 -> 3.8 s**: every `setprop`/`wait_for_prop` opened the sysroot. A spawn is ~4
  ms (`[spawn] ... sysroot 0.0`), `exec_start`s 80-130 ms (were 170-240), a helper app's host reaches
  its program in ~85 ms (was 160-240).
- Private commit 4.04 -> 3.87 GB; the system host without trims 0.81 -> 0.66 GB (each guest process's
  copy of the sysroot's maps is gone).
- The trims again give **+1.7 GB available** (11.10/11.22 vs 9.45/9.49) -- but this time CPU a frame
  is higher with them, 2/2: all threads 41.5/41.1 vs 40.5/37.9 ms, the top thread 12.3/12.0 vs
  11.8/10.4 (s10 saw no cost). Suspect: re-trimming the game's hot set every 120 s, which then
  faults back. s18 tries one trim only (`OMNI_WS_TRIM_ONCE=1`).
- What is left of init's boot commands: `wait_for_prop apexd.status activated` 1.86 s, bpfloader 0.39
  s, init_user0 0.27 s, linkerconfig 2 x 0.19 s.

## Session s14 (06:40-07:02): one sysroot per host process, side by side (`s14-sysroot.csv`, the s13 build)

| arm | fps | all ms | private commit | system_server | boot_completed | DID_LOG_IN | Joining | onGameLoaded |
|---|---|---|---|---|---|---|---|---|
| shared (default) | 59.50 / 59.39 | 39.75 / 40.27 | **3.832 / 3.851** | **4.1 / 4.1** | 21.1 / 21.2 | 46.8 / 47.0 | 59.4 / 61.0 | **73.8 / 73.0** |
| per open (`OMNI_SYSROOT_SHARED=0`) | 59.25 / 59.58 | 40.09 / 37.71 | 3.932 / 3.975 | 5.2 / 5.1 | 21.9 / 22.2 | 47.6 / 48.4 | 61.7 / 58.4 | 75.6 / 75.5 |

**The shared sysroot: system_server -1.05 s, the world -2.2 s, private commit -110 MB (2/2).** Of the
old build's 9.9 s before system_server (s11: boot commands 7.6 s, class_start 2.3 s sequential), the
rest went to class_start side by side (-2.2 s), `exec_start`s ~175 -> ~100 ms each (-0.8 s), and
init's waits (apexd's activation is guest code: decompressing the APEXes), with the cheaper JIT.

## Session s15 (06:55-07:30): the idle apps left out of the image, on four E-cores (`s15-idleout-e4.csv`)

| arm (`F0000`) | fps | top ms | all ms | system_server | boot_completed | DID_LOG_IN | Joining | onGameLoaded |
|---|---|---|---|---|---|---|---|---|
| default (disabled after boot) | 30.59 / 28.49 | 19.85 / 20.92 | 49.18 / 51.59 | 9.4 / 9.6 | 44.5 / 45.5 | 95.6 / 95.3 | 110.8 / 107.6 | 151.4 / 144.9 |
| `OMNI_DEVICE_IDLE_APPS=out` | 30.18 / 29.06 | 20.61 / 21.01 | 50.23 / 50.25 | 9.5 / 10.7 | 48.1 / 45.5 | 96.4 / 95.7 | 112.7 / 109.9 | 148.2 / 151.3 |

**No gain even on four cores** (the three apps it removes of the ~14 that start are small next to
the rest); the default stays. On this build four E-cores reach the world at **145-151 s** (s8's
older build: 179-181 s) and play at 28.5-30.6 fps.

## Session s16 (07:30-08:05): snapshots on tonight's build (`s16-snap.csv`, the s13 build)

`snap` = `OMNI_JIT_SNAPSHOT=<new dir> _LAZY=1 _LIB_ZONE=1 _FORGET=1`, live 512 MiB (one `fill` run first);
`off` = no snapshots (live 256 MiB).

| arm | fps | all ms | private commit | private WS | available | system_server | boot_completed | DID_LOG_IN | Joining | onGameLoaded |
|---|---|---|---|---|---|---|---|---|---|---|
| fill | 59.47 | 37.19 | 3.938 | 0.839 | 10.904 | 4.1 | 21.3 | 47.1 | 59.5 | 70.6 |
| **snap** | 59.19 / 59.33 | 39.99 / 38.65 | 4.033 / 4.017 | 0.946 / 0.866 | 10.702 / 10.691 | 4.1 / 4.1 | 20.2 / 20.2 | **36.4 / 43.1** | **47.7 / 51.2** | **59.7 / 67.5** |
| off | 59.49 / 59.42 | 39.97 / 37.67 | 3.822 / 3.835 | 0.734 / 0.743 | 10.966 / 10.917 | 4.1 / 4.1 | 21.4 / 21.4 | 47.9 / 47.1 | 60.3 / 60.1 | 74.1 / 73.2 |

**Snapshots: the world 6-14 s sooner (10 s on average), sign-in 4-11 s sooner, the same fps -- and
+190 MB private commit, -250 MB available, +130 MB working set (2/2).** Correction to s12: "no RAM
cost" compared the restoring run with a filling one, both with snapshots and the 512 MiB live
budget; against none, there is this cost (the live budget and the restored code read in by page
until code aging trims it -- the game's commit fell 3.27 -> 2.71 GB in s12's restore once it did).

## Session s17 (07:50-08:35): snapshots on four E-cores (`s17-snap-e4.csv`, the s13 build, s16's snapshots)

| arm (`F0000`) | status | fps | system_server | boot_completed | DID_LOG_IN | Joining | onGameLoaded |
|---|---|---|---|---|---|---|---|
| e4snap | **crashed** | -- | | | 76.5 | 92.5 | **130.8**, then the game's host died at ~+265 s |
| e4off | ok | 31.32 | | 45.7 | 94.2 | 109.5 | 140.4 |
| e4off | ok | 30.53 | | 45.8 | 96.2 | 108.5 | 140.7 |
| e4snap | **crashed** | -- | | | | | 134.1, then the game's host died seconds later |

**Both snapshot runs on four E-cores ended in a crash of the game's host process** (exit
`0xC0000005`, an access violation in host code; nothing logged), the first at the end of its
code-aging pass, the second during the world's load -- each while the shared cache was retiring
regions (66-79 retired). Runs without snapshots on four E-cores retire as many (88-92) and never
crashed (6 of 6, s15 + s17); with snapshots at full CPU (s12, s16: 3 runs, 90-110 retired) none did.
So: a snapshot-specific race that slow cores expose. **Snapshots are not to be made a default until
it is found** (the recommendation in the morning report is withdrawn). The next build reports a
host crash's instruction (`[host-crash]`, 2b34552); s26 reproduces it there.

## Session s18 (08:17-08:40): trimming once against every 120 s (`s18-trimonce.csv`, the s13 build)

| arm | fps | CPU ms/frame (all) | commit GB | game WS private GB | available GB | DID_LOG_IN | onGameLoaded |
|---|---|---|---|---|---|---|---|
| periodic | 59.09 | 44.44 | 3.826 | 0.768 | 10.846 | 45.4 | 69.4 |
| once (`OMNI_WS_TRIM_ONCE=1`) | 58.94 | 42.93 | 3.880 | 0.918 | 10.610 | 48.6 | 74.7 |
| once | 59.46 | 36.41 | 3.865 | 0.886 | 10.700 | 47.0 | 72.0 |
| periodic | 59.52 | 38.30 | 3.866 | 0.744 | 10.824 | 45.8 | 70.3 |

Trimming once leaves the game's working set ~145 MB larger and ~180 MB less memory available
(2/2); its CPU is 39.7 against 41.4 ms a frame, a difference smaller than the one between the two
pairs (44 vs 38 ms). s13's hint of a cost of the periodic trim is not borne out. **Periodic stays
the default.**
