# Omnidroid B2: Playable GPU Mode — Design

**Date:** 2026-07-21
**Status:** Approved design, not yet implemented
**Repo:** omnidroid (engine). Spec B of the omni-apps brainstorm; B2 of two
(B1 = lean bases + farming mode, already built offline).

## Problem — in plain terms

Today every emulator instance draws Roblox's graphics the SLOW way — using the
main CPU (software rendering via SwiftShader), never the real graphics chip.
That is fine for **farming** (headless, nobody watching). But when a **human
wants to actually play or watch** an instance, software rendering looks choppy
and ugly. **Playable mode = make that instance use the real GPU so the game
looks good and runs smoothly for a person watching live.**

## The one unknown that gates everything

For GPU rendering to work, the Android system INSIDE the emulator must have a
driver that can talk to a host-accelerated virtual GPU. It might already have
one; it might not. **Neither the user nor the engine code can know without
trying it once.** So B2 is SPIKE-FIRST: run one experiment, then branch.

## Scope & decisions (locked in brainstorming)

- **Playable = a human plays it LIVE, at the SAME machine that runs the
  instance.** So QEMU can open a native GPU-accelerated WINDOW on the host —
  no need to stream accelerated frames over the network (the hardest case,
  explicitly avoided). A headless server naturally falls to the non-accelerated
  path.
- **COMPATIBILITY IS THE TOP PRIORITY — above smoothness.** playable-GL must
  work across `{macOS-arm, Linux-x86, Linux-arm, Windows-x86}`. Where real GPU
  acceleration isn't available on a host, it must **degrade cleanly** to today's
  behavior (headless + VNC viewer) and NEVER crash or regress a boot. This is a
  capability-detection + graceful-degradation design, not a single GL flag.
- **Spike + plumbing; defer the big case.** B2 = the feasibility spike PLUS, IF
  the spike shows the guest already accelerates, the full playable-GL mode. IF
  the spike shows the guest needs a GPU-driver stack added to the image, B2
  STOPS at a documented finding and opens a **B3** spec — adding a guest GPU
  driver stack (Mesa virgl / gfxstream) is a large image project that deserves
  its own design with real data, not a blind commitment.
- **playable-GL coexists** with farming/hard/brutal and every existing
  instance untouched. It is host-local only.
- **Measurement rig = B1's** (user-set): the base's pre-installed Roblox is
  flagged and black-screens — instead install the bootstrap APK
  `~/Desktop/overnight tests/update test/roblox-v2.726-bootstrap.apk`, log in,
  and **join place id `8737899170`**, then assess rendering there (not the home
  screen).

## Current state (grounded in the code)

- Both bases spawn a virtual GPU already: arm uses `-device virtio-gpu-pci`
  (engine.py ~1259), x86 uses `-device virtio-vga` / `virtio-gpu-pci`
  (~1315-1327) — but always with **`-display none`** (headless) and a
  localhost-only **`-vnc`** server. Viewing is via omnidroid's built-in VNC
  VIEWER, opened by the existing `--window` flag (`_spawn_builtin_viewer`,
  ~2212-2220). So `--window` today = a VNC client watching the
  software-rendered framebuffer. Real GPU acceleration is OFF (the VirGL path
  was dropped as "moot" for the headless model — engine.py ~1153).
- Host awareness ALREADY exists: `default_accel()` (~1109) returns
  `whpx`/`hvf`/`kvm` per platform using `IS_WINDOWS`/`IS_MACOS`/`IS_LINUX`
  constants. B2's capability detector mirrors this proven pattern.

## Architecture — spike-first, then branch

```
Phase 0  FEASIBILITY SPIKE (on the ARM Mac first)
   Boot the arm base with an ACCELERATED virtual GPU + a native window:
   `-device virtio-gpu-gl` + `-display cocoa,gl=on` (macOS). Install the
   bootstrap APK, log in, join place 8737899170, and OBSERVE:
     - real 3D acceleration (smooth) ?
     - software-slow but rendering ?
     - black screen / crash ?
   Also confirm login + join still work through the accelerated window.
   │
   ├── GUEST ACCELERATES ALREADY  ->  build playable-GL mode (Components A + B)
   │
   └── GUEST SOFTWARE-ONLY / BLACK ->  STOP. Document the finding. Open a B3
        spec for the guest GPU-driver stack. Do NOT build that blind.
```

## Component A — host GL-display capability detection (if spike is green)

A `default_display()`-style function mirroring `default_accel()`:

- Per platform, choose the display backend + accelerated GPU device:
  - macOS: `-display cocoa,gl=on` + `-device virtio-gpu-gl`
  - Linux: `-display gtk,gl=on` (or `sdl,gl=on`) + `-device virtio-gpu-gl`
  - Windows: `-display gtk,gl=on` (or `sdl,gl=on`) + `-device virtio-gpu-gl`
- **Detect usability, don't assume:** verify the running QEMU build advertises
  the chosen display + `virtio-gpu-gl` (e.g. parse `qemu-system-* -display
  help` / `-device help`), and that a host display is present (not a headless
  SSH session with no `$DISPLAY`/GUI). Return a small descriptor:
  `{available: bool, display_args: [...], gpu_args: [...], reason: str}`.
- **Compatibility-first:** on ANY host/arch where the accelerated combo isn't
  usable, return `available=False` with a human reason. Pure, unit-testable
  (feed it fake platform + fake `-display help` text; assert the right
  descriptor per cell of the matrix).

## Component B — the playable-GL boot path (if spike is green)

- A per-start opt-in (e.g. `--play` / a `playable`-mode + local-window request).
  When chosen AND Component A reports `available=True`, spawn QEMU with the
  accelerated `gpu_args` + `display_args` INSTEAD of `-device virtio-gpu-pci
  -display none` FOR THAT INSTANCE ONLY.
- **Graceful degradation:** if Component A reports `available=False`, fall back
  to today's headless + VNC-viewer path — the boot still succeeds, just without
  acceleration, and prints one honest line explaining why (e.g. "GL accel
  unavailable on this host (no display); using VNC viewer").
- Every other mode and every existing instance is byte-for-byte unchanged.
- Offline-testable seam: the FUNCTION that decides "accelerated args vs
  headless args" given a capability descriptor + the request is pure — unit
  test it. The actual window is a live runbook step.

## Compatibility matrix (the design contract)

Behavior is DEFINED for every cell; none may crash or regress:

| Host \ GPU accel | available | unavailable |
|---|---|---|
| macOS-arm | cocoa,gl=on + virtio-gpu-gl | headless + VNC (degrade) |
| Linux-x86 | gtk/sdl,gl=on + virtio-gpu-gl | headless + VNC (degrade) |
| Linux-arm | gtk/sdl,gl=on + virtio-gpu-gl | headless + VNC (degrade) |
| Windows-x86 | gtk/sdl,gl=on + virtio-gpu-gl | headless + VNC (degrade) |

"unavailable" includes: headless server (no GUI/display), QEMU build lacking
the GL display or `virtio-gpu-gl`, or the guest-driver case (which, if the
SPIKE reveals it, sends B2 to the B3 branch rather than shipping a broken mode).

## Verification

**Offline (unit tests, omnidroid convention — tests git-tracked, `unittest`):**
- Component A: given fake platform + fake `-display help` / `-device help` text
  + fake display-presence, returns the right descriptor for each matrix cell
  (including every "unavailable" case).
- Component B decision function: given a capability descriptor + a play request,
  returns accelerated args when available and the headless args when not —
  never raises.

**Live (the spike + runbook, the user runs on the Mac — NOT unit-testable):**
- Phase 0 spike: accelerated window boots, bootstrap APK installs, login works,
  joins place 8737899170, and the render result is recorded (accel / software /
  black). This single result decides the branch.
- If green: playable-GL boots on the arm base with a smooth window; farming and
  the other modes still boot headless unchanged; degradation path verified by
  forcing `available=False`.

## Risks

- **The guest may lack the GPU driver** — the whole reason for the spike. If so,
  B2 stops and defers to B3 (not a failure — a bounded, data-driven decision).
- **GL-over-QEMU is host-fragile** (driver/version/build differences). Mitigated
  by capability detection + always-safe degradation: a host that can't
  accelerate simply gets today's behavior.
- **macOS `gl=on` maturity** — QEMU's cocoa GL path is newer/less battle-tested
  than Linux. The spike on the Mac tells us directly; if the Mac can't but Linux
  can, the matrix still holds (Mac degrades, Linux accelerates).
- **Anti-cheat / detection** — a different GPU/renderer string could change how
  Roblox behaves. The spike (real login + join) surfaces this early.

## Out of scope

- **B3 — guest GPU-driver stack** (Mesa virgl / gfxstream in the image), only
  pursued if the spike shows it's needed. Its own spec.
- **Accelerated frames streamed over the network** (remote play) — explicitly
  avoided; playable is host-local.
- **Changing farming/hard/brutal or the default headless behavior.**

## Sequencing

1. Phase 0 SPIKE (live, on the Mac) — the gate. Everything else is conditional
   on its result.
2. IF green: Component A (capability detection) + Component B (decision
   function + boot wiring), offline-testable seams first, then the live
   playable-GL runbook.
3. IF not green: document + open B3. Stop.
