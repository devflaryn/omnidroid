# Morning report -- 2026-10-10: lighter and faster (branch `perf/lighter-1010`)

Autonomous night on Windows (i7-13700F, RTX 4060, 32 GB; the owner's llama-server, MuMu VM and
RobloxPlayerBeta running throughout), Delta APK `Desktop/Roblox-2.740.931.apk`, PS99, cookie
`HeZmI_ImYu1080`, WARP (TR). **`main` is untouched**; everything is on `perf/lighter-1010` (based on
`main` dc72223), each change with a switch back. Full ledger with every A/B, including what did not
help: `docs/NIGHT-2026-10-10-lighter.md`.

## Where things stand (fresh boots, PS99 fully loaded)

| | `main` (dc72223, s1) | branch now | change |
|---|---|---|---|
| fps (60 Hz cap) | 57.4-59.0 | 59.0-59.6 | at the cap |
| CPU a frame, all hosts | 42.5-47.4 ms | 38.6-45 ms | lower, noisy |
| engine worker ms/frame | 13.8-14.0 (s3 `old`) | 10.6-12.3 | **-2..-3 ms** (faster translation in-world) |
| private working set (Task Manager) | 3.04-3.21 GB | **2.97-2.99 GB**; **0.73 GB** with `OMNI_WS_TRIM=120` | -0.1 GB; -2.25 GB trimmed (real RAM freed: being measured) |
| system host | 0.90-0.92 GB | **0.82 GB**; 0.14 GB trimmed | -90 MB |
| system_server starts | 12.1-12.5 s | **10.3-10.4 s** | -2 s |
| boot_completed | 30.8 s | **28.6-29.6 s** | -1.3..-2 s |
| world loaded (onGameLoaded) | 87-88 s | **84-86 s**; 76-82 s with snapshots (s5) | -2..-5 s; -6..-10 s |
| 8 E-cores (weaker PC): world loaded | 188 s | **160 s** | -28 s |
| 4 E-cores | (not measured working) | **plays at ~33 fps**, world at 179 s | Delta's 20 s window met |

## What changed (each measured; numbers in the ledger)

1. **JIT translation ~30% cheaper** (dynarmic patches 0077 IR accessors inline + verification on
   request, 0079 Xbyak labels without heap nodes, 0080 register-allocator location hint; all
   byte-identical code): i5-4460 first translation 24.5 -> 17.2 us/block. In-world the game keeps
   translating (after code aging), so the engine worker got ~2 ms/frame cheaper and fps +0.9.
2. **Fault thunks 94% smaller** (0078): a fresh code cache 0.88 -> 0.13 MiB resident, every guest
   process starts faster. It exposed a code-aging floor that small services then fell under; the
   floor is now 6 MiB: **-90..-110 MB private WS**.
3. **Boot: no fixed 1.5 s sleep before system_server** (it waits for servicemanager, which was ready
   "after 0 ms"): system_server -2 s, world -2.5 s. Sign-in polled every 0.2 s.
4. **The game's hottest loop, Luau's bytecode dispatch** (`ldrb w8,[x26,#4]!; ldr x8,[x21,x8,lsl #3];
   br x8`, the engine worker's top function), translated without a store-and-reload of X8 (0081: a
   register read forwarded across a W/X width change) and without stack spills while registers are
   free (0082: a value moved out of the way goes to a free callee-saved register). Both checked with a
   new guest-visible differential (every register after every block, ~385k block runs over five
   libraries including libroblox: identical on and off; it caught a wrong first version of 0081).
   In-world A/B queued (s11).
5. **Measurement**: the harness simulates weaker PCs (`-Affinity`), refuses to run without the
   network bypass (a run without WARP looked exactly like Delta's 20 s crash), times boot milestones
   from the log's own clock, and records available memory and the compressed store;
   `OMNI_JIT_TIME`, `[boot-ms]`, `[init] exec_start ... in N ms`.

## Found, being measured

- **Most of the RAM omnidroid holds resident is cold.** Two minutes after a working-set trim the
  system host had touched 187 of 1,294 MB again, the game 748 of 3,011, each helper app 2 of ~150.
  `OMNI_WS_TRIM=<s>` (periodic) and `OMNI_WS_TRIM_IDLE=<s>` (idle processes only) -- s10 measures
  what really leaves RAM (the compressed store keeps part of it).
- **Translation snapshots: 4-16 s sooner into the world (about 5 s typical)** (last night's WIP, now built, fixed and on
  this branch; opt-in). With the game's live code budget at 512 MiB its snapshot survives, and from
  the second run on it verifies up to 70% of what it restores: world at 81.6 / 68.6 / 79.6 s against 84.8-85.9 s without
  (s9). Cost: +145 MB private WS and 2.2 GB of snapshot files, rebuilt after every new host binary.
  system_server's part is still weak (8% verify; s12 names where its code moves). Yours to weigh:
  `OMNI_JIT_SNAPSHOT=<dir> OMNI_JIT_SNAPSHOT_LAZY=1 OMNI_JIT_SNAPSHOT_LIB_ZONE=1
  OMNI_JIT_SNAPSHOT_FORGET=1 OMNI_JIT_SHARED_CACHE_LIVE_MB=512`.
- **init's class_start**: 47 services spawned one after another; `OMNI_INIT_PARALLEL=1` -- s10.

## For you to decide

See the ledger's "For the owner to decide": saved devices by hard link instead of a 781 MB copy per
boot; ~3 GB of old `-gutted` saved devices in `%TEMP%\omni-golden`.
