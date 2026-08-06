# Two use cases, one engine

> **Updating the game:** `omni bake-data-game <new-roblox.apk>` — installs it
> into the base's /data as an updated system app, ~2 minutes, no system-image
> rebuild and no scratch space. Every run starts from the pristine rooted
> /data, so updates never chain. The replacement APK must be signed with the
> same key as the build already baked into the system image, or the guest
> rejects it and the bake aborts without touching the shipping image.
> Run `omni bake-data-game` with no APK to bake only the kiosk's game-package
> setting.


OmniDroid serves two jobs that pull in opposite directions, and almost every
tuning decision in the codebase is a choice between them.

| | `--mode gaming` | `--mode farming` |
|---|---|---|
| what matters | frames, input latency | RAM and CPU per instance |
| what does not | density, host footprint | speed, quality, anything visual |
| instances per host | 1–2 | 50+ (on a 64 GB Linux host) |
| host window | yes, when the host can open one | never |
| guest display | the base's native 1280x800 | 480x270 @ 80 dpi |
| engine tick | 240 fps target | 5 fps cap |
| balloon | none | 896 MB (with zram) / 1536 (without) |
| vCPU / RAM | 4 / 4096 MB | 1 / 2048 MB |
| zram | off | on (baked into the base) |
| scheduler | game on the `top-app` cpuset | game on `background` |
| doze | disabled | force-idled |

`playable`, `hard` and `brutal` are unchanged headless RAM/CPU tiers, not use
cases. `playable` is still `DEFAULT_MODE`, so a bare `omni start` behaves
exactly as it always has.

---

## Gaming mode

```sh
omni start <account> --mode gaming
```

### The window

A gaming boot asks for a native QEMU window and takes the best tier the host
can actually provide. The capability is detected by asking the QEMU binary
what it has (`qemu_proc.default_display`), never assumed:

| tier | what you get | when |
|---|---|---|
| `gl` | `virtio-gpu-gl-pci` + `<backend>,gl=on` — real 3D acceleration | QEMU built with virglrenderer/OpenGL |
| `window` | `virtio-gpu-pci` + `<backend>` — native window, software rendering | QEMU with a cocoa/gtk/sdl backend |
| `none` | today's headless `-display none` | no host GUI, or no windowing backend |

Degradation is total and silent-safe: a malformed capability, a headless SSH
session or a QEMU without any window backend all produce the byte-for-byte
headless command, so a detection bug can cost a window but never a boot.

The instance still runs its VNC server in **every** mode — `omni screenshot`,
the autocap recorder and the `omnidroid-input` skill all attach to that
framebuffer. What a native window suppresses is only the second *viewer*
window `omni start` would otherwise open onto the same instance.

### The GL tier is not reachable on the dev Mac today

MEASURED 2026-08-06, Homebrew QEMU 11.0.2 on Apple Silicon:

```
$ qemu-system-aarch64 -display cocoa,gl=on
qemu-system-aarch64: OpenGL support was not enabled in this build of QEMU
$ qemu-system-aarch64 -device help | grep gpu
name "virtio-gpu-pci", bus PCI, alias "virtio-gpu"        # no -gl variant
$ brew info virglrenderer
Error: No available formula with the name "virglrenderer".
```

So on that host gaming mode lands on the `window` tier: a real cocoa window,
software rendering. That is still the large input-latency win, because host
input events go straight into the guest's `usb-tablet`/`usb-kbd` instead of
making a VNC round trip through framebuffer encode → decode → synthesised
input.

Reaching the `gl` tier needs **two** things, and neither exists yet:

1. a QEMU built `--enable-opengl --enable-virglrenderer` (source build on
   macOS; on Linux the distro packages generally already have it), and
2. a **guest** driver that can drive virgl — the LineageOS arm64 image renders
   in software today. This is the open B3 question, and it is the bigger half.

Until both land, "GPU acceleration" for this project means the window tier.
The code is already capability-gated, so a QEMU that gains virgl lights up the
`gl` tier with no code change.

### What a gaming boot does inside the guest

Applied after boot (`gaming.build_tuning_sequence`), and note that most of it
is an **undo**: the farming levers persist in a non-ephemeral account's
`/data`, so an account that was farmed and is then started in gaming mode
keeps a 480x270 display until something reverses it.

- `wm size reset` / `wm density reset` — back to native
- all three animation scales to 0
- `swappiness` 10 — keep the game's pages resident (root only)
- `deviceidle disable` + game whitelisted — no throttling of a foreground game
- IME re-enabled — farming disables it, and nothing can be typed without it
- ClientAppSettings with a 240 fps tick target and low render cost

Then, **after** the session is delivered (the broadcast is what launches the
game), `pin_game_to_top_app` moves it onto the `top-app` cpuset.

### Verified live, 2026-08-06

On `HezMi_ImYu`, arm64 rooted base, Apple Silicon:

```
boot completed after 0.6–1.5 min
roblox settings: applied (tick target 240 fps, low render cost)
gaming tune-up: native resolution, animations off, doze off, swappiness 10
cpuset: game on top-app (latency-critical scheduler set)
```

Read back out of the guest independently:

| lever | verified |
|---|---|
| QEMU window | `QEMU omni-HezMi_ImYu` present on screen |
| display | `Physical size: 1280x800` (not the farming postage stamp) |
| swappiness | `10` |
| tick target | `"DFIntTaskSchedulerTargetFps": 240` |
| cpuset | `3:cpuset:/top-app`, still `/top-app` 15 s later |

### Two honest limits

- **The cpuset pin does not survive a game restart.** Observed: the game
  restarted (pid 5507 → 5730) and the new process came up in `/background`.
  The pin holds for the process it was applied to; it is not a policy.
- **Frame rate has not been measured.** The guest currently shows a
  `System UI isn't responding` ANR and foregrounds the Magisk manager rather
  than the kiosk. That reproduces identically on the **unchanged** headless
  `--mode playable` path, so it is a property of the freshly-built rooted base
  (dual-use Phase 2, still unverified per its own spec), not of gaming mode.
  No fps claim can be made until that is fixed.

---

## Farming mode

Unchanged by this work. See `FOOTPRINT.md` for the full measured picture. The
short version: ~896 MB per instance with zram, ~71 instances on a 64 GB Linux
host, and the ~400 MB per-instance target is not reachable with Roblox, whose
engine alone is ~680 MB resident.
