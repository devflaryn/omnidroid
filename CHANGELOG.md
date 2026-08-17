# Changelog

> **Resuming from a fresh session? Read `HANDOFF.md` first**, then `PLAN.md`,
> then this file, then `git log`. Entries are newest-first.

All notable base-image and manager changes. Bases are immutable and
versioned; each new base is flattened self-contained (no backing file).

## 2026-08-17 (later) — farming stops paying for memory it is not using

**A live instance was being reported as DEAD, and that is why nothing worked.**
`instance_live()` asked QEMU over QMP with a 0.25 s budget. QMP is served by
QEMU's main loop, and a guest running a game keeps that loop busy: a live PS99
farming instance answered `query-name` in **3 s** and was declared dead. So
`list` showed it stopped, `stop` could not stop it, and — the expensive one —
**the memory governor exited with "QEMU process is gone" after a single
shrink**, which is exactly the "it always uses the whole `-m`" symptom. The
instance was left orphaned at 3.2 GB with its ports still held.

Liveness now uses the pid's **creation time**, recorded at spawn
(`runtime.process_start_ticks` → `run.json: pid_started`). That is the standard
durable process identity, costs microseconds, cannot be starved, and is
definitive in both directions — a match means it is our process, a mismatch
means the pid was recycled. QMP is now only a fallback for pre-upgrade records,
and its budget was raised from 0.25 s to 4 s because it is no longer a hot path.

**The governor was the thing wedging guests, and it is a different mechanism
now.** `EmptyWorkingSet` on a timer never converges: the next trim lands before
the guest has faulted its live set back. Measured, trimming every 30 s against a
live PS99 instance — host RSS stuck at 1–37 MB, **adb stopped answering within
12 s**, the client was killed, and the guest never recovered even after the
trims stopped. A hard **working-set maximum** asks the memory manager for the
same thing and lets it choose which pages and when:

| ceiling | host RSS | client | adb round trip |
|---|---|---|---|
| none | 3417 MB | alive | 0.05 s |
| 1000 | 1000 | alive | 0.05 s |
| 650 | 650 | alive | 0.04 s |
| 500 | 500 | alive | 0.10 s |
| 384 | 384 | alive | 0.10 s |
| 300 | 300 | **dead** | 0.04 s |

At 384 MB the client burned **152% of a guest core** (genuinely playing) and
QEMU read **0.01 MB/s** off disk — the faults are soft, from the standby list,
which is why the guest stays responsive at 8.9× less resident memory. The
governor now walks the ceiling down from this boot's own `-m` while the guest
is healthy, stops clear of anything that hurt it, and never climbs past `-m`
(an earlier cut ran away to 4736 MB against a client that had died for its own
reasons). The right ceiling is a property of **the game**, so it is searched
for rather than configured.

**CPU is capped the same way, and that is what decides instance count.**
Farming's cost is the game's own arm64 translation — 148% of a guest core
against SurfaceFlinger's **6.7%**, so there is nothing to "render less" of;
blanking the display saves ~7%, not the 4.6× a first measurement suggested
(that saving was the client dying). A job-object hard cap gives each instance a
slice instead: 160.9% uncapped → 49.9% at a 50% ceiling, client alive and adb
answering in 0.06 s. Farming defaults to `cpu_ceiling_pct: 50`; gaming has none
and no governor at all — `performance` is the opposite trade by definition.

**Per farming instance, measured end to end: 3417 MB → 384 MB, 161% → 50% of a
core, client in-world throughout.**

**Launches are 2.0–2.6× faster.** The 120 s post-squeeze survival wait was 47%
of a 257 s launch, spent asleep — and the governor is a process that already
runs for the instance's life and already asks "is the client alive" on every
poll. It is now started *before* that wait instead of after, the wait is
skipped, and a client that dies is recorded into run.json
(`client_died_after_s`) by the governor instead of by a blocking check.
`OMNI_SQUEEZE_GRACE` still forces the old behaviour for anyone bisecting.

| | before | cold | warm (pool) |
|---|---|---|---|
| boot | 34.5 s | 34.7 s | **0.07 s** |
| game load + squeeze | 100 s | 88 s | 100 s |
| survival wait | 120 s | **0 s** | **0 s** |
| **total** | **257 s** | **126 s** | **101 s** |

All measured through the executor's own argv, with auto-login and auto-join on
PS99 (place `8737899170`), `in_world: true` every run.

**`doctor` now says how many instances fit, which wall is in the way, and what
it would take to move it.** Four walls, and on Windows it is usually not the
RAM everybody plans for — the governors made RAM and CPU cheap, which moved the
wall to **commit**.

Commit measured against a paused QEMU (the floor), and the accelerator matters:

```
-m 1024 whpx, -display none      1065 MB commit   (+41)
-m 2048 whpx, -display none      2092 MB          (+44)
-m 3072 whpx, -display none      3117 MB          (+45)
-m 3072 whpx + gtk,gl=on         3258 MB         (+186)
```

⚠ The same probe **without `-accel whpx`** reads +1070 MB, because TCG reserves
a ~1 GB translation buffer by default. That is an artifact of the probe, not a
cost of an instance, and it is why `COMMIT_OVERHEAD_MB` carries the numbers and
a warning rather than a rule of thumb. `memory-backend-ram,reserve=off` — which
would move guest RAM off the commit limit entirely — **is not in this build**
(`Property 'memory-backend-ram.reserve' not found`), so commit tracks `-m` at
1:1 and there is no way around it in QEMU.

Because it tracks `-m` 1:1, `-m` is the one lever entirely in the launcher's
hands, and `capacity_ladder` reports what each size buys. On this box:

| `-m` | instances | wall |
|---|---|---|
| 3072 | 9 | commit |
| 2048 | 13 | commit |
| 1536 | 13 | **disk** |
| 1024 | 13 | **disk** |

— so below 2048 shrinking the guest buys nothing, because the scratch reserve
takes over. `capacity_shortfall(30)` turns that into a shopping list rather
than a refusal: at `-m 2048`, **35 GB short on commit and 31 GB short on
disk**. Both are disk (the pagefile lives there too), so the answer is "free
~66 GB and raise the pagefile", not "buy more RAM".

## 2026-08-17 — the window is up for the boot, and it keeps the guest's shape

**The window appears at spawn, not at the end of the launch.** A gaming boot
opened a QEMU window, hid it, booted for a minute with nothing on screen, and
showed it once Roblox was already running — so the whole boot looked like the
app had frozen and the loading animation nobody could see was rendered for
nobody. `qemu_proc.place_window` now decides at spawn and has three answers:
**present** it as ours at the panel size (a boot somebody is watching —
gaming), **hide** it and keep it hidden (farming, fifty at a time), or **leave
it alone** (`--gpu window`, the debugging hatch, which is specified as having
none of this code in its path). `OMNI_HIDE_BOOT_WINDOW=1` / config
`qemu.hide_boot_window` restores the old behaviour.

Measured on a real boot: window exists hidden at 640x505 by t+2.4 s, **visible
at 1280x800 by t+9.7 s**, and it stays up through Android coming up.

Measured on a clean product boot: the window exists hidden at 640x505 by
t+3.1 s, GTK shows it at t+3.5 s, and it is **1280x800 and named `omni:
HezMi_ImYu` by t+3.7 s** — against ~133 s before, which is when `start`
returned and the app finally called `view`.

**It keeps the aspect ratio at every window size, LIVE**, which took two
things:

* `-display gtk,...,keep-aspect-ratio=on` — QEMU letterboxes instead of
  stretching (`ui/gtk.c gd_update_scale`: `MIN(sx, sy)` vs independent
  `sx`/`sy`). Verified at the pixel level: at 1400x500 the left/right columns
  read a constant 25.0, at 700x800 the top/bottom rows do, at 1280x800 neither
  does. ⚠ The suboption is absent from `-display help` (that text is
  hand-maintained; it lives in the QAPI schema) and QEMU **refuses an unknown
  suboption rather than ignoring it**, so a wrong guess here costs the boot,
  not the chrome — it was checked against the shipped binary and there is a
  test that keeps asking, with a control.
* `hostwin.aspect_lock`, run as a detached `_windowlock` process, correcting
  the window's **client** area **while the drag is happening** — not on
  release. A user resize runs inside `DefWindowProc`'s modal size loop, so an
  outside `SetWindowPos` "should" be undone by the next mouse move; measured
  against a real modal drag it is not, because 8 ms is shorter than the gap
  between mouse moves:

  | corrector | samples off-ratio >2% | final ratio |
  |---|---|---|
  | none | 76/246 (31%) | 1.98 (24% off) |
  | every 8 ms | 2/246 (1%) | 1.600 |

  Idle cost 0.16% of one core. The drag axis is **latched** for the drag —
  re-deciding per frame makes the lock and the drag argue on an edge drag.
  Measured across all three: corner 880x550→1482x926 (1.6004, 0/188 off),
  bottom edge →1264x790 (1.6000), right edge →1200x750 (1.6000), each keeping
  the dimension the user was dragging. The guest is still told only once,
  because QEMU coalesces (`timer_mod(ui_timer, now + 1000)`) and the lock stops
  as soon as the shape is right. With the guest booted, `wm size` read
  `Physical size: 1280x800` against a 1000x624 window — same 16:10, uniform
  scale, no distortion.

**Our name and our icon, on QEMU's own window.** `WM_SETTEXT`/`WM_SETICON` are
marshalled between processes, so `QEMU (omni-<account>)` + the QEMU logo become
`omni: <account>` + `omnidroid/assets/omni-icon.png` — no patched build, no
second window stacked on top to caption it. That PNG had been in the tree since
2026-08-16 with nothing consuming it (it was added for the `QEMU_WINDOW_ICON`
env var no build reads); `LoadImageW` cannot read a PNG and this repo ships no
`.ico`, but **`CreateIconFromResourceEx` takes PNG bytes directly**, so no
conversion and no temp file. QEMU rewrites its caption on run-state changes, so
the lock re-asserts the name every 2 s if it drifts.

⚠ **Renaming the window broke finding it**, and the fix is not the obvious one.
`find_window` matched the identity as a title substring, which our title no
longer contains. Falling back to the pid is necessary but **not sufficient**: a
QEMU process owns half a dozen windows (the NVIDIA driver's invisible pbuffer,
GDI+'s 1x1 hook, GDK's display-change listener, IME windows) and its real
window does not exist until t+0.22 s, while `find_window` runs ~50 ms after
spawn — so a naive pid fallback returned the pbuffer, and everything downstream
styled, renamed, resized and watched a window nothing is ever drawn in. A
pid-only candidate must now *look* like a guest window (top-level, not a known
decoy class, ≥64x64 client), which restores `find_window`'s "wait for it"
behaviour.

**Client pixels, not window pixels, everywhere.** QEMU hands the guest the size
of its drawing area, so sizing the *window* to 1280x800 gives the guest
1264x761 (the frame is 16x39 on this host). Every size in this path is now the
client area.

**`window-close=off` is back.** It was dropped because "our patched QEMU asks
'Stop this instance?' on the X" — there is no such build, so on the shipped
binary the X killed the guest instantly, no prompt. Survivable while the window
only appeared at the end; not survivable now it sits there for the whole boot.
The cost is an X that does nothing, taken deliberately: the window is a view
onto an instance, and the app now offers **Hide** (put it away, keep playing)
and **Stop** as separate buttons.

**Bridge (omni-executor).** `account_status` reports `native_window`,
`window_visible`, `window_client` and `has_vnc`, so the app can draw the right
button and stop printing a `vnc_port` that nothing is listening on (a GL boot
has no VNC server at all). The row's viewer button is now a **View/Hide
toggle** — `engine_hide` existed and nothing had ever called it.

`view` branches on `boot_shows_in_a_window` (are the pixels in a window?)
rather than `boot_has_hidden_window` (was it hidden?), because the second is
False on exactly the boots that most need the window path, and an already-open
window is raised rather than re-set-up.

## 2026-08-16 (later still) — the memory governor, and why Windows still cannot have it

**New: a demand-tracking balloon.** `omnidroid/balloon.py` holds the policy,
`spawn_qemu` applies a **boot cap** before the guest's virtio-balloon driver
has probed, and `omnidroid govern <name>` — started automatically by `start`,
detached like the autocap recorder — then tracks real demand: it grows the
moment the guest's free slack drops below 256 MB, shrinks only in 256 MB
steps, only after usage has plateaued, and never below the mode's floor. Mode
defaults: gaming `balloon_boot` 1536 / floor 1024 / headroom 512, farming
1024 / 896 / 384. 31 unit tests in `tests/test_balloon_governor.py`.

This is **prevention**, not the reclaim that `apply_balloon_target` does. An
uncapped guest goes from 38 MB of host RSS at t+10s to the full `-m` by t+29s,
which is Android's page cache filling whatever it is offered while the client
is still on its loading screen — so the hope was that a host which cannot take
pages *back* might still avoid ever handing them *out*.

**Measured on Windows, and it does not work there.** PS99, `-m 3072`, cap
1536, against an uncapped control:

| | uncapped | boot cap 1536 |
|---|---|---|
| host RSS at rest | 3403–3414 MB | 3335–3345 MB |
| guest cap / using | 3072 MB / — | 1536 MB / 862 MB |
| boot | 0.3 min | 1.4 min |
| `qemu.log` | ~0 | 31 MB |

~60 MB for a 4x slower boot. The host pays for the **union of pages ever
touched**, and the balloon descends at only ~25 MB/s (one failed
`ram_block_discard_range` and one log line per 4 KB page), so it is still
descending while Android boots and the guest touches nearly all of `-m` on
*different* pages. Ballooning during boot increases page-set churn.

So the governor is gated on the existing `host_can_reclaim_balloon()`: it runs
on Linux/macOS, not on Windows. `OMNI_FORCE_GOVERNOR=1` overrides, for
measuring the `docs/windows-ram-discard.md` patch — the same missing `madvise`
makes the descent slow *and* makes reclaim impossible, so that patch fixes
both halves.

**A bug worth keeping.** The first cut's grow rule was `want > cap` alone, so
a healthy guest moved its cap every poll (observed 1536 → 1543 → 1585 MB) and
then oscillated against the shrink rule forever. On Windows that *ratchets* —
each grow lets the guest touch pages it had given back, permanently — and the
oscillating run ended at **3412 MB, indistinguishable from the uncapped
control**. `GROW_TRIGGER_MB` plus the band around it is the fix; the
`Stability` test class is the regression.

## 2026-08-16 (later) — the bar's geometry fix, and a second stale message

**Both defects the hardware pass below found were fixed the same day**, in
`4daa97e` — the entry underneath still describes them as open follow-up
work; this corrects that.

**The bar's size.** The suspected cause logged below — a race between
`root.resizable(False, False)` and `follow()`'s `SetWindowPos` — was wrong.
On Windows, `wm resizable(False, False)` does two things: it strips
`WS_THICKFRAME`/`WS_MAXIMIZEBOX` (wanted), and it also locks the window's
`WM_GETMINMAXINFO` min/max track size to Tk's own ~200×200 default for an
empty toplevel — and Windows **re-enforces that lock on every later
`SetWindowPos`**, which is exactly the measured 216×239 clamp. The fix never
calls `resizable()`: `_strip_resize_border()` strips those two style bits by
hand, the same technique `hostwin.apply_chrome` already uses on QEMU's own
window. `follow()` also reads back the height Windows actually granted — a
`WS_CAPTION` window has a system minimum, measured 40px against the nominal
`BAR_HEIGHT` of 34 — and repositions (never resizes) so the bar's bottom
edge lands exactly on the guest's top edge. Hardware-verified: bar
`(208,168)-(864,208)` against guest `(208,208)-(864,752)`, full width, zero
overlap, zero gap. Kill-safety re-verified across the change:
`totalFrames` 1952 → 2059 across a force-kill of the bar's process.

**The stale `[gpu]` message, and a second copy of it found alongside.**
`qemu_proc.py`'s GL-window `[gpu]` line was reworded away from the deleted
`SetParent` design. The same stale "HOSTS that window inside our own
viewer" phrasing also turned up in `engine.py`'s
`vnc_unavailable_reason()` (surfaced by `cmd_view`'s `no_vnc_gl_window`
failure) — found by reading every `[gpu]`-prefixed message in
`qemu_proc.py` plus its siblings, not just the one flagged below — and
fixed the same way.

`MODES.md`'s known-issue paragraph got the equivalent `### CORRECTION`
treatment rather than a silent rewrite. Full diagnosis, rejected
alternatives, and the new test coverage:
`.superpowers/sdd/2026-08-15-gaming-gpu-window/task-9-report.md`, "Geometry
fix" section.

## 2026-08-16 — gaming's window redesign, measured on real Windows hardware

**The `SetParent`-hosted viewer is gone.** Gaming's GPU window used to make
QEMU's window a *child* of our Tk viewer; force-killing the viewer took the
child down with it — instance alive, answering adb, `totalFrames = 0` forever.
The replacement restyles QEMU's OWN window in place (caption/sysmenu/min/max
stripped, `WS_THICKFRAME` kept) and spawns a thin bar of ours as a separate
process whose window is made an OWNER of QEMU's via `GWLP_HWNDPARENT` — never
the reverse. Destroying an owned window does nothing to its owner, which is
the property this whole redesign exists for.

**Reconfirmed on this box's hardware** (x86 base, `admn1b12farm3`, `--mode
gaming --gpu auto`, QEMU 11.0.50, `gtk,gl=on,show-menubar=off,window-close=off,
zoom-to-fit=on` + `virtio-gpu-gl-pci`):

```
window hidden through boot        confirmed: 30 screenshots sampled every 4s
                                   across a ~117s cold boot, no flash
run.json                          "display_kind": "gl-window"
chrome after `view`               no QEMU menu bar, no second title bar;
                                   QEMU style 0x16040000 (no WS_CAPTION,
                                   no WS_SYSMENU); bar style 0x16CA0008
                                   (has both); bar owner == QEMU hwnd

SurfaceFlinger --timestats, bar running     totalFrames = 3305 / 35s (~94 fps)
taskkill /F /PID <bar pid>                  bar dies; QEMU pid untouched
SurfaceFlinger --timestats, bar dead        totalFrames = 3464 / 34s (~102 fps)
```

Frame production did not dip after the force-kill (94/102 fps here are idle
Android/BlissOS compositing, not PS99 gameplay — see below) — the single most important
check in this pass, and it holds. The close prompt's three buttons (Cancel /
Hide / Stop) were each exercised via UI Automation + synthetic clicks and
behaved as designed: Cancel does nothing, Hide destroys the bar and hides the
window (`list` still shows it running, `view` restores it), Stop powers the
instance off. Separately, `omnidroid view <name> --hide` was run with the bar
open specifically to settle a question code review could not: whether Windows
cascades a bare `SW_HIDE` to an owned window. **It does not**, and the code
already accounts for that (`--hide` persists geometry, kills the bar, clears
its pid file, *then* hides) — reconfirmed on screen: no orphaned bar, no
guesswork left.

**What could not be measured:** the fps figures above are idle Android/BlissOS
setup-wizard compositing, not PS99 gameplay. The saved account's Roblox
session cookie had been server-side invalidated (HTTP 401) since an earlier
network change on this box, and installing a plain (non-Omni-baked) Roblox APK
for the run doesn't pair with the kiosk's session receiver (`no_kiosk_reply`).
This does not weaken the result being measured (window/chrome/ownership/kill-
safety, none of which depend on Roblox) — the existing 24.2–58 fps PS99 band
in `MODES.md` predates this redesign and still stands, since only the window's
ownership and chrome changed, not the render path.

**Bug found by this hardware pass, fixed the same day (see the entry
above):** the bar's on-screen size did not match `bar_geometry()`'s intent —
it should be exactly as wide as the guest window and 34px tall, sitting
flush above it; on this box it instead came up ~216×239px (overlapping the
guest's top-left corner). Position was correct, size was not. Suspected at
the time: `root.resizable(False, False)` in `windowbar.py` runs before the
geometry-setting `SetWindowPos`, and a later `WM_GETMINMAXINFO` clamps the
window back to Tk's own default size — **that suspicion was wrong; the
entry above has the real cause and the fix.** Logged in `MODES.md`.

**Also found, fixed the same day alongside a second copy of it (see the
entry above):** `qemu_proc.py`'s `[gpu]` log line for the GL-window tier
still described the deleted design ("`omnidroid view` HOSTS it inside our
own viewer instead") — stale text from before this plan, printed on every
gaming boot on Windows.

**Local environment notes, not repo changes:** this box's `images_dir`
(`C:\Users\berat\OmniImages`) had no `x86/` subfolder populated even though
`configs/paths.json` (locally modified, uncommitted) already expects one; the
real base files were only present in an old flat layout at
`Desktop\OmniImages`. Fixed locally with four zero-cost NTFS hardlinks into
`OmniImages\x86\` rather than copying ~2.96 GB with ~5.7 GB free. Separately,
`omnidroid` was not pip-installed on this box, so the window bar's non-frozen
dev-mode spawn (`python <path to engine.py> _windowbar ...`, run as a bare
script rather than `-m omnidroid`) could not resolve `from omnidroid import
awake` and crashed instantly (`ModuleNotFoundError`) every time `view` tried
to open a bar. This is a dev-only gap shared with the pre-existing VNC viewer
spawn (same shape, same file) — invisible in the shipped PyInstaller build,
which is frozen — fixed locally with `pip install -e . --no-deps`.

## 2026-08-16 — farming reaches the PS99 world, and a warm POOL

**`--mode farming` gets into Pet Simulator 99 and stays there, squeezed.**
Screenshot-verified in-world: Roblox's top bar, PS99's live player leaderboard
(real usernames, ranks, diamond counts), its chat scrolling and its own
teleport logic running — with the display already shrunk to 480x270 by the
squeeze, which is the state farming exists to produce.

```
start admn1b12farm3 --place 8737899170 --mode farming --mem 3072
  guest MTU 1420 (from this host's egress interface)
  the game has finished loading (2252 MB resident) — squeezing now
  balloon: not inflating — this host cannot return the pages
  boot 155 s; guest 2.9 GB / 587 MB available; game PSS 2.1 GB; host RSS 3239 MB
```

Four things were in the way and only two were bugs here: the squeeze ran
before the client had loaded, the balloon squeezed the guest for no host gain,
Roblox was blocked on this network (GoodbyeDPI passes HTTPS and mangles the
game's UDP — hence `Error Code: 279` on every join), and the guest's MTU did
not fit the VPN's. Each is written up below.

**PS99 needs `--mem 3072`.** At the shipped 2048 the client is OOM-killed,
measured three times with nothing else in the way. That is a per-GAME
property, not a tuning constant — see `FOOTPRINT.md`.

## 2026-08-16 — a warm POOL, and a renderer mask that cannot work here

### The pool: 0.08 s instead of 47-190 s

`omnidroid pool start|fill|status|stop` keeps N instances pre-booted to the
**account-free ready point** — the exact state `warmboot.bake_entry` used to
freeze — and `start` adopts one instead of booting. Instances are diskless
(`snapshot=on`), so every account on one offset boots byte-identical disks;
what makes an instance somebody's is the session broadcast, which arrives long
after boot. That is why an account-free pre-boot is possible at all.

Measured on the x86 base, PS99, gaming 2048 MB / 2 vCPU:

```
pool fill --size 1        slot ready in 58.8 s
start <account> --place   warm pool: took slot _pool0
                          timings.stages.boot   = 0.082 s   (was 47-190 s)
                          session delivered     = 7.6 s
```

This is the answer to "instant boots on Windows", and it works for the reason
the warm CACHE cannot: **nothing is serialised.** WHPX blocks migration at CPU
realize time, so `bake_entry` can never produce an entry there; a pool never
asks it to.

Four decisions that are load-bearing rather than stylistic:

* **A slot is named `_pool<n>` and lives in the ordinary runtime root**, so
  `allocate_ports`, `running_instances`, `instance_live` and
  `reconcile_runtime` all handle it with no special case. It shows up in
  `omnidroid list` tagged `[warm pool]` — hiding it would also hide it from
  the "refuse while an instance is running" guards, which is how a guard
  silently stops guarding.
* **Adoption copies `run.json`; it never moves the directory.** QEMU holds
  `qemu.log` open and Windows will not move a directory out from under an open
  handle. The copy keeps the slot's `identity` (`omni-_pool0`) VERBATIM: the
  QEMU process was named at spawn and cannot be renamed, and `instance_live`
  compares the recorded identity against QMP `query-name` — rewrite it and a
  healthy adopted instance reads as dead.
* **The claim is an `O_EXCL` create**, so two concurrent launches cannot be
  handed one guest. That failure would put the second account's cookie into
  the first account's live game.
* **The key hashes the RESOLVED spec**, not the flags as typed — including the
  offset's image identity (size+mtime), because `offset delete X` +
  `offset create X <other apk>` reuses the name for a different Roblox.
  `--mode playable` and `--mode gaming` therefore share a slot (one machine);
  `--mem 2048` and `--mem 4096` never do.

The manager boots slots one at a time: two guests booting at once on this host
starve each other, and a pool that fills slowly beats one that makes the
instance somebody is playing stutter while it fills.

### The renderer mask: measured, and it kills the client on x86

`omnidroid/glmask.py` puts `MESA_GL_RENDERER_OVERRIDE` / `..._VENDOR_OVERRIDE`
into the game's environment via the `wrap.<pkg>` property, so the client is
told it is running on `Adreno (TM) 650` instead of `llvmpipe` or
`virgl (NVIDIA GeForce RTX 4060/PCIe/SSE2)`.

**It works, the property sticks, and the client dies three seconds later:**

```
Cmdline: com.roblox.client
signal 31 (SIGSYS), code 1 (SYS_SECCOMP)
Cause: seccomp prevented call to disallowed x86_64 system call 165
  #00 libc.so (mount+10)
  #01 libnativebridge.so (PreInitializeNativeBridge+1204)
  #02 libart.so (art::Runtime::Start()+6419)
```

Syscall 165 is `mount`, and the caller is the **arm64 translator setting
itself up**. On the ordinary fork path Zygote bind-mounts the native-bridge
paths before it installs the app's seccomp filter; the `invokeWith` path
re-execs through `/system/bin/sh`, so by then the filter is on and the process
is killed. Roblox is arm64-only, so on the x86 base every launch goes through
that bridge — `wrap.` and arm64 translation are mutually exclusive there.
Clearing the property brings the client straight back.

**So the mask defaults to OFF**, and a boot that does not want it CLEARS the
property rather than merely not setting it (an adopted pool slot warmed with
it on would otherwise still carry it). The two mechanisms that could still
work are both image-side and named in `glmask.py`: `export` in zygote's init
rc, or `setenv()` from OmniBootstrap, which already runs inside the game
process.

### Farming: the squeeze was running at the wrong TIME

A farming instance never reached the PS99 world. The last session blamed the
translator and swapping; both were real observations and neither was the
cause. Re-measured on PS99 with memory held at 3072 MB and no balloon, so
memory could not be the variable:

| | |
|---|---|
| farming, full squeeze | client alive, PSS FLAT ~400 MB, engine parked in `futex_wait`, **guest 200% idle** for 6 min |
| farming, `--quality balanced` (tick 240) | identical stall — the 5 fps tick is not it |
| farming, `OMNI_FARM_SKIP=<every step>` | **PSS 1173 MB at 111 s, and climbing — it loads the place** |
| gaming at the same 2048 MB / 2 vCPU (control) | zero translator aborts; PSS to 1476 MB, then OOM-killed |

**The squeeze ran in `_ensure_booted`, i.e. before `cmd_start` delivers the
session — before the client has been told which place to load.** Every lever
in it exists to make a JOINED, IDLE instance cheap; applied to a client that
is still starting, they starve the load. It moves to
`settle_density_instance()`, called after delivery and after the client's
memory has stopped growing (`wait_for_game_settled`: PSS plateau above a
700 MB floor, so a splash screen never counts). `OMNI_SETTLE_TIMEOUT` /
`qemu.settle_timeout` bounds the wait; a density launch is now minutes rather
than seconds and reports it as `timings.stages.density_settled`.

Two beliefs corrected on the way, both by measurement:

* **Swapping hard does not break the translator.** The gaming control drove
  its zram to `SwapFree: 0.2 MB` with zero aborts.
* **The engine deadlocks, it does not crawl.** `debuggerd -j` on a stalled
  client: Roblox's `Main` and its single ` RBX Worker A` both in `futex_wait`
  (syscall 202, NULL timeout) on a 200%-idle guest.
* **The 5 fps tick cap is innocent.** Swept 5 / 30 / 240 by rewriting
  ClientAppSettings and restarting only the client (the file is read at
  process start, so the restart IS the experiment): all three reached
  "Joining server" within 30 s at 1.7-1.9 GB PSS.

### No balloon on a host that cannot take the pages back

`apply_balloon_target` now declines to inflate where QEMU cannot decommit —
Windows, which has no `madvise` — and says why. MEASURED: guest squeezed to
830 MB by the balloon while the QEMU process held **2190 MB**. The inflate
was never a host saving there; it was a pure cost to the guest, and a large
one — at the 896 MB cap the session handover itself timed out (`pm path` did
not answer in 45 s, twice). The lever that works on Windows is `-m`.
free-page-reporting was already dropped there for the same reason; this is
that fact applied to the explicit inflate. An explicit `--balloon` still wins
outright (`balloon_explicit`), and `host_can_reclaim_balloon()` names the
capability so the logic can be tested on a host that lacks it.

### The guest's MTU has to fit the host's way out

Roblox is blocked in Türkiye, so this host reaches it through a VPN. QEMU's
user networking hands the guest **1500** and then sends its packets out
through the host's stack, which on ProtonVPN is **1420**. TCP survives that
(the ends negotiate an MSS); **UDP does not**, and Roblox's gameplay traffic
is UDP. The result reads exactly like a broken emulator:

```
assets load                → the game's own loading screen appears
the server connects        → "Connection accepted from 128.116.13.34|59036"
the world starts streaming → "Disconnected (Error Code: 277)"
```

`omnidroid/netmtu.py` probes the MTU of the interface this host would reach
the internet through, and `virtio-net-pci,host_mtu=N` hands it to the guest —
virtio has a feature bit for it (`VIRTIO_NET_F_MTU`), so the guest kernel
brings `eth0` up at N with nothing running inside the guest. Only applied when
it is BELOW 1500, so an untunnelled host's command line is unchanged.
`OMNI_GUEST_MTU` / config `network.mtu` override; the probe is memoised per
process and never raises into a boot.

**Two traps, both hit:**

* **The adapter's link MTU is the wrong number.** `GetAdaptersAddresses`
  reports **65535** for ProtonVPN's TUN adapter while the IP interface is
  1420. A probe reading the first would report "no tunnel" on exactly the
  hosts that have one. The IP-layer `NlMtu` is what packets have to fit in.
* **A default argument binds at definition time.** `guest_mtu(probe=host_egress_mtu)`
  ignored a replaced `netmtu.host_egress_mtu` silently — resolved at call time
  now.

**277 now has two distinct causes and they need opposite fixes:** the asset
CDN being DNS-blocked (fixed by Private DNS — see HANDOFF §8b) and this one.
Tell them apart by whether the game's own loading art appears: if it does, the
assets are fine and the problem is the game-server path.

### Also

* `OMNI_FARM_SKIP=<step,...>` leaves named squeeze steps out
  (`farming.STEP_NAMES`). The squeeze has now twice been what stopped Roblox
  running on the x86 base, and bisecting it by editing `farming.py` makes
  every attempt a different build of the product.
* `docs/windows-ram-discard.md` — the `DiscardVirtualMemory` patch for
  `ram_block_discard_range()`, written and reviewed but NOT built (this host
  has 6.3 GiB free disk), plus the measurement that matters more: one PS99
  client needs ~1.5 GB resident in-world, so 30 instances in 32 GB is out of
  reach whatever the hypervisor does.

## 2026-08-15 (night) — farming's real blocker was the arm64 translator

A farming instance booted, joined PS99 and then never left the Roblox splash.
Four suspects were eliminated by running them (`--quality balanced` for the
5 fps tick, the new `--guest-display native` for the 480x270 display, and
reading the trim list and the doze sequence). The fifth, memory, was eliminated
by `--mem 4096 --balloon 3072` — and that run is the one that gave the answer,
because with **2.1 GB free and no OOM kill** Roblox still died:

```
F libc  : Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid (Thread-19)
F DEBUG : Abort message: 'Cannot process signal 11'
F DEBUG : #04 libndk_translation.so (ndk_translation::HandleHostSignal(...))
```

The TRANSLATOR aborted. Roblox is arm64-only and the x86 base runs it through
`libndk_translation`; translated code took a SIGSEGV the translator's
host-signal handler could not process. Farming is the only mode that swaps hard
(`swappiness 100`, `page-cluster 0`, zram on), and evicting translated code
pages is how that fault gets made.

`MODES["farming"]` now carries `swappiness_x86: 10` and `zram_x86: False`. The
arm base runs Roblox natively, has no translator to upset, and keeps both
levers — so this is an ARCH OVERRIDE, not a retreat. `resolve_mode`'s arch
mechanism was generalised from a fixed key list to any `<key>_<arch>` for it.

Measured with the swappiness half alone: **translator aborts 1 → 0**, and the
client got past the black splash onto Roblox's loading screen.

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

**Two traps inside the fix:**

* Skipping the zram step is not the same as zram being off — the base ships it
  ON (`persist.sys.zram_enabled` in build.prop + `zram.rc`'s `swapon_all`). A
  launch that had just printed "zram: OFF for this mode" still had
  `SwapTotal: 1045168 kB`. x86 farming now issues an explicit `swapoff`.
* With zram genuinely off, `apply_balloon_target` correctly selects the
  non-zram floor (1536 MB) instead of 896 — it probes the guest rather than
  trusting the mode. That is the honest cost of not being able to swap here.

## 2026-08-15 (evening, later still) — three viewer states, and a knob to ask farming a question

Two follow-ups to the embedded viewer, both found by running it rather than by
reading it.

**`EnumWindows` does not list child windows, and that broke the guard.** Once
the viewer embeds QEMU's window it IS a child, so it vanished from the search
-- and a second `omnidroid view` on an already-open instance reported the
guest's display as DESTROYED. Three states now, told apart properly:

| | |
|---|---|
| window found, not a child | embed it |
| window found, already a child | a viewer has it — bring that one forward |
| window not found at all | the display really is gone (a force-killed viewer); say so |

`hostwin` walks children as well as top-level windows and can match on the QEMU
**pid** from run.json, not only the title, so it works when several instances
are up and when a title is not what we expect.

**`--guest-display WxH|WxHxDPI|native`.** The GUEST display (`wm size`, what
Android lays out at) is not the PANEL (`--panel`, the virtio-gpu device's mode),
and farming shrinks the first one hard: 480x270 at 80 dpi. A farming instance
boots, joins PS99 and then never leaves the Roblox splash, and that postage
stamp is one of the two remaining suspects (the other is the 896 MB balloon).
`--quality balanced` already ruled out the 5 fps tick cap. This flag makes the
question answerable with a launch instead of an edit.

`None` is a real value here ("leave the base's own resolution alone"), so the
absent case is the string sentinel `"unset"` — the same trap `--balloon 0`
documents, and the reason `resolve_mode` grew a sentinel rather than another
`None`.

**And a third adb timeout came out of `start` as a traceback**, which is twice
too many for one afternoon. A squeezed farming guest answers adb LATE, and the
launch path is full of probes with 8-45 s budgets. First it was the ordered
`am broadcast` that hands over the session; then `pm path` checking the kiosk
is installed (15 s) -- on a boot that had ALREADY joined a place. Fixed as a
class rather than one at a time:

* `adb.adb_soft()` returns a CompletedProcess with `returncode -1` instead of
  raising, so "no answer" can be handled like "answered with nothing".
* `kiosk_installed()` uses it, with a 45 s budget and one retry, and tells a
  late answer apart from a real "not installed" (which does not retry).
* **`deliver_session()` now enforces the promise its docstring always made** --
  the whole delivery is wrapped, so any failure inside becomes
  `{"delivered": False, "reason": "delivery_error"}` and the launch reports it
  instead of dying. `tests/test_slow_guest_tolerance.py` pins the property
  rather than the numbers.

## 2026-08-15 (evening, later) — you only ever see OUR viewer

The rule the product needed: a QEMU window must never be what the user looks
at, in either mode, on any host. On Windows that collides with two QEMU facts
measured earlier the same day -- no GL context without a window, and no VNC
server beside one.

**Resolved by not fighting either.** The window is opened (QEMU needs it),
hidden immediately, and `omnidroid view` REPARENTS it into our own Tk window
with `SetParent`. The guest then lives inside the product's viewer -- our
title, our chrome -- with QEMU's GL surface composited straight into it.

Three measurements this rests on, all on PS99:

| | |
|---|---|
| a hidden window keeps rendering | 303 frames / 30 s while invisible |
| hiding at spawn does not break the boot | booted and joined normally |
| embedded in our viewer | **702 frames / 12.1 s = 58 fps** |

Nothing is copied, encoded or decoded per frame, and input goes into the
guest's `usb-tablet`/`usb-kbd` directly rather than being synthesised from an
RFB event -- lower latency than a framebuffer protocol can reach. The
alternatives were all worse: VNC does not exist on that boot, QMP `screendump`
answers "no surface", and `screenrecord` over adb costs a guest-side H.264
encode on the CPU that is already the bottleneck.

New: `omnidroid/hostwin.py` (find/hide/keep-hidden/show a QEMU window),
`omnidroid/embedview.py` (the reparenting viewer + its `_embedview`
subcommand). `omnidroid view` picks the embedded viewer automatically on a boot
whose window is hidden, and the VNC viewer everywhere else -- Linux presents
windowless through `egl-headless` and macOS has no virgl, so both are unchanged.

**Two behaviours found by measurement and handled:**

* **GTK re-shows the window during early boot.** A single hide at spawn was
  undone by the time the guest had joined a place. `keep_hidden()` re-hides for
  the length of a boot and then stops; afterwards one hide sticks (30 s of
  polling never saw it return).
* **A force-killed viewer destroys the window and blinds the guest.** Windows
  destroys a child window with its parent and QEMU does not make another: the
  instance stays alive on adb and renders nothing (`totalFrames = 0`). Closing
  the viewer with its X is clean. `omnidroid view` detects the destroyed-window
  state and reports it instead of opening an empty window onto a blind guest.

**Also measured: asking for more than the base's native panel costs and buys
nothing.** `--panel 1080p` took 3.3+ minutes without reaching adbd (0.8 at
1280x800), with QEMU alive and zero scanout errors -- and the guest came up
**still 1280x800**, confirmed by `screencap`. It ignores a mode its panel does
not carry, after a long stall trying. Smaller-than-native panels are fine
(farming uses 640x480). The ceiling is the base's mode list, not the engine.

**And ruled out, with QEMU's own log as the evidence:** `-display
dbus,p2p=on,gl=on` starts cleanly on Windows and fails exactly like
`egl-headless` -- 602 `ctrl 0x103, error 0x1203` rejections in one boot. So the
scanout failure is not about dmabuf or about a particular display backend: it
is that every windowless GL display on Windows takes its context from ANGLE at
ES 2.0, and virglrenderer cannot serve a scanout from one. The GTK path gets
desktop GL through WGL, which is why it is the only one that works.

## 2026-08-15 (evening) — two modes, an explicit GPU policy, and three walls named

Four goals: instant boots, native speed at high resolution, two modes instead
of five, and headless-QEMU-with-a-VNC-viewer that keeps the GPU. Three landed.
The fourth is not achievable on a Windows host, and the reason is now measured
rather than suspected. Every number below is off a live instance running **Pet
Simulator 99** (place `8737899170`) on the Windows box (i7-13700F, RTX 4060).

### Five modes became two

`gaming` (the old `playable`) and `farming`. `playable` was `gaming` without a
window and there is no window any more; `hard`/`brutal` were fixed RAM tiers
that `--mem`/`--smp` already express. **All three retired names are still
accepted** and resolve to `gaming` in `resolve_mode()` — omni-executor persists
the chosen mode and 1.0.14 ships `"mode": "playable"`, so rejecting them would
break every launch from a client that has not updated. Nothing downstream ever
sees the old name.

### The GPU is a policy now, not an accident

`--gpu auto|headless|window|off` (config `qemu.gpu`, env `OMNI_GPU`, and a
"Graphics" selector in the app's Launch panel). `auto` reaches the GPU whatever
it takes, preferring no window; `headless` never puts one on screen.

**The bug this replaced:** `-vnc` was being dropped for ANY GL context. QEMU
only refuses it beside a **windowed** GL display — `egl-headless` is the
display it documents as the one to pair with VNC. So every GPU-accelerated boot
ran with no VNC server at all, and the missing server was reported as "the
viewer is black". `blocks_vnc()` scopes the drop correctly and
`tests/test_qemu_accepts_devices.py` constructs a real QEMU to prove
`virtio-gpu-gl-pci` + `egl-headless` + `-vnc` is accepted.

**And why `auto` still opens a window on Windows:** `egl-headless` there
renders on the GPU and never presents. Three boots (plain,
`blob=true,hostmem=512M`, and without a forced `video=` mode), all identical —
`[drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1203 (command 0x103)`
(SET_SCANOUT → ERR_INVALID_RESOURCE_ID), `totalFrames = 0`, black. The guest's
GL was healthy the whole time (`virgl (ANGLE (NVIDIA … RTX 4060))`, no GL errors
in logcat). `HEADLESS_GL_PRESENTS` records that per platform; Linux keeps it on.

| at 1280x800, in-world | fps | VNC viewer |
|---|---|---|
| `--gpu auto` / `window` | **24.2–44.8** (two runs) | ✗ |
| `--gpu headless` | 3.2 | ✓ |

`omnidroid view` on such a boot now explains itself instead of timing out on a
port nobody is listening on, and autocap skips rather than spinning.

### `--panel`: the guest display is settable

`--panel 1920x1080` / `720p` / `1080p` / `1440p`, config `qemu.panel`, env
`OMNI_PANEL`. `gaming.density_for_panel()` scales the guest dpi with it so a
bigger panel does not shrink Roblox's on-screen controls.

`video=Virtual-1:<mode>` was tried for the GL boot's 640x480 physical mode and
**reverted**: it changed nothing on the headless paths and **hung the windowed
GL boot** (five minutes without adbd, QEMU alive and `running` over QMP). Kept
behind `qemu.force_video_mode`, default off.

### Instant boots: three blockers, two fixed, one final

The warm-boot cache had never produced a single entry on Windows.

1. **Disk, silently** — `has_room()` wanted `projected + 10 GiB` against 7.4 GiB
   free and printed nothing. Reserve is configurable (`qemu.warm_reserve_gb` /
   `OMNI_WARM_RESERVE_GB`) and `room_report()` lets a skipped bake say why.
2. **QEMU cannot migrate to a FILE on Windows** — "Failed to set FD nonblocking:
   Input/output error"; Windows has no non-blocking file handles.
   `mapped-ram`+`multifd` killed the QEMU process outright. New
   `omnidroid/migfile.py` relays the stream over a loopback socket (0.2 s for a
   throwaway guest) and keeps `file:`+mapped-ram+multifd for Linux/macOS; the
   transport is recorded in the entry's meta.json. A unit test firing a real RST
   at the relay caught it publishing a truncated entry as a success, so
   `_shortfall()` cross-checks bytes written against `ram.transferred`.
3. **WHPX blocks migration outright** — "State blocked due to non-migratable
   CPUID feature support,dirty memory tracking support, and XSAVE/XRSTOR
   support", verbatim from QEMU's `whpx-all.c`. Nothing changes it.
   `_warm_cache_allowed()` refuses under a non-migratable accelerator so the
   cost is not paid on every launch. The cache still works on KVM and HVF.

For fast boots on Windows the answer is a warm **pool** (N instances pre-booted
to the account-free ready point), not a snapshot.

### Farming on x86 works now, and its footprint is a Linux story

Two bugs that made an x86 farming launch fail outright:

* **`smp 1` could not carry the session handover.** Roblox's arm64 build runs
  through `libndk_translation`; the ordered `am broadcast` did not return within
  45 s and adb's `TimeoutExpired` came out of `cmd_start` as a traceback.
  `smp_x86: 2`, `kiosk_broadcast` treats a timeout as a result, budget 120 s.
* **The balloon was called missing when it was slow.** At 30 s a 2048→896 MB
  inflation read 1805 MB and printed "guest balloon driver missing?"; the same
  guest reached target a minute later. 90 s now, and a moving balloon says so.

Measured after both: balloon reaches `897 MB (target 896)`, game 508 MB PSS /
743 MB RSS in-guest — and **host RSS 2198 MB**, because QEMU on Windows has no
`madvise`, so `ram_block_discard_range` fails and the pages the guest returns
are never released. On Windows the only lever is `-m` itself. The ~400–900 MB
per-instance story needs Linux.

**Still open:** a farming instance boots, joins, stays alive at ~48% of one core
and never leaves the Roblox splash on PS99. `--quality balanced` made no
difference, so the 5 fps tick cap is not the cause; the same place used 2.36 GB
PSS in a 4 GB gaming instance, so memory is the prime suspect.

### Smaller

* `omnidroid measure` reports host RSS on Windows (`ps` → GetProcessMemoryInfo);
  it printed `-` on the platform the product ships on.
* `arm_edk2_code()` looks beside the resolved QEMU, so the arm firmware is
  findable on Windows at all (the candidate list was three POSIX paths).
* The built-in viewer polls at 8 ms instead of 40 ms and blits into the
  existing Tk pixmap instead of allocating a ~4 MB PhotoImage per frame.

## 2026-08-12 — the never-blank guarantee is actually applied now

`omnidroid/awake.py` shipped complete on 2026-08-09, with tests and a HOWTO
section describing it as applied on every boot — and **nothing ever called
it**. No engine import, no CLI command: the module was dead code, the
documented guarantee did not exist in the product, and the 33 red tests in
`test_gaming_apply.py` + `test_warm_boot_policy.py` were all mocking an
`engine.apply_awake` that had never been written.

It is now wired the way those tests already specified: `apply_awake` in the
shared post-boot tail, EVERY boot and EVERY mode, above the profile branch
(so each mode stays the last writer on the display levers it owns) and inside
the shared tail (so a WARM RESTORE is kept awake too — a restored instance
that blanks is the same bug as a cold-booted one). It reports the reading, not
the intent, and names the step it had to skip without root.

**No watchdog and no `awake` command**, deliberately: the guarantee is
Android's own developer settings, written once into /data where they stay.
Developer options is now enabled too (`development_settings_enabled=1`) so
**Stay awake** is the real, visible Developer-options toggle rather than an
invisible provider row. `build_awake_recheck` is left in the module unused —
it was already unused before this change.

Verified on the case that used to fail: with the AC coincidence removed
(`dumpsys battery unplug` → `mIsPowered=false`) the instance still reads
`mWakefulness=Awake`, `Screen off timeout: 2147483647 ms`, kernel wakelock
`omni_awake` held. The pre-change build reached `mWakefulness=Dozing` in 22 s.

## 2026-08-12 — unattended consent: no permission taps, no ANR/crash dialogs

An instance that stops for a modal is not headless. Two of those modals were
reachable in normal use: Roblox/Arceus asking for **full disk access** and for
**install-unknown-apps**, and the framework's own **"… keeps stopping" / "isn't
responding"** dialogs, which park over the screen until somebody dismisses them.

New pure module `omnidroid/consent.py` (shaped like awake.py/farming.py: it
BUILDS the guest sequence, the engine applies it) and a new post-boot step
`apply_consent`, called on EVERY boot between `assert_kiosk_game` and the mode
tuning:

  * `settings put global hide_error_dialogs 1`
  * app-ops set to `allow` for every covered package: MANAGE_EXTERNAL_STORAGE
    (full disk access), REQUEST_INSTALL_PACKAGES, LEGACY_STORAGE,
    READ/WRITE_EXTERNAL_STORAGE, SYSTEM_ALERT_WINDOW
  * every DANGEROUS runtime permission each package's own manifest declares,
    read from `dumpsys package` rather than from a hardcoded list

Covered packages are the third-party ones **plus the game** — the game is an
updated system app, so `pm list packages -3` does not list it, and a `-3`-only
loop would have skipped the one package the feature exists for.
`OMNI_NO_CONSENT=1` turns the whole step off (you want the dialogs back when
reproducing a crash-loop by eye).

MEASURED on a live arm64 instance (Android 16 / SDK 36) — all four of these
changed the implementation:

  1. **`hide_error_dialogs` is read live.** A/B'd by crashing a stock app
     (`am crash com.android.settings`): at 0 the "Settings keeps stopping"
     dialog appears, at 1 the identical crash leaves the screen untouched — no
     configuration change, no framework restart. So this step touches no
     `wm size`/`wm density`, which would have silently undone a farming boot's
     480x270.
  2. **None of it needs root** — `appops`, `pm grant` and `settings` all work
     as uid shell, so it applies on an unrooted deployment too.
  3. **Only the settings half is IMAGE state.** `omnidroid offset consent
     <name>|--all` bakes the policy into an offset image (overlay-then-commit,
     so a failed bake leaves the image byte-identical). Booting the committed
     image with the boot-time step disabled shows `hide_error_dialogs=1`
     persisted — but the app-ops read back as `default`, on both images, twice.
     They reach disk within the boot (`appops write-settings` survives an
     `appops read-settings` round trip) yet the permission APEX
     (`/data/misc_de/0/apexdata/com.android.permission/`) re-derives them at
     boot. Runtime grants are worse: no shell verb flushes them at all. The
     bake therefore claims ONLY the dialog half — `consent.baked_summary` is a
     separate function from `summary_line` precisely so it cannot overclaim.
  4. **SYSTEM_ALERT_WINDOW is a no-op for a package that doesn't declare it.**
     `appops set` exits 0 and the mode still reads `default` (Roblox does not
     declare it). Kept in the list for a companion/executor APK that does, and
     deliberately excluded from the probe so it can never fail a bake.

NOT automated, deliberately: after REQUEST_INSTALL_PACKAGES is granted, an app
that installs an APK still gets PackageInstaller's own confirm screen ("Update
this app?"). No setting suppresses it — it is a platform consent step, and the
only ways past are a UI tap or a privileged installer.

## 2026-08-12 — images_dir is classified by architecture, and carries only live images

`images_dir` was a flat pile of 33 files — every base lineage ever built,
their `.bak`/`.safebak-*` copies, and the removed dev base — 30 GB of which
only ~5 GB was reachable from the config. It is now two arch folders holding
exactly what the manager opens:

    images_dir/
      arm/  base_arm_system_rooted.qcow2   the booted system (standalone)
            base_arm_data_rooted.qcow2     the PRISTINE /data
            base_arm_data_offset_*.qcow2   one thin overlay per Roblox build
            base_arm_efivars.fd            base_arm_devkit.qcow2
      x86/  base_x86.qcow2 / .kernel / .initrd.img
            base_x86_devkit.qcow2          data-template-8g.qcow2
      warm/ the warm-restore boot cache (arch-neutral, unchanged)

**The subfolder is part of the recorded name.** Every `images_dir / <name>`
join is unchanged; what changed is the constants in `bases.py`
(`ARM_DIR` / `X86_DIR`), `offsets.offset_image_name`, and the values in
`configs/paths.json`. Two rules follow from qcow2 backing references, which
resolve relative to the OVERLAY's own directory:

  1. a base and every overlay of it live in the SAME arch folder — that is
     what keeps `bake_offset`'s `rebase -u -b <bare name>` (and therefore a
     relocatable images_dir) working, and
  2. anything writing a backing reference must strip the prefix — hence
     `Path(src_name).name` in `bake_offset`. Writing the recorded name would
     look for `arm/arm/base_arm_data_rooted.qcow2` and the offset would not
     open.

**`base_disk` is gone from the arm base.** `base_arm_system_rooted.qcow2` is
standalone (no backing file) and is what boots; the v1/v2 lineage under it was
never opened, only validated for existence. `autoregister_bases` now accepts
EITHER shape — the rooted pair alone, or the legacy `base_arm.qcow2` +
overlay + data trio (which still records `base_disk`) — because a base needs
only what it boots: system + data + efivars.

Archived out of `images_dir` (moved, not deleted, to
`~/Desktop/OmniImages-backup/`): the v1/v2 arm lineage, the unrooted
system/data pair, every `.bak`/`.safebak-*`, the removed dev base
(`base_arm_devsystem*`, `base_arm_devdata*`), the unreferenced
`data-template-arm.qcow2` / `efi_vars_arm.fd`, and four superseded offsets
(`legacy`, `arceus`, `patched`, `arceus-local`), which were also unregistered.
`arceusae` (default) and `arceusfull` remain.

Verified on the arm64 Mac: two instances booted side by side on different
offsets (`arceusae` + `arceusfull`), each with its own APK inside
(md5 `97e2b57c…` vs `c48d79b4…`), both backed through `arm/`. Suite: 655
passed, 33 pre-existing failures (`engine.apply_awake` harness drift, present
at HEAD before this change).

## 2026-08-09 — An instance can no longer blank: the never-sleep guarantee

Instances went black after a stretch with no input. Diagnosed on a live arm
instance rather than from the docs, and the reading is the point:

```
$ settings get system screen_off_timeout       ->  -1
$ dumpsys power | grep 'Screen off timeout'    ->  Screen off timeout: 10000 ms
```

`-1` reads like "never" and is not: PowerManagerService clamps the setting up
to `mMinimumScreenOffTimeoutConfig`, so **every instance shipped with a ten
second blank**. It only looked healthy because the guest reported AC power and
`stay_on_while_plugged_in` happened to cover AC — a coincidence, not a
guarantee. `dumpsys battery unplug` put the same instance into
`mWakefulness=Dozing` inside 22 s.

**New `omnidroid/awake.py`** builds the sequence that closes all six
independent rungs of the sleep ladder (`screen_off_timeout`,
`stay_on_while_plugged_in`, `sleep_timeout`, `attentive_timeout`,
`adaptive_sleep`, the dream manager), plus the `dumpsys battery` override
WITHOUT WHICH the stay-on setting is a silent no-op — it is a mask of plug
types, and a QEMU guest with no battery HAL reports nothing plugged in. Root
adds a kernel wakelock so the guest cannot suspend either. Four adb round
trips, not fifteen: the settings writes ride in one shell script.

* **Applied on EVERY boot in EVERY mode** (`apply_awake`, in `_ensure_booted`'s
  shared tail, so a warm-restored instance gets it too). Deliberately ABOVE the
  profile branch — a dark screen is not a mode trade-off. It is also
  deliberately not `deviceidle`: doze stays the mode's decision.
* **The watchdog re-asserts every 5 min.** The battery override is the one
  lever that expires (a framework restart drops it).
* **`omnidroid awake <name> [--check]`** applies or reads it back for an
  instance already running.
* **Verified against `dumpsys power`, never `settings get`** — the whole bug is
  that those disagree. `never_blanks()` is a separate question from
  `is_awake()`: the instance that was 10 s from black read `Awake`.
* Kiosk `MainActivity` takes `FLAG_KEEP_SCREEN_ON` / `TURN_SCREEN_ON` for the
  stretch before the game fronts. Needs a `launcher/build.sh` + base re-bake to
  ship; the host-side half above is what is load-bearing and it needs neither.
* Kill switch `OMNI_NO_AWAKE=1`. `tests/test_awake.py` (34 tests) covers the
  builders, the quoting, and the parser against the real captured dumps.

## 2026-08-08 — OFFSETS: many Roblox versions on one clean base; playable takes the machine; one debugging surface

Three changes, driven by one requirement each.

### 1. The base ships no Roblox. Versions are offsets.

`bake-data-game` baked into ONE fixed filename (`base_arm_data_game.qcow2`)
and then pointed `bases.<tag>.data` at it. Two consequences the product could
not live with: the BASE carried a Roblox version, so "which Roblox am I
running?" was a property of the image rather than of the launch; and a second
version could not exist — baking build B destroyed build A, and getting A back
meant re-baking from an APK you might no longer have.

An **offset** is a named, thin qcow2 COW overlay of the base's PRISTINE /data
carrying one baked build. Offsets are siblings, one is the DEFAULT, and the
base's own `data` stays pristine forever.

```
omnidroid offset create 2.731.944 --apk roblox.apk   # ~2 min, base untouched
omnidroid offset create test --apk build.apk         # coexists with the above
omnidroid offset list | show | default <n> | remove <n>
omnidroid start alice                                # the DEFAULT version
omnidroid start alice --offset test                  # a specific version
omnidroid start alice --no-offset                    # the clean base
```

- **Per LAUNCH, never per account.** Nothing about an account selects a
  version; cookie injection into the bootstrapped Roblox is unchanged.
- **Anti-chaining, kept.** Every offset overlays the pristine /data
  (`data_bake_source`), never another offset — so re-baking stays as cheap as
  the first bake and deleting one offset cannot harm another.
- **No silent fallbacks.** An unknown `--offset` is a hard `no_offset` error
  listing what IS baked; two offsets with no recorded default is
  `no_default_offset`. Running the wrong Roblox under the right name is the
  most expensive way for this to be wrong.
- **Existing installs migrate themselves.** `autoregister_bases` detects a base
  still pointing at the old single bake, adopts that image as offset `legacy`
  (default), and restores `data` to the pristine image. Idempotent.
- `bake-data-game` survives as a deprecated alias for
  `offset create --default --force`.
- `omnidroid bake-game --remove` is new: it strips the Roblox baked into the
  system image's `/product/app/Roblox`, so the base is clean at that layer too.
  Build-machine command (e2fsprogs + ~6 GiB scratch). Until it is run, that
  copy is simply shadowed by every offset's `pm install -r -d`.

### 2. `playable` takes the machine; `farming` still gives it back

`playable` is the DEFAULT mode, what a human plays in AND what the AI tests
in, so it now sizes itself to the host instead of sitting at a constant
4096 MB / 4 vCPU:

```
mem = clamp(min(host_ram/2, host_ram - 6 GB), 4096, 8192)   # 512 MB steps
smp = clamp(host_cores - 2, 4, 8)
```

The reserve is the load-bearing half: a guest sized past the host's spare RAM
makes the HOST swap, and a swapping host misses QEMU's vCPU deadlines — slower
than the smaller guest would have been. `--mem`/`--smp` (the latter is new)
win outright, and an unreadable host falls back to the old constants: an
unreadable host costs you the upgrade, never the boot.

New `--quality high|balanced|low` selects the Roblox `ClientAppSettings`
profile; `playable`/`gaming` default to the new **`high`** profile (quality
level 10, post-FX on, DPI scaling on). Rationale: the AI reasons about
screenshots, and a screenshot at quality 3 with post-FX off is a screenshot of
a different program. MSAA stays at 0 — the guest has no 3D acceleration on the
primary host, so it is the one quality key that multiplies per-pixel cost for
almost nothing.

**A real bug fixed on the way.** `_ensure_booted` branched on `mode_name` —
the raw `--mode` argument — against the literals `"gaming"` and `"farming"`.
A bare `omnidroid start` passes no `--mode`, resolves to `playable`, and
matched NEITHER: the most-used mode was the only one receiving no post-boot
tuning at all. Modes now declare a `profile` (`performance` | `density`) and
the engine branches on that. `tests/test_gaming_apply.py::PlayableBoot` pins
it.

### 3. One debugging surface, for people and for both AIs

```
omnidroid debug-info <name>          # what can I actually do to this instance?
omnidroid su <name> -- <command>     # root, correctly quoted
omnidroid frida <name> [--status|--stop]
```

`su` exists because three silent traps were being re-derived (and re-broken)
by every caller: Magisk's `su` is not on `$PATH`; MagiskSU permutes argv so
`su 0 id -u` is read as an su OPTION; and `adb shell` joins-and-reparses its
argv so an unquoted `a; b` runs a fragment of itself and still reports
success. It fails with `no_root` rather than quietly running as uid `shell` —
a silent privilege downgrade produces wrong output that looks right.

`frida` starts the devkit's hidden frida-server and `adb forward`s it to a
host port, printing the `-H` target. It distinguishes "not a debug boot" from
"debug boot but unrooted", because those need different fixes and the message
that conflated them sent people to the wrong one. Status probes the PORT, not
a process name — the server deliberately runs under a randomized name.

`debug-info` reports what IS true rather than what was requested, with a fix
attached to each missing capability.

`omnidroid version` now advertises `capabilities.offsets`,
`capabilities.debug` and the per-base offset registry, so a client can offer a
version picker and know whether a bare `start` will resolve at all.
`omnidroid doctor` reports offsets and hints when none is default.

536 tests pass (was 457 before this work; 79 new).

## 2026-08-06 — `bake-data-game`: the game lives in /data, and updating it is one command

`omnidroid bake-data-game [apk]` installs the game into the base's **/data** and
bakes `omni_game_package` there, then points the base at the result.

Why /data: `omnidroid bake-game` writes the APK into `/product/app` inside the
2.3 GB system image, so every Roblox update meant a new base and ~6 GiB of
scratch. `pm install -r -d` lands an UPDATED SYSTEM APP in `/data/app`, which
is all a kiosk that launches by package name needs, and the package name never
changes between Roblox versions. An update is now one ~2-minute run.

The output is a THIN COW overlay of the pristine rooted /data (32 MB for the
setting alone). Every re-bake starts from that same pristine image —
`data_bake_source()` reads `root_manifest.rooted_data`, never the current
`data`, so updates never chain overlays and no superseded APK is carried
forward. Verified: a bad 221 MB capture was repaired back to 32 MB simply by
re-running the command.

This also removes the last of the kiosk race. With the setting already in
/data, the kiosk picks the right package at ITS OWN boot:

```
lock task configured for [com.omni.kiosk, com.roblox.client]
launching com.roblox.client (boot)
```

— correct whitelist from the start, and no `Attempted Lock Task Mode
violation` at all.

**A failed install must fail the bake.** The first version of the script
checked only that the SETTING read back, so this shipped:

```
guest: Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package
       com.roblox.client signatures do not match newer version; ignoring!]
OMNI_GAME_BAKE_OK
captured -> base_arm_data_game.qcow2 (221 MB thin overlay)
```

`pm install` exits 0 and prints its verdict on stdout, so the exit status says
nothing. The script now greps for `Success` and aborts with a distinct marker
otherwise, and the command explains the cause.

**Constraint this surfaced, and it shapes the update workflow:** a replacement
APK must be signed with the SAME key as the build baked into the system image.
An officially-signed Roblox will not install over a re-signed one, or the
reverse. Use APKs from the same signing pipeline as the baked build.

## 2026-08-06 — fix: Roblox black-screened because the settings installer took its files/ dir

Not the APK, and not "the pre-installed Roblox is flagged" as `FOOTPRINT.md`
recorded — `farming.build_client_settings_script` was locking the game out of
its own data directory.

The script runs as root and starts with `mkdir -p
/data/data/com.roblox.client/files/ClientSettings`, which CREATES THE
INTERMEDIATE `files/` as root:root when it does not already exist. The chown
that follows only covered the leaf. Measured on a live instance — every other
directory in the sandbox belongs to the app, and one does not:

```
drwx------ 12 10138 10138  /data/data/com.roblox.client/
drwxrwx--x  2 10138 10138  ./databases
drwxr-xr-x  3 0     0      ./files              <-- root:root
drwxr-xr-x  2 10138 10138  ./files/ClientSettings
```

so the game could not create anything under its own `files/`:

```
E SplitCompat:      Unable to create directory: .../files/splitcompat
E CrossProcessLock: .../files/generatefid.lock: EACCES (Permission denied)
E FA:               .../files/google_app_measurement.db: EACCES
```

It never finished initialising and dropped out of the foreground, which is the
black screen. `files/` is now chowned and `restorecon`ed alongside
`ClientSettings/` — but deliberately NOT with a blanket `chown -R` over the
package dir, because `cache/` and `code_cache/` are owned `10138:20138` and a
recursive chown would corrupt their group.

Verified live: `files` owned `10138 10138`, and
`topResumedActivity=com.roblox.client/.ActivityNativeMain` with the account
logged in and the home screen rendering. The only dialog left is Roblox's own
"your version is out of date" — which is what `bake-data-game` is for.

## 2026-08-06 — fix: instances came up showing Magisk instead of the game

Reported as "when I open an instance it launches with magisk, not the apk".
Traced on a live rooted arm instance; the whole chain is recorded in
`assert_kiosk_game`'s docstring and `tests/test_kiosk_boot_app.py`.

The kiosk chooses what to launch in `MainActivity.resolveGamePackage()`. When
`Settings.Global.omni_game_package` is unset it falls back to a dev-mode guess:
"the first launchable NON-SYSTEM app". Observed timeline:

```
02:15:40.882  OmniKiosk: launching com.topjohnwu.magisk (boot)
02:15:43.274  settings put global omni_game_package com.roblox.client
02:15:44.904  E ActivityTaskManager: Attempted Lock Task Mode violation
                 r=...com.roblox.client/.ActivityProtocolLaunch
```

The setting arrived 2.4 s AFTER the kiosk had already resolved, launched and
PINNED its choice under Lock Task. Four things combined:

- `omni_game_package` was only written by `deliver_session` (whose own comment
  says it is for "a LATER REBOOT") and by `provision_settings`, which never
  runs on the arm bases;
- instances are EPHEMERAL, so `/data` is discarded at power-off and that later
  reboot never inherits it — every boot came up with the setting unset;
- rooting production (the dual-use change) installed the Magisk MANAGER as a
  launchable non-system app, so the guess started landing on it. Roblox is a
  SYSTEM app (`/product/app/Roblox/Roblox.apk`) and can never win that scan;
- the kiosk then whitelisted and pinned MAGISK for Lock Task, so the real
  game was not on the whitelist when the session arrived and its launch was
  refused.

`_assert_kiosk_foreground` — which force-stops the Magisk app and re-fronts
the kiosk — existed, but its only caller was inside `_devkit_activate`, i.e.
only on a `--debug` boot. That is exactly the dev-base-era gating the dual-use
change was meant to remove; its own spec says this should be unconditional.

Fixed at the cause: `assert_kiosk_game` now runs on EVERY boot, writes
`omni_game_package` before the session is delivered, and then re-fronts the
kiosk. Best-effort throughout — it reports and returns rather than failing a
boot. Verified live: Magisk no longer takes over, the kiosk is the resumed
activity, the Lock Task whitelist is `[com.roblox.client, com.omni.kiosk]`,
and Roblox now actually starts (it previously never did).

STILL OPEN, and needs a kiosk APK change rather than a host one: Roblox starts
but does not reach the foreground — the kiosk stays on top showing its black
view while the game sits frozen in the background. `launchGame` sets
`launchedThisBoot = true` even when the START was refused by Lock Task, and
`onResume` therefore never retries. The clean fix is to bake
`omni_game_package` into the base `/data` so the kiosk's FIRST
`configureLockTask()`/`launchGame()` already picks the game and no race
exists; failing that, stop latching on a refused start.

## 2026-08-06 — `gaming` mode: the second use case gets its own tuning

The engine now serves two jobs explicitly instead of one job with tiers. See
`MODES.md` for the full comparison; `FOOTPRINT.md` still owns the farming
numbers, which this change does not touch.

`omnidroid start <acct> --mode gaming` opens a native QEMU window on the host,
returns the guest to its native resolution, zeroes the animation scales,
disables doze, drops swappiness to 10, installs a 240 fps ClientAppSettings
profile, and — after the session lands, because that broadcast is what starts
the game — pins the game to the `top-app` cpuset. Verified live end to end;
every lever was read back out of the guest, not inferred from a log line.

`gaming` is a NEW mode rather than a change to `playable`, because `playable`
is `DEFAULT_MODE`: teaching it to open a window would have put a QEMU window
on every existing `omnidroid start`, including automated ones. Every other mode's
QEMU command is byte-for-byte what it was.

**The window is capability-detected, and the detection found a wall.**
`default_display` asks the QEMU binary what it actually has. On the dev Mac
(Homebrew QEMU 11.0.2, Apple Silicon) the answer is that there is no GL at
all:

```
$ qemu-system-aarch64 -display cocoa,gl=on
qemu-system-aarch64: OpenGL support was not enabled in this build of QEMU
$ qemu-system-aarch64 -device help | grep gpu
name "virtio-gpu-pci", bus PCI, alias "virtio-gpu"        # no -gl variant
```

So there are three tiers, not two: `gl` (virgl, 3D accelerated), `window`
(native window, software rendering) and `none` (headless). The middle tier is
what that host gets today, and it is still the large input-latency win —
window input goes straight to the guest's usb-tablet/usb-kbd instead of a VNC
round trip. Reaching `gl` needs a QEMU built with virglrenderer AND a guest
driver that can drive it; the second is the open B3 question.

**This also fixes the B2 spike, which could not have worked.** The spike
emitted `-device virtio-gpu-gl -display cocoa,gl=on` unconditionally whenever
`OMNI_GL_WINDOW` was set. `virtio-gpu-gl` is not a device model on that QEMU
build, so the command could not start — while `test_gl_spike.py` went green,
because it only ever compared strings to strings. That test now runs the
generated command against the REAL local QEMU and asserts every `-device` and
`-display` it names is one the binary advertises. `OMNI_GL_WINDOW` survives as
an alias for the window request, now routed through the same capability gate.

**Three silent no-ops were found by checking the guest instead of the log**,
all of them the failure shape `farming.sh` already documents (a step that runs,
fails, hits its trailing `; true`, and reports success):

- **swappiness was never set.** `echo 10 > /proc/sys/vm/swappiness` as uid
  shell is `Permission denied`. Root-needing steps now go through `su` and are
  OMITTED — and reported — when there is no root, rather than emitted to fail.
- **the cpuset move could never find a pid.** It ran in the post-boot
  sequence, but the game only starts when the session is delivered afterwards,
  so `pidof` matched nothing every time. It is now its own step, run after
  delivery, and it waits for the pid instead of sampling once.
- **`apply_roblox_settings` described the wrong profile.** The success line
  hardcoded the farming text, so a gaming boot printed "fps cap + lowest
  quality; ~2x less host CPU" while doing the opposite. It now reports the
  profile it installed.

**One UX bug fixed on the way in:** an interactive gaming start would have
opened the QEMU window AND the built-in VNC viewer — two windows onto one
instance, the VNC one laggier. `spawn_qemu` now records `native_window` in
`run.json` (read off the command actually handed to QEMU) and the viewer
stands down, unless `--window` was passed explicitly or the boot degraded to
headless. The VNC *server* stays on in every mode: screenshot, autocap and the
`omnidroid-input` skill all attach to it.

Not measured, and not claimed: actual frame rate. The guest currently ANRs
SystemUI and foregrounds the Magisk manager instead of the kiosk — identically
on the unchanged headless path, so it belongs to the freshly-built rooted base
(dual-use Phase 2), not to this change.

## 2026-08-06 — dual-use bases: the dev base is gone

The separate `dev` base is removed. Every shipped base (`arm`, `x86`) is now
**dual-use**: it ships to production AND omni-agent debugs on that same image.
What used to be the dev base is split into three independent things:

- **root** — a Magisk-patched boot, baked into the shipped image (`"rooted":
  true`). `omnidroid root-base [--base <tag>]` bakes it into a THIN COW overlay of
  the production system (`base_arm_system_rooted.qcow2`, host-side qemu-io
  write — no flatten, no nbd, no guest root) plus a matched rooted `/data`.
- **hiding** — Zygisk + Enforce DenyList with `com.roblox.client` on the
  DenyList, re-enforced on EVERY boot (`_enforce_hiding`), so production
  presents as an unrooted device.
- **toolkit** — the devkit disk `base_<arch>_devkit.qcow2` (frida + `omni-*`
  tools), built by `omnidroid build-devkit [--arch arm|x86]`, attached as vdc ONLY
  on a `--debug` boot. A production instance's hardware profile is unchanged.

`debug` is a per-BOOT option (`omnidroid start --debug`, agent `debug=true`,
`OMNI_DEBUG_BOOT=1`) — not a base and not an account property. `--apk` (swap
the Roblox build) now works on EVERY base and no longer requires debug. Gone:
`OMNI_DEV_MODE`/`OMNI_USE_DEV_BASE`, `start --dev`, `build-dev-base`, the dev
base entry, and the dev-visibility gate. See `DUAL-USE-BASE.md`.

## 2026-08-05 — per-instance footprint: a real memory model, and three silent no-ops fixed

Goal: 2-3 playable instances on a workstation, 50+ farming instances on a
server. Everything below was measured on the arm64 base (LineageOS 23.2,
HVF); no number here is an estimate.

**Three things were silently not working.** Each looked fine and reported
success:

- **`omnidroid start --mem N` was ignored.** `_ensure_booted` called
  `resolve_mode()` without it, so the flag never reached QEMU. A farming boot
  therefore always ran at the mode's own 512 MB and surfaced as an
  unexplained boot timeout. Two separate 5- and 7-minute "boot failures"
  during this work were this flag being dropped.
- **`farming` mode's 512 MB never booted.** It was set on the theory that a
  squeezed instance needs no more, and had never been run. 512 MB does not
  reach adbd at all; 1024 MB boots but idles with 143 MB available, which the
  ~614 MB game does not fit into.
- **Most of the farming squeeze was a no-op.** `adb shell` does not forward
  argv — it joins the arguments and lets the guest's shell re-parse them — so
  `["shell","sh","-c","pm disable-user X; am force-stop X"]` arrived as
  `sh -c pm` followed by a separate `am force-stop X`. The zram, swappiness,
  lmkd, doze, trim-memory and cpuset steps never ran, and the package trim
  disabled nothing while its force-stop half worked. Every script is now
  `shlex.quote`d (`farming.sh`), verified live: the same package went from
  "not disabled" to disabled immediately.

**The memory model.** `mem` is the guest's ADDRESS SPACE (must be big enough
to boot and hold the game); `balloon` is the post-boot cap on what the HOST
pays. Sizing `mem` down to the footprint you want is the mistake that
produced the 512 MB mode.

- **virtio-balloon with `free-page-reporting=on`, on every instance, both
  architectures.** The guest returns freed pages without being asked, so host
  RSS tracks the live set instead of `-m`. Measured: a booted 2 GB guest
  idling at ~850 MB guest-used sat at ~120-250 MB host RSS.
- **`apply_balloon_target` polls.** Inflation is asynchronous; a single eager
  `query-balloon` reports the pre-inflation size, which is indistinguishable
  from a missing balloon driver — it reported exactly that on a guest that
  did reach its target ~20 s later.
- **`farming` is `mem 2048 / smp 1`, with TWO balloon targets** — 1536
  without zram, 1024 with it — chosen by probing the guest, not by assuming.
  Both are measured. Without zram, 1024 kills the game as it finishes loading
  (`has died: fg TOP`, `mem-pressure-event`) and 1536 holds it at 614 MB with
  336 MB spare.

**zram is the single biggest memory win, and it was nearly missed.** The
squeeze had a zram step from the start; it never worked, because writing
`/sys/block/zram0/*` and `swapon` need CAP_SYS_ADMIN and the production base
is not rooted — so it failed silently and nobody had measured what it would
have bought. Run properly (dev base, root, lz4) it compresses **496 MB of
guest pages into 167 MB — 2.97x** — and the game then survives caps that
previously killed it: 1280, 1024, and even 896, with ZERO kills and no
mem-pressure events (98 threads, state S at 1024).

So the per-instance host cap drops **1536 -> 1024 MB, a third**. 1024 rather
than 896 deliberately: 896 left only 89 MB available, and every number here
was measured against a game on its login screen, not joined to a place.
Capacity: a 64 GB server goes from ~40 to **~62 instances**, which is what
finally clears the 50+ target.

`enable_zram()` runs it through su when the instance has root and REPORTS
whether swap actually came up; `zram_active()` then probes `SwapTotal` so the
balloon picks the right floor. Choosing the low cap without zram is not a
missed optimization, it is an OOM — hence the probe rather than a flag.

**`omnidroid enable-zram-base` (new) — the production delivery, and it turned out
to be one property.** Runtime zram needs privileges the production base does
not grant, so it had to be baked. The first implementation added a zram line
to the image's fstab via debugfs surgery. Then the actual image was read,
which showed that was wrong twice over:

  /vendor/etc/fstab.virtio      /dev/block/zram0 none swap defaults zramsize=50%
  /vendor/etc/init/zram.rc      on early-init -> modprobe zram.ko
                                on init       -> comp_algorithm = lz4
                                on property:persist.sys.zram_enabled=1
                                              -> swapon_all

The base already ships the device, the compressor, the fstab entry and the
swapon, all wired together. zram was never missing — it is switched OFF
behind `persist.sys.zram_enabled`. And the fstab paths the surgery probed did
not include the real one (`/vendor/etc/fstab.virtio`), so it would have
failed outright. That code is deleted rather than kept as a fallback:
reading the image beats guessing at it.

VERIFIED end-to-end on the real base: `setprop persist.sys.zram_enabled 1`
made init run swapon_all and SwapTotal went 0 -> 470980 kB immediately, sized
at 50% of guest RAM by the fstab — which scales better than any fixed number
this project could pick. The runtime `enable_zram()` now flips that property
instead of poking /sys/block/zram0 by hand.

It is a `persist.*` property, so it IS settable at runtime — but SELinux
denies uid shell (measured: "Failed to set property"). Hence root on the dev
base, or baked into build.prop for production, which is what the command
does, reusing the build.prop surgery `strip-base` already had under test.
Unlike `strip-base` it is NOT gated: that gate exists because the low-RAM
PROFILE was measured to break boot, whereas this is a single
LineageOS-supported toggle for a subsystem the image already carries.

NOT YET RUN on the real base: the qcow2 round trip needs ~6 GiB scratch and
this machine has under 1 GiB free. The refusal now names the specific backup
images whose original still exists, with sizes, and does not touch them.

**Roblox's own settings: the CPU lever (`apply_roblox_settings`).** The only
change that reaches INSIDE the game, and the one "quality and speed do not
matter, you can disable rendering" licenses. A ClientAppSettings.json with
`DFIntTaskSchedulerTargetFps: 5` plus lowest-quality render flags is written
into the client's ClientSettings dir. Measured on the dev base with the real
Roblox APK installed (`omnidroid install`), rooted:

- memory 680 MB -> 677 MB — **no change**. Not a surprise in hindsight: the
  game's footprint is engine code, assets and script state, not framebuffers.
- host CPU **36% -> 18.8%, roughly halved.**

Filed honestly as a CPU optimization, not a memory one. It matters anyway for
the farming target: 50 instances at 36% of a core each need ~18 cores just to
idle, at 18.8% they need ~9. For a fleet, CPU binds as hard as RAM.

It needs ROOT (the file is in the game's private data dir; `adb shell` is uid
shell and `run-as` needs a debuggable build), so it applies on the dev base
only. On production it prints a LOUD skip naming the two real delivery
options — bake the file into the base /data image, or have the OmniBootstrap
APK (which already injects the session cookie and runs AS com.roblox.client)
write it. It does not pretend to have run.

**`omnidroid measure` (new).** Reports what running instances actually cost:
median host RSS over repeated samples (single samples are near-meaningless —
observed 72 MB to 1244 MB on one idle instance inside a minute), guest used,
balloon size, and capacity. Capacity is planned against the **balloon cap**,
not the observed median, so an idle fleet cannot flatter the number.

**Platform honesty.** balloon + free-page-reporting decommit for real on
Linux/KVM. On macOS/HVF QEMU's madvise is advisory: a balloon inflate left
host RSS high and rising (from thrash). The 50+-instance target is a Linux
number; macOS runs the 2-3 playable instances. `omnidroid measure` prints which
regime it is in.

**The tier-1 trim never ran on production instances.** `TRIM_PACKAGES` is
applied by `lockdown_and_trim` from `provision_settings`, which only fires on
a FIRST boot — and `build_acct()` hands the production path a handle with
`first_boot_done` already True, so on every ephemeral instance that list was
dead code. deskclock, lineageos.updater, lineageparts and permissioncontroller
were all found resident on a booted farming instance. The list now also lives
in `lean.PROVISION_TRIM_PACKAGES` and runs from the squeeze: measured -70 MB
guest-used with the game running. Trim list 21 -> 34 packages.

**SystemUI: measured, not assumed.** It is the largest non-game process
(~336 MB RSS) and the obvious next thing to cut. `pm disable-user --user 0
com.android.systemui` on a healthy instance takes the WHOLE GUEST DOWN — adb
goes offline permanently and the QEMU process collapses to ~1.6 MB RSS. It
stays in `KEEP_ALWAYS` and the evidence is recorded there so nobody has to
brick an instance to re-learn it.

**KSM is now reported by `omnidroid measure`**, per-instance (`ksm_merged_mb`,
kernel >= 6.1) and fleet-wide. This is the only mechanism that gets a 50+
fleet near 400 MB/instance: 50 guests booted from one base hold overwhelmingly
identical pages and KSM collapses them to one physical copy, while
per-instance RSS counts a shared page once per instance and therefore
over-states a large fleet. When KSM is off or absent, measure says so and why.

**Where the floor actually is** (arm64, measured): the squeezed Android
baseline is ~620 MB and Roblox with its engine active is ~609 MB, so a joined
farming instance needs ~1.23 GB live — which is what sets balloon=1536 and
why 1024 kills the game. ~400 MB per guest is not reachable: the game alone is
more than that, and no host-side or Android-side tuning changes it. 400 MB as
an AMORTIZED cost across a large Linux fleet is a different question and is
what the KSM reporting above exists to answer.

**x86 is validated against a real QEMU, not just against our own
expectations** (`test_qemu_accepts_devices.py`). Every other test checks the
command line we intend to emit, which is not the same as QEMU agreeing to
build the machine — a gap this session demonstrated painfully, shipping ~200
lines of fstab surgery whose unit tests all passed while the code probed
paths that did not exist on the real image. The new test constructs the
machine for real with `-S` and asks QMP whether it came up and whether the
balloon is actually present. Both x86 modes pass, confirming
`virtio-balloon-pci,free-page-reporting=on` is valid on x86/q35 as well as
arm — the memory model depends on it on both, and only arm had ever been
booted. x86 specifically because it cannot be booted on this project's Apple
Silicon dev machine without TCG, so it is the arch most likely to rot
unnoticed.

Tests: `test_qemu_footprint.py`, `test_lean_profile.py`,
`test_squeeze_quoting.py`, `test_strip_base_props.py` (builds a real ext4 and
runs the debugfs surgery against it). Suite 222 -> 286.

## 2026-07-17 — `omnidroid play` no longer creates a profile without a login; `custom_name`; agent-side headless login

**Root cause of the failed overnight test:** the agent was handed a cookie.txt
+ a stock (non-bootstrapped) `roblox-v2.726.apk` and asked to launch place
`8737899170`. It had no tool to turn a cookie into a saved account (the only
exposed path was a human running `omnidroid login`), so it fell back to the
generic `run_apk_test_session` pipeline, which installed the STOCK apk onto
the hardcoded `omniagent` instance and delivered the cookie to it anyway.
Per contracts/omni-session.md §1.2 a stock Roblox build has NO code path that
ever reads the session cookie — so it silently landed on Roblox's own login
screen while `run_apk_test_session` still reported install+launch as
successful. Fixed on both sides:

- **`omnidroid play <name>` refuses to create ANY instance for a name with no
  saved cookie and no override** (`manager/omni.py`: `cmd_play`). `resolve_token`/
  `load_session` are pure reads and don't need an instance directory to exist,
  so the token/place validation now runs BEFORE `ensure_instance()` — a
  brand-new or misspelled name fails `no_token` before any overlay/`/data`/QEMU
  disk gets created for it, instead of after. `--no-token` (the deliberate
  "show Roblox's own login screen" escape hatch) is unaffected. Regression
  tests: `tests/test_session.py::PlayGatesOnLogin`.
- **`custom_name`** (`manager/cookies.py`: `save_account`, `set_custom_name`,
  `list_accounts`) — a saved account may carry a friendly display-only label
  via `omnidroid accounts --set-custom-name <username> <name>`, preserved across a
  cookie refresh. The username stays the account's real identity and the
  instance name; this never renames anything. Tests: `tests/test_cookies.py::CustomName`.
- **omni-agent: `login_roblox_account`** (`tools/roblox_session.py`) — wraps
  `omnidroid login --token-file/--token/--token-stdin` as a tool, so the agent can
  register/refresh an account from a cookie it was handed, headlessly, with no
  human running `omnidroid login` first. Same upsert-by-username store, so
  re-registering the same cookie never creates a duplicate account or
  instance.
- **omni-agent: `launch_roblox_build`** — the one-call pipeline for "cookie +
  place id (+ optionally a new APK)": login_roblox_account -> (if apk_path
  given) decode_apk -> inject_session_bootstrap -> recompile_apk -> sign_apk ->
  install -> play_roblox, every stage idempotent and stage-tagged on failure.
  `run_apk_test_session`'s description now explicitly says NOT to use it for a
  Roblox cookie/login flow (it has no concept of accounts or cookies).
- Verified end to end against real assets: `login_roblox_account` correctly
  rejected an actually-expired cookie.txt with a clean `stage: "login"` error
  (no instance created) in ~37s; `play_roblox` against an existing saved
  account with a still-valid cookie reused its existing instance (no
  duplicate) and delivered the session.
- Removed the stray `omniagent` dev instance this bug had left on disk (via
  `omnidroid stop` + `omnidroid remove`, not a raw file delete).

## 2026-07-17 — `omnidroid login` accepts an already-obtained cookie (headless), not just an interactive sign-in

- **`omnidroid login --token/--token-file/--token-stdin`** (`manager/omni.py`:
  `cmd_login`, `_token_flag_given`; `manager/cookies.py`:
  `capture_login_from_cookie`, `_driver(..., headless=)`). Adopts a
  `.ROBLOSECURITY` cookie you already have (e.g. exported from another
  browser/device) instead of driving a fresh interactive sign-in. No window:
  the cookie is loaded into a **headless** Chrome/Firefox and held to the same
  bar an interactive login has to clear before being trusted — it must land on
  `/home` (not `/login`) AND `users/authenticated` must resolve it to a real
  user — before the account is saved under its username in `accounts.json`,
  same store as the interactive path.
- **Fail-fast on an unusable token**: a `--token*` flag that resolves to an
  empty cookie (blank file, empty stdin, `--token ""`) is a hard `bad_token`
  error, detected by presence (`_token_flag_given`, `is not None`) rather than
  truthiness. Without this an empty value would silently fall through to the
  interactive flow — turning a supposedly-headless, few-second call into an
  unattended up-to-5-minute wait on a visible browser window nobody is
  watching. Caught in testing before shipping (both the truthy-empty-string
  and the blank-file case).
- No engine contract or CLI-shape change for `omnidroid play`/`omnidroid session`/the
  in-Roblox bootstrap — this only adds an alternate way to populate
  `accounts.json`. See `contracts/omni-session.md` §3.0.
- Tests: `tests/test_cookies.py::CaptureLoginFromCookie` (mocked
  driver/whoami, no real browser), `tests/test_session.py::LoginTokenFlag`.

## 2026-07-14 — dev base moved to arm + delivered as an extra disk (vdc), not a new base

**Big change (replaces the 2026-07-13 x86 dev base).** The dev/debug base is no
longer a separate flattened x86 image. It is the shared, immutable `base_arm`
**plus one extra virtio disk** (`base_arm_devkit.qcow2`, attached to dev accounts
as **vdc**) carrying the whole toolkit. `base_arm.qcow2` is never modified.

- **Deleted the entire x86 `base-dev` machinery**: the `base-dev.qcow2`/`.kernel`/
  `.initrd.img` files, the `dev` config entry, the `DEV_BASE_DISK/KERNEL/INITRD`
  constants, the `_stage_devkit`/`_devkit_mutate` `/system`-baking + flatten
  pipeline, and the `base-dev` auto-registration. The old `omni-devkit.rc` init
  service is gone (no more `/system` editing).
- **`omnidroid build-dev-base` rebuilt for arm** (`manager/omni.py`: `_stage_devkit_arm`,
  `_build_ext4_qcow2`, `_find_mke2fs`, `_gpt_partition`, `_patch_dev_boot`,
  `build_dev_base`). It now, **all host-side (no guest boot, no root, cross-
  platform)**: stages the android-**arm64** frida-server + the **Magisk** APK
  (with its extracted arm64 `magiskboot`/`magiskinit`/`magiskpolicy`/`busybox` +
  `boot_patch.sh`) + the `omni-*` scripts + a manifest; builds
  `base_arm_devkit.qcow2` via **`mke2fs -d`** (rootless ext4 populate) →
  `qemu-img convert` to qcow2 (~256 MiB); creates the thin `base_arm_devsystem.qcow2`
  overlay (COW on `base_arm.qcow2`) to hold the rooted boot; registers `dev`
  (arm-uefi + `devkit`). `current_base` is never changed. Downloads prefer
  **curl** (system certs) with a urllib fallback.
- **Dev account model**: `omnidroid create <n> --base dev` copies the arm trio + makes
  a cheap COW overlay of the devkit disk (`devkit.qcow2`), wired into
  `qemu_command_arm` as **vdc**, and flags the account `dev:true`. On start/resume,
  `_devkit_activate` mounts vdc read-only + stages the exec-capable tools; all
  tools run as **root via Magisk `su`** (arm base is a `user` build — `adb root`
  is unavailable). Auto-screenshots / capture dev-gates now key on the account's
  `dev` flag (`acct_is_dev`), not an x86 base tag.
- **Root = Magisk (user-chosen) — WORKING + VERIFIED (2026-07-14).**
  `--patch-boot` roots the dev overlay's boot (`vda6`) via an **offline**
  magiskboot patch (raw-export → GPT-locate `boot` → run Magisk `boot_patch.sh`
  with the full arm64 toolset in a throwaway arm guest → write back → re-import).
  Keeps `base_arm.qcow2` immutable; OFF by default + disk-space guarded. Verified:
  the guest boots with `magiskd` running as root + the Magisk app auto-installed;
  `su` → `uid=0(root) context=u:r:magisk:s0`.
  - **`su` is not on `$PATH`** (all-read-only base → Magisk keeps it in its own
    tmpfs `/debug_ramdisk/su`); the engine + agent probe it (`resolve_su` /
    `_resolve_su`).
  - **Headless su via a dev `/data` template**: MagiskSU prompts for approval
    (GUI) the first time, which hangs headless — so root is granted **once**
    through the app and baked into **`base_arm_devdata.qcow2`** (a copy of the
    provisioned `/data` with the shell grant Forever, `root_access=3`, Zygisk +
    DenyList on). Dev accounts use it → root works from first boot, no prompt.
    Matched FBE pair with `base_arm_devsystem.qcow2`.
  - Hiding = Magisk Zygisk/DenyList (+ Shamiko) via `omni-magisk-setup` /
    `omni-hide`, hiding root, Magisk, and frida.
- **arm64-native win**: the base runs arm64 natively (no libndk), so frida native
  Interceptor/Stalker hooks of the app's own arm64 `.so` now work (the x86 dev
  base couldn't).
- **omni-agent updated** (`tools/android_emulator.py`, `tools/frida_tools.py`,
  `TOOLS.md`): dev detection via the vdc devkit manifest + a precise "dev but not
  rooted" error; `su`-path-resolving activation/`omni-fridad`/`omni-hide`; arm64
  frida notes.
- See `DEV-BASE.md` + `devkit/README.md` (both rewritten).

## 2026-07-13 — dev/prod x86 split: `build-dev-base` + `base-dev.qcow2` (frida + root/frida hiding)

- **New `omnidroid build-dev-base` command** (`manager/omni.py`: `_stage_devkit`,
  `_devkit_mutate`, `build_dev_base`, `cmd_build_dev_base`). Remasters the
  pristine `base_x86` into a SEPARATE dev/debug base, `base-dev.qcow2`,
  registered under the tag `dev`. Reuses the exact `rebuild-base` pipeline shape
  (throwaway builder on `base_x86`, `adb root` + `mount -o remount,rw /`, mutate
  `/system`, `qemu-img convert` flatten) but the source is always the pristine
  x86 base (never `current_base`), the output is `base-dev.*`, and **`current_base`
  is NEVER changed** — the shipped product keeps booting `base_x86`.
- **`base_x86` / `base_arm` are untouched** — same filenames, same config, same
  behavior. `base-dev` is add-only. `autoregister_bases()` re-registers the
  `dev` tag if the `base-dev.*` triple is present but never makes it current.
- **Baked devkit** (`/system`, dev base only; scripts sourced from `devkit/`,
  binaries fetched at build time): `frida-server` (x86_64, pinned 17.15.4),
  `omni-fridad` (start frida hidden — custom loopback port not 27042 +
  randomized process name; prefers `frida-server-patched` if dropped in),
  `omni-frida-stop`, `omni-hide` (Magisk `resetprop` prop-spoofs of the
  build-tag/verified-boot root tells + KernelSU per-app denylist), `omni-magisk`
  (the Magisk multicall binary, used ONLY as `resetprop` — NOT a full Magisk
  install; full Magisk over KernelSU on x86 soft-bricks), an `omni_fridad` init
  service (disabled by default), and a `manifest.json`.
- **Root model:** the base is already KernelSU-rooted (kernel-level — that's why
  `adb root` + remount work, independent of `ro.debuggable`); the dev base keeps
  KernelSU and adds only Magisk's `resetprop` for hiding. SELinux is Permissive
  on this Bliss build, so frida runs without ptrace friction. Measured on a
  booted dev account: the base ALREADY ships `ro.build.tags=release-keys`,
  `ro.boot.verifiedbootstate=green`, `ro.debuggable=0`, so the prop-based root
  tells look stock by default. Honest residuals (see `DEV-BASE.md`): Permissive
  is itself detectable; KernelSU su/manager artifacts remain; stock frida thread
  names remain unless a patched `frida-server-patched` is dropped in.
- **Selection:** opt-in only via `omnidroid create <name> --base dev`. `omni-agent`
  wires this through `ensure_emulator_running(dev=true)` / `OMNI_USE_DEV_BASE=1`
  and adds `ensure_frida_server` + `hide_root_from_app` tools. New doc:
  `DEV-BASE.md`; `devkit/README.md` documents the on-device payload.

## 2026-07-12 — `capture`: millisecond-precise VNC keyframes + crash/black diagnostics

- **New `omnidroid capture <name>` command** (`manager/capture.py` +
  `cmd_capture`). Attaches to the instance's loopback VNC server and observes
  EVERY completed framebuffer update via `vncview.RFBClient`'s `on_frame` hook
  (host-monotonic `perf_counter_ns` per update; the recv-thread callback only
  enqueues, a worker diffs/encodes — so two updates can never coalesce and a
  loading screen shown for a few ms before a black screen is captured as TWO
  keyframes with the true `delta_ms` between them). Change detection keeps only
  meaningful frames (a looping spinner is ignored; a real transition or a move
  into/out of black is kept — `KeyframeSelector`).
- **Crash vs black-screen distinction.** A background `_PidTracker` times when
  the tracked app process starts and DISAPPEARS; `capture` also grabs
  `logcat -b all -v epoch` and scans it. `metadata.json` carries per-frame
  `app_state`/`crash`, `process_events[]`, `start_epoch_ms`, and top-level
  `crash_detected`/`exit_detected` — so a client can tell "app crashed/closed"
  from "screen is black but the app is alive".
- **`version` now advertises `capabilities.capture`** (`supported`,
  `metadata_version: 2`, `coverage: "vnc_framebuffer"`, `options`) so a client
  prefers it and falls back to adb-screencap polling on an older engine.
  Additive only; all `[CURRENT]` output byte-identical. Contract updated:
  `contracts/omnidroid-api.md` §4, §6.8. Rebuild the bundled agent engine
  (`build-exe.ps1` → copy into `omni-agent/tools/omnidroid/`) to ship it.

## Integration milestone — 2026-07-09 — frozen contract v1, both clients wired, Finding B closed

Project-level milestone (spans the workspace, not just this engine — recorded
here and in HANDOFF "Integration milestone status").

- **This checkout is now the canonical arch-aware engine** (`base_x86` +
  `base_arm`, cross-arch guard); images external in `OmniImages`; rollback tag
  **`hub-reconciled`**.
- **Froze `contracts/omnidroid-api.md` v1** and made the engine honor it:
  `version` handshake; `--arch`/`--base` on create; `arch` in
  create/start/list + `bases --json`; ABI-safe install/test-apk (default
  `--abi arm64-v8a` on x86, `native_bridge_used`/`abi_installed`,
  `--require-translation`); cross-arch refusal normalized to
  `{"ok":false,"error":"arch_boundary"}`+exit 1. `[CURRENT]` behavior kept
  byte-identical (additive `arch` field only).
- **Finding B (wrong-ABI trap) closed end-to-end** — engine + both clients:
  a fat APK on x86 installs arm64 and exercises libndk translation
  (`native_bridge_used=true`); a wrong ABI hard-fails.
- **QEMU** resolves from / downloads into the PRODUCT dir only (never PATH on
  Windows), hard-timeout, config-only URL.
- **Clients wired to the contract** (separate repos): omni-executor
  (`97d6553`) — version-gate + arch UI; omni-agent (`ac789c9`) — ABI-safe
  install/test asserts the path, plus the workspace/Docker redesign
  (`b4e3d8d`, pick any host folder → bind-mount → build in container →
  host-side ABI-safe install).

See HANDOFF "Integration milestone status" for DONE / DEFERRED / safety nets.

## Naming — 2026-07-09 — canonical `base_x86` + `base_arm` (versionless filenames)

Unified the two bases onto ONE naming scheme. The x86 base files were renamed
`base-v5.* → base_x86.*` (`base_x86.qcow2` / `.kernel` / `.initrd.img`),
mirroring `base_arm.*` exactly: **the filename no longer carries a version**.
Version tracking did NOT go away — it moved *inside* the config entry
(`configs/paths.json` base `x86`: `"version": 5` + a `"changelog"` map of
v1→v5). `base_x86` + `base_arm` are now the **two canonical bases**; the host
architecture selects between them at runtime.

**What changed**
- `configs/paths.json`: the `v1..v5` entries collapse into one `x86` entry
  (versionless disk/kernel/initrd + `version`/`changelog`); `current_base:
  "x86"`. `base_game` cleared (the pre-installed-game production base is v4's
  lineage, not carried on the dev x86 base used here).
- `manager/omni.py`: new `X86_BASE_*` constants next to the `ARM_BASE_*` ones;
  `autoregister_bases()` registers the canonical `base_x86` triple as tag
  `x86` (legacy `base-vN` triples still auto-register for old deployments);
  `current_base` defaults to `x86` when unset; `_next_base_tag()` counts the
  internal `version` field so a future `rebuild-base` continues the lineage
  (x86@5 → next build `v6`). `base_setup_help` and the docs updated to the new
  filenames.
- **Migration guard (correctness):** `migrate_account` / `migrate_account_fast`
  / `update-all` now refuse to cross the architecture boundary — arm-uefi
  accounts are provisioned matched-pair copies (FBE /data), so an overlay
  repoint would corrupt them. `update-all` skips arm accounts and rejects an
  arm target.

**Arch-aware selection verified on this x86_64 Windows host.** `doctor`:
`host_arch=amd64 → effective_base=x86 (x86-bliss)`, `qemu-system-x86_64` +
WHPX. The `base_arm*` files sit in the same `images/` folder and are
harmlessly ignored (no arm/HVF attempt on Windows).

**End-to-end x86 loop — PASSED** (fresh account `xtest`, `test_arm64.apk`):
create → first-boot dexopt + provision (kiosk HOME, **device owner**, 23-pkg
trim) → clean shutdown → **cold** production boot (`boot_completed` in ~0.4
min) → kiosk auto-launched the app (`topResumedActivity=…ActivityNativeMain`).
**libndk ARM translation validated after the rename:** with the app forced to
its arm64-v8a lib (`primaryCpuAbi=arm64-v8a`), the live process maps show
`libndk_translation.so` **and** the app's arm64 native libs
(`lib/arm64/libroblox.so`, `libbacktrace-native.so`, …) loaded, and the arm64
native GL splash renders on screen. `ro.dalvik.vm.native.bridge=
libndk_translation.so` throughout. **Device-owner lockdown intact:**
`mLockTaskModeState=LOCKED`, status bar disabled (`mDisabled1=0x7a60000`), a
swipe-down gesture did nothing. Clean shutdown on app close via the host
watchdog. Account removed.

> **Note — `test_arm64.apk` is a *fat* APK** (`lib/{arm64-v8a,armeabi-v7a,
> x86_64}`), not arm-only. A plain `install` on the x86 base lets Android pick
> the **x86_64** lib → the app runs native and libndk is NOT exercised. To
> actually validate the translation path we reinstalled with `--abi
> arm64-v8a`. If the intent is "always translated," the base would need its
> x86_64 lib stripped or an arm-only APK.

## Manager — 2026-07-09 — `omnidroid view`: live VNC viewer (self-contained + native)

New `omnidroid view <account> [--start]` opens a LIVE window onto an instance —
real-time screen with mouse + keyboard control — launched from the terminal
(detached; returns immediately, output → `accounts/<name>/viewer.log`). It
resolves the account's localhost `vnc_port`, optionally boots the instance
and waits for the port, then opens a viewer.

- **Default: a self-contained cross-platform viewer** (`manager/vncview.py`)
  — a Tkinter window + a minimal pure-Python **RFB/VNC client** (Raw +
  CopyRect + DesktopSize; 32bpp BGRX pixel format decoded via Pillow so
  colours are correct on any QEMU build). Same viewer on Windows/macOS/Linux;
  no dependence on an OS screen-sharing app or an external VNC client. Deps:
  tkinter + Pillow. Verified against a live arm instance: the decoded
  framebuffer is **pixel-identical to a QMP screendump**, and injected
  pointer events reach the guest input stack (`getevent` shows ABS_MT_*/
  BTN_MOUSE). Mouse (move/left/middle/right/wheel) + keyboard (X11 keysyms)
  are forwarded.
- **`--native`** keeps the OS-client path: macOS launches the built-in
  **Screen Sharing.app by PATH** (not `open vnc://` — that scheme is often
  hijacked by a third-party handler like RealVNC, which silently opens the
  wrong app / nothing); `--viewer`/config `qemu.vnc_viewer` force a specific
  client; Linux tries TigerVNC/remmina/gvncviewer, Windows vncviewer.exe.
- Localhost-only, no auth (safe only on the loopback bind — the port-scheme
  HARD RULE). x86 paths untouched. build-exe.ps1 / build-linux.sh gained the
  `--hidden-import vncview/tkinter/PIL` flags the frozen builds need (the
  viewer is imported lazily by name).

## arm64 / Apple Silicon — 2026-07-08 — base_arm (LineageOS 23.2), arch-aware engine

Second arm session (after the 2026-07-08 proof-of-life). Built a working
**arm64 kiosk base** that runs the target app **arm64-native under HVF, no
translation layer**, and made the engine **host-architecture-aware** without
touching any x86 path. See HANDOFF "ARM64 / Apple Silicon" for the full state.

**App gate (Step 1) — PASSED.** The heavy test app is Roblox
(`com.roblox.client`, arm64-v8a). Under QEMU/HVF it installs, launches, and
renders its native UI at ~0.5–2% jank, `primaryCpuAbi=arm64-v8a` (proves the
no-translation premise). Its "Connection error" is host-ISP SNI/DPI censorship
of Roblox (google:443 works, only Roblox blocked) — a networking matter the
user handles host-side via VPN, NOT an image problem. Gate bar = launches +
renders → met.

**Engine (arch-aware, x86 untouched).** `manager/omni.py`:
- New `BASE_TYPE_ARM` ("arm-uefi") alongside the default `BASE_TYPE_X86`
  ("x86-bliss"); every arm branch is gated on base type so x86 code paths are
  byte-identical. Host detection: `IS_MACOS`, `IS_ARM64_HOST`, `HOST_ARCH`.
- `default_accel()` → **hvf** on macOS; `qemu_system_name()` →
  **qemu-system-aarch64** on Apple Silicon; `resolve_images_dir` gained a
  `darwin` key (falls back to the linux `~/OmniImages`).
- `qemu_command_arm()`: the proven boot from `tools/arm64/boot_arm64.sh`
  (`-machine virt -accel hvf -cpu host`, EDK2 pflash + per-account efivars,
  virtio-blk vda/vdb, virtio-gpu-pci, `-display none` + **localhost-only VNC**,
  adb hostfwd). `effective_base_tag()` picks the arm base on an arm64 host and
  `current_base` (x86) elsewhere — **base selection by host architecture**.
- arm accounts are created by **copying a provisioned matched pair** (see FBE
  note) instead of first-boot provisioning; `post_boot` verifies
  `arm64-v8a` (native) instead of the libndk bridge; `_shutdown` uses
  `reboot -p` on arm (ACPI powerdown alone does not halt this image).
- `doctor`/`autoregister` are arch-aware; the arm base auto-registers from
  `base_arm*.qcow2` in images_dir.
- Kiosk APK now builds on macOS/Linux via `launcher/build.sh` (aapt2→javac→
  d8→apksigner; jars classes so paths with spaces work). The kiosk Java is
  arch-independent — one APK runs on x86 and arm64.

**KEY FINDING — FBE matched pair.** LineageOS `/data` is file-based-encrypted
with keys in `/metadata` (a partition on the **vda system overlay**). So the
system overlay and `/data` disk are a MATCHED PAIR captured together: a fresh
overlay against a provisioned `/data` fails at boot (`init_user0_failed`); a
half-copied data disk fails (`set_policy_failed:/data/misc`). base_arm is
therefore a pristine shared system (`base_arm.qcow2`) + a **provisioned
overlay+data+efivars trio** (`base_arm_system/…_data/…_efivars`); an account
copies the trio (overlay stays backed by the shared base). Verified end to
end via `omnidroid create/start/install/stop`.

**DONE & verified on arm:** silent-of-*console* aside (see below), an account
boots the provisioned kiosk in ~15–40 s; kiosk is HOME and auto-launches the
app; **device-owner Lock Task fully blocks the status bar AND the swipe-down
Quick-Settings panel** (verified: swipe-from-top does nothing); app renders
arm64-native; `omnidroid stop` powers off cleanly via the arm path. Device-owner
is assigned by the workaround the proof-of-life predicted: complete the
first-boot wizard (adb needs it), then `settings put global device_provisioned
0` → `dpm set-device-owner` succeeds (no root, 0 accounts).

**DEFERRED by user (2026-07-08): base_arm ships as-is** — a functional kiosk
without silent boot / custom animation / root. Phase C below is intentional
future work, done in its own session once an approach is chosen.

**Phase C (deferred).** Silent boot (TianoCore UEFI
splash → GRUB 8 s menu → scrolling kernel console are all visible today),
custom loading animation (`/product/media/bootanimation.zip`), and in-guest
root all require writing the **read-only vda** (grub.cfg, /product). On this
user build there is no adb root, and macOS has **no qemu-nbd/libguestfs** path
to edit the qcow2 offline (`qemu-nbd: Kernel /dev/nbdN support not available`).
The only on-macOS route is booting **LineageOS Recovery** (root context) to
mount partitions rw and edit grub.cfg + swap the boot animation (and/or install
Magisk for runtime root) — a real sub-project with brick risk. Awaiting the
user's go/no-go on approach + effort before doing image surgery.

## Manager — 2026-07-07 — renamed `qemu-manager` → `omnidroid`

The engine/CLI (and its artifacts) is now **`omnidroid`**
(`omnidroid.exe` on Windows, `omnidroid` ELF on Linux). This supersedes
the 2026-07-06 naming split ("engine stays qemu-manager"): the user
decided the CLI itself is the omnidroid product. Rename only — CLI
commands, JSON contract, and behavior unchanged. Updated build scripts
(`build-exe.ps1`, `build-linux.sh` output names), all user-facing hint
strings in `manager/omni.py`, and all docs. Older entries below may
still say `qemu-manager`/`omni.exe` where they describe historical
artifacts.

## Manager — 2026-07-06 — fresh-install guards, base auto-register, doctor

**Bug fixed:** on a blank deployment (exe in a new folder, setup run,
images_dir still empty) `create` crashed with `KeyError: None` —
`load_config` indexed `bases[current_base]` with `current_base: null`.

**1. Missing-base guard everywhere.** `load_config` (the gate every
base-needing command goes through: create/start/update-base/update-all/
rebuild-base/update-kiosk/bench-ksm) now handles a null/unregistered
`current_base` and missing base files explicitly: clean actionable
error listing the EXACT files + full images_dir path (never a
traceback), `{"ok":false,"error":…}` + exit 1 in `--json` mode. The
create `--data-size` default lookup moved out of `main()` into
`cmd_create` so it's inside the same guard/JSON wrapper.

**2. Blank deployment self-bootstraps.** `read_config` (not just
`setup`) creates the default `configs/paths.json` next to the exe on
first use — drop the exe into any folder and every command
works. Malformed config JSON also errors cleanly now. Default template
gains `default_src` (kernel SRC= for auto-registered bases).

**3. Base AUTO-REGISTRATION.** Complete `base-vN.qcow2 + .kernel +
.initrd.img` triples found in images_dir that aren't registered yet are
registered automatically on the next command (src from `default_src`;
`current_base` = highest vN when unset). Copy the files in — nothing
else to do. This is the exact hook the future server download lands on.
Registration only ADDS config entries; bases/accounts never touched.

**4. `doctor` command + airtight setup guidance.** `doctor [--json]`
reports config path, images_dir, registered bases, per-file presence
with FULL missing paths, data-template, QEMU/adb resolution, and a
`ready` verdict (exit 0/1 — the GUI can gate on it). `setup` now prints
the same missing-file list + the copy-these-files help block
(exact names: `base-vN.qcow2`, `base-vN.kernel`, `base-vN.initrd.img`,
`data-template-8g.qcow2`).

**Verified both states with the shipped exe in a sandbox folder:**
empty images_dir → `create`/`create --json`/`update-all`/`setup`/
`doctor` all fail clean with the file list (no tracebacks, exit 1);
then base-v5 files + template copied in → `create` auto-registered v5
(current=v5), provisioned, `start --wait` booted with libndk OK, then
stop/remove clean. Healthy repo install regressed: `doctor` ready,
config byte-identical (no rewrite).

## Manager — 2026-07-06 — VNC wired (localhost-only), GUI JSON contract, remove, HOWTO

Engine features for the separate GUI app (naming split of 2026-07-06,
since superseded by the 2026-07-07 rename above). Host-side flags +
CLI only — no base change, no /system or bridge props touched.

**1. VNC attach point WIRED (was reserved-only).** Every instance
(production and dev/builder profiles) now starts QEMU's built-in VNC
server on its reserved `vnc_port` (18001+i → QEMU display `:12101+i`),
bound to **127.0.0.1 ONLY**. Instances stay `-display none` headless;
VNC is an optional attach surface, always on because an idle listener
does no framebuffer encoding (measured host-rss 3235 MB at `-m 3072`
with Roblox ≈ the documented pre-VNC 3195 MB) — hours-long unwatched
runs pay nothing. Port-triple distinctness asserted at spawn.
**NEW HARD CONSTRAINT #4: no-auth VNC is safe ONLY because of the
localhost bind — never bind a network interface without adding auth in
the same change.**
- **Color note (measured):** the VNC framebuffer serves a clean **R/B
  swap** vs adb-screencap ground truth (mean RGB 46.4/51.8/52.6 vs
  52.6/51.8/46.4 on the Roblox login screen) — the SAME documented
  host-side presentation bug family as the old SDL blit on this QEMU
  dev snapshot. Guest rendering is true-color (screencap proves it);
  candidates if it matters for the GUI: stable QEMU build (already
  flagged) or swap channels in the viewer. Never touch gralloc.

**2. GUI contract: `--json` + `remove` + stop semantics.**
- `--json` on `create`/`start`/`stop`/`remove`/`list`: stdout carries
  EXACTLY one JSON payload (progress → stderr); fatal errors become
  `{"ok":false,"error":…}` + exit 1. `start --json` returns pid +
  adb/qmp/vnc ports immediately (detached); `--wait` adds
  `booted`/`native_bridge_ok`. `list --json [--stats]` returns the
  full fleet with live state/RSS/guest-used.
- **NEW `remove <name>`** — the project's first destructive op, with
  hard guardrails: exact `[A-Za-z0-9_-]+` name only (no globs/paths);
  the resolved delete target is asserted to live inside `accounts/`
  (structurally cannot touch a base/images dir — double-checked that
  images_dir is not inside the target); stop-first with the bounded
  chain, refuses to delete if the instance won't stop; Windows
  file-lock retry. Deletes overlay + data.qcow2 + state; ports freed
  (index reused by next create).
- **Disconnect ≠ shutdown (documented contract):** a VNC/adb viewer
  disconnect is a no-op — instances keep running headless (default).
  `stop [--timeout S]` is the only power path (adb `svc power
  shutdown` → QMP quit → kill, every step hard-bounded, reports
  `method`). All GUI commands are headless with hard timeouts (adb
  readiness only) and identical on Windows/Linux.

**3. HOWTO.md** — new detailed usage guide (setup, concepts, full
command reference with JSON schemas, VNC + security rule, workflows,
troubleshooting).

**Verified live (throwaway account, then removed):** create --json
(provisioned ~1 min) → start --json --wait hard (booted 0.3 min) →
netstat: adb/qmp/vnc all 127.0.0.1-LISTENING, no collisions → real RFB
3.8 handshake + full 4,096,000-byte raw framebuffer → probe disconnect
→ instance still up (boot_completed=1) → libndk OK → **Roblox
foreground + renders (screencap)** → stop --json (method=powerdown) →
remove --json (folder gone, ports freed) → fleet list + images dir
byte-identical to pre-test snapshot.

## Manager — 2026-07-06 — headless-always, engine packaging, FAST update-all

**1. Headless always.** `--headless`, `--gpu` and `--window` REMOVED; every
instance (production and dev/builder) boots with `-display none` — no code
path opens a host window (verified by grep + live boot). Modes are now pure
RAM/CPU tiers (playable 4G/4c, hard 3G/4c, brutal 2G/2c; `--mem` override).
VirGL path + fallback deleted (needed a GL window); the old R/B swap is
moot (guest rendering/screencap always was true-color). **Port scheme
(invariant):** one shared index i per account → adb 16001+i, qmp 17001+i,
**vnc 18001+i RESERVED** for the future local VNC (recorded in
account.json, shown in `list`/`start`, NOT yet passed to QEMU). Ranges
1000 apart → no collision below 1000 instances; old accounts backfilled
automatically.

**2. Engine packaging + setup.** Artifact renamed `omni.exe` →
a standalone engine exe (since 2026-07-07: **`omnidroid.exe`**; built,
CLI unchanged). New **`setup`** command
(idempotent, also implicit on first use): Windows = create folders +
download portable QEMU into ./qemu ONLY (nothing installed to the host
system); Linux = create `~/OmniImages`, preflight system QEMU
(`sudo apt install qemu-system-x86 qemu-utils android-tools-adb`),
`/dev/kvm`, KSM — with exact fix commands. **Two-build process:**
PyInstaller cannot cross-build — `build-exe.ps1` on Windows,
`build-linux.sh` ON the Linux box → `dist/omnidroid` (ELF). Same
source, identical CLI; Linux additionally gets `-accel kvm` + KSM.

**3. FAST update-all (scales to 100+ accounts).** `update-all` now AUTO-
picks per account:
- **FAST**: discard + recreate the disposable overlay against the NEW
  base (fresh `qemu-img create -b` — the correct way to change backing
  files; never rebase, never edit a base in place). No boot, no
  re-provision, data.qcow2 untouched. **Measured: 6 accounts in 0.2 s.**
  Correct whenever provisioned /data state stays valid: OS/game/kiosk
  updates all live in /system and arrive via the overlay itself.
- **FULL** (boot + idempotent re-provision): auto when the base's game
  package changes for that account (omni_game_package lives in /data);
  force with `--full` for /data policy changes (lockdown, trims).
  `--fast` forces repoint-only.
**Verified live:** fake base-v6 (byte-copy of v5) registered → `update-all
--to v6` = 6/6 FAST in 0.2 s → alice COLD-BOOTED headless on v6 in ~30 s,
kiosk auto-launched Roblox, **still logged in** (data preserved), libndk
OK → fleet fast-reverted to v5 (0.1 s), v6 deregistered + deleted.

**4. Server updates (design note only).** Production flow documented in
HANDOFF: server-downloaded base file → register + set current →
`update-all` fast-repoints everyone in seconds. No networking built.

Measure-first pass on a fresh v5 account (`regcheck`, hard/headless,
Roblox at login). Fixed A/B protocol: cold boot → Roblox process up →
measure at exactly T+480 s after `boot_completed`.

**Applied (16 more `pm disable-user` packages — same proven per-`/data`
reversible mechanism as v5's 7):** the running weather service
(`org.omnirom.omnijaws`), two persistents (`org.lineageos.updater` OTA
updater, `com.android.touch.gestures` Bliss gestures — Lock Task blocks
gestures anyway), and 13 boot-spawned apps idling in cached state
(taskbar main pkg, gamespace, phonograph music player, deskclock, dialer
UI, contacts, messaging, Android Auto, gm.exchange, calendar sync,
printspooler, imsserviceentitlement, cellbroadcast). All folded into
`TRIM_PACKAGES`; existing accounts pick them up on next re-provision
(`update-all` or `update-base`).

**Measured:** guest-used **1789 → 1670 MB (−119 MB)**, guest processes
206 → 191, boot 36 → 31 s (noise-level). zram (1.5 GB zstd, on by
default via `persist.sys.zram_enabled=1`) confirmed working, ~360 MB used.

**Honest finding — host RSS UNCHANGED (3195 MB at `-m 3072`):** on
Windows/WHPX the guest page cache expands into whatever RAM the trims
free, so QEMU still touches ~its full allocation and Windows shares/
reclaims nothing. Guest-side trims buy in-guest headroom (safer at
brutal's 2 GB, less lmkd pressure) — NOT host RAM. Moving the host
number needs `-m` reduction, ballooning (proposed below), or Linux+KSM.

**Regression passed on the trimmed instance:** `ro.dalvik.vm.native.bridge
= libndk_translation.so`, Roblox foreground + rendering (screencap), kiosk
still DeviceOwner (Lock Task active).

**Decisions (user, 2026-07-06) — Windows optimization CLOSED:**
- *Tier 2 (virtio-balloon + free-page-reporting / QMP squeeze):*
  **REJECTED** — lmkd-kills-the-game risk not worth it, and host RSS
  won't drop on WHPX regardless (page cache expands into freed RAM).
- *Tier 3 (`ro.config.low_ram`, zram resize, telephony/SE/contacts-
  provider disables):* stays **documented-only**.
- **Key finding, now a settled decision:** on Windows/WHPX package trims
  buy in-guest headroom (good for brutal's 2 GB), NOT host RAM or more
  instances — that only comes from KSM on Linux. This is the documented
  reason the Linux port matters.
- Tier-1 trims rolled out fleet-wide via `update-all` (all accounts
  re-provisioned on v5; per-account data preserved; regression passed).
- Kept untouched: GMS + Play Store, latin IME, Settings (FallbackHome),
  managedprovisioning, /system libs + bridge props (off-limits).

## Manager — 2026-07-06 — Linux/KVM+KSM readiness (host-side prep; no Linux hardware yet)

Phase 8 groundwork done **entirely on Windows** — code paths are correct
and guarded, NOT simulated, and untested-on-Linux parts say so:
- **Accel auto-detect**: Windows→`whpx,kernel-irqchip=off`, Linux→`kvm`
  with explicit `-machine mem-merge=on` (marks guest RAM MADV_MERGEABLE so
  KSM can dedup identical pages across instances). `start --accel <str>`
  overrides. Linux preflight warns if `/dev/kvm` is missing/unwritable.
- **`omnidroid ksm [status|on|off] [--aggressive]`** — drives
  `/sys/kernel/mm/ksm/*`, prints stats + MB deduped; clean no-op message
  on Windows. `list --stats` shows per-instance `ksm-merged` MB on Linux.
- **`omnidroid bench-ksm`** — Phase 8 measurement scaffold (Linux-guarded):
  adds identical headless instances one at a time, waits for
  `pages_sharing` plateau, records the **marginal MemAvailable drop** per
  instance (RSS double-counts shared pages), stops at a RAM floor (never
  a count cap), JSON per step + summary; stops instances unless `--keep`.
- **Per-platform `images_dir`** — `configs/paths.json` now maps
  windows→`C:/Users/berat/OmniImages`, linux→`~/OmniImages` (legacy string
  form still accepted; `~` expanded) so one checkout works on both hosts.
- **Windows regression**: generated QEMU command line verified
  byte-identical to pre-change (production and dev profiles); CLI sanity
  (`list`/`bases`/`ksm`/`qemu-info`) OK; fresh v5 account end-to-end
  (boot → kiosk → Roblox renders via libndk) re-run.

**Expectation note (do not oversell):** the planned first Linux host is an
8 GB Ubuntu 24.04 laptop → ~6 GB usable → **~3–4 brutal instances even
with KSM**, fewer than Windows' ~7. The laptop proves cross-platform
parity; real scale needs a high-RAM Linux box.

## base-v5 — 2026-07-06 — status-bar lockdown + faster boot / less RAM

**Problem confirmed on a fresh v4 kiosk account:** swiping down still opened
the full Quick-Settings panel (immersive mode only *hides* the bar), and an
"Android Setup — finish setting up…" notification lingered. `dpm
list-owners` = no owners.

**Lock Task Mode lockdown (device-owner kiosk pinning).** The kiosk now has
a `DeviceAdminReceiver`; provisioning runs `dpm set-device-owner
com.omni.kiosk/.OmniDeviceAdminReceiver`. As device owner the kiosk:
`setLockTaskPackages([kiosk, game])`, `setLockTaskFeatures(NONE)`,
`setStatusBarDisabled(true)`, and `startLockTask()` around the game launch.
Result: status bar, Quick-Settings pull-down, notifications, and home/recents
gestures are fully disabled while the game runs — no escape surface. All
per-`/data` (device owner + policies live in `/data`); no `/system` libs or
bridge props touched.

**Setup-wizard notification killed** — `pm disable-user
com.google.android.setupwizard` in provisioning.

**Less RAM** (`pm disable-user`, per-`/data`, reversible): disabled Google
Assistant/search (`googlequicksearchbox`, ~215 MB), device restore,
AboutBliss, and the preinstalled Camera/Termux/file-manager apps; zeroed UI
animation scales. GMS + Play Store KEPT (the game may use Play Integrity —
regression confirms Roblox still launches/renders). **Measured with Roblox
running: guest RAM dropped from ~2289 MB (v3) to ~2004 MB (v5), ≈285 MB
saved per instance** — all 7 trimmed processes confirmed absent.

**Boot time — honest result: unchanged.** Measured `boot_completed` back to
back under identical host load: v3 ≈35 s, v5 ≈35 s. The trimmed apps don't
run on the boot-critical path (they start after `boot_completed`), so
disabling them saves RAM, not boot time. Meaningful boot-time reduction
would need riskier system-service/zygote-preload trimming (deferred; every
such change must keep passing the ARM regression check).

**New manager command:** `omnidroid update-kiosk [--apk ...]` — ship a new kiosk
launcher in a new base version (reuses the generalized base-builder), then
`omnidroid update-all` rolls it out (per-account data preserved).

base-v5 = v3 (dev) + the lock-task kiosk. Existing accounts migrated with
`update-all` (re-provision applies the device-owner lockdown + trims to each
account's `/data`).

## Manager — 2026-07-06 — color fix, performance modes, dev harness

**R/B color swap FIXED via VirGL.** The swap was in QEMU's software 2D
virtio-gpu→SDL blit on this build. Confirmed exhaustively: gralloc backends
(`GRALLOC=gbm` even breaks boot), display backends, and virtio device
variants all still swap under software rendering. The fix is **VirGL**
(`-device virtio-gpu-gl -display sdl,gl=on`): host OpenGL presents correct
colors AND accelerates the GPU. Verified visually on the Roblox screen
through the manager — blue links blue, orange terrain orange (vs the
software A/B where they were swapped). Roblox still renders via libndk.
Software rendering still swaps (host-side blit bug); documented per mode.

**Performance modes** — `omnidroid start <name> --mode playable|hard|brutal`.
Instance counts are NEVER capped; modes only tune the per-instance
footprint (host free RAM decides how many run).
- `playable` (default): VirGL (correct color + GPU), 4 GB, 4 vCPU. Smooth,
  few instances.
- `hard`: software rendering, 3 GB, 4 vCPU. More instances (R/B swapped on
  the host window; use `--gpu virgl` for correct color).
- `brutal`: headless (no window), software, 2 GB, 2 vCPU. Max instances.
- Overrides: `--gpu virgl|software`, `--mem MB`, `--headless` (any mode).
- **VirGL graceful fallback**: if VirGL fails to start (host GL issue),
  `start` detects the immediate QEMU exit and relaunches in software.
- Mode recorded in `accounts/<name>/run.json`. Dev/builder boots are
  unchanged (virtio-vga + serial, visible for debugging).

**Dev / testing harness (scriptable, JSON output, headless).**
- `omnidroid test-apk <name> --apk <apk> [--mode hard] [--window] [--reuse]` —
  one-shot: ensure a FRESH session with no app pre-baked (v3 dev base +
  kiosk), install the APK, let the kiosk launch it, emit one JSON line:
  `{account, base, mode, package, installed, launched, foreground, pid,
  adb_port, qmp_port, adb_serial, ok}`. Headless by default.
- `omnidroid screenshot <name> [--out path]` — pull a framebuffer screenshot
  (true colors, works headless); prints JSON `{ok, path}`.
- `omnidroid logcat <name> [--tag T] [--clear]` — read/clear guest logcat.
- `omnidroid adb <name> -- <args>` — arbitrary adb (existing).
  An agent scripts: `test-apk` → parse JSON → `screenshot`/`logcat`/`adb`
  against the reported `adb_serial`.

## Manager — 2026-07-06 — base migration, QEMU auto-install, exe, prod updates

**Base migration (update accounts to a newer base, keeping their data).**
An account = a disposable `system.qcow2` overlay on a shared base + an
independent `data.qcow2` (all logins/settings/apps). Migration recreates
only the overlay against the new base; `data.qcow2` is never touched.
- `omnidroid update-base <name> [--to vN]` — migrate one account.
- `omnidroid update-all [--to vN] [--skip-current]` — migrate every account.
- Each migration re-provisions (idempotent): applies the new base's kiosk/
  HOME/settings without erasing data. **Verified:** alice v1→v3 kept a
  `/sdcard` marker file + installed Roblox, gained the v3 kiosk, and
  auto-launched Roblox. All 5 accounts migrated v1/v2→v3, data preserved.

**Production pre-installed-game update (no data loss for any user).**
- `omnidroid rebuild-base --game <apk>` — boots a throwaway builder on the
  current base, bakes/replaces the game as a `/system/app` system app
  (`/system/app/OmniGame/OmniGame.apk`, correct SELinux context),
  **extracts the APK's native `.so` libs into `lib/<abi>`** (a `/system/app`
  APK is NOT auto-extracted like a `/data` install, so an ARM game would
  crash at load without this — libndk still translates the ARM libs),
  flattens to a new self-contained base version, registers it, makes it
  current.
- Roll out to everyone: `omnidroid update-all` → each account's overlay repoints
  to the new base (new game) while its `data.qcow2` (per-account login/
  saves) is preserved. So updating the pre-installed APK reaches all users
  without erasing data.
- `provision_settings` sets the kiosk's target game from the base's
  pre-installed game (production) or the adb-installed game (dev).

**Dev vs production mode switch.**
- `omnidroid bases` — list registered bases (marks current) + any pre-installed
  game per base.
- `omnidroid use-base <tag>` — set the default base for new accounts (e.g. a dev
  base with no game vs a production base with the game baked in).
- Dev workflow: base without game; `omnidroid install <acct> <apk>` per account.
  Production workflow: game baked in base via `rebuild-base`; every account
  gets it.

**QEMU auto-install on first use (not bundled in the exe).**
- `qemu_bin()` resolves the QEMU executable: config `qemu.dir` → local
  `./qemu` (auto-installed) → PATH.
- `ensure_qemu()` runs before any command that needs QEMU; if QEMU is not
  resolvable it downloads a portable Windows QEMU installer and silently
  installs it into `./qemu` (NSIS `/S /D=`), no global install. Overridable
  via config `qemu.download_url`. No-op when QEMU is already present.
- `omnidroid qemu-info [--install]` — show/repair QEMU resolution.

**Single Windows exe.**
- `build-exe.ps1` → `dist/omni.exe` (PyInstaller onefile, ~9.5 MB, stdlib
  only). Ships next to `configs/`; `accounts/`, `work/`, `qemu/` are created
  beside it. The exe's CLI is identical to `python omni.py …`, so external
  scripts call it the same way. QEMU is NOT inside the exe — fetched on
  first use. Verified: `omni.exe list` and `omni.exe qemu-info` work.

## base-v4 — 2026-07-06 — PRODUCTION base (game pre-installed)

`v3 + Roblox baked as a `/system/app` system app` with its 11 arm64 `.so`
libs extracted into `lib/arm64`. Built via `omnidroid rebuild-base --game
roblox.apk`. **Verified:** a brand-new account on v4 (`prod2`) boots
straight into Roblox — pre-installed system app, kiosk auto-launches it,
renders via libndk — with NO adb install and no manual steps.

This is the production lineage. `current_base` is kept at **v3 (dev
default)**; switch to production with `omnidroid use-base v4`. Dev accounts (v3,
game via `omnidroid install`) and production accounts (v4, game pre-installed)
coexist. Updating the pre-installed game for everyone: `omnidroid rebuild-base
--game <newapk>` (→ v5) then `omnidroid update-all` — each account's overlay
repoints to the new base while its data.qcow2 (login/saves) is preserved.

## base-v3 — 2026-07-06

Two cosmetic boot leaks fixed. Fresh-account end-to-end verified.

### Silent boot — no console text (host-side, no image change)
The production QEMU profile (`manager/omni.py` `qemu_command`, non-dev branch)
now shows **nothing** on the visible display from power-on to the loading
screen:
- `-vga none -device virtio-gpu-pci` instead of `-device virtio-vga`.
  Removing the legacy VGA text device means SeaBIOS/iPXE firmware text has
  nowhere to print. virtio-gpu-pci uses the **same `virtio_gpu` DRM driver**
  as virtio-vga, so ARM game rendering is unchanged (verified: Roblox
  renders identically).
- `console=null` on the kernel cmdline: the Bliss/Android-x86 initrd script
  output ("Detecting Android-x86…", the BLISS ASCII art) goes to nowhere
  instead of the framebuffer console.
- NIC `romfile=` (empty): skip loading the iPXE option ROM entirely.
- Kept: `quiet loglevel=0 vt.global_cursor_default=0 SETUPWIZARD=0`.
- **Dev boots unchanged**: `-device virtio-vga` + `console=tty0
  console=ttyS0,115200` + `-serial file:` so firmware/kernel/init text stays
  visible and logged for debugging.

Removed leaks that were visible on base-v2 production boots: SeaBIOS banner,
`iPXE (http://ipxe.org)…`, `Booting from ROM…`, `Detecting Android-x86…`,
`Found at /dev/vda1`, the BLISS ASCII-art logo.

### No wallpaper flash (per-account /data)
The kiosk (`com.omni.kiosk`) now sets a **solid-black system wallpaper** on
first launch via `WallpaperManager.setBitmap()` (new `SET_WALLPAPER`
permission). Eliminates the brief default-Bliss-wallpaper (pink lotus) flash
between the boot animation ending and the game launching. `provision_settings()`
launches the kiosk once during provisioning so the black wallpaper is written
to `/data` before the first production boot. The kiosk window itself was
already opaque black (`Theme.Black` + `setBackgroundColor(BLACK)`).

### Image content
base-v3 = base-v2 flattened + the rebuilt kiosk APK (with black-wallpaper
code) swapped into `/system/app/OmniKiosk/OmniKiosk.apk`. Kernel/initrd
identical to v2 (the console fix is host-side flags, not an initrd change).
Self-contained, 2.74 GiB.

### Verification (brand-new account `erin`, base-v3, production profile)
- Silent boot: frames t=0–6 s pure black + custom loading screen; NO
  firmware/console/BLISS text.
- No wallpaper flash: "Tablet is starting…" and all transitions on pure
  black (previously the pink lotus); straight into Roblox splash.
- Kiosk auto-launches Roblox, zero intervention.
- ARM translation: `ro.dalvik.vm.native.bridge=libndk_translation.so`,
  abilist has `arm64-v8a`; Roblox renders.
- "Viewing full screen" absent (`immersive_mode_confirmations=confirmed`).
- Lock screen absent (`locksettings get-disabled=true`).
- Close game → host watchdog `RUNNING→GRACE`→ clean shutdown, QEMU exits.

### Constraints honored
Only a HOME app (kiosk), media, `/data` settings, and host-side QEMU flags
changed. No `/system` libraries or native-bridge props touched.

## base-v2 — 2026-07-05
base-v1 + custom loading screen (`/system/media/bootanimation.zip`) + kiosk
launcher as `/system/app` system default HOME. Silent-boot kernel flags
(`quiet loglevel=0 SETUPWIZARD=0`), lock screen + immersive-confirmation
disabled via per-account `/data` provisioning.

## base-v1 — 2026-07-05
Initial immutable base: user's Bliss OS 16.9.7 (Android 13, x86_64) qcow2
with libndk ARM translation. Kernel/initrd extracted for direct kernel boot
(no GRUB). adb-over-TCP, KernelSU root.
