# Two modes, one engine

> **Adding or updating a Roblox version:** `omnidroid offset create <name> --apk
> <roblox.apk>` — see `HOWTO.md`. Versions are **offsets**: named, thin /data
> overlays that coexist on one clean base, with one marked default.

OmniDroid serves two jobs that pull in opposite directions, and almost every
tuning decision in the codebase is a choice between them. Each mode declares
which job it is for as a **profile**, and that — not the mode's name — is what
the engine branches on:

- **`performance`** — spend the host on ONE instance. `gaming`, the default.
- **`density`** — spend quality on instance COUNT. `farming`.

| | `--mode gaming` (default) | `--mode farming` |
|---|---|---|
| what matters | frames, resolution, input latency | RAM and CPU per instance |
| what does not | density, host footprint | speed, quality, anything visual |
| instances per host | 1–2 | many (see the footprint note below) |
| host window | on screen from spawn; QEMU's own window, restyled in place (see **The GPU**) | never |
| guest MTU | matched to the host's egress (see **Farming**) — both modes | same |
| guest panel | **1920x1080 where the host's screen can show it**, else 1280x800 (`--panel` overrides) | 640x480, `wm size` 480x270 |
| engine tick | 240 fps target | 5 fps cap |
| render quality | `high` — real textures, lighting, post-FX | lowest everything |
| balloon (post-boot reclaim) | none | 896 MB (with zram) / 1536 (without) — **skipped entirely on a host that cannot return the pages; see Farming** |
| memory governor | boot cap 1536 MB, floor 1024, headroom 512 | boot cap 1024 MB, floor 896, headroom 384 |
| vCPU / RAM | **sized to the host**, capped 4 GB / **8 vCPU** on WHPX | 2048 MB, 1 vCPU (3 on x86) |
| zram | off | on (baked into the base) |
| scheduler | game on the `top-app` cpuset | game on `background` |
| GPU policy | `auto` | `headless` |

### The three modes that are gone

`playable`, `hard` and `brutal` were removed on 2026-08-15.

* `playable` was `gaming` without a window. Once no mode opened a window,
  there was no difference left to name.
* `hard` (3072 MB / 4 vCPU) and `brutal` (2048 MB / 2 vCPU) were fixed "give
  this instance less" tiers that predate `--mem`/`--smp` being honoured
  properly. `--mem 3072` says the same thing in the flag that already exists.

**All three are still ACCEPTED and resolve to `gaming`.** An installed app
persists the mode it was configured with — 1.0.14 ships `"mode": "playable"` as
its default — so rejecting the name would break every launch from a client that
has not updated. The alias is resolved once, in `resolve_mode()`, so run.json,
the warm-cache key, the tuning branch and the UI all see `gaming`.

---

## Gaming takes the machine

```
mem  = clamp(min(host_ram/2, host_ram - 6 GB), 4096 MB, 8192 MB)   # 512 MB steps
smp  = clamp(host_cores - 2, 4, 8)
```

...then capped at **4096 MB on WHPX**. KVM and HVF keep the larger memory
ceiling.

### CORRECTION, 2026-09-01: the vCPU cap was 4 and it should never have been

The 2026-07 reading behind "4096/4 or else" moved **memory and vCPUs in one
step** — 4096 MB/4 vCPU against 8192 MB/8 vCPU — measured 0.9 min against
5.9 min, and attributed all of it to the vCPUs. Re-measured with memory held
constant at 4096 MB:

| | boot to bootcomplete |
|---|---|
| `--smp 4` | 33.6 s |
| `--smp 8` | 34.6 s |
| `--smp 8 --panel 1080p` | 35.6 s |
| `--smp 8 --panel 1440p` | 35.6 s |

**There is no 6.5x.** There is no penalty worth the name. Whatever the 2026-07
run measured, it was not the vCPU count — the obvious suspect is the 8192 MB
half, on a host where commit charge tracks `-m` at 1:1.

**And the guest was starved at 4.** Sampled per-process out of `/proc/*/stat`
on a live in-world PS99 session at `--smp 4`:

```
297.3%  com.roblox.client        <- of the 400% a 4-vCPU guest has
  1.8%  surfaceflinger
  0.9%  composer@2.4
        everything else under 1%
```

Roblox alone was using three of the four vCPUs. SurfaceFlinger — the whole
render path — was 1.8%.

**What 8 buys.** Measured with a hand-assembled static x86-64 dependency-chain
loop (`tools/bench/`, see "The translator is not the wall" below) run 1..8 ways
in parallel inside the guest, wall clock:

| parallel copies | 1 | 2 | 4 | 6 | 8 |
|---|---|---|---|---|---|
| ms | 223 | 282 | 405 | 420 | 342–506 |

8 vCPUs deliver **~5x** one vCPU's throughput where 4 deliver **~3.1x**. WHPX
does not scale linearly and never has; "sublinear" is not "negative", and 4 was
leaving the rest on the floor. `WHPX_SMP_CEIL` is 8. `PERF_SMP_HOST_RESERVE`
still applies underneath it, so a 6-core host gets 4, not 8.

**QEMU's process priority makes no reliable difference either, and this is
how nearly-shipping a placebo looked.** A 13th-gen host has P-cores and
E-cores, and vCPU threads landing on E-cores is a plausible explanation for
sublinear scaling. First measurement at `--smp 8`, in-guest parallel loop:

| parallel copies | 1 | 2 | 4 | 6 | 8 |
|---|---|---|---|---|---|
| Normal (first reading) | 321 | 328 | 353 | 521 | **576** |
| High | 334 | 279 | 298 | 377 | **451** |
| AboveNormal | 263 | 273 | 297 | 376 | **416** |
| **Normal again (control)** | 284 | 265 | 271 | 349 | **426** |

That last row is the whole result: repeated, Normal is indistinguishable from
AboveNormal and High. **The 576 was the outlier**, not the 451 — the host had
other work on it. Priority is NOT changed. One A/B pair on a machine you are
also working on is not a measurement.

**`kernel-irqchip` makes no difference and was also never measured.**
`default_accel()` has returned `whpx,kernel-irqchip=off` since PLAN.md, copied
from a recipe. QEMU's WHPX backend does support the in-hypervisor X2APIC
(`WHvX64LocalApicEmulationModeX2Apic`, `target/i386/whpx/whpx-all.c:3152`), and
turning it on looked like the obvious fix for the poor SMP scaling. Measured at
`--smp 8`, same image, same everything:

| parallel copies | 1 | 2 | 4 | 6 | 8 |
|---|---|---|---|---|---|
| `kernel-irqchip=off` | 223 | 282 | 405 | 420 | 506 |
| default (on) | 268 | 270 | 348 | 459 | 526 |

Inside the noise. The guest logs `x2apic enabled` either way. **Left alone** —
but do not spend a session on it again.

"As much as it safely can" is the load-bearing half: a guest sized past the
host's spare RAM makes the **host** swap, and a swapping host misses QEMU's
vCPU deadlines — slower than the smaller guest would have been. An explicit
`--mem`/`--smp` always wins outright.

### Resolution

`--panel WxH` (or `720p`/`800p`/`1080p`/`1440p`), config `qemu.panel`, env
`OMNI_PANEL`. Gaming's default is now **host-aware**: `panel_for` grows the
mode's declared panel toward `PERF_PANEL_CEIL` (1920x1080) when this machine's
primary screen can show it with room for the window's own frame, and leaves it
alone otherwise. Density is never grown — farming's 640x480 is the point of
farming.

#### CORRECTION, 2026-09-01: the base was never capped at 1280x800

The old text here said, in bold, that asking for more than 1280x800 "costs and
buys nothing" — that `--panel 1080p` stalled a boot for 3.3+ minutes and the
guest came up 1280x800 anyway. That was one boot in 2026-08 and it is **not
true**. Re-measured on the current QEMU and the same base image, reading
`wm size` back out of the guest:

| | boot | `wm size` reported |
|---|---|---|
| `--panel 800p` | 33.6 s | `1280x800` |
| `--panel 1080p` | 35.6 s | `1920x1080`, density 240 |
| `--panel 1440p` | 35.6 s | `2560x1440`, density 240 |

No stall, no fallback, exactly what was asked for. Two things inside the guest
say why it was always going to work:

* **there is no `video=` on the base's kernel command line.** The whole of it
  is `stack_depot_disable=on cgroup_disable=pressure root=/dev/ram0 noexec=off
  SRC=... DATA=vdb quiet loglevel=0 console=null vt.global_cursor_default=0
  SETUPWIZARD=0`. Nothing pins a mode.
* **the DRM connector already advertises the bigger modes.**
  `/sys/class/drm/card0-Virtual-1/modes` reads `1280x800, 5120x2160, 4096x2160,
  3840x2160, 1920x1440, 2560x1080, 1856x1392, 1792x1344, ...`

The panel was only ever `xres`/`yres` on the virtio-gpu device, which is ours
to set. **No base change is needed for high resolution.**

Bigger does cost frames — it is real fill — so `--panel 800p` is the way to buy
them back, and that is why the ceiling is 1080p rather than the largest mode
the connector will take.

#### And 1080p turned out to be FREE — measured properly, with the GUI out of the way

*2026-09-01, PS99, in-world, `--smp 8`, GPU, two 30 s `--timestats` samples each.*

⚠ **The autoexec script has to come out first, and this is the trap that
invalidated three earlier readings.** `%LOCALAPPDATA%\OmniExec\autoexec\`
carries `zaphub.lua`, which puts a full-screen opaque GUI over the game. With
it up, SurfaceFlinger reports a clean 60 fps at 6% of the guest's CPU — a
number that says nothing about this stack, because there is almost nothing
being drawn. Every fps figure in this file has to state whether the world was
actually visible; these were taken with `zaphub.lua` moved aside and confirmed
by screenshot.

| | frames / 30 s | fps | guest CPU (of 800%) |
|---|---|---|---|
| `--panel 800p` (1280x800) | 1407 / 1352 | 46.9 / 45.1 | 196% / 224% |
| `--panel 1080p` (1920x1080) | 1404 / 1421 | **46.7 / 47.2** | 185% / **135%** |
| 1080p + `blob=true,hostmem=1G` | 1349 / 1291 | 44.8 / 42.9 | 181% / 164% |

**2.25x the pixels for the same frame rate, and less guest CPU.** At these
sizes the guest is not fill-bound — the GPU absorbs the extra pixels and the
client does the same work either way — so the old "bigger costs frames"
warning does not apply between 800p and 1080p on a machine with a real GPU.
That is what makes 1080p a safe default rather than a trade. `--panel 800p` is
kept as the escape hatch for a host where it is NOT free (a weak iGPU), where
the same table would look different.

**`blob=true` is not a win and is NOT shipped.** Blob resources looked like the
obvious next lever once the guest stopped being CPU-bound. Measured
uncontrolled — with the ZapHub GUI up on the blob run and not on the control —
it read as +33%, which is exactly the placebo this section exists to warn
about. Controlled, it is 42.9-44.8 against 46.7-47.2: **slightly worse.**
`gpu_extra_opts` (`OMNI_GPU_OPTS` / config `qemu.gpu_opts`) is still the hatch
for trying it on another host.

**Where the remaining frames actually go is now an open question, and it is
not any of the usual suspects.** At 1080p the guest sits at 135-185% of the
800% it has (5.5 cores idle), SurfaceFlinger costs ~2%, the translator is
1.0-1.5x, and dropping to 800p changes nothing. So the ceiling is the host GL
path or Roblox's own frame pacing, and neither has been instrumented.



---

## Input latency: virtio, not USB

*Measured 2026-09-01, host to guest, on the guest's own `/dev/input` node.*

The guest was given `qemu-xhci` + `usb-kbd` + `usb-tablet` from the first
commit in this repo and nobody ever asked what that costs. **A USB HID device
is polled**: QEMU's `usb-hid` advertises a 10 ms interrupt endpoint interval,
so every click and every mouse move waits for the next poll window.

Method: 40 QMP `input-send-event` absolute-motion events, 4 ms apart, timed by
the guest's own `getevent -t` on the device that received them.

| | p50 | p90 | **max** | lost |
|---|---|---|---|---|
| `usb-tablet` | 3.84 ms | 5.31 ms | **9.53 ms** | 0 |
| `virtio-tablet-pci` | 4.01 ms | 6.02 ms | **6.20 ms** | 0 |

The medians are the same — at a 4 ms send rate the poll window averages out —
and **the tail is where it shows**: USB's worst case is the 10 ms polling
ceiling, virtio's is not. Jitter, not mean latency, is what "input lag" feels
like, so this is worth having; it is also a smaller win than it sounds and is
recorded as such.

`virtio-keyboard-pci` + `virtio-tablet-pci` are named FIRST on the command
line and **the USB pair stays attached behind them**. That ordering is the
whole safety story: `qemu_input_find_handler` (`ui/input.c`) walks its handler
list and takes the first whose mask covers the event, and handlers register in
command-line order. So if `virtio_input` ever fails to bind in some future
guest, the USB mouse and keyboard behind it are still real, enumerated and
working — the worst case of the faster device is the old behaviour, not an
instance nobody can click on (which `MODES["farming"]`'s `usb` note already
records the cost of).

Verified on hardware, not reasoned: with both attached, `input-send-event` at
x=5000/25000/12000 landed on `QEMU Virtio Tablet` (`/dev/input/event6`) with
those exact values and **nothing** reached the USB tablet.

The base carries the driver — `CONFIG_VIRTIO_INPUT=m`, `virtio_input.ko` under
`/system/lib/modules/6.1.112-gloria-xanmod1/` — and autoloads it when the
device appears, the same way it already does for `virtio_gpu` and `virtio_net`.
**No base change was needed.** `OMNI_INPUT=usb` / config `qemu.input` is the
way back.

---

## Which graphics card the HOST gives QEMU

*The most likely answer to "it is slow on a high-tier computer".*

Everything this project measured about the GPU — 3.2 fps in software against
16.5–58 with virgl — assumed that once QEMU has a GL context, the context is on
the good adapter. On a desktop with one card that is true. **On a laptop it is
not**, and a laptop is what most of the people this ships to are using.

Windows decides which adapter an application gets from a per-application **GPU
preference**, and the default for an unknown executable is "let Windows
decide", which in practice is the **power-saving** adapter. QEMU is an unknown
executable on every machine this installs onto: it is downloaded into
`%LOCALAPPDATA%`, it is on no vendor's optimisation list, and it renders
through ANGLE/WGL rather than through anything a driver profile recognises as
a game. So the guest gets composited on an iGPU while the discrete card sits
idle — and a "high-tier computer" is precisely the machine where the gap
between its two GPUs is widest.

`omnidroid/hostgpu.py` writes the same key the Settings app writes, under the
current user, before QEMU starts (the preference is read when the adapter is
enumerated):

    HKCU\Software\Microsoft\DirectX\UserGpuPreferences
        "<full path to qemu-system-x86_64.exe>" = "GpuPreference=2;"

**A preference the user set by hand is never overwritten** — the one
legitimate reason to pin QEMU to the integrated adapter is a laptop on battery
or a broken discrete driver, and someone who has been into
*Settings > Display > Graphics* has made that choice about this exact
executable. `OMNI_NO_GPU_PREF=1` turns the whole thing off.

### ...and the check that was missing entirely

`--gpu auto` decides what to ASK the host for. **Nothing ever confirmed the
guest actually came up on the GPU**, and when it does not the failure is
completely silent: QEMU starts, the window appears, the game runs, and every
frame is rasterised by llvmpipe. A user on that path is not "a bit slower",
they are on a different product, and they had no way to find out.

`report_guest_renderer()` reads SurfaceFlinger's own `GLES:` line after the
tuning step and says which renderer the guest got:

    [start acct] renderer: Mesa, virgl (NVIDIA GeForce RTX 4060/PCIe/SSE2),
                 OpenGL ES 3.2 Mesa 24.0.8  (GPU-accelerated)

...or, on the software path, a loud warning naming the two things that fix it.
"Could not ask" is reported as nothing at all, never as software: telling
somebody with a working GPU that they have none is worse than silence.

⚠ **Writing that warning is how `_harden_console_encoding()` got written.** One
`U+26A0` raised `UnicodeEncodeError` on a cp1252 console *inside* the `print`,
and the `try/except` that keeps a diagnostic from ever costing a boot swallowed
the entire warning. The message that exists to end a silent failure failed
silently. `main()` reconfigures stdout/stderr with `errors="replace"` now.

---

## The 60 fps ceiling, and the layer that was costing two thirds of the frames

*Measured 2026-09-02, PS99 in-world, 1080p, `--smp 8`, autoexec GUI removed,
30 s of `dumpsys SurfaceFlinger --timestats` per reading.*

"45 fps in the emulator when the same PC runs the same game at 170" is two
separate problems, and neither of them is the translator or the GPU.

### 1. Every frame was CLIENT composited, and one stray dialog caused it

```
totalFrames             1664        (51.8 fps)
clientCompositionFrames 1667        <- 100%
missedFrames            1209/1805   <- 67%
```

`clientCompositionFrames == totalFrames` means SurfaceFlinger did a
**full-screen GPU blend in the guest, through virgl, on every single frame**
instead of handing the game's buffer to the display controller.

The cause is one layer. Android shows **"Viewing full screen — to exit, swipe
down from the top"** the first time an app goes immersive and leaves it up
until somebody taps "Got it". Nobody ever does: farming has no hands on it,
and on gaming it looks like a harmless toast. It is not harmless — it is a
THIRD composited layer over the game's SurfaceView and the app's own window,
and this guest's hwcomposer (`drm_minigbm_celadon` on virtio-gpu) has **one
plane**. Three layers is one more than it can place, so the whole frame falls
back to client composition.

`settings put secure immersive_mode_confirmations confirmed` removes it
(that is the value the platform itself writes when a user taps "Got it"), and
it is in `consent.SETTINGS` now, so every boot gets it:

| | with the toast | without |
|---|---|---|
| clientCompositionFrames | 1667 (100%) | **0** |
| missedFrames | 1209 / 1805 (67%) | **8 / 1710 (0.5%)** |
| frames / 30 s | 1664 (51.8 fps) | 1710 (53.3 fps) |

**The frame rate barely moves and that is not the point.** Two thirds of
frames were missing their deadline and now essentially none are: the number
is the same and the judder is gone. It is also the difference between the GPU
doing one full-screen blend per frame and doing none.

### 2. The ceiling is 60, and it is GTK's frame clock

The guest is not capped by anything of Roblox's — `ClientAppSettings.json`
really does carry `DFIntTaskSchedulerTargetFps: 240`, read back off the live
instance — and the guest display is a genuine 144 Hz mode:

```
displayModes = {id=0, resolution=1920x1080, refreshRate=144.00 Hz}
VSYNC period: 6944411 ns
```

...yet a cheap scene renders at **exactly 60.0 fps** (1805 frames / 30.065 s)
and the layer's own present timestamps say why:

```
dumpsys SurfaceFlinger --latency <game layer>
  refresh period reported   6.94 ms   (144 Hz)
  present -> present  p50  16.51 ms   (60.6 fps)   min 2.09   p90 26.62
```

SurfaceFlinger believes it is on 144 Hz and is being handed frames at 60. The
throttle is below it, and it is **not** in this repo and not in QEMU's virtio
path: `virtio_gpu_fence_poll` runs on a 10 ms timer, and
`gd_gl_area_scanout_flush` (`ui/gtk-gl-area.c:182`) just calls
`gtk_gl_area_queue_render()`. GTK then renders on **GDK3's frame clock, which
is a hardcoded 16667 µs — 60 Hz — on Windows** regardless of the monitor.
The host's primary display here is 144 Hz and it makes no difference.

**So the presentation cadence is 60 Hz, and no setting in this project can
raise it.** Getting past it needs a display backend that is not GTK:

* `-display sdl,gl=on` is the candidate. Our own QEMU is **not built with
  SDL** (`-display help` lists gtk/egl-headless/curses/dbus only) — the
  msys2 build environment had no SDL2 dev package. The stock 11.0.50 bundle
  DOES have it, and was tried: the guest never reached adbd in 10 minutes,
  the same "GL window that never scans out" shape as `egl-headless` on
  Windows. So SDL is a build-AND-debug task, not a config flip, and it would
  cost the four `ui/gtk.c` patches (window identity, aspect lock, panel pin,
  close prompt) which are GTK-only.
* Anything else means patching how QEMU drives GTK, and the clock being
  hardcoded is inside GDK, not QEMU.

Worth knowing before anyone spends a week on it: at 1080p in-world the guest
is at **135-185% of the 800% it has** (5.5 cores idle), SurfaceFlinger is
~2%, the translator is 1.0-1.5x, and 800p renders at the same frame rate as
1080p. Nothing downstream of the cadence is saturated. The 60 Hz cap is the
whole remaining story on this host.

---

## The translator is not the wall

*Measured 2026-09-01 on the x86 base, inside a live guest.*

Every performance note in this file that could not explain itself has reached
for `libndk_translation`, and the belief hardened into "the guest is CPU-bound
on arm64 translation and no flag removes it". **It was never measured**, and it
is mostly wrong.

### What the base actually ships

```
ro.dalvik.vm.native.bridge   libndk_translation.so
ro.ndk_translation.version   0.2.3
ro.dalvik.vm.isa.arm64       x86_64
/system/lib64/libndk_translation.so          2.5 MB, 2024-10-12
/system/lib64/arm64/                         the guest-side arm64 system libs
/system/etc/binfmt_misc/{arm,arm64}_{exe,dyn}
```

Google's ndk_translation, not Intel's Houdini.

### How it was measured

There is no arm64 benchmark on this machine and no NDK to build one, so the
benchmark is four hand-assembled **static ELFs** — two aarch64, two x86-64 —
emitted byte by byte from Python (`tools/bench/mkbench.py`): a 200,000,000-
iteration loop over a register dependency chain, then `exit(0)`. No libc, no
linker, no allocator, nothing but the instructions under test. The arm64 pair
runs through the native bridge's own program runner
(`/system/bin/ndk_translation_program_runner_binfmt_misc_arm64`), which needs
`binfmt_misc` mounted and the base's own handler registered:

```
mount -t binfmt_misc none /proc/sys/fs/binfmt_misc
cat /system/etc/binfmt_misc/arm64_exe > /proc/sys/fs/binfmt_misc/register
```

⚠ The runner rejects a hand-made ELF with `has invalid e_shstrndx` unless it
carries a real section-header table — a NULL section, `.text` and `.shstrtab`
with `e_shstrndx` pointing at the last. A program header alone is not enough.

Each figure below is the loop's wall time **minus** the same binary built with
a single iteration, so the native bridge's ~70 ms of process startup is out of
the number.

### The result

| workload | x86-64 native | arm64 translated | tax |
|---|---|---|---|
| integer chain (`mul`/`add`/`eor`) | 221 ms | 251 ms | **1.14x** |
| SIMD chain (`fmul`/`fadd` on 2x double) | 476 ms | 466 ms | **0.98x** |
| indirect call through a register | 173 ms | 266 ms | **1.54x** |

**A translator that runs vector code at native speed and a dependency chain at
1.14x is not a 3x tax on anything.** The one place it does lose is the indirect
call — the classic weak spot of binary translation, since every `blr` has to
resolve a translated target — and even that is 1.5x, not 10x.

For scale, the x86-64 integer figure is 200M iterations of a 5-cycle chain in
221 ms, i.e. an effective **4.3 GHz** — so the WHPX guest is also running x86
code at the host's real clock, which is worth knowing on its own.

### What this means for the "use a modern translator" question

Swapping ndk_translation 0.2.3 for a newer translator — Google's **Berberis**
(present in the android-36 emulator system images alongside ndk_translation) or
Intel's **Houdini** — is a real option and the images are already on this disk,
but it is a **base transplant with a version-matching hazard**, not a drop-in:
`/system/lib64/arm64/` is a full set of arm64 builds of *this Android's* system
libraries (libc, libandroid_runtime, ...), and they have to match the framework
they call into. Android 13 base, Android 16 translator.

Against a measured ceiling of ~14% on integer code and ~35% on indirect calls,
that is not where the next win is. **The measured wins were elsewhere** — the
vCPU ceiling (starved at 4 of 4), the panel (capped for no reason), the host
GPU preference, and, for farming, the commit charge. Revisit the translator
when something measures it as the binding constraint.

### And a caution about measuring Roblox at all

PS99 fps readings on this stack are close to useless for A/B. Two 30-second
samples of the same instance, same account, same place, minutes apart:

```
1710 frames / 30.1 s = 56.8 fps     guest 138% of 400%
1047 frames / 30.1 s = 34.8 fps     guest 220% of 400%
```

It is a busy server-authoritative place and how much is streaming in when the
sample is taken moves the number further than any change in this repo does.

**And the biggest single confounder is ours**: the executor's `autoexec/zaphub.lua` puts a full-screen opaque GUI over the game, and with it
up SurfaceFlinger reports a flat 60 fps at ~6% of the guest's CPU no matter
what the stack underneath is doing. Three readings in this session were that
GUI. **Move the autoexec scripts aside before benchmarking and confirm the
world is on screen with a screenshot** — `--timestats` on a client that is
merely "in world" is not enough.

---

## The GPU

One setting, `--gpu` (config `qemu.gpu`, env `OMNI_GPU`), four values:

| | |
|---|---|
| `auto` | **default.** Reach the GPU whatever it takes. If that needs a window, open it HIDDEN and restyle it in place (`omnidroid view`). |
| `headless` | Never a window. Keeps the VNC viewer. GPU only if it can be had windowless. |
| `window` | Always open a native QEMU window. |
| `off` | Software rendering, headless. |

Two host facts decide what `auto` actually does, and both are measured rather
than assumed:

**1. QEMU refuses a VNC server beside a GL WINDOW.**

```
qemu: -vnc 127.0.0.1:12101: Display vnc is incompatible with the GL context
```

Re-verified on QEMU 11.0.50 across `gtk`/`sdl` × `gl=on`/`gl=es`/`gl=core` —
all four refuse. It does **not** refuse `egl-headless`, which QEMU documents as
the display to pair with VNC. Conflating those two cases is what left every
GPU-accelerated boot with no VNC server at all, which then got reported as "the
viewer is black".

**2. Whether `egl-headless` can actually PRESENT is per-platform.**

On Linux it can; that is what the backend was written for. On Windows the guest
renders on the GPU and never scans out. Measured 2026-08-15 across three boots
(plain, `blob=true,hostmem=512M`, and without the forced `video=` mode), all
identical:

```
dmesg      [drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1203 (command 0x103)
timestats  totalFrames = 0
screencap  solid black
VNC        1 update, mean brightness 0.0
```

`0x103` is `SET_SCANOUT`, `0x1203` is `ERR_INVALID_RESOURCE_ID`. The guest's GL
was fine — SurfaceFlinger came up on `virgl (ANGLE (NVIDIA … RTX 4060))` with no
GL errors in logcat — so this is presentation, not rendering.

**So on Windows a GPU-accelerated instance has a window and no VNC server.**
It is not hidden behind a viewer of ours — it IS the window you look at, we
just take away QEMU's chrome and put a strip of our own above it:

### QEMU's own window, restyled, with a bar of ours OWNING it

The design this replaced (`SetParent`) made QEMU's window a *child* of our Tk
viewer, so the guest lived *inside* the product's window. That is gone. What
runs now:

| | |
|---|---|
| the window is **hidden** at spawn | it exists only to hold the GL context |
| a hidden window **keeps rendering** | reconfirmed 2026-08-16: no flash across a full boot, sampled every 4s |
| `omnidroid view` **restyles it in place** | `hostwin.apply_chrome`: caption/sysmenu/min/max stripped, `WS_THICKFRAME` kept, so it still resizes but shows none of QEMU's own chrome |
| a thin bar of ours is spawned | a separate process (`windowbar.py`), its window made an OWNER of QEMU's window via `GWLP_HWNDPARENT` — never the reverse |

QEMU's window stays top-level for its whole life; only its *style bits* change
(caption/sysmenu/minimize/maximize cleared, `WS_THICKFRAME` kept — confirmed
2026-08-16 by reading the live style word off a running instance: `0x16040000`
= `WS_VISIBLE|WS_CLIPSIBLINGS|WS_CLIPCHILDREN|WS_THICKFRAME`, no `WS_CAPTION`,
no `WS_SYSMENU`). The bar is a real top-level window with its own caption
(style `0x16CA0008` includes `WS_CAPTION|WS_SYSMENU`) that Windows floats
above its owner automatically and minimises/restores with it — no polling, no
z-order management of our own. Nothing is copied, encoded or decoded per
frame (there was never a framebuffer in this design, restyled or not), and
input goes into the guest's `usb-tablet`/`usb-kbd` directly.

**THE OWNERSHIP DIRECTION IS THE WHOLE POINT.** The old design's one hazard
was that Windows destroys a *child* window when its parent dies, so a viewer
that was FORCE-killed took QEMU's window with it — instance alive, answering
adb, rendering nothing (`totalFrames = 0`, measured under that design). Making
the bar an OWNED window instead of an owner inverts the failure: destroying an
owned window does nothing to its owner. **Reconfirmed on hardware 2026-08-16:**
`taskkill /F /PID <bar pid>` mid-render, then re-measured —

```
before the kill   totalFrames = 3305   over 35s   (~94 fps)
taskkill /F /PID <bar>
after the kill    totalFrames = 3464   over 34s   (~102 fps)
```

— frame production did not even dip. (The 94/102 fps here are idle
Android/BlissOS setup-wizard compositing — this pass could not reach PS99 on
this box, see below — not a gameplay number and not comparable to the
24.2–58 fps PS99 band further down.) This is the check the whole redesign
exists for, and it holds.

**Known issue, found on this same hardware pass:** the bar's *size* does not
match `windowbar.bar_geometry()`'s intent. Position lands exactly where
computed; on this box the bar instead came up ~216×239 px (Tk's own default
toplevel size) rather than the intended "exactly as wide as the guest window,
34 px tall" — overlapping the guest's top-left corner rather than sitting
flush above it. Ownership, ownership-triggered kill-safety, hide, and the
close dialog's three buttons all still work correctly; only the strip's shape
was wrong. Suspected cause at the time: `root.resizable(False, False)` runs
before `bar.follow()`'s raw `SetWindowPos`, so a later `WM_GETMINMAXINFO` may
clamp the window back to Tk's own requested size. **That suspicion was
wrong — see the correction immediately below.**

### CORRECTION, 2026-08-16: it was a Tk min/max lock, not the caption arithmetic

Fixed and hardware-verified in `4daa97e`, the very next commit after the
hardware pass above. The suspected cause — a race between the caption
arithmetic and `follow()`'s `SetWindowPos` — was never it. On Windows, Tk's
`wm resizable(False, False)` does two things, not one: it strips
`WS_THICKFRAME`/`WS_MAXIMIZEBOX` (wanted), and it *also* locks the window's
`WM_GETMINMAXINFO` min/max track size to whatever Tk's own "natural" client
size happens to be at that instant — for a bare toplevel with no child
widgets yet, that is Tk's built-in ~200×200 default, nothing to do with the
guest window's real rect (not known yet at that point in the function). The
lock is not a one-time race to lose, either: **Windows re-enforces it on
every later `SetWindowPos`**, including the one `follow()` makes once the
real rect is known — which is exactly the measured 216×239 clamp.

The fix never calls `resizable()` at all. `_strip_resize_border()` removes
`WS_THICKFRAME`/`WS_MAXIMIZEBOX` by hand via `GetWindowLongPtrW`/
`SetWindowLongPtrW`/`SetWindowPos(..., SWP_FRAMECHANGED)` — the same
technique `hostwin.apply_chrome` already uses on QEMU's own window — so no
lock is ever installed. `follow()` additionally reads back the height
Windows actually granted: a `WS_CAPTION` window has a system-enforced
minimum caption height (measured 40 px on this box, against `BAR_HEIGHT`'s
nominal 34), so it repositions — never resizes — so the bar's bottom edge
lands exactly on the guest's top edge no matter what floor a given
machine's DPI/theme enforces.

Hardware-verified the same night: bar `(208,168)-(864,208)` against guest
`(208,208)-(864,752)` — full width, zero overlap, zero gap. Kill-safety was
re-verified against the new code too: SurfaceFlinger `totalFrames` went
1952 → 2059 across a force-kill of the bar's process, guest untouched. Full
diagnosis, rejected alternatives (`root.geometry()`, `minsize()`/
`maxsize()`, reordering the calls, `overrideredirect(True)`), and the new
test coverage are in the "Geometry fix" section of
`.superpowers/sdd/2026-08-15-gaming-gpu-window/task-9-report.md`.

GTK re-shows the window during early boot, so `hostwin.keep_hidden()` re-hides
it for the length of a boot and then stops; after that a single hide sticks.
Re-verified 2026-08-16 across a full ~117 s cold boot (screenshots sampled
every 4 s): the window never appeared on screen.

`--gpu window` is the one setting that leaves it on screen, for when a GL
problem has to be seen with none of this project's code in the path.

**Hiding from the app (`omnidroid view <name> --hide`) is a different path
from the bar's own X**, and it has to be: Windows only cascades a *destroy* to
an owned window, not a bare `ShowWindow(SW_HIDE)` on the owner — an untouched
bar would be left on screen captioning nothing. So `--hide` persists the
window's geometry, kills the bar itself, clears its pid file, *then* hides.
Reconfirmed on hardware 2026-08-16 with the bar open: after `--hide`, the bar
process was gone, its window handle invalid, and the screen showed neither the
bar nor the guest — no orphan, exactly as designed.

| | fps at 1280x800 on PS99 | what you watch |
|---|---|---|
| `--gpu auto` (default) | **24.2–58** across runs | QEMU's own restyled window, our bar above it |
| `--gpu headless` | 3.2 | our viewer, over VNC |

That PS99 band predates this redesign but still applies: only the window's
*ownership and chrome* changed here, not the render path
(`gtk,gl=on`+`virtio-gpu-gl-pci`), so the same GPU throughput is expected. The
2026-08-16 hardware pass above could not reach PS99 itself on this box (the
saved account's cookie had been server-side invalidated — HTTP 401 — and a
plain, non-baked Roblox APK installed for the run doesn't pair with the kiosk's
session receiver), so its 94–102 fps figures are idle Android/BlissOS
setup-wizard compositing, not gameplay — real GPU-accelerated frames, useful
to prove the mechanism, but not a PS99 number and not to be read as one.

The GPU spread is real rather than noise in the method: PS99 is a busy
server-authoritative place and how much is streaming in when the sample is
taken moves it a long way. The ratio to software does not move.

On a host whose `egl-headless` presents — Linux — none of this is needed:
`auto` gets GPU, no window and VNC all at once, and a viewer connects the
ordinary way. macOS has no virgl yet, so it renders in software and also uses
VNC. **Only Windows gaming shows QEMU's own window; Linux and macOS still
connect a viewer over VNC to a windowless guest.**

---

### Open follow-ups on the window bar

Found by the final review of the window work (2026-08-16) and deliberately
NOT fixed there. None blocks use; they are listed in the order worth doing.

* **The bar's top-of-screen clamp is measured against the PRIMARY monitor.**
  `bar_geometry` clamps to `y=0`, so a guest window on a monitor *above* the
  primary pins the bar to the primary's top edge — and the `<Configure>` that
  generates makes `drag_owner_to_bar` teleport the whole composite down onto
  the primary. It triggers on a resize up there, and on `view` restoring a
  remembered negative `y`, which normal use persists. The right clamp is the
  guest's own monitor work area (`MonitorFromWindow` + `GetMonitorInfo`), not
  absolute zero.
* **`drag_owner_to_bar` predicts the owner's new rect instead of reading it
  back**, which is the rule `follow()` documents ten lines away. Traced to
  self-correct within one poll cycle — except in the clamp case above, which
  is why that one goes first.
* **The bar drag was verified with `SetWindowPos`, never with a physical
  mouse.** Whether Tk delivers `<Configure>` *during* Windows' modal move
  loop is reasoned, not measured. Worst case is cosmetic: the guest snaps to
  the bar on release rather than tracking during the drag. Five minutes for
  whoever next has the machine and a mouse.
* **`attach_follow` has no test**, though it returns its tick function so a
  test can step the poll by hand. The bindings, the reschedule and the
  owner-gone teardown are the riskiest new code.
* **`--gpu window` can be hidden but not re-shown.** `--hide` succeeds on such
  a boot; `view` then refuses it, saying the window is already on screen —
  which is false in exactly the state `--hide` just created. Recovery is
  `stop`/`start`. Debug hatch only.
* **The spec's window ICON is not implemented.** `hostwin._apply_icon` is
  correct, seam-clean and tested, and has no caller, because there is no
  `.ico` anywhere in this tree — the only icon in the product family is
  `omni-executor/packaging/icon.icns`, which is macOS-only and unreadable by
  `LoadImageW`. The DWM half of the chrome (dark caption, border colour,
  rounded corners) IS applied and verified. Ship an `.ico` and pass it.
* **`run.json`'s atomic write uses a fixed temp name**, so two concurrent
  writers could interleave and publish garbage. Narrow, and strictly better
  than the overwrite it replaced.


## Farming

**It reaches the PS99 world.** Measured 2026-08-16, screenshot-verified
in-world (Roblox's top bar, PS99's live player leaderboard, its chat and its
own teleport logic) with the squeeze already applied — display override
480x270, zram on, no balloon:

```
start <acct> --place 8737899170 --mode farming --mem 3072
  boot 155 s;  game PSS 2.1 GB / RSS 1.5 GB
  guest 2.9 GB, 587 MB available, 341 MB swap free
  host RSS 3239 MB
```

Two things that had to be true first, and neither is a knob in this table:

* **The squeeze runs AFTER the client has loaded**, not in the boot tail. See
  the correction below.
* **The guest's MTU has to fit the host's egress.** Behind a VPN it does not
  by default, and Roblox's gameplay traffic is UDP, so the client connects and
  then dies. `virtio-net-pci,host_mtu` — see `omnidroid/netmtu.py`.

And one number that is a property of the GAME rather than of this mode:
**PS99 needs `--mem 3072`.** At the mode's own 2048 the client is OOM-killed.

### CORRECTION, 2026-09-01: guest RAM is a FILE now, and commit stops mattering

Everything below this line about Windows is still true of a **stock** QEMU and
is no longer true of the one the product ships. The binary carries
`qemu-patches/0007-omni-win32-ram-file` and `0008-omni-win32-punch-hole`, and
as of today the engine actually turns them on: `ram_file_env()` sets
`QEMU_RAM_FILE_DIR` for the density profile, so every guest RAM block comes
from a **mapped sparse file in the scratch** instead of from private,
committed memory.

MEASURED on a live in-world PS99 farming instance, `-m 3072`, `-accel whpx`:

| | |
|---|---|
| system commit with no instances | 30787 MB |
| ...with one farming instance | 31858 MB — **+1071 MB** |
| the same, before the patch (2026-08-17) | **+4065 MB** |
| QEMU private bytes | 979–1003 MB |
| the RAM file | 3072 MB logical, **0 MB allocated** (`fsutil file queryallocatedranges`) |

`doctor`'s commit rung goes from **5 instances to 17**, and the binding wall on
this box moves to **disk**. The whole `-m` term moved from commit to the
scratch volume, so `scratch_room` budgets it too — at the WORST case (`-m`),
and it **refuses** a launch rather than warning, because running that volume
out no longer fails the launch: a mapped view that cannot fault in is
`STATUS_IN_PAGE_ERROR`, and it kills whichever already-farming guest touches a
cold page next.

`free-page-reporting=on` is back on Windows for the same reason and by the same
gate — `qemu_supports_punch_hole()`, the binary's own `--version` suffix, not
the platform. It was dropped because the SHIPPED QEMU logged 925 failed
discards a minute and reclaimed nothing; that was a property of the build.

The three levers now stack: the working-set ceiling holds host RSS at ~384 MB,
the job-object cap holds CPU at 50% of a core, and the RAM file takes `-m` off
the commit limit.

---

The stated target is ~400 MB per instance. **On Windows that is not reachable,
and the reason is the host side rather than the guest.** Measured on PS99,
2026-08-15:

| | |
|---|---|
| guest MemTotal after balloon | 1450 MB (balloon reported `capped at 897 MB`) |
| Roblox PSS / RSS in-guest | 508 MB / 743 MB |
| **host RSS for the QEMU process** | **2198 MB** |

The balloon works — the guest really does give the pages back — but **QEMU on
Windows has no `madvise`**, so `ram_block_discard_range` fails and the host
never gets them. free-page-reporting is dropped there for the same reason (it
logged ~925 failed discards per minute and reclaimed nothing). So on Windows
the host pays the full `-m` plus overhead, whatever the guest does.

Two consequences:

* **The lever that works on Windows is `-m` itself**, not the balloon:
  `--mem 1536` costs the host ~1.6 GB where 2048 costs ~2.2 GB. The floor is
  set by the game (~740 MB RSS) plus a squeezed Android.

  **So as of 2026-08-16 the balloon is not inflated at all on such a host.**
  It was never a saving there and it is a real cost to the guest: at the
  896 MB cap the session handover itself timed out (`pm path` did not answer
  in 45 s, twice) and the client could not load PS99. `apply_balloon_target`
  now says so and skips; `host_can_reclaim_balloon()` is the predicate, an
  explicit `--balloon` still wins outright, and `OMNI_FORCE_BALLOON=1` puts
  it back for measurement.
* **The ~400 MB story is a Linux story.** There the balloon and
  free-page-reporting decommit for real and KSM dedups identical pages across
  instances, so per-instance cost tracks the 896 MB cap and falls further
  across a fleet. `FOOTPRINT.md` has the full picture.

### The memory governor, and why capping at BOOT does not rescue Windows either

The obvious next idea, once reclaim is known not to work, is **prevention**:
never let the guest touch the pages in the first place. QEMU's guest RAM is
committed at spawn but resident only on first touch, and an uncapped guest
climbs from 38 MB of host RSS at t+10s to the full `-m` by t+29s — Android
filling every page it is offered with page cache while the client is still on
its loading screen. Set the balloon target at spawn, before the guest's
virtio-balloon driver has probed, and the driver inflates on arrival.

That is what `omnidroid/balloon.py` implements: a **boot cap** applied inside
`spawn_qemu`, plus a **governor** (`omnidroid govern <name>`, started
automatically by `start`) that then tracks real demand — growing the instant
the guest's free slack falls below 256 MB, shrinking only in steps, only once
usage has plateaued, and never below the mode's floor.

**It was measured on Windows and it does not work there.** PS99, `-m 3072`,
cap 1536, against an uncapped control:

| | uncapped | boot cap 1536 |
|---|---|---|
| host RSS at rest | 3403–3414 MB | **3335–3345 MB** |
| guest cap / actually using | 3072 MB / — | 1536 MB / 862 MB |
| boot | **0.3 min** | 1.4 min |
| `qemu.log` | ~0 | **31 MB** |

~60 MB saved for a 4x slower boot and 31 MB of log. The cause is that the host
pays for the **union of pages ever touched**, and the balloon descends at only
~25 MB/s — one failed `ram_block_discard_range` and one log line per 4 KB
page. The descent is therefore still running while Android boots, so the guest
is handed different physical pages each time and touches nearly all of `-m`
anyway. **Ballooning during boot increases page-set churn rather than
preventing the fill.**

A first cut made this worse in an instructive way. With the grow rule written
as "want > cap" alone, a healthy guest moved its own cap every poll
(1536 → 1543 → 1585 MB observed) and then oscillated forever. On Windows that
oscillation *ratchets*: every grow lets the guest touch pages it had given
back, and a touched page is never released. The oscillating run ended at
**3412 MB — indistinguishable from the uncapped control.** `GROW_TRIGGER_MB`
and the band around it exist for this, and `tests/test_balloon_governor.py`
has the regression tests.

So the governor is gated on the same `host_can_reclaim_balloon()` predicate as
the reclaim inflate: **on Linux/macOS it runs, on Windows it does not.**
`OMNI_FORCE_GOVERNOR=1` turns it back on for measuring the
`docs/windows-ram-discard.md` patch, which fixes both halves at once — the
missing `madvise` is what makes the descent slow *and* makes reclaim
impossible.

The user's "~400 MB in the desktop Roblox app" is the closest comparison to the
**game process** (508 MB PSS here), not to an instance: an instance is that
game *plus a whole Android* plus QEMU.

### Two farming bugs fixed on 2026-08-15

* **`smp 1` could not get x86 through the session handover.** Roblox's arm64
  build runs through `libndk_translation` on the x86 base, and the ordered
  `am broadcast` that hands over the session did not return within 45 s — which
  raised `TimeoutExpired` straight out of `cmd_start` as a traceback. Farming
  took `smp_x86: 2` for that, `kiosk_broadcast` treats a timeout as a RESULT
  rather than an exception, and its budget is 120 s. **It is `smp_x86: 3`
  now** — reaching the world was not the only bar; see "The executor needs a
  vCPU of its own" below.
* **The balloon was reported as broken when it was merely slow.** At the 30 s
  mark a 2048→896 MB inflation read 1805 MB and the launch printed "guest
  balloon driver missing?"; the same guest read 938 MB a minute later and
  reached its target. The wait is 90 s now, and a balloon that is still moving
  says so instead of blaming the guest kernel.

### Farming on x86: the translator cannot be swapped out

A farming instance used to boot, join PS99 and then sit on the Roblox splash
forever. Bisected on 2026-08-15 by running each suspect:

| suspect | test | result |
|---|---|---|
| the 5 fps tick cap | `--quality balanced` | not it |
| the 480x270 display | `--guest-display native` | not it |
| the package trim | read the list | not it (no WebView; game is in `KEEP_ALWAYS`) |
| doze | read the sequence | not it (game whitelisted before `force-idle`) |
| memory | `--mem 4096 --balloon 3072` | not it — **and it revealed the answer** |

With 2.1 GB free and no OOM kill, Roblox still died. `logcat -b crash`:

```
F libc  : Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid 5013 (Thread-19)
F DEBUG : Abort message: 'Cannot process signal 11'
F DEBUG : #04 libndk_translation.so (ndk_translation::HandleHostSignal(...))
```

**That is the TRANSLATOR aborting, not the game.** Roblox ships arm64 only, so
the x86 base runs it through `libndk_translation`; translated code took a
SIGSEGV, and the translator's host-signal handler could not process a fault
arriving in translated context.

Farming is the only mode that swaps hard — `swappiness 100`, `page-cluster 0`,
zram on — and evicting translated code pages is exactly how that fault is
manufactured. Gaming runs at swappiness 10 with no zram and has never crashed
this way.

**The fix is an arch override, not a retreat.** `MODES["farming"]` carries
`swappiness_x86: 10` and `zram_x86: False`; the arm base runs Roblox
*natively*, has no translator to upset, and keeps both levers. Measured with
just the swappiness half in place: **translator aborts 1 → 0**, and the client
got past the black splash to Roblox's loading screen.

**Two traps inside that fix:**

* **Skipping the zram step is not the same as zram being off.** The base ships
  it ON — `persist.sys.zram_enabled` is baked into build.prop and
  `/vendor/etc/init/zram.rc` calls `swapon_all` at boot. A launch that had just
  printed "zram: OFF for this mode" still had `SwapTotal: 1045168 kB`. x86
  farming now issues an explicit `swapoff`.
* **The balloon cap follows zram, and it should.** `apply_balloon_target`
  probes the guest rather than trusting the mode, so with zram genuinely off it
  selects the non-zram floor (1536 MB) instead of 896 — which is correct, and
  is the honest cost of not being able to swap on this base.

**CORRECTION, measured the same night: keep zram, only lower swappiness.**
Turning zram off as well made things worse, not safer:

| | translator aborts | guest | outcome |
|---|---|---|---|
| swappiness 10, zram ON | 0 | 830 MB / 315 MB free | reached Roblox's loading screen |
| swappiness 10, zram OFF | 0 | 1485 MB / 648 MB free | **Roblox OOM-killed 3x** (`mem-pressure-event`) |

zram is not what breaks the translator — swapping HARD is — and with lz4
compressing ~3x it is the only reason a 2 GB guest holds this game at all. So
`swappiness_x86: 10` stays and `zram_x86` is gone; the explicit `swapoff` went
with it.

### CORRECTION, 2026-08-16: it was WHEN the squeeze ran, not what it did

The section above is right about the crash and wrong about the cause, and the
difference matters because the fix is different. Re-measured on PS99, holding
memory constant at 3072 MB with the balloon off so it could not be the
variable:

| tuning | guest | what the client did |
|---|---|---|
| farming, full squeeze | 1.8 GB free | alive, PSS FLAT at ~400 MB, engine parked in `futex_wait`, **guest 200% idle** for 6 min |
| farming, `--quality balanced` (tick 240) | 1.8 GB free | identical stall — the 5 fps tick is not it |
| farming, `OMNI_FARM_SKIP=<every step>` | 1.8 GB free | **PSS 1173 MB at 111 s and climbing — it loads the place** |
| **gaming at the same 2048 MB / 2 vCPU** (control) | — | zero translator aborts, PSS to 1476 MB, then OOM-killed |

Two things fall out of that, and both contradict what was believed:

* **Swapping hard does not break the translator.** The gaming control drove
  its zram to `SwapFree: 0.2 MB` with zero aborts.
* **The engine does not crawl, it deadlocks.** `debuggerd -j` on a stalled
  client: Roblox's `Main` thread and its single ` RBX Worker A` both in
  `futex_wait` (syscall 202, NULL timeout), guest 200% idle. Nothing is going
  to wake them.

**The squeeze was running in the boot tail — before `cmd_start` delivers the
session, i.e. before the client has been told which place to load.** Every
lever in it exists to make a JOINED, IDLE instance cheap; applied to a client
that is still starting, they starve the thing they are supposed to shrink.

So the squeeze, zram and the balloon now run in `settle_density_instance()`,
which `cmd_start` calls AFTER the session is delivered and after the client's
memory has stopped growing (`wait_for_game_settled` — PSS plateau above a
700 MB floor, so a splash screen never counts as settled). `OMNI_SETTLE_TIMEOUT`
/ config `qemu.settle_timeout` bounds the wait; 0 squeezes immediately.

### AMENDMENT, 2026-08-17: the PANEL goes back to the boot tail

"After the client has loaded" is right for every lever that spends quality to
buy memory. It is exactly **wrong** for the one lever that is not a memory
lever at all — `wm size` / `wm density`.

Roblox's activity is `RESIZE_MODE_UNRESIZEABLE`. A display change handed to it
while it is running is a configuration change it cannot follow, so Android
puts it in **size compat mode**: it keeps rendering at the panel it launched
with, gets scaled down and letterboxed, and SystemUI parks **"Tap to restart
this app for a better view."** over the game. Measured on a live farming
instance, in-world (`dumpsys activity activities`):

```
resizeMode=RESIZE_MODE_UNRESIZEABLE
mSizeCompatScale=0.5584416   mSizeCompatBounds=Rect(62, 0 - 419, 258)
areBoundsLetterboxed=true    letterboxReason=SIZE_COMPAT_MODE
```

— a client that launched at 640x480 still rendering 640x462 and being squashed
into 357x258 of a 480x270 screen. Confirmed by **intervention**, not
inference: on a live *gaming* instance with the game up and no prompt on
screen, one `wm size 480x270` + `wm density 80` put the same prompt up within
20 s, same pid, no relaunch.

So `farming.build_display_sequence` is now its own call, applied by
`apply_farming_display` in the density branch of `_ensure_booted` — i.e.
**before the session is delivered**, while there is no client to disturb. This
is the slot gaming has always used for its own `wm size reset`; farming was
the odd one out. The client comes up at the final panel, never size-compats,
and renders 480x270 instead of 640x462 — 39% fewer pixels, for free.
`OMNI_FARM_SKIP=display` still means "do not touch the panel".

### The executor needs a vCPU of its own (`smp_x86` 2 → 3)

A farming instance could join, farm, and look perfect while the **in-guest
executor never loaded** — no OMNI-EXEC menu, no auto-exec, ever. Gaming was
always fine.

The chain is invisible from outside: it writes nothing to logcat and nothing
to Roblox's client log, so "the executor did not load" and "the executor is
not installed" look identical. What made it legible was packet-capturing it
**inside the guest** (`adb shell tcpdump -i eth0 host <exec server>`, root is
available on the x86 base). The chain is 11 font fetches → `Costumers/arceus.lua`
→ `/gist` (the menu) → `/omni/exec/claim` → a 1 Hz poll.

MEASURED 2026-08-17, PS99, x86, same account/place/offset:

| farming `smp_x86` | what the capture shows |
| --- | --- |
| **2** | 3 fonts in 11 s, then **nothing for the rest of the session**. No `arceus.lua`, no `/gist`, no menu. |
| **3** | all 11 fonts, `arceus.lua`, `/gist`, claim, polling. Menu on screen. |
| **4** | same, ~5 s sooner — not worth a fourth vCPU across a fleet. |

The stall is ~85 s **before** the squeeze runs, so the squeeze is not what does
it; the executor's startup simply loses its race with the place load on two
translated vCPUs. `--quality balanced` does not help (tested), so it is not the
5 fps tick either.

**What the third vCPU costs: almost nothing at steady state.**
`cpu_ceiling_pct: 50` caps the whole QEMU *process* at half a core once the
client has loaded, and that cap is per-process, not per-vCPU. The extra vCPU is
spent where the starvation was — startup — and is idle afterwards, which is
where a farming fleet lives. The arm base runs Roblox natively and keeps
`smp 1`.

**A density launch is therefore MINUTES rather than seconds**, and that is the
honest reading of "this instance is ready" — `timings.stages.density_settled`
reports it.

`OMNI_FARM_SKIP=<step,...>` (see `farming.STEP_NAMES`) leaves named steps out,
because this sequence has now twice been what stopped Roblox running on the
x86 base and bisecting it by editing `farming.py` makes every attempt a
different build.

---

## What a performance boot does inside the guest

Applied after boot (`gaming.build_tuning_sequence`), and note that most of it
is an **undo**: the farming levers persist in `/data`, so an offset whose /data
was last touched by a farming boot keeps a 480x270 display until something
reverses it.

- `wm size` / `wm density` to the panel (density scales with it — see
  `gaming.density_for_panel`, so a 1080p panel does not shrink the UI)
- all three animation scales to 0
- `swappiness` 10 — keep the game's pages resident (root only)
- `deviceidle disable` + game whitelisted — no throttling of a foreground game
- IME re-enabled — farming disables it, and nothing can be typed without it
- the `high` ClientAppSettings profile (240 fps tick, quality 10, post-FX on)

Then, **after** the session is delivered (the broadcast is what launches the
game), `pin_game_to_top_app` moves it onto the `top-app` cpuset.

---

## Boot time

### The warm POOL — the fast path that does work on Windows

**As of 2026-09-01 the app warms it by itself.** The pool has worked since
2026-08-15 and nothing in the executor ever started one: `startPool` was
exposed on the engine hook and no component called it, so every launch
cold-booted — which is the "it takes too long to start" complaint, exactly.
`Api._autowarm_after_launch` records what the launch just used and the
HEARTBEAT warms one slot for it, gated on the engine advertising `pool`, on
the user not having turned it off (`pool_set_auto_warm`), and on the host
still having 3 GB of RAM free with the slot counted — a pool that pushes the
machine into its pagefile makes the instance being PLAYED slower to make the
next launch faster. Re-verified end to end today: `boot` 0.078 s, whole launch
1.66 s, against 33-39 s cold.

```
omnidroid pool start --size 2 --mode gaming     # keep 2 warm, in the background
omnidroid pool fill  --size 1 --mode gaming     # boot them now, in this process
omnidroid pool status
omnidroid pool stop
```

A slot is an ordinary instance booted to the **account-free ready point** —
Android up, kiosk up, DNS/consent/awake/mode tuning applied, no session
delivered. `start` then adopts one instead of booting:

```
cold   spawn -> 47-190 s boot -> deliver session -> playing
pool                             deliver session -> playing
```

**Measured 2026-08-15, x86 base, PS99, gaming 2048 MB / 2 vCPU:**

```
omnidroid pool fill --size 1 ...            slot ready in 58.8 s
omnidroid start admn1b12farm3 --place ...   warm pool: took slot _pool0
                                            timings.stages.boot = 0.082 s
                                            session delivered   = 7.6 s
```

**0.08 s instead of 47-190 s.** Nothing is serialised, so WHPX has nothing to
object to — which is the whole reason this exists and the warm CACHE cannot
(see below).

Three things about it are load-bearing:

* **A slot is only handed to a launch that would have booted the same
  machine.** The key hashes the RESOLVED spec — arch, base + version, offset
  *and its image's identity*, mode, mem, smp, accel, gpu, panel, quality,
  guest display — so `--mode playable` and `--mode gaming` share a slot (they
  are one machine) while `--mem 2048` and `--mem 4096` never do.
* **Adoption copies `run.json`, it does not move the directory.** QEMU holds
  `qemu.log` open and Windows will not move a directory out from under an open
  handle. The copy keeps the slot's `identity` (`omni-_pool0`) verbatim: the
  QEMU process was named at spawn and cannot be renamed, and `instance_live`
  compares the recorded identity against QMP `query-name`, so rewriting it
  would make a healthy adopted instance read as dead.
* **The claim is an `O_EXCL` file create.** Two concurrent launches cannot be
  handed one guest — which would put the second account's cookie into the
  first account's game.

The manager boots slots **one at a time**. Two guests booting at once on this
host starve each other badly enough to have earned its own gotcha, and a pool
that fills slowly beats one that makes the instance somebody is playing
stutter while it fills.

A slot appears in `omnidroid list` while it is warm, tagged `[warm pool]`.
That is deliberate: the "refuse while an instance is running" guards read the
same list, and hiding pool slots from them is how a guard silently stops
guarding.

### There is no warm-boot CACHE on Windows, and that is a hypervisor limit
QEMU/WHPX registers a migration blocker at CPU realize time:

```
warm bake failed (migration State blocked due to non-migratable CPUID feature
support,dirty memory tracking support, and XSAVE/XRSTOR support)
```

WHPX exposes no way to read back guest CPUID state, no dirty-page log and no
XSAVE area, so there is nothing for QEMU to serialise. No capability, transport
or flag changes it. `_warm_cache_allowed()` refuses the whole mechanism there
rather than paying a guest stop, two staged qcow2 overlays and a refused
migration on every launch. It still works on KVM and HVF.

Two other things were found while chasing this, and both are fixed:

* **QEMU cannot migrate to a FILE on Windows at all** — `file:` fails with
  "Failed to set FD nonblocking: Input/output error" (Windows has no
  non-blocking file handles), and `mapped-ram`+`multifd` killed the QEMU
  process outright. `omnidroid/migfile.py` relays the stream over a loopback
  socket instead, which works. It is still gated off by the WHPX blocker above,
  but the transport is correct for any host that gets a migratable accelerator.
* **The disk check was silent.** This dev box had 7.4 GiB free against a
  hardcoded 10 GiB reserve, so `has_room()` said no on every launch and nothing
  was ever printed. The reserve is configurable now
  (`qemu.warm_reserve_gb` / `OMNI_WARM_RESERVE_GB`) and a skipped bake says
  what it needed and what it found.

Cold boot on the Windows host, measured on PS99: **47–102 s** to a joined game,
depending on mode.

---

## The scratch, and why it — not RAM — caps instance count

*Measured 2026-08-15, Windows host, PS99, x86 base.*

Every ephemeral boot runs its disks `snapshot=on`. That is what makes an
instance diskless: QEMU keeps the guest's writes in a **temporary overlay** and
throws it away at exit. Two things about that file were never budgeted for.

**It is big.** One farming instance in the PS99 world grew its overlay to
**1.3 GB** — the game downloads its assets into `/data` and every byte lands
there.

**It went to `%TEMP%`, and it leaked.** QEMU creates it with the libc temp
directory (`GetTempPath` on Windows, `TMPDIR` elsewhere), and a QEMU that
*dies* rather than exits never unlinks it. This box had **3.7 GB** of leaked
overlays from three sessions, the oldest two days old.

**And a full volume kills instances silently.** With the disk exhausted QEMU
aborts — and cannot write the reason into `qemu.log`, because writing the log
needs the same disk. The symptom is an instance that was in the world a moment
ago and is now simply gone, with a **zero-byte log** and no Windows error
report. It was diagnosed twice as a guest crash before the temp directory was
measured.

So:

| | |
|---|---|
| overlays live in | `<data dir>/scratch` — ours, not `%TEMP%` |
| set by | `TMP`/`TEMP`/`TMPDIR` on QEMU's child env (`scratch_env`) |
| leaked ones | reaped on every boot and every pool tick (`reap_scratch`) |
| a live guest's overlay | **cannot** be reaped on Windows (the open handle refuses the unlink), which is what makes the reaper safe to run from the boot path |
| preflight | `scratch_room()` warns below `SCRATCH_PER_INSTANCE_MB + SCRATCH_FLOOR_MB` |
| visible in | `doctor` → `scratch_dir`, `scratch_free_mb`, `scratch_fits_instances` |
| override | `qemu.scratch_dir` / `OMNI_SCRATCH_DIR` |

**Plan capacity off the disk, not only the RAM.** At ~1.3 GB of scratch and
~2.2–3.2 GB of host RSS per instance, a 32 GB box with 100 GB free runs out of
RAM first, and a 32 GB box with 8 GB free runs out of **disk** at three
instances — while `list` still shows the others as healthy right up until they
vanish.

## Farming's memory floor is a property of the GAME

`MODES["farming"]["mem"]` is 2048. PS99 is OOM-killed at that size, measured
three times with no squeeze and no balloon in the way, and needs **3072**.
That is not a farming constant that was set too low; it is a per-game number
that had no home. `lean.GUEST_MEM_FLOOR_MB` is now that home, and
`guest_mem_floor_mb(place_id, default)` only ever **raises** — so gaming's
host-derived autoscaling is untouched, and an explicit `--mem` always wins.

**`pool fill` takes `--place` for the same reason.** `mem` is part of the slot
key, so a pool warmed at the mode's 2048 is *invisible* to a PS99 launch that
resolves to 3072: every launch cold-boots while `pool status` cheerfully
reports slots ready.

An unmeasured place gets the default and may OOM. Measure a place before
promising a fleet size for it.

## The warm pool, measured end to end

*2026-08-15, Windows/WHPX, PS99, x86 base, farming.*

```
pool fill --size 1 --mode farming --place 8737899170
    slot ready in                     34.8 s

start <acct> --place 8737899170 --mode farming
    guest RAM raised to 3072 MB for place 8737899170
    guest clock resynced (was -18s behind host)
    warm pool: took slot _pool0 — no boot needed
    timings.stages.boot               0.093 s      (cold: 35-60 s)
    timings.stages.session_delivered  0.52 s
    timings.stages.density_settled    147.9 s
    client.in_world                   true
```

**A snapshot would be slower than this, not faster.** Reproduced on QEMU
11.0.50 here: under `-accel whpx` all three save paths (`migrate`, `savevm`,
QMP `migrate`) refuse with the same blocker, while the same binary under
`-accel tcg` snapshots fine — so the machinery works and WHPX is fenced off.
The only route is a patched QEMU with the blocker removed, which is exactly
what Google's Android Emulator fork does. Even then, `loadvm` has to read
~2.2 GB of guest RAM off disk; the pool hands over a live slot in 0.08 s. A
snapshot's value here would be **capacity** (parking idle instances to disk)
and surviving a host reboot — never latency.

HVF (macOS/arm64) and KVM register no such blocker, so the warm CACHE stays
enabled there. Neither was exercised this session.

**The clock is why adoption is not just "hand over a pid".** A slot is a live
VM whose clock ticks, so the usual answer is "nothing to do" — but a desktop
SLEEPS, and a guest that wakes behind fails Roblox auth and TLS with a symptom
indistinguishable from a dead cookie. The resync runs on every adoption, costs
one adb round trip when there is nothing to fix, and corrected 18 s on a slot
that was 35 seconds old.

## The render floor: what it actually bought (mostly nothing)

*2026-08-15, PS99, in-world, farming, `--mem 3072`, x86/WHPX. Four runs.*

Farming boots `-display none` with an idle VNC server that encodes nothing, so
there is **no host-side render cost to attack**. Every remaining lever is
inside the guest. `--quality minimal` was added to pull two of them: a 3 fps
tick target (down from 5) and a 320x180 panel (down from 480x270).

| run | client | guest CPU (2 vCPU) |
|---|---|---|
| `--quality low` (the shipped default) | alive, in-world | user 150-156%, **idle 24-35%** |
| `--quality minimal`, floor as a SECOND resize | **DEAD** — process gone, black screen | 200% idle |
| `--quality minimal`, `OMNI_FARM_SKIP=render` (3 fps, no resize) | alive, in-world, PSS 1349 MB | user 140-158%, **idle 19-27%** |

Two conclusions, and the second one is the useful one.

**1. The 320x180 panel kills the client — by itself.** The `minimal` boot's
Roblox process was simply gone: `screencap` solid black, the guest's
MemAvailable jumping 591 MB → 2202 MB as its 1.6 GB was released, guest at
200% idle. That last number is a trap — it reads exactly like the "engine
deadlocks rather than crawls" signature in the x86 section, and it is not
that. It is simply dead.

The first bisect (`OMNI_FARM_SKIP=render`) kept the client alive, which made
"a SECOND mid-session `wm size`" the obvious culprit — but that skip removed
the small panel too, so both explanations fitted. Folding the floor into a
single resize and running it again killed the client just the same. **The
panel itself is fatal, not the repetition.** 480x270 is measured in-world
repeatedly and is fine.

**2. Dropping the tick target 5 → 3 fps buys nothing measurable.** Idle at
3 fps (19-27%) is indistinguishable from idle at 5 fps (24-35%) — if anything
it is worse, which is noise. That is consistent with what the resolution
section already says: **the guest is CPU-bound on arm64 translation, not
fill-bound.** (That reading is corrected twice over — once immediately below,
and once by measurement in "The translator is not the wall", where the
translator's tax is 1.0-1.5x rather than the wall it is called here.) Roblox running through `libndk_translation` is where the 150%
goes, and no render setting reaches it.

So `minimal` is a profile whose only measured effects are "no saving" and
"kills the game". It is **removed from `QUALITY_PROFILES`** rather than left
selectable, and `display_for_quality` is gated on the profile being live so a
programmatic caller cannot re-apply the fatal panel either. The dicts stay
defined as the record of what was tried.

Farming stays at `low`. The measured way to fit more instances on this host is
`-m` (host RSS tracks it almost exactly) and free scratch disk — not render
settings.

## CORRECTION: farming's CPU is llvmpipe, and the GPU halves it

*2026-08-15, PS99, in-world, per-thread out of `/proc/<pid>/task/*/stat`.*

Two sections above say the guest is "CPU-bound on arm64 translation, not
fill-bound". **That is wrong**, and it was inference from two null results
rather than a measurement. (Wrong twice, in fact: this section shows the CPU
was llvmpipe rather than the translator, and "The translator is not the wall"
later measured the translator itself at 1.0-1.5x.) Attributing the CPU per thread settles it:

| software (`--gpu headless`) | | GPU (hidden GL window) | |
|---|---|---|---|
| `llvmpipe-1` | 52.8% | *gone* | |
| `llvmpipe-0` | 51.8% | *gone* | |
| `HttpClient` | 11.1% | `FunctionMarshal` | 18.3% |
| `FunctionMarshal` | 10.0% | ` RBX Worker A` | 17.3% |
| ` RBX Worker A` | 6.8% | ` RBX Worker B` | 20.0% |
| **TOTAL** | **141.1%** | **TOTAL** | **72.3%** |

**Three quarters of a software farming instance's CPU is llvmpipe** —
software GL, rasterising frames nobody looks at. The arm64-translated game
code (` RBX Worker *`) is a small minority of it.

That also explains why the render floor measured as nothing. The fps cap
throttles Roblox's *task scheduler* and the panel changes its *pixel count*;
neither reaches the software rasteriser's per-frame work. The lever was never
"render less" — it was **"render somewhere else"**.

So farming's GPU policy is now `auto`, and the settle also got faster
(67 s vs 116–148 s) because the client loads against a GPU. Since CPU is what
decides how many instances a host holds, this roughly doubles the ceiling.

**`auto`, not `window`.** An explicit `window` means "I want to see it", so
`place_window` leaves it exactly as QEMU made it — unstyled, on screen,
verified. `auto` opens one only because this host has no other route to a GL
context. On a real headless farm box with no window server, `auto` finds
nothing and falls back to software, which is the old behaviour.

**Farming's window is HIDDEN; gaming's is SHOWN.** Same `auto` policy, opposite
outcome, and `qemu_proc.window_shown_at_spawn` is the one line that decides it.
Farming is fifty instances nobody is watching and fifty windows across the
desktop is not a product, so its window is hidden at spawn and kept hidden (GTK
re-shows it during early boot). Gaming is one instance a person started and is
waiting on, so its window goes up at spawn — see the next section.

**What it costs:** QEMU refuses `-vnc` beside a GL context, so `capture` and
`autocap` are unavailable on a GPU farming boot and `omnidroid view` shows
QEMU's own window — no reparenting and no copy. **`screenshot` goes through adb
and is unaffected** — verified against a GPU farming instance.

## The gaming window is on screen for the BOOT

*Built and measured 2026-08-17 on the Windows box, QEMU 11.0.50, x86 base.*

A gaming launch used to open its QEMU window, hide it, boot for a minute with
nothing on the user's screen, and show the window at the very end — so the
first thing anyone ever saw was Roblox already running, and the whole boot
looked like the app had frozen. It now goes up at spawn, at the panel size,
and stays up:

```
t+0.0s   no window yet
t+3.1s   window exists, hidden, 640x505      title 'QEMU (omni-HezMi_ImYu)'
t+3.5s   window VISIBLE,        640x505      (GTK shows it)
t+3.7s   window VISIBLE,       1280x800      title 'omni: HezMi_ImYu'
```

That last line is the whole change: **3.7 s into the launch**, against ~133 s
before, which is when `start` returned and the app finally called `view`.

`place_window` takes the decision at spawn and has exactly three answers:
present it (watched), hide it (farming), or leave it untouched (`--gpu window`,
the debugging hatch, which must have none of this code in its path). Turn it
off with `OMNI_HIDE_BOOT_WINDOW=1` or config `qemu.hide_boot_window`.

**The size it opens at is the CLIENT size, not the window size**, and that is
load-bearing rather than pedantic: QEMU hands the guest the size of its drawing
area, so a window sized to 1280x800 gives the guest 1264x761 once the caption
and border are taken off it (measured: the frame is 16x39 on this host).

### Keeping the aspect ratio

Two mechanisms, and both are needed.

**QEMU must not stretch.** `-display gtk,...,keep-aspect-ratio=on`, which is
now named explicitly in `_WINDOW_FLAGS`. `ui/gtk.c`'s `gd_update_scale()` is
the whole of it — `keep_aspect_ratio` picks `MIN(sx, sy)` over independent
`sx`/`sy` — so with it off, any window that is not the guest's shape stretches
the picture. Verified on the shipped binary at the pixel level, by grabbing the
window's client area at three shapes:

| window client | ratio | what the sampled edges read |
|---|---|---|
| 1280x800 | 1.600 | no bars — content everywhere |
| 1400x500 | 2.800 | left/right columns a constant 25.0 — **pillarboxed** |
| 700x800 | 0.875 | top/bottom rows a constant 25.0 — **letterboxed** |

⚠ The suboption is **absent from `-display help`'s text** (that text is
hand-maintained; the option lives in the QAPI schema) and QEMU **refuses an
unknown suboption outright rather than ignoring it** — so it was verified
against the real binary before being added, and there is a test that re-asks it
(`test_window_at_boot.py`) plus a control that proves the probe discriminates.

**The window must not drift off the guest's shape, and it must not drift
DURING the drag.** `hostwin.aspect_lock`, run as a detached `_windowlock`
process for the life of the window, corrects the client area **live** — while
you are still dragging — not when you let go.

The obvious objection is that it cannot work: a user resize runs inside
`DefWindowProc`'s modal size loop, which recomputes the rect from its own
tracked state on every mouse move, so an outside `SetWindowPos` ought to be
undone by the next one. Measured against a real modal drag (`WM_NCLBUTTONDOWN`
+ `HTBOTTOMRIGHT`, then `SendInput` mouse moves), sampling the client rect
every 10 ms:

| corrector | samples off-ratio by >2% | ratio at the end |
|---|---|---|
| none | 76 / 246 (31%) | 1.98 — 24% off 16:10 |
| every 8 ms | **2 / 246 (1%)** | **1.600** |

The correction wins because a mouse move is milliseconds apart and 8 ms is
less: what is on screen for almost all of the drag is the corrected shape.
Idle cost measured at **0.16% of one core**.

Two pieces of state make it stable, and neither is optional:

* **`applied`** — the size *we* last set. Anything else read back is a change
  the user made, and telling those apart stops the loop reacting to itself.
* **`axis`** — which dimension the user is dragging, **latched for the drag**.
  Re-deciding per frame flips it the moment our own correction has moved the
  other dimension, and then the lock and the drag argue: the user drags the
  bottom edge, we widen to match, the next mouse move puts the width back
  (Windows recomputes it from the rect the drag *started* with), and now the
  width looks like the dimension that moved.

Measured on all three drag kinds, against the real lock:

| drag | start → end | final ratio | off-ratio during |
|---|---|---|---|
| corner | 880x550 → 1482x926 | 1.6004 | 0 / 188 |
| bottom edge | 880x550 → 1264x790 | 1.6000 | 2 / 188 |
| right edge | 880x550 → 1200x750 | 1.6000 | 3 / 188 |

Each keeps the dimension the user was actually dragging and moves the other.
The 1–3 stray samples are the first 10–30 ms, before the idle poll notices the
drag has begun.

**The guest is still only told once.** QEMU coalesces —
`qemu_console_set_ui_info(..., delay=true)` does `timer_mod(ui_timer, now +
1000)`, re-armed on every change — so the guest hears one number, a second
after the drag stops, and by then the shape is already right. The lock stops
correcting the moment it is (`aspect_is_close`), which is what lets that timer
fire at all; a corrector that never stopped would re-arm it forever, and that
is what an earlier 120 ms attempt did before the guest ended up at "Display
output is not active".

With the guest booted, `wm size` read `Physical size: 1280x800` against a
1000x624 window — the same 16:10, so QEMU's scale is uniform and nothing is
distorted at a window size the user chose freely.

### Our name and our icon, on QEMU's own window

QEMU calls its window `QEMU (omni-<account>)` and gives it the QEMU logo. Both
are replaced from outside, on QEMU's own window, with no patched build and no
second window stacked on top to caption it: `WM_SETTEXT` and `WM_SETICON` are
both marshalled between processes.

The icon is `omnidroid/assets/omni-icon.png`, which has been in the tree since
2026-08-16 and which nothing consumed — it was added for the
`QEMU_WINDOW_ICON` env var that no build reads. `LoadImageW` cannot read a PNG
and this repository ships no `.ico`; **`CreateIconFromResourceEx` takes the PNG
bytes directly** (a PNG-compressed icon image is a documented icon-resource
form since Vista), so it needs no conversion, no generated `.ico` and no temp
file. One HICON is loaded per size — 16 for the caption, 32 for the taskbar —
rather than one stretched for both.

QEMU rewrites its own caption whenever the machine's run state changes
(`gd_update_caption`), so the aspect lock re-asserts the name every 2 s if it
has drifted. It is the only long-lived thing watching that window, so it is the
only place that can notice.

⚠ **RENAMING THE WINDOW BROKE FINDING IT, and the fix is not the obvious one.**
`find_window` matched the identity as a *title substring*, and our title
(`omni: farm3`) does not contain the identity (`omni-farm3`), so every later
`view`/`hide`/lock lookup found nothing. Matching on the pid instead is
necessary but not sufficient — measured, a QEMU process owns half a dozen
windows:

```
gdkWindowToplevel       the one, and the only one, that ever is
NVOpenGLPbuffer         'NVOGLDC invisible', 1914x994, invisible — the driver's
GDI+ Hook Window Class  1x1
GdkDisplayChange        0x0
Default IME/MSCTFIME UI 0x0
```

and QEMU's real window does not exist until **t+0.22 s**, while `find_window`
is called ~50 ms after spawn. A naive pid fallback therefore returned the
NVIDIA driver's invisible pbuffer, and everything downstream styled, renamed,
resized and watched a window nothing is ever drawn in — while the real one sat
on the user's screen at 640x505 wearing QEMU's own name. A pid-only candidate
now has to *look* like a guest window (top-level, not a known decoy class, a
client area of at least 64x64), which puts `find_window` back to **waiting**
for the real window rather than grabbing whatever the process owned first.

**The X no longer quits QEMU** (`window-close=off` is back). It was dropped on
the belief that a patched QEMU asked "Stop this instance?" on the close — there
is no such build, so on the shipped binary the X killed the guest instantly. A
window that is on screen for the whole boot is one the user is looking at with
nothing else to do, and an accidental click there cost the entire launch. The
window is now managed from the app: **Hide** puts it away and keeps the game
running, **Stop** powers it off.

**Untested:** GPU contention with many concurrent instances. Only one Roblox
cookie was live when this was measured, so a single instance is the only
in-world data point. That is the number to take before promising a fleet size.

## Linux/KVM: both things Windows cannot do, measured

*2026-08-15, WSL2 Ubuntu (kernel 6.6.87.2-microsoft-standard-WSL2), nested
KVM, QEMU 8.2.2. A minimal Linux guest, not the Android base — these
characterise the HYPERVISOR, not Roblox's working set.*

### Host RSS actually falls

QEMU process RSS (MiB), `-m 2048`, `virtio-balloon-pci`:

| | idle | guest dirties 1 GiB | guest frees it | after QMP balloon → 1024 |
|---|---|---|---|---|
| `free-page-reporting=on` | **189.4** | 1213.6 | **195.6** | 187.8 |
| `free-page-reporting=off` | 217.5 | 1213.6 | 1213.6 | **189.7** |

Three things, and all three are the opposite of the Windows measurement:

* **A `-m 2048` guest idles at ~190 MiB, not 2048.** QEMU only backs pages the
  guest has touched. On Windows host RSS tracks `-m` almost exactly.
* **The guest freeing 1 GiB returned 1018 MiB to the host in under 30 s, with
  no host-side action at all** — that is `free-page-reporting` working.
* The control isolates it: with reporting off, freeing returns nothing, but an
  explicit balloon inflate still reclaims 1024 MiB. **Both mechanisms decommit
  for real.** On Windows neither does, because there is no `madvise`.

**So ~400 MB/instance is reachable on Linux and the hypervisor is no longer
what stands in the way.** Host RSS ≈ guest live set + ~150-190 MiB of QEMU.
Reaching 400 MB now needs the squeezed Android guest's live set to sit around
210-250 MiB — a guest-side question, which is precisely the question Windows
made unanswerable.

### savevm/loadvm works, and it is fast

`-m 2048` with 768 MiB of **incompressible** guest data live:

```
savevm                          1.73 s   (947 MiB of state, 444 MiB/s)
loadvm                          1.26 s   state verified correct
migrate to file (defaults)      7.22 s   131 MiB/s  <- QEMU's default
                                                       max-bandwidth throttle
migrate, max-bandwidth 4 GiB/s  0.76 s   1183 MiB/s (8.8x)
restore into a FRESH qemu       0.74 s   guest responsive at 0.75 s
```

Extrapolated to a farming instance (`-m 3072`, ~2 GiB live): **~1.7 s to save,
~1.7 s to restore**, against a 47-190 s cold boot. No migration blocker exists.

**Zero pages are free, so do not measure with `/dev/zero`.** A first attempt
snapshotted 1280 MiB of guest data to 117 MiB in 0.28 s because QEMU skips
zero pages entirely. The numbers above were re-taken with `/dev/urandom`. A
freshly booted, ballooned Android guest will snapshot far smaller than its
`-m` — which helps, but do not quote the zero-page number as a result.

### WSL2 is a test bench, not a runtime

It validated the Linux code paths, which had never been exercised. It cannot
ship:

| | |
|---|---|
| storage | `/mnt/c` (drvfs) **219 MB/s** vs **7.1 GB/s** on the distro's ext4 — images must never live on `/mnt/c` |
| GPU | `/dev/dxg` exists, **`/dev/dri` does not** — no DRM render node, so no virgl for a nested guest: headless farming only |
| network | NAT behind NAT, on top of the VPN's 1420 MTU already documented above |
| disk | `ext4.vhdx` grows and never shrinks (`wsl --manage <d> --set-sparse true` reclaims, distro stopped) |

`/dev/kvm` needed no `.wslconfig` change here, but the user must be in the
`kvm` group — `check_accel()` already prints that exact fix.

The Linux code paths themselves are already written and were simply never run:
`default_accel()` returns `kvm`, `machine_arg()` adds `mem-merge=on` for KSM,
and `check_accel()` preflights `/dev/kvm`. KSM is available in this kernel, so
cross-instance dedup stacks on top of the balloon.

## OPEN: the squeeze kills the PS99 client about a minute after it runs

*2026-08-15. This is the most important unresolved thing in farming, and the
reason it went unnoticed is worth as much attention as the bug itself.*

**Every check a density launch makes runs inside the first minute.** `ok`,
`delivered`, `played`, and `probe_client_join`'s `in_world` are all sampled
right after the squeeze — and the client is alive then. It is gone by t+90.
So the launch has been returning `ok: true, in_world: true` for an instance
with about a minute to live, for the life of the mode.

| configuration | last seen alive |
|---|---|
| full squeeze, 3 GB, GPU | **t+60** |
| full squeeze, 3 GB, software *(control)* | **t+60** |
| full squeeze minus `lmkd`, 3 GB | t+90 |
| full squeeze, **4 GB** | t+151 |
| **whole squeeze skipped**, 3 GB | **t+421, still alive** |

What the matrix establishes:

* **It is the squeeze.** Skipping it entirely gives 7x the lifetime and the
  client was still running when the watch ended.
* **It is not the renderer.** The GPU and software runs died within **0.05 s**
  of each other. That also means it is a timer, not a crash.
* **It is not memory exhaustion.** The 4 GB run died with **1604 MB
  available**. More headroom only moves the deadline.
* **It is not one lever.** Removing `lmkd` — the obvious suspect, since it is
  configured `kill_heaviest_task=true` and Roblox is by far the heaviest task —
  bought 30 seconds.

The client writes **nothing** to its log when this happens, which is what a
process that is *killed* looks like. Evidence has to come from the killer's
side: `logcat` for lmkd/ActivityManager, and the guest's memory state at the
moment of death. `scratchpad/watch_death.py` does both.

**Before any more lmkd bisecting, read `ro.lmk.*` on a FRESH boot.** They read
`true` after a squeezed run — but that is our own step setting them. If the
Bliss base already ships them true, then `OMNI_FARM_SKIP=lmkd` never disabled
anything and that row means something entirely different.

**Related and probably contributing: the settle heuristic fires early.**
`wait_for_game_settled` declares the client loaded on **two** samples within
4%, and across three runs it fired at 1241 MB, 1327 MB and 1359 MB — while
`FOOTPRINT.md` records PS99 reaching ~1528 MB in-world. So the squeeze lands
on a client that is still growing. Requiring a longer plateau
(`SETTLE_STABLE_SAMPLES`) is the obvious thing to try, and it is **untested**.

Until it is fixed, `verify_client_survived()` at least makes the failure
visible: a density launch now watches for 120 s past the squeeze and reports
`client_died_after_squeeze` instead of `ok: true`.
