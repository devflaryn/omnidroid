# Handoff: close the 44→75 fps gap by putting the guest on Vulkan (venus)

## STATUS 2026-09-03: CLOSED — negative result

Rebuilt `libvirglrenderer-1.dll` 1.3.0 with venus (mingw64): builds clean,
exports identical to stock, still can't serve `venus=on` here — the render
server is POSIX-only (fork/socketpair/epoll) and venus's memory export is
fd/dma_buf-only while this GPU's Windows driver only has
`VK_KHR_external_memory_win32`. Measured launch: guest boots with **no GPU
at all**, worse than GLES. Evidence: `docs/bench-2026-09-03.md` "Third pass
2026-09-03". Build kept for a fast redo: `tools/virglrenderer-venus/`.
`OMNI_VENUS=1` (`omnidroid/qemu_proc.py`) stays as an inert/harmful gate,
off by default. Rest of this file is the original brief, kept for context.

Written 2026-09-03. Everything below was measured on **this** Windows box
(i7-13700F 8P+8E, RTX 4060, 32 GB). **Nothing is deployed.** Read
`docs/HANDOFF-WINDOWS.md` "PICK UP HERE — 2026-09-03" for the full
prior context; this file is the single open task and its evidence.

## The one finding that matters

The user ran the SAME ARM Roblox APK, SAME game (Pet Simulator 99), SAME
PC in **MuMu** emulator and got **~75 fps avg (80 peak)**. OmniDroid gets
**~44 fps**. So the ceiling is NOT the arm64 translator (that was a wrong
conclusion from an earlier session — ndk_translation tax is only 1.1-1.5x).
Something specific to our GPU path costs ~30 fps.

### Diagnosis (evidence, do not re-derive)

Roblox's own MicroProfiler + `nvidia-smi`, in-world PS99:

- **GPU is only ~21% busy.** We are NOT GPU-compute-limited.
- ~10 ms of a ~22 ms frame is the render thread **stalled**: `queryOcclusion`
  5-9 ms + `Present` 5 ms (both group Render). These are GPU **occlusion-query
  readbacks** and the swap.
- Our guest renders **GLES via virglrenderer**. virgl does a **synchronous
  host round-trip** for each occlusion-query result → the stall. MuMu uses a
  **Vulkan/native path** where those queries are async/cheap → no stall. That
  is the whole 44 vs 75 difference.

### Why we are stuck on the slow path — everything is ready EXCEPT one piece

| piece | status |
|---|---|
| guest Vulkan driver `vulkan.virtio.so`, `getprop ro.hardware.vulkan` = `virtio`, `/system/lib64/libvulkan.so` | ✅ present in the base |
| QEMU `virtio-gpu-gl-pci,venus=<bool>` option (also `blob=<bool>`) | ✅ the device advertises it |
| host Vulkan `C:\Windows\System32\vulkan-1.dll` (NVIDIA) | ✅ present |
| **virglrenderer built WITH venus** | ❌ **the shipped `libvirglrenderer-1.dll` has ZERO venus/vulkan symbols** (`strings libvirglrenderer-1.dll \| grep -ic venus` = 0) |

Because virglrenderer has no venus, Roblox tries Vulkan, venus init isn't
there, it falls back to GLES/virgl (the stalling path). Roblox's own flags to
disable the GPU queries are **ignored** — Roblox allowlisted local
`ClientAppSettings.json` flags in Sept 2025, so `FFlagRenderOcclusionQueries2`
etc. written locally do nothing (verified: the flag lands in the file,
`queryOcclusion` stays 5 ms). The network-MITM route to serve flags also
fails because the client caches settings as **zstd-dictionary-compressed
`.dcz` with 304**s (dict = `assets/android/shared_compression_dictionaries/
5174b6….dict`, itself JSON). So flags are a dead end; the GPU PATH is the fix.

## THE TASK

**Rebuild `libvirglrenderer-1.dll` with venus enabled, drop it into a QEMU
bundle, launch with `virtio-gpu-gl-pci,venus=on,blob=true,hostmem=…`, and
measure whether the guest gets a working Vulkan device, Roblox uses it, and
`queryOcclusion` collapses / fps rises toward ~70.**

⚠ **Real risk:** venus is Linux-host technology. virglrenderer's venus backend
proxies guest Vulkan to a **host** Vulkan driver; on a **Windows host** through
the WGL/ANGLE path QEMU uses, this is **unproven and may not build or work at
all.** If venus cannot render on Windows, say so clearly and stop — do not
spend days. A clean negative ("venus does not work on a Windows host because
X") is a valid, valuable result.

### How to build it (subagent-driven; the build is iterative, watch errors)

Environment: msys2 at `C:\msys64`. Invoke builds as
`MSYSTEM=MINGW64 CHERE_INVOKE=1 /c/msys64/usr/bin/bash -l <script>` and set
`TMP`/`TEMP` to a Windows-form path inside. `/mingw64/bin/cc.exe` exists.

1. **Deps** (the step the user paused before — run it):
   `pacman -S --noconfirm --needed mingw-w64-x86_64-vulkan-headers
   mingw-w64-x86_64-vulkan-loader mingw-w64-x86_64-vulkan-utility-libraries
   mingw-w64-x86_64-meson mingw-w64-x86_64-ninja mingw-w64-x86_64-python`
   (virglrenderer 1.3.0, vulkan-headers, vulkan-loader are the versions in msys2).
2. **Source:** clone `https://gitlab.freedesktop.org/virgl/virglrenderer.git`
   at tag **1.3.0** (match the installed `pacman -Q mingw-w64-x86_64-virglrenderer`
   = 1.3.0-1) into scratch. Or take the msys2 MINGW-packages PKGBUILD for the
   mingw patchset if a bare configure fails.
3. **Configure:** `meson setup build -Dvenus=true -Dvenus-validate=false
   --buildtype=release --prefix=<out>` — venus needs the Vulkan headers +
   the venus protocol (Venus-Protocol; virglrenderer's meson pulls it as a
   subproject/wrap, or install `vulkan-utility-libraries`). Expect the venus
   option to fight the mingw build; the interesting question is whether it
   even configures on Windows.
4. **Build+install:** `ninja -C build && ninja -C build install`. Confirm the
   new dll has venus: `strings libvirglrenderer-1.dll | grep -ic venus` > 0.
5. **Bundle:** copy the new dll over the one in a COPY of `C:\qemu-omni-next`
   (do NOT touch the app's `%LOCALAPPDATA%\OmniExec\qemu`). Keep QEMU itself as
   `C:\qemu-omni-next\qemu-system-x86_64.exe` (already has patches 0001-0014 +
   the QEMU_VIRGL_STATS instrumentation).
6. **Wire the device flags:** the guest is launched by `omnidroid/qemu_proc.py`.
   The GPU device line is built there (`virtio-gpu-gl-pci,xres=…,yres=…`). Add
   `,venus=on,blob=true,hostmem=1G` for a test (gate behind an env var like
   `OMNI_VENUS=1` so it is A/B-able and never on by default). venus REQUIRES
   `blob=true` and a `hostmem` window; without them it will not init.

### How to launch + measure (all proven this session)

Env for a checkout launch against the app's data + the patched QEMU:
```
export OMNI_DATA_DIR="C:\Users\berat\AppData\Local\OmniExec"
export OMNIDROID_CONFIG_PATH="C:\Users\berat\Desktop\Omni Apps\omnidroid\configs\paths.json"
export OMNI_QEMU_DIR="C:\qemu-omni-venus"   # the copy with the new dll
cd omnidroid && python manager.py start admn1b12farm2 --place 8737899170 \
    --mode gaming --offset omniexec-2.735.1138-lock2 --json
```
- Accounts admn1b12farm2/3/4 are live; password `<redacted — ask the owner>`
  if a cookie expires. Use a SPARE (not the one the user is playing) — one Roblox account
  in two guests kicks the first.
- **Confirm Roblox actually chose Vulkan:** `adb -s 127.0.0.1:16001 shell`
  then grep the client log
  `/data/data/com.roblox.client/files/appData/logs/<newest>` for
  `Vulkan`/`vulkan`/`VkDevice`/`graphicsMode`; and check the guest picked the
  venus ICD (`dumpsys | grep -i vulkan`, `logcat | grep -iE "venus|vulkan"`).
  If venus init fails it silently falls back to GLES — so PROVE Vulkan is live
  before trusting any fps number.
- **fps:** `omnidroid/tools/bench/sf-timestats.sh <label> 30` (env `ADBP=16001
  OUTDIR=…`). It screenshots before/after — confirm the 3D world is on screen,
  not a loader (the BIG Games / login-bonus screens fake plausible numbers).
- **The profiler (the real oracle):** an autoexec Lua
  `UserSettings():GetService("UserGameSettings").MicroProfilerWebServerEnabled
  = true` (drop as `%LOCALAPPDATA%\OmniExec\autoexec\50_x.lua`), then over QMP
  `human-monitor-command {command-line: "hostfwd_add tcp:0.0.0.0:1340-10.0.2.15:1338"}`
  and open `http://<LAN-IP>:1340/` in Chrome (this box: 192.168.0.15;
  Chrome cannot load localhost here). The page decodes its own frame blob;
  read `TimerInfo` for `queryOcclusion`, `Present`, and `Frames` for frame_ms.
  If venus works, `queryOcclusion` should drop sharply.
- **QEMU_VIRGL_STATS=1** env prints per-second virgl cmd time by class + fence
  latency to `runtime/<acct>/qemu.log` (`omni-virgl-stats`, `omni-fence-stats`).

### Traps that cost time this session (avoid)

- `nvidia-smi -lgc` needs admin; the NVAPI per-app profile does not — it is
  already applied (`omnidroid/nvprofile.py`, committed) and pins the card to
  P0 2475 MHz for `qemu-system-x86_64.exe`. Confirm P0 during any measurement
  (`nvidia-smi --query-gpu=pstate,clocks.gr --format=csv`).
- `pkill`/Stop-Process on `python`/`bash` by name kills THIS session's shell —
  filter by full CommandLine match.
- PS99 fps varies ~10% between boots; the MicroProfiler's 30-frame capture can
  show a lucky 18 ms while sf-timestats over 30 s says 43. Trust the 30 s
  average; use the profiler only for the per-timer BREAKDOWN.
- Bringing Chrome/a browser tab to the foreground on the guest makes Roblox
  LEAVE the place. Measure the guest, don't touch its foreground.
- A black `screencap` right after join is just PS99 still loading (~35 s); wait
  and re-shot before concluding a render failure.

## State of the repo and the box (all local, nothing pushed)

- **omnidroid** branch `perf/native-speed-pass`, committed this session:
  Skylake CPU default (`qemu_proc.WHPX_KVM_CPU_MODEL`), `nvprofile.py` (NVIDIA
  full-clock, wired into every Windows launch), mouse-look lock (patches
  0013 + `mouselock.py` + the `-lock2` offset), host-cursor fix, QEMU patches
  0011-0014, `docs/bench-2026-09-03.md` (every number). `git log --oneline -8`.
- **QEMU** `C:\qemu-omni-next` = 11.1.0 + patches 0001-0014 + QEMU_VIRGL_STATS
  instrumentation. Source worktree `C:\qemu-omni-v11.1.0`, build via
  `/c/qemubuild-tmp/ninja-one.sh` (~2 min) or reconf.sh (~10 min). The shipped
  app QEMU is `%LOCALAPPDATA%\OmniExec\qemu` (1.0.38, no venus, don't touch).
- **The app is set up for the user right now:** `%LOCALAPPDATA%\OmniExec\
  paths.json` was edited (backup `paths.json.bak-pre2735-*`) to register offset
  `omniexec-2.735.1138` (image copied into the app's `images\x86\`), set it as
  the base `x86` `default_offset`, and set `qemu.cpu = Skylake-Client-v4`. So a
  launch from the desktop Omni Executor app now runs the 2.735 executor
  (scripts work) at ~44 fps with the GPU at full clocks. venus would improve
  THIS too, but the app's shipped QEMU lacks the dll — venus testing is on the
  checkout + `C:\qemu-omni-venus` only until a deploy.
- Offsets baked this session (images in `C:\Users\berat\OmniImages\x86\`):
  `omniexec-2.735.1138`, `-lock` (Java pointer-capture gate), `-lock2` (gate +
  cursor stays at drag origin — the one to use), `-lock3` (adds our CA to the
  client cert bundle, for the settings-MITM experiment — not needed for venus).
- Disk was tight (~13 GB) mid-session, now ~27 GB free. A venus build + a QEMU
  bundle copy is ~1-2 GB. Watch it.

## If venus works

Then it is the real win: wire `venus=on,blob=true,hostmem=1G` into the gaming
GPU path in `qemu_proc.py` (behind the mode/gpu logic, Windows+NVIDIA only),
rebuild the shipped QEMU bundle to include the venus dll, and it deploys with
the next app version. If it does NOT work on Windows, the honest fallback is
that OmniDroid on Windows is virgl-GLES-bound at ~44-52 fps and MuMu-parity
needs either a Linux host or a different host GPU backend — document that and
stop.
