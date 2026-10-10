# Morning report -- 2026-10-10: lighter and faster (branch `perf/lighter-1010`)

Autonomous night on Windows (i7-13700F, RTX 4060, 32 GB; the owner's llama-server, MuMu VM and
RobloxPlayerBeta running throughout), Delta APK `Desktop/Roblox-2.740.931.apk`, PS99, cookie
`HeZmI_ImYu1080`, WARP (TR). **`main` is untouched**; everything is on `perf/lighter-1010` (based on
`main` dc72223), each change with a switch back. Full ledger with every A/B, including what did not
help: `docs/NIGHT-2026-10-10-lighter.md`.

## Where things stand (fresh boots, PS99 fully loaded)

| | `main` (dc72223, s1) | branch now (s13) | change |
|---|---|---|---|
| fps (60 Hz cap) | 57.4-59.0 | 59.2-59.6 | at the cap |
| CPU a frame, all hosts | 42.5-47.4 ms | 37.9-41.5 ms | lower, noisy |
| engine worker ms/frame | 13.8-14.0 (s3 `old`) | 10.4-12.3 | -2..-3 ms |
| private commit, all hosts | 4.0-4.1 GB (s11) | **3.83-3.87 GB** | **-165 MB** |
| private working set (Task Manager) | 3.04-3.21 GB | **0.70-0.73 GB** (trims, default) | **+1.7-1.9 GB available to the machine** |
| system host (no trims) | 0.90-0.92 GB | **0.65-0.66 GB** | -250 MB |
| system_server starts | 12.1-12.5 s | **4.0-4.1 s** | **-8 s** |
| boot_completed | 30.8 s | **21.2-21.3 s** | **-9.5 s** |
| world loaded (onGameLoaded) | 87-88 s | **71.8-73.1 s** | **-15 s** |
| ... with translation snapshots | -- | **59.7 / 67.5 s** (s16) | -10 s more, opt-in (+190 MB) |
| 8 E-cores (weaker PC): CPU a frame | -- | **-11%** with 0081-0083 (53 vs 59.5 ms) | |
| 8 E-cores: world loaded | 188 s | 157-164 s (s11, older build) | -25..-30 s |

## What changed (each measured; numbers in the ledger)

1. **One sysroot per host process** (5f524ca). Every spawn, and every `setprop` and `wait_for_prop`
   init ran, opened the AOSP image again: hashed its 0.9 MB manifest and meta, looked at ~5,000
   files, built its maps (535 times a session; 62 copies of the maps kept in the system host).
   **init's boot commands 7.7 -> 3.8 s, system_server 10.3 -> 4.0 s, a spawn 70 -> 4 ms**, the system
   host -150 MB. `OMNI_SYSROOT_SHARED=0` is the old way (s14 measures it side by side).
2. **The JIT ~25% cheaper per block** (dynarmic 0077, 0079, 0080 last night; tonight 0084 Xbyak
   writes bytes in place, 0086/0087/0093 IR accessors and predicates inline or from tables, 0089
   labels from a free list, 0090 no atomics per instruction, 0091 get/set elimination without
   per-access sweeps, 0092 a free register taken at once, 0094 no perf-map name per block; all
   byte-identical code, measured with the device's own switches): i5-4460 frontend 4.80 -> 3.8
   us/block, emit 11.06 -> 8.8. On the device: JIT CPU to the world 56 -> 51 s, the game 13.4 -> 11.4
   us/block.
3. **The game's hottest loop, Luau's dispatch, translated tighter** (0081 a register read forwarded
   across a W/X width change, 0082 spills to free callee-saved registers, 0083 no re-zero-extension
   of a zero-extended load; checked with a new guest-visible differential): **on 8 E-cores the
   game's CPU a frame 59.5 -> 53 ms (-11%, 2/2)**; at full CPU at the cap either way.
4. **Snapshots install 28% faster** (0095: buffered reads, flat arrays, one commit a region): the
   game's 1.4 M-block snapshot took 1.3 s at its process start.
5. **RAM: most of what omnidroid held resident was cold** -- every host process trims its working
   set every 120 s, an idle one after 30 s: **~1.7-1.9 GB more available memory** (twice in s10,
   twice in s13), Task Manager's figure 3.0 -> ~0.7 GB. s13 hints at a small CPU cost of the
   periodic trim (2/2, +1-2.5 ms a frame); s18 tries trimming once instead. `OMNI_WS_TRIM=0` /
   `OMNI_WS_TRIM_IDLE=0` turn them off.
6. **Boot**: init's services started side by side (2.37 -> 0.42 s); no fixed 1.5 s sleep before
   system_server; fault thunks 94% smaller (0078) and code aging's floor 6 MiB (-90..-110 MB).
7. **Measurement**: weaker PCs simulated (`-Affinity`), the network bypass checked before a run,
   boot milestones from the log's own clock, available memory and the compressed store recorded;
   `OMNI_JIT_TIME`, `OMNI_SPAWN_TIME`, `[boot-ms]`, `[init] ... in N ms`; new benchmarks for
   snapshot installs and for threads translating side by side.

## Found, being measured (queued sessions s14-s20)

- **Translation snapshots: the world ~10 s sooner (6-14 s; s16, tonight's build: 59.7/67.5 s
  against 74.1/73.2 s), sign-in 4-11 s sooner, the same fps; cost +190 MB private commit / -250 MB
  available, and their disk** (2.2 GB uncompressed; the directory is now NTFS-compressed, c887b87,
  and a process re-saves only when 2% of its snapshot is new -- one fill run wrote 492 saves, 5.3 GB;
  both measured in s23). Small daemons verify 99.9%, apps 99.7%, the game 91%. One switch now:
  **`OMNI_JIT_SNAPSHOT=1`** (the temporary directory's `omni-jit-snapshot`, with lazy pages, the library
  zone, forgetting and 512 MiB of live code by default). s17 measures them on 4 E-cores. **My recommendation: on by default** -- yours to weigh against the disk.
- The idle apps left out of the image on 4 E-cores (s15), trimming once vs every 120 s (s18), the
  saved-device path you actually boot (s19), and who holds the system host's 13-16 MiB blocks (380
  MiB of its 1.03 GB commit; s20).

## For you to decide

- Translation snapshots on by default (above).
- Saved devices by hard link instead of a ~780 MB copy per boot; ~3 GB of old `-gutted` saved
  devices in `%TEMP%\omni-golden` (yours to delete).
