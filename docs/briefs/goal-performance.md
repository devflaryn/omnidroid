# Brief: the performance goal -- fast, stable, lightweight on all three hosts

How to build, run and measure on each host is in `docs/HANDOFF.md` (machines, network, accounts,
log markers, switches); the bottleneck history is in `docs/ports/<os>.md`. This file is the goal.

## What "done" means

Measured **in Pet Simulator 99 (place 8737899170) with the world loaded**; menus, Home and loading
screens do not count.

| host | in-world target | best so far (HANDOFF) |
|---|---|---|
| Windows | ~120 fps | ~50 fps; the engine itself paces at 60 Hz on these flags, as a phone does |
| macOS | 60+ fps | 34-56 fps |
| Linux | ~10+ fps | 8-14 fps, GPU-bound (Fermi) |

On every host:

1. **Stable**: joins and loads on a fresh storage and on a relaunch; 30+ minutes with no guest
   thread dying, no crash, hang or freeze; a clean close (`SessionHistory "IAB"`/`"IB"`).
2. **Fast**: the targets above with smooth pacing (no multi-second hitches), and short launch →
   Home and join → `onGameLoaded`.
3. **Lightweight**: as little RAM and CPU as possible, never a sustained ~50% of total CPU or
   growth toward 10 GB; memory and CPU strictly on demand, so many instances fit (the owner's
   products A and B, HANDOFF "Where it stands").

## Method

- Profile in the world (`OMNI_PERF=5`, idle camera 2-3 min, then moving) and keep the ranked
  bottleneck list in `docs/ports/<os>.md`: translation and invalidation, crossings and dispatch,
  the exclusive monitor, handler cost, waits, the GPU/present path, pacing, memory.
- Most expensive first: measure → fix → re-measure in the same scene → commit → re-profile.
  n ≥ 2 per arm, median and spread, the commit each number was taken at.
- Never buy speed with correctness: no skipped engine work, no fake frames. A trade-off gets a
  `DECISIONS.md` entry. A change to vendored dynarmic is a patch in `crates/dynarmic-sys/patches`
  with its own entry, and `vendor/PIN.txt` is touched before building.
- Use subagents where they save wall-clock time (decoding, independent fixes, builds on the other
  hosts); the main agent orders the work and judges every measurement. One host runs the app at
  a time.

## Proof

A screenshot of each host in-world at each milestone, title bar visible. Windows:
`CopyFromScreen` of the window's monitor. macOS: `screencapture` from a `.command` opened with
`open -g -a Terminal` (never `osascript` over ssh: it raises a permission dialog on the owner's
screen). Linux (Wayland): `xwd -id <the app's client window>`.

Ask the owner before anything destructive or outside the repo, and for a new cookie or a VPN
change.
