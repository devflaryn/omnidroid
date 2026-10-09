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
| weak8e | 8 E-cores | **33.79** | 54.55 | 20.15 / 10.33 | 1.84 | 150 s | 188 s | 3.158 GB | 0.972 GB |
| weak4e | 4 E-cores | -- | | | | (no network: see above) | | | |

On eight E-cores only 1.84 cores are busy at 33.8 fps: the frame is a cross-thread critical path
(the engine worker 20 ms of each 29.6 ms frame), not a shortage of cores.

## Ledger

| # | change | A/B | result | kept? |
|---|---|---|---|---|
