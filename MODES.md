# Two use cases, one engine

> **Adding or updating a Roblox version:** `omnidroid offset create <name> --apk
> <roblox.apk>` — see `HOWTO.md`. Versions are **offsets**: named, thin /data
> overlays that coexist on one clean base, with one marked default. The old
> `bake-data-game` is a deprecated alias for it.

OmniDroid serves two jobs that pull in opposite directions, and almost every
tuning decision in the codebase is a choice between them. Each mode declares
which job it is for as a **profile**, and that — not the mode's name — is what
the engine branches on:

- **`performance`** — spend the host on ONE instance. `playable` (the
  default), `gaming`, and the smaller fixed tiers `hard`/`brutal`.
- **`density`** — spend quality on instance COUNT. `farming`, and only
  `farming`.

| | `--mode playable` (default) | `--mode gaming` | `--mode farming` |
|---|---|---|---|
| what matters | frames, quality, fast boots | the above **plus** a host window | RAM and CPU per instance |
| what does not | density, host footprint | density, host footprint | speed, quality, anything visual |
| instances per host | 1–2 | 1–2 | 50+ (on a 64 GB Linux host) |
| host window | never (attach with `view`) | yes, when the host can open one | never |
| guest display | the base's native 1280x800 | native 1280x800 | 480x270 @ 80 dpi |
| engine tick | 240 fps target | 240 fps target | 5 fps cap |
| render quality | `high` — real textures, lighting, post-FX | `high` | lowest everything |
| balloon | none | none | 896 MB (with zram) / 1536 (without) |
| vCPU / RAM | **sized to the host**, up to 8 / 8192 MB | same | 1 / 2048 MB |
| zram | off | off | on (baked into the base) |
| scheduler | game on the `top-app` cpuset | same | game on `background` |
| doze | disabled | disabled | force-idled |

`hard` (3072 MB / 4 vCPU) and `brutal` (2048 MB / 2 vCPU) are fixed smaller
tiers for a tight host. They are performance-profile modes too — they are
explicit "give this instance LESS" requests, so they are the two modes that do
**not** grow to the host.

## Playable takes the machine

`playable` is what a human plays in **and** what the AI tests in, so it is
sized to the host rather than pinned at a constant:

```
mem  = clamp(min(host_ram/2, host_ram - 6 GB), 4096 MB, 8192 MB)   # 512 MB steps
smp  = clamp(host_cores - 2, 4, 8)
```

"As much as it safely can" is the load-bearing half. A guest sized past the
host's spare RAM makes the **host** swap, and a swapping host misses QEMU's
vCPU deadlines — slower than the smaller guest would have been, and on macOS
eventually fatal to the process. So the host keeps a 6 GB reserve and two
cores. The 4096 MB floor is the measured requirement: at 1024 MB the game is
OOM-killed outright (`has died: fg TOP` + `mem-pressure-event`).

An explicit `--mem` / `--smp` always wins outright over the host-derived size,
and a host whose capacity cannot be read falls back to the declared 4096/4 —
an unreadable host costs you the upgrade, never the boot.

### Render quality

`--quality` picks the Roblox `ClientAppSettings.json` profile:

| | tick | quality level | post-FX | used by |
|---|---|---|---|---|
| `high` | 240 | 10 | on | playable, gaming (default) |
| `balanced` | 240 | 3 | off | hard, brutal — maximum frame rate |
| `low` | 5 | 1 | off | farming |

`high` is the default for the modes the AI screenshots, and that is the point:
a screenshot of a deliberately ugly render is a screenshot of a different
program — a missing texture, a bad shader or a mis-lit model is simply not
visible at quality level 3 with post-FX off.

MSAA stays at 0 even in `high`. The guest has no 3D acceleration on the
primary host (see below), so every sample is resolved in software on the same
CPU running the game; it is the one quality key that multiplies cost per pixel
with almost nothing to show for it at this resolution.

---

## Gaming mode

```sh
omnidroid start <account> --mode gaming
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

The instance still runs its VNC server in **every** mode — `omnidroid screenshot`,
the autocap recorder and the `omnidroid-input` skill all attach to that
framebuffer. What a native window suppresses is only the second *viewer*
window `omnidroid start` would otherwise open onto the same instance.

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

### What a performance boot does inside the guest

Applied after boot (`gaming.build_tuning_sequence`) on **every**
performance-profile mode — `playable` included — and note that most of it is
an **undo**: the farming levers persist in `/data`, so an offset whose /data
was last touched by a farming boot keeps a 480x270 display until something
reverses it.

- `wm size reset` / `wm density reset` — back to native
- all three animation scales to 0
- `swappiness` 10 — keep the game's pages resident (root only)
- `deviceidle disable` + game whitelisted — no throttling of a foreground game
- IME re-enabled — farming disables it, and nothing can be typed without it
- the `high` ClientAppSettings profile (240 fps tick, quality 10, post-FX on)

Then, **after** the session is delivered (the broadcast is what launches the
game), `pin_game_to_top_app` moves it onto the `top-app` cpuset.

> **This used to apply to `gaming` only, by accident.** The engine compared
> `mode_name` — the raw `--mode` argument — against the literals `"gaming"`
> and `"farming"`. A bare `omnidroid start` passes no `--mode`, resolves to
> `playable`, and therefore matched neither: the most-used mode was the only
> one that got no post-boot tuning at all. Branching on `profile` fixes it at
> the cause. `tests/test_gaming_apply.py::PlayableBoot` is the regression test.

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
