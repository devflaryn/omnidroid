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
| host window | never *visible* — hidden, and hosted by our viewer (see **The GPU**) | never |
| guest MTU | matched to the host's egress (see **Farming**) — both modes | same |
| guest panel | 1280x800 (`--panel` overrides) | 640x480, `wm size` 480x270 |
| engine tick | 240 fps target | 5 fps cap |
| render quality | `high` — real textures, lighting, post-FX | lowest everything |
| balloon | none | 896 MB (with zram) / 1536 (without) — **skipped entirely on a host that cannot return the pages; see Farming** |
| vCPU / RAM | **sized to the host**, capped 4 GB / 4 vCPU on WHPX | 2048 MB, 1 vCPU (2 on x86) |
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

...then capped at **4096 MB / 4 vCPU on WHPX**, because on Windows the
autoscaled 8192/8 booted 6.5x SLOWER than 4096/4 on the same image (0.9 min vs
5.9 min). KVM and HVF keep the larger ceilings.

"As much as it safely can" is the load-bearing half: a guest sized past the
host's spare RAM makes the **host** swap, and a swapping host misses QEMU's
vCPU deadlines — slower than the smaller guest would have been. An explicit
`--mem`/`--smp` always wins outright.

### Resolution

`--panel WxH` (or `720p`/`800p`/`1080p`/`1440p`), config `qemu.panel`, env
`OMNI_PANEL`. Defaults to the mode's own.

Bigger costs frames, and not for the reason you would guess: the guest is
**CPU-bound on arm64 translation, not fill-bound**. Measured on the x86 base,
640x480 gave 19 fps against 14 at 1280x800 — a 4.2x pixel cut for 33% more
frames. `libndk_translation` is the wall and no flag removes it.

---

## The GPU

One setting, `--gpu` (config `qemu.gpu`, env `OMNI_GPU`), four values:

| | |
|---|---|
| `auto` | **default.** Reach the GPU whatever it takes. If that needs a window, open it HIDDEN and let the viewer host it. |
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
That does not mean you have to look at a QEMU window, and you never do:

### You only ever see OUR viewer

| | |
|---|---|
| the window is **hidden** at spawn | it exists only to hold the GL context |
| a hidden window **keeps rendering** | measured: 303 frames / 30 s while invisible |
| `omnidroid view` **hosts** it | `SetParent` makes it a child of our Tk window |

So the guest lives *inside* the product's viewer — our title, our chrome — with
QEMU's GL surface composited straight into it. Nothing is copied, encoded or
decoded per frame, and input goes into the guest's `usb-tablet`/`usb-kbd`
directly instead of being synthesised from an RFB event. **Measured embedded:
702 frames / 12.1 s = 58 fps.**

GTK re-shows the window during early boot, so `hostwin.keep_hidden()` re-hides
it for the length of a boot and then stops; after that a single hide sticks
(30 s of polling never saw it return).

`--gpu window` is the one setting that leaves it on screen, for when a GL
problem has to be seen with none of this project's code in the path.

**The one hazard, and it is reported rather than silent:** Windows destroys a
child window when its parent dies, so a viewer that is FORCE-killed (rather
than closed with its X) takes QEMU's window with it. The instance keeps running
and keeps answering adb, and renders nothing at all — measured,
`totalFrames = 0`. `omnidroid view` detects exactly that state and says so,
instead of opening an empty window onto a blind guest.

| | fps at 1280x800 on PS99 | what you watch |
|---|---|---|
| `--gpu auto` (default) | **24.2–58** across runs | our viewer, hosting the hidden window |
| `--gpu headless` | 3.2 | our viewer, over VNC |

The GPU spread is real rather than noise in the method: PS99 is a busy
server-authoritative place and how much is streaming in when the sample is
taken moves it a long way. The ratio to software does not move.

On a host whose `egl-headless` presents — Linux — none of this is needed:
`auto` gets GPU, no window and VNC all at once, and the same viewer connects
the ordinary way. macOS has no virgl yet, so it renders in software and also
uses VNC. **The viewer is the same on all three; only what it attaches to
differs.**

---

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

The user's "~400 MB in the desktop Roblox app" is the closest comparison to the
**game process** (508 MB PSS here), not to an instance: an instance is that
game *plus a whole Android* plus QEMU.

### Two farming bugs fixed on 2026-08-15

* **`smp 1` could not get x86 through the session handover.** Roblox's arm64
  build runs through `libndk_translation` on the x86 base, and the ordered
  `am broadcast` that hands over the session did not return within 45 s — which
  raised `TimeoutExpired` straight out of `cmd_start` as a traceback. Farming
  now takes `smp_x86: 2`, `kiosk_broadcast` treats a timeout as a RESULT rather
  than an exception, and its budget is 120 s.
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
fill-bound.** Roblox running through `libndk_translation` is where the 150%
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
rather than a measurement. Attributing the CPU per thread settles it:

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
`_hide_window_if_wanted` leaves it on screen — verified. `auto` opens one only
because this host has no other route to a GL context, then hides it. On a real
headless farm box with no window server, `auto` finds nothing and falls back to
software, which is the old behaviour.

**What it costs:** QEMU refuses `-vnc` beside a GL context, so `capture` and
`autocap` are unavailable on a GPU farming boot and `omnidroid view` uses the
embedded window. **`screenshot` goes through adb and is unaffected** — verified
against a GPU farming instance.

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
