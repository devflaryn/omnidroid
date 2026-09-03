# Handoff: get PS99 to 60+ fps (MuMu does 75+ on this same PC/APK/game)

Paste the block at the very bottom into a fresh Claude Code session started in
`C:\Users\berat\Desktop\Omni Apps`.

**Supersedes `HANDOFF-FPS.md`.** That file asked "what limits in-world fps".
It is now answered (see "Settled" below). This file is the next job: **close
the gap to MuMu.**

## The one-paragraph situation

Same PC (i7-13700F, RTX 4060, 32 GB), same ARM Roblox APK, same game (Pet
Simulator 99): **MuMu gets ~75 fps avg (80 peak) at full quality; OmniDroid
now gets a stable ~46.** The difference is the GPU path, not a setting. MuMu
uses an **in-process renderer** (gfxstream-class: guest Vulkan/GLES + guest GPU
memory shared with the host renderer in one process) so an occlusion-query
readback or a swap is a shared-memory read. OmniDroid runs guest GLES through
**virtio-gpu + virglrenderer** over a virtqueue, so every GPU fence the render
thread waits on is a CPU→GPU→CPU round-trip. ~8 per frame, and they serialize.

## Where things stand (2026-09-03, session 646bf8ae)

| | |
|---|---|
| engine | `omnidroid`, branch `perf/native-speed-pass`, HEAD `4c95a74` |
| new this session | patch **0015** omni-win32-fence-watch (`5567ee7`), bench 4th pass (`4c95a74`) |
| patched QEMU | `C:\qemu-omni-next\qemu-system-x86_64.exe` = 11.1.0 +0001-0015 +QEMU_VIRGL_STATS. Source worktree `C:\qemu-omni-v11.1.0`, build `MSYSTEM=MINGW64 CHERE_INVOKE=1 /c/msys64/usr/bin/bash -l /c/qemubuild-tmp/ninja-one.sh` (~2 min) |
| deployed app | app-win **1.0.38** — its bundled QEMU is an OLDER patch level (0001-0008, no 0009-0015). **Do NOT hot-swap the whole exe into the live install** and do not deploy/publish without asking |
| virglrenderer source | clone at tag 1.3.0 in `C:\Users\berat\AppData\Local\Temp\claude\C--Users-berat-Desktop-Omni-Apps\fcae51d4-c369-4304-90ca-1d82c857bb78\scratchpad\venus\virglrenderer`; build recipe + mingw patch in `omnidroid/tools/virglrenderer-venus/` |

### Settled this session — do not re-measure

* **The WGL fence poll IS the constraint, non-linearly.** On the GTK/WGL path
  there is no EGL display, so QEMU never runs virglrenderer's async fence
  thread and a fence the guest blocks on (Mesa occlusion-query readback, the
  Present swap) was only retired on the 10 ms `fence_poll` timer. That latency
  pushes the frame just past the display deadline and the present rate HALVES.
  Two clean samples of the shipped path: **21.6 / 21.9 fps, ~50% frames
  missed, present p50 46 ms.** This is the "22-25 one day, 47-53 the next" the
  old handoff could not explain. It **corrects** the earlier "fence poll is not
  the constraint" (that A/B landed both boots above the boundary by luck).
* **Patch 0015 fixed it (committed, on by default).** After virglrenderer
  creates its GL fence, QEMU arms its own `glFenceSync`; a watcher thread with
  a shared WGL context blocks in `glClientWaitSync` and schedules a BH the
  instant the GPU signals. Timer stays as fallback; `OMNI_FENCE_WATCH=0`
  disables; no-op off Windows / headless farming. Result: fence latency 12 →
  0.3 ms, **PS99 44.5 / 46.0 / 47.2 fps, 0 missed**, and the vsync-doubling
  failure mode is gone. armed==fired every second (no leaks).
* **With latency gone, the frame is now GPU-completion-serial-bound.** Roblox's
  MicroProfiler, render-thread EXCLUSIVE ms/frame: **queryOcclusion 7.3
  (6.5 calls), Present 5.5, updateRenderQueue 3.0.** GPU 15% util, guest 70%
  idle → NOT compute/GPU-bound. These are the CPU↔GPU round-trips.
* **Negatives, do not repeat:** `idle=poll` WITH the watcher = 31/35 fps
  (8 spinning vCPUs oversubscribe the cores, starve the main loop + watcher).
  `venus` is structurally dead on Windows (`HANDOFF-VENUS.md`,
  `venus-cannot-work-on-windows-host` memory). Quality level, thread pinning,
  affinity, Berberis, power plan: all null (`docs/bench-2026-09-03.md`).

## THE JOB: get to 60 (stretch 75)

Two independent tracks. **Track A is reachable and the place to start.**

### Track A — async occlusion (kills the 7.3 ms queryOcclusion stall) → ~58-62 fps

Roblox reads occlusion queries back on the render thread, same frame. Each read
blocks until the GPU finishes the depth pass. Return the **previous frame's**
result instead (standard temporal-occlusion technique; every AAA engine does
it). Quality cost: 1 frame of culling latency → at worst a rare edge-pop on a
fast camera cut, imperceptible in a pet-sim. Confirm with the user that this
counts as "no quality loss" for them.

**Do it host-side in virglrenderer (recommended — build tooling exists, no base
image change):** in `src/vrend/vrend_renderer.c`, the occlusion query result is
written to a coherent guest buffer by `vrend_check_query` only after the GL
query is available (post-GPU). The guest Mesa (`virgl_get_query_result`,
wait=true → `resource_wait` → `DRM_IOCTL_VIRTGPU_WAIT`) blocks on that buffer's
busy state. Make the query result buffer report `VIRGL_QUERY_STATE_DONE` with
the last-known sample count **immediately** at query-end (so the guest's wait
returns at once and reads the stale-but-valid result), and update the cached
value when the GPU actually finishes, for the next frame. The busy/fence
tracking for that specific buffer is the tricky part — study `vrend_check_query`
/ `vrend_get_query_result` / `vrend_get_one_query_result` and how the query
buffer resource's busy flag is set. Gate behind an env var
(`OMNI_ASYNC_OCCLUSION=1`), build the DLL with `omnidroid/tools/virglrenderer-venus/build.sh`
minus `-Dvenus=true` (i.e. `meson setup build --buildtype=release`), drop it
into a copy of `C:\qemu-omni-next`, and A/B.

**Alternative (guest-side, cleaner logic but touches the base image — ask
first):** patch Mesa's `virgl_get_query_result` to call with wait=false and
return the last result. That means building Android x86_64 Mesa and swapping
`libgallium_dri.so` in the base — higher risk, base-image change.

Reality check: async occlusion removes ~7 ms of a ~21 ms frame → ~14 ms →
~58-70 fps depending on how Present then overlaps. That alone should clear 60.

### Track B — MuMu parity via gfxstream (the real 75+ path; large effort)

**venus is dead on Windows, but gfxstream is NOT venus.** gfxstream (Android's
own GPU streaming, used by the Android Studio emulator and by MuMu) shares guest
GPU memory with the host via **Win32 memory handles** and works on Windows today
(the AOSP emulator proves it). It is the architecturally correct way to match
MuMu. Cost: QEMU's `virtio-gpu` does not speak gfxstream; you would need the
gfxstream host backend (`libgfxstream_backend`) wired as a virtio-gpu context
type, or to adopt the AOSP emulator / crosvm device model. This is a multi-week
spike, not a patch. Scope it before committing. If Track A gets the user to 60+
they may not need this.

### Track C — small, safe, only if A stalls

* Confirm whether Present (5.5 ms) has any host-side slack: patch 0012 is
  `omni-present`; check `virtio_gpu_virgl_process_cmd` RESOURCE_FLUSH timing
  (`QEMU_VIRGL_STATS=1` already prints per-class cmd time to qemu.log). Host
  flush was only 0.09 ms each this session, so Present is guest-side GPU
  finish — likely not host-addressable, but verify.

## Measurement protocol — get this wrong and every number is fiction

* **Account `admn1b12farm2`, place 8737899170 (PS99), offset
  `omniexec-2.735.1138-lock2`.** Accounts admn1b12farm2/3/4 are spares; never
  measure on one the user is playing (`executor-and-checkout-share-the-account-slot`
  memory). Launch:
  ```
  export OMNI_DATA_DIR="C:\Users\berat\AppData\Local\OmniExec"
  export OMNIDROID_CONFIG_PATH="C:\Users\berat\Desktop\Omni Apps\omnidroid\configs\paths.json"
  export OMNI_QEMU_DIR="C:\qemu-omni-next"       # or your async-occlusion copy
  export QEMU_VIRGL_STATS=1
  cd omnidroid && python manager.py start admn1b12farm2 --place 8737899170 \
      --mode gaming --offset omniexec-2.735.1138-lock2 --json
  ```
* **CONFIRM THE WORLD IS ON SCREEN before every sample.** A boot that sits on
  the Roblox home screen or the BIG-Games loader reads **53-59 fps at
  clientComp=100%** (that is the 2D web UI, not the game) and it looks real.
  The in-world rule: `dumpsys SurfaceFlinger --timestats` shows
  **clientCompositionFrames = 0** AND a screenshot shows the 3D scene. PS99 can
  take 100-170 s to reach the world; poll, do not assume.
* **fps:** `omnidroid/tools/bench/sf-timestats.sh <label> 30` (env `ADBP=16001
  OUTDIR=...`). It screenshots before/after. Take **two 30 s samples minimum**;
  PS99 varies ~10% between boots.
* **The profiler (the per-timer oracle):** drop
  `UserSettings():GetService("UserGameSettings").MicroProfilerWebServerEnabled
  = true` as `%LOCALAPPDATA%\OmniExec\autoexec\40_x.lua`, then over QMP
  `hostfwd_add tcp:0.0.0.0:1340-10.0.2.15:1338` (helper
  `<scratchpad>\qmp.py <qmp-port> <hmp cmd>`), open `http://192.168.0.15:1340/`
  in Chrome (this box; Chrome cannot load localhost here), and read TimerInfo
  via `PreprocessCalculateAllTimers()` + `t.ExclusiveFrameAverage`. **Delete
  that autoexec after** — it leaves the profiler server on.
* Keep the QEMU window on the **144 Hz panel** (primary at 0,0); the Parsec
  Virtual Display at x=-1920 is 60 Hz and halves the guest rate
  (`window-monitor-decides-guest-fps` memory).
* `QEMU_VIRGL_STATS=1` prints `omni-fence-stats` (fence create→retire latency),
  `omni-fence-watch` (armed/fired/wakeups — armed==fired means no leak), and
  `omni-virgl-stats` (per-class cmd time) to `runtime/<acct>/qemu.log`.

## Traps that cost time

* `manager.py stop`/Stop-Process by exe or python name can kill the user's own
  instance or this shell — filter by full CommandLine / the runtime pid.
* The running guest holds `qemu-system-x86_64.exe` open; `cp` over it fails
  "Device or resource busy" until you `stop`.
* `bash sleep 100` in a chained command is fine; a bare foreground `sleep` in
  this harness is blocked — use `run_in_background` + an `until grep` poll.
* PIL is not installed in this Python; check the world via SurfaceFlinger
  `clientCompositionFrames` + a visual screenshot, not a pixel script.
* Bringing a browser tab to the foreground ON THE GUEST makes Roblox leave the
  place. Measure the guest; never touch its foreground.

## Rules

Do not deploy, publish, or push an app version. Ask before any base-image
change. Say plainly when a number is a single sample, and confirm clientComp=0
before trusting any fps.

---

## PASTE THIS INTO THE NEW SESSION

> Read `omnidroid/HANDOFF-FPS2.md` first — it is the full brief and supersedes
> `HANDOFF-FPS.md`.
>
> Short version: OmniDroid's PS99 in-world fps is now a stable ~46 (was a
> chaotic 21-47). Last session found the cause — on Windows' WGL path virgl
> fences were only retired on a 10 ms timer, which halved fps via vsync
> doubling — and fixed the latency with patch 0015 (event-driven fence
> watcher, committed, on by default). With latency gone the frame is now
> GPU-completion-serial-bound: the render thread spends queryOcclusion 7.3 ms +
> Present 5.5 ms per frame on synchronous CPU↔GPU round-trips, while the GPU is
> 15% busy and the guest 70% idle. MuMu gets 75 on the same PC/APK/game because
> it uses an in-process gfxstream renderer with no such round-trips.
>
> Your job: get PS99 to at least 60 fps at the same resolution and no
> perceptible quality loss. Start with Track A in the handoff — async occlusion
> (return the previous frame's occlusion-query result so the render thread stops
> waiting for the depth pass), done host-side in virglrenderer behind
> OMNI_ASYNC_OCCLUSION=1, then A/B it. That removes ~7 ms of a ~21 ms frame and
> should clear 60. Track B (gfxstream, the real MuMu-parity path to 75+) is a
> larger spike — scope it only if the user wants beyond 60.
>
> The measurement protocol in the handoff is not optional: spare account
> admn1b12farm2, place 8737899170, offset omniexec-2.735.1138-lock2, QEMU at
> C:\qemu-omni-next (copy it for your DLL), keep the window on the 144 Hz panel,
> and CONFIRM clientCompositionFrames=0 plus a world screenshot before trusting
> any fps — the home screen and the BIG-Games loader fake 53-59 fps. Two 30 s
> samples minimum. Do not deploy or change the base image without asking.
