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
| 6 | code aging's floor 8 -> 6 MiB (`code_trim::MIN_BYTES`) | to measure (s5) | the +17 MB of #5: a cache's commit counts its prelude, ~1 MiB smaller with 0078, so ~6 small quiet services (ueventd, gatekeeperd, HALs) fell under the 8 MiB floor and kept ~110k blocks (block map 542k -> 652k entries, JIT tables 78 -> 95 MiB) | pending |
| 7 | dynarmic **0079**: Xbyak's label manager without heap nodes (tsl robin map/set, flat waiting list) | `the_speed_of_emission`, byte-identical | emit **15.7 -> 13.1 us/block (-16.5%)**, i5-4460 | yes (device check s5) |
| 8 | dynarmic **0080**: a value's host location from a checked hint, not a search | same | emit **13.1 -> 11.9 us/block (-9%)** | yes (device check s5) |

**Why the in-world worker got cheaper with faster translation:** the game keeps translating in the
measured window -- after its code-aging pass (3 min) it translates the hot code again in bursts of
30-45k blocks per 5 s (~0.5-0.65 s of JIT per 5 s, 10-13% of a core) -- and the engine worker does
much of that itself, on its own frame time.

**Where the system host's memory is** (s3 `[mem]`, ~5 min after start): 62 guest processes, guest
memory 210 MiB, translated code 326-338 MiB committed, the JIT's tables on the heap 78-95 MiB (block
map, links, fastmem sites, guest ranges: ~155 bytes per live block, beside ~380 bytes of code). The
JIT is about half of that host's working set.
