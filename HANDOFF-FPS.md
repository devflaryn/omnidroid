# Handoff: get gaming from 60 fps to 100–120

Paste everything below the line into a fresh Claude Code session, started in
`C:\Users\berat\Desktop\Omni Apps`.

---

## Goal

OmniDroid's gaming mode is pinned at ~60 fps on a machine that runs the same
Roblox place natively at 170+. Get it to 100–120. The cause is already
root-caused and the fix is already written — **the job is to BUILD it and
MEASURE it**, then decide what else is left.

**Do not deploy, publish, or push a new app version without asking me first.**
Read `omnidroid/MODES.md` before starting; it has every measurement below with
the method.

## Where things are

| | |
|---|---|
| engine | `C:\Users\berat\Desktop\Omni Apps\omnidroid`, branch `perf/native-speed-pass`, HEAD `125a253` |
| app | `...\omni-executor`, branch `feature/2captcha-account-creator`, HEAD `eb373a3` |
| currently deployed | app-win **1.0.37** — leave it alone unless I say otherwise |
| QEMU source | `C:\qemubuild` (git, v11.1.0) |
| build worktree | `C:\qemu-omni-v11.1.0` — **already has all 9 patches applied** |

## What is already established — measured, do not re-litigate

* **Not the ARM translator.** 1.14x integer, 0.98x SIMD, 1.54x indirect call
  vs native x86-64, measured with hand-assembled static ELFs
  (`omnidroid/tools/bench/mkbench.py`).
* **Not GPU- or fill-bound.** 800p and 1080p render at the *same* fps
  (46.9/45.1 vs 46.7/47.2), and the guest uses only **135–185% of the 800%**
  it has. Nothing downstream of the cadence is saturated.
* **Composition is already fixed** (shipped in 1.0.37): Android's
  "Viewing full screen" toast was a third composited layer on a hwcomposer
  with one plane, forcing client composition on every frame.
  clientCompositionFrames 100% → 0%, missedFrames 67% → 0.5%.
* **THE REMAINING CAP.** `gd_update_monitor_refresh_rate()` in `ui/gtk.c` sets
  `dcl.update_interval` — the timer deciding how often QEMU presents — from
  `gdk_monitor_get_refresh_rate()`, and **GDK returns 60000 mHz for every
  monitor on Windows**. 1000*1000/60000 = 16 ms. The guest agrees:
  SurfaceFlinger thinks the refresh period is 6.94 ms (144 Hz) while actual
  present→present p50 is **16.51 ms**. Roblox is not capping
  (`DFIntTaskSchedulerTargetFps: 240`, read back live) and virtio's fence poll
  is 10 ms.

## The task

`qemu-patches/0009-omni-win32-refresh-rate.patch` asks Win32 directly
(`MonitorFromWindow` → `GetMonitorInfoW` → `EnumDisplaySettingsW` →
`dmDisplayFrequency`) instead of trusting GDK, and adds `QEMU_UI_REFRESH_HZ`
to pin the rate for measurement. `update_interval` is integer ms, so 144 Hz →
6 ms → ~166 Hz of headroom. **It has never been compiled**; the frame-rate
expectation is arithmetic, not a result.

1. Build it: `tools/build_qemu.py --source C:/qemubuild --out C:/qemu-omni-refresh --targets x86_64-softmmu`
2. Point the engine at it with `OMNI_QEMU_DIR=C:/qemu-omni-refresh` and measure.
3. Sweep `QEMU_UI_REFRESH_HZ` (60 / 100 / 120 / 144) and see where the guest
   actually lands.

### Three build traps, all hit on 2026-09-02 — start past them

1. **Two `git`s.** The worktree is CRLF (Git for Windows); the patches are LF.
   msys2's `/usr/bin/git` has `core.autocrlf=false` and rejects every patch,
   which the script reports as *"the tree is in a state this script does not
   recognize"*. The patch stage needs **Git for Windows** first on PATH.
2. **Configure needs the opposite PATH.** Git's `sh` hands msys2's python a
   `/c/...` path it reads as `C:/c/...` and mkvenv dies. Configure/ninja want
   a real msys2 MINGW64 environment.
3. **`cc` and `TMP`.** msys2's mingw64 has `gcc.exe` but no `cc`, so configure
   finds *Git's* broken `/mingw64/bin/cc`; and with no inherited `TMP`, gcc
   falls back to `C:\WINDOWS` and every probe fails "Permission denied" —
   which configure reports as **"C compiler does not work"**. Pass `--cc=`
   explicitly and set `TMP`/`TEMP` to a *Windows-form* writable path.

Running the whole thing from an actual msys2 MINGW64 shell is the shape of the
answer. Driving `msys2_shell.cmd -c` through `cmd //c` from Git Bash silently
ran nothing — don't retry that wrapper without checking it executed.

### Fallback route, if 0009 is not enough

`-display sdl,gl=on`. Our QEMU is not built with SDL (msys2 has no SDL2
package installed); stock QEMU 11.0.50 does have it and its SDL+GL boot
**never scanned out** (10 min, no adbd) — same shape as `egl-headless` on
Windows. SDL would also cost the four `ui/gtk.c` patches, which are GTK-only.

## ⚠ Measurement protocol — get this wrong and every number is fiction

* **Move `%LOCALAPPDATA%\OmniExec\autoexec\zaphub.lua` aside first**, and put
  it back afterwards. It draws a full-screen opaque GUI over the game, and
  with it up SurfaceFlinger reports a flat 60 fps at ~6% CPU no matter what
  the stack is doing. Three readings in the last session were that GUI, and it
  made `blob=true` look like a +33% win when controlled it is slightly worse.
* **Confirm the 3D world is on screen with a screenshot** before believing a
  number. "in world" is not enough.
* Method: `dumpsys SurfaceFlinger --timestats -clear` then `-enable`, wait
  30 s, `-dump`. `totalFrames / displayOnTime` is the fps. Also read
  `missedFrames` and `clientCompositionFrames`.
* PS99 is server-authoritative and varies a lot run to run (56.8 and 34.8 fps
  from the same instance minutes apart). **Two samples minimum**, and say so.

## Expectation to set

Even uncapped this will not reach 170: there is ~1.2–1.5x of ARM translation
and a virtio-GPU round trip that native Roblox does not pay. 100–120 looks
reachable; parity does not.
