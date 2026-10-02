# Report 2026-10-02: `--cookie --place` on the warm device (branch `perf/warm-join`)

Goal (owner): with a warm Android up and **no APK installed**, the time from giving the command
with `--cookie` and `--place` to being **in the game** -- PS99 (`8737899170`), any cookie in
`Desktop/cookies/`. Roblox's own join/loading screens do not count; the place's own (in-game)
loading screen does.

**Where it landed.** `omnidroid aosp --cookie <file> --place <id>` now runs on the warm device
when one is up (before: it booted a device of its own beside it). Interleaved A/B, 4 pairs (ABBA),
same hour, i7-13700F, Windows, `Roblox-2.740.931.apk` (Delta), cookie `HeZmI_ImYu1080`:

| | "Joining game" | PS99's loading screen |
|---|---|---|
| before: the command boots its saved signed-in device | 83.1 85.7 84.2 85.8 s (median 84.2) | 92.1 92.6 91.6 102.7 s (median **92.1**) |
| after: warm device, nothing of the app installed | 41.8 44.9 43.8 48.2 s (median 43.8) | 62.3 87.6 79.2 97.9 s (median **79.2**) |

Warm first in 4 of 4 pairs. Best single run 45.6 s. Batches on the branch (6 runs each, other
hours): median 76.1 s (50.9-86.0) and 91.5 s (65.5-96.0) -- the hour moves it more than anything
the flow controls (see "What's open"). Without a saved device for the APK + account, the old path
was a first boot of ~8 min.

## What landed (commits on `perf/warm-join`, on top of `feat/re-on-fastboot`)

1. `d15218d` **fix(omni-linux): the owners table is ordered, a removal always logged.** Every
   `rename` scanned the whole owners table twice (`Path::starts_with` per entry): ~20 ms a rename
   once a device held ~18 000 owned files; Roblox writes its asset cache as temp file + rename,
   ~200 renames each 5 s on one worker thread pinned in path comparisons for most of the game's
   load (profile: `OMNI_THREAD_CPU`, `OMNI_SYSCALL_STATS`, symbols from the PDB). Now a
   `BTreeMap` range: **1.2 ms a rename**. And the table no longer leaks: `forget` logged a removal
   only when the removing process's own table held it, so installd's removals of an uninstalled
   app's files were never logged (500 -> 20 000 entries in an hour; 16 413 stale). The leak made a
   warm device slower with every session (its first run ~88 s, later ~100-117 s).
2. `d35261b` **feat(omnidroid): aosp --cookie --place on the warm device.** New crate `omni-warm`
   (the warm device's code, moved out of omni-mcp so the launcher does not pull the debugger).
   The session: install, plant, start in one control-channel command (`install_then`); the cookie
   written into the app's WebView store *before its first start* (`plant_cookie.py --new` makes the
   Chromium store, schema 21) -- no start / stop / plant / start again; the place's link sent once
   the main Activity starts; `omnidroid warm-release` stops the app when the launcher is killed
   (checked: 1.1 s after a kill). `--instance`, `--standby`, `--fresh-device`, `OMNI_AOSP_WARM=0`:
   the old path.
3. `dfc8588`, `da06baa` **tools**: `join_timer.py` (any command, timed to PS99's own loading
   screen from the display and log), `ingame_screen.py` (the screen detector: checked against every
   frame of the day; rejects Roblox's join pages, its white join page, its dimmed Home), 
   `warm_join_bench.ps1` (N runs), `warm_join_ab.ps1` (interleaved A/B), `warm_join.py` (the
   prototype, with the levers below).

Where the time goes now (warm, typical): install + plant + start 8 s; the app's start to its main
Activity ~15-19 s (one thread, ~1/3 of it in dynarmic translating); signed in ~33-39 s; "Joining
game" ~41-45 s; then **10-50 s** of game load before PS99's screen.

## Tried, not kept

| lever | result |
|---|---|
| link sent after the sign-in instead of at the main Activity | same "Joining game" (41-44 s) on the fixed build; kept at main Activity (no worse, earlier) |
| link on the launcher's own start intent | ignored by the app (never joined) |
| seed the app's caches (OTA patches, UniversalApp, settings, flags, content store, one filled in PS99) | no faster sign-in or join |
| seed the feature-flag cache alone | post-join 34 / 29 / 11 s: no |
| seed the saved device's whole app data | 54.7 / 52.6 / 77.0 s: no clear change |
| no spare app process | slower (87-102 s; sign-in later) |
| no all-files access | one run stuck (Settings' page), the other slow |
| device processes pinned to the P-cores | 89.0 / 90.6 / 65.4 s: no |
| 1 ms host timer resolution + Windows power throttling opted out, every host process | A/B 4 pairs: 77.3 vs 72.1 s median, both ways: reverted |

## What's open

- **The game load after "Joining game" is the clock: 10-50 s on the warm device** (two worker
  threads at 100-120% in translated code -- PS99's client work; server-independent: one server gave
  7 to 50 s). On the saved device the same phase took 7-17 s in all 5 runs measured, against about
  1 in 3 on the warm device; not the spare, the flags, the app's data, the GPU path (the same
  Vulkan setup on both), core placement or the timer, as tested above. The next step is a
  thread-CPU profile of a *saved* run's load beside a warm one.
- App start (~15-19 s) is translation-bound: a translation cache shared across app processes
  (the fast-boot report's first open item) would cut it.
- The MCP server's `start_instance` *with* a cookie still boots its own device (it passes
  `--instance`); it could use the warm device the same way.
- The warm device's system host process keeps one thread at ~90% of a core while idle; unexamined.

## Reproduce

```powershell
$env:OMNIDROID_DYNARMIC_BUILD_DIR = "C:\od-unified"
cargo build --release -p omnidroid -p omni-linux; cargo test --release -p omni-linux --test r_roblox --no-run
target\release\omnidroid.exe aosp --warm            # or let omni-mcp boot it; ready: <dir>\data\local\tmp\warm-ready
target\release\omnidroid.exe aosp --apk $HOME\Desktop\Roblox-2.740.931.apk --cookie $HOME\Desktop\cookies\HeZmI_ImYu1080.txt --place 8737899170
powershell -File tools/warm_join_bench.ps1 -Runs 6           # the app uninstalled before each run
powershell -File tools/warm_join_ab.ps1 -Pairs 4              # saved vs warm, ABBA
```
