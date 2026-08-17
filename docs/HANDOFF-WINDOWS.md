# Omni Executor — handoff

Rewritten 2026-08-14, extended 2026-08-15. Everything below was **run on the
machines named**; where something is unverified it says so.

> **Continuing in a new session? Start at "NEXT SESSION STARTS HERE —
> 2026-08-17 (late evening)".** It supersedes the block after it and carries
> the warning that matters most: every launch-time measurement older than it
> was taken through a path that reported success for instances whose client
> was already dead.

> **Older orientation, kept for the detail:** The newest
> section (2026-08-17) is the window: it is on screen for the whole boot now
> and holds the guest's aspect ratio. The 2026-08-15 work — GPU rendering, the
> DNS block, auto-update, the Mac — is §8, and the four things still open are
> listed with what each one needs.

> **2026-08-14 (later): a fresh machine could never finish setup, and then
> could not even start the app.** Nothing installed QEMU or adb and the setup
> screen refused to begin without them (§0); separately, the downloaded .zip
> carried Windows' Mark-of-the-Web, which killed the app on launch before any
> of that could run (§0b). Both fixed, and there is now a real installer
> (§0c): **http://72.62.59.232/omni/dist/blob/setup-win**

Machines: Windows 11 (i7-13700F, RTX 4060, 32 GB) · Mac mini M1 at
`berat@192.168.0.30` · VPS `72.62.59.232` (root, password in the `# VPS`
comment in `omni-backend/.env.development.local`).

---

## Install it / update it

**Give users this link. Nothing else.**

```
http://72.62.59.232/omni/dist/blob/setup-win
```

It is a 12 MB stub installer. It downloads the current build, verifies it by
sha256, installs per-user to `%LOCALAPPDATA%\Programs\OmniExecutor` (**no
administrator**), makes Start Menu and Desktop shortcuts and an uninstaller,
and launches the app. The link never goes stale: the installer resolves
whatever `app-win` currently is at run time, so a new release needs no new
installer.

⚠️ **SmartScreen will warn once** — the binaries are unsigned, so a downloaded
copy is marked and Windows shows "Windows protected your PC". The user clicks
**More info → Run anyway**. Say so wherever you publish the link; see §0c.

**Updating an existing install:** the in-app banner (or Settings → Updates)
downloads and swaps the app itself. Re-running the installer does the same and
is the supported repair — it closes the running copy, replaces the directory,
restores the old one if the copy fails, and never touches
`%LOCALAPPDATA%\OmniExec` (the accounts and the images).

A machine still stuck on the FIRST-BOOT screen never sees the update banner —
that lives in the app shell, behind the `ready` gate. Those need the installer.

Currently live: **app `1.0.14`**, installer `setup-win`, `qemu-portable-win`
`11.0.50`.

**1.0.14 updates itself.** From this version on, a new build installs on launch
without being asked, and a build published while the app is open downloads
quietly and puts up "Version X is ready. Restart to update." with one button.
Older installs still need the one manual click to get to 1.0.14. See §8.

---

## NEXT SESSION STARTS HERE — 2026-08-17 (late evening)

**This block REPLACES the one below it**, which was written before the launch
path was found to be reporting success for instances with no game running in
them. Everything here was measured on this box today. **Nothing is deployed.**
Code is committed on `gaming-gpu-window` / `slice-c-windows-exe`.

### Read this before trusting any older measurement

**A farming launch reported `ok: true, in_world: true` for an instance whose
Roblox client was dead** — six minutes dead, with the launch still running.
Three defects, all fixed today (CHANGELOG, "the launch was calling dead
instances a success"):

* `in_world` came out of the client's LOG FILE, and join markers outlive the
  client that wrote them;
* `start` decided success before anything watched the client — the governor
  recorded `client_died_after_s` into run.json 11 s later, where nothing
  surfaced it;
* `wait_for_game_settled` burned its whole 420 s deadline on a dead process,
  because dumpsys reads a dead package the same as a slow one.

⚠ **Every launch-time reading this project has ever taken came through that
path.** `in_world: true` in an older note is not evidence an instance was
farming, and "126 s cold / 101 s warm" was measured on launches that happened
to settle fast. Re-measure before quoting.

### PS99 does not run at `-m 2048`. Do not re-open it on one run.

The 3072 floor in `lean.GUEST_MEM_FLOOR_MB` is correct. It was re-opened here
because it predates zram and the working-set ceiling, and one 15-minute run at
2048 looked like a clean pass. Across five launches: **1 survivor, 4 clients
OOM-killed**, always during the load, always with zram exhausted
(`SwapFree: 360 kB`). The failure is probabilistic; a single sample cannot see
it, which is exactly how a day got spent on it.

### What one instance costs — and it is NOT what `doctor` thinks

Six PS99 farming instances at `-m 3072`, through the executor's own argv:

```
commit     4065 MB   mean marginal across six (QEMU alone 3777-4009 MB)
RAM         384 MB   resident, exactly, every governed instance
CPU          50%     of one core — 44-51% observed, the cap never slipped
launch    165.8 s    mean (139-187 s)
scratch     1.3 GB   overlay per instance (planner budgets 2.0)
```

⚠ **`COMMIT_OVERHEAD_MB = 192` is wrong by 4x** — it was measured on a PAUSED
QEMU with no guest (the source says so and calls it a floor), and
`instance_capacity` then uses it as the cost. Real overhead ~825 MB. **Fixing
this constant is the first thing on the list below.**

| pagefile | limit | instances (host lean) | (host as-is) | disk left | scratch@30 |
|---|---|---|---|---|---|
| 30 GB (today) | 63 GB | 13 | 10 | 111 GB | 41 GB |
| **80 GB** | 113 GB | **25** | 21 | 61.5 GB | 41 GB |
| 96 GB | 129 GB | 29 | 25 | 45.5 GB | 41 GB |
| 100 GB | 133 GB | 30 | 26 | 41.5 GB | **no margin** |

**30 does not fit on this box.** The pagefile that buys the commit takes the
disk the scratch needs, and they meet at 30 with nothing spare. 25 at an 80 GB
pagefile is the honest target. The desktop baseline is worth ~4 instances by
itself (25.3 GB of commit; Opera alone is 7.4 GB across 62 processes).

**The pagefile has NOT been changed** — it needs an elevated shell and a
reboot, and it is the user's disk. Do not raise it until the item below is
fixed: buying commit for instances that die is buying the wrong thing.

### ⚠ THE OPEN ONE: instances die about 15 minutes in

The costs above hold all the way through. Survival does not. 20-minute watch,
six instances:

```
farm2   survived      0 deaths   guest headroom 1236 MB
farm3   survived      0 deaths   guest headroom 1306 MB  <- only one still in-world
farm9   died t+892s   1 death    guest headroom  680 MB
farm8   died t+919s   2 deaths   guest headroom  693 MB
farm6   died t+931s   2 deaths   guest headroom  709 MB
farm7   died in load  launch correctly failed: client_not_running
```

Deaths cluster at **892-931 s from each instance's OWN launch**, and split
perfectly on guest headroom: ~700 MB free inside the guest → dead at ~15 min;
~1250 MB → alive. All are lmkd `TOP` kills.

**Hypothesis, NOT yet measured: serialise the launches.** The two survivors
were launched into an empty box; the three that died were loading while other
instances were loading, took longer, and ended with half the headroom.
`pool_boot_slot` already boots one slot at a time for exactly this reason;
`cmd_start` does not. Test it before believing it.

### Do these, in this order

1. **Fix `COMMIT_OVERHEAD_MB`** (192 → ~825, measured) so `doctor` and
   `capacity_shortfall` stop under-counting by 1.8x. Everything anybody plans
   from those numbers is currently wrong.
2. **Test the serialised-launch hypothesis** above. Six launches with a
   wait-for-settled gate between them; watch guest headroom at t+0 and whether
   the ~15-minute deaths stop. This is the whole goal — "the game should
   perform as intended, it shouldn't crash".
3. **If headroom is the cause and serialising is not enough**, the next lever
   is `-m 3584/4096` for PS99, and it costs instance count directly: at
   4065 MB/instance today, every extra 512 MB is ~2 instances off the ceiling.
4. Only then the pagefile (80 GB, script at
   `scratchpad/set-pagefile-64g.ps1` — **re-size it to 80 GB first**, it was
   written for the dead 2048 rung), and only then the deploy.
5. Gaming latency numbers, and the macOS pass.

### Traps that cost time today

* **`OMNI_DATA_DIR` is the other half of `OMNIDROID_CONFIG_PATH`.** The config
  path selects the IMAGES; `config.data_dir()` decides `runtime/`,
  `accounts.json` and the scratch, and follows `OMNI_DATA_DIR` (default: the
  repo). Set BOTH to `%LOCALAPPDATA%\OmniExec` and the repo's engine runs
  against the app's accounts, runtime and image set. This is the fix for the
  "two config roots" trap below — no account syncing needed.
* **A stopped instance leaves its `governor.log` behind** (Windows will not
  unlink a file the detached governor still holds), and until today that log
  had no timestamps in it. A previous run's "THE CLIENT IS GONE" read as the
  live instance's. Now dated, and `stop` reports the wipe honestly.
* **Do not estimate elapsed time from how much work you have done.** Half an
  hour went into a nonexistent governor bug because a launch was assumed to be
  17 minutes old when it was 6. Read the clock.
* **`_pidof` can exit the process.** It calls `fail()` for a portless handle,
  which raises SystemExit, which is a BaseException and walks through its own
  `except Exception`. Use `game_is_running()` in probes.
* **Measure commit with `-accel whpx`** — without it QEMU reserves a ~1 GB TCG
  buffer and every per-instance number is wrong by that much.
* **Python output is buffered when redirected.** Use `-u`, and do not pipe a
  long probe through `tee` — it buffers too, and the log looks like a hang.
* **Cookies: 7 of 8 live** (`admn1b12farm4` is expired, HTTP 401). The app's
  store at `%LOCALAPPDATA%\OmniExec\accounts.json` is the good one and now
  holds the gaming account too.

## SUPERSEDED — 2026-08-17 (evening)

Read this block, then the two "PICK UP HERE" sections below for the detail.
**Everything claimed here was measured on this box, and every code change is
committed and pushed** (`omnidroid@6d957f2` on `gaming-gpu-window`,
`omni-executor@b70d5b8` on `slice-c-windows-exe`). **Nothing is deployed** —
the user asked for the deploy to be held until the whole goal is covered.

### The state of the box, after the user's restart

* disk **107 GB free** (was 30 — they cleared ~78 GB, which unblocks the
  pagefile)
* commit limit **63 GB** — the pagefile has **NOT** been enlarged yet, so
  commit is still the wall
* the user has **added several farming accounts through the installed app**
  and wants the benchmarks run across many of them, in-game, with the app
  closed

### Do these, in this order

1. **Sync the accounts.** The app's store and the repo's store are DIFFERENT
   FILES and they do not share (see the trap below). The app store had
   `admn1b12farm2/3`; the repo store had `admn1b12farm3/HezMi_ImYu`. The user
   says accounts sync through the server — `omni-executor/accountsync.py` is
   the path. Get one store holding all of them before benchmarking, and check
   each cookie: `admn1b12farm3` returned **HTTP 401 (expired)** earlier today.
2. **Verify PS99 at `-m 2048`.** THE WHOLE CAPACITY LADDER RESTS ON THIS and
   it is still unmeasured. `lean.GUEST_MEM_FLOOR_MB` raises PS99 to 3072
   because 2048 was once measured to OOM — but that predates zram and the
   working-set ceiling. If 2048 holds, the fleet ceiling goes 9 → 13 before
   any disk is touched.
3. **Multi-account in-game benchmark**, app closed, as many accounts as have
   live cookies. This is the number the user actually asked for. Watch commit,
   not RAM — see below.
4. **Govern the pool slots.** A warm slot today costs full `-m` and is
   ungoverned: measured **575–747 MB each** across 5 slots, cut to
   **384–406 MB** by applying the ceilings by hand. `maybe_start_governor` is
   only called from `cmd_start`, never from the pool.
5. **Auto-warm the pool for farming** in the executor. The pool works
   (adoption takes boot from 34.7 s to **0.07 s**) but is off by default,
   which is exactly the "always cold booting" complaint.
6. Gaming latency numbers, and the macOS pass. Then deploy.

### Traps that cost time today

* **Two config roots, and they do different things.** `OMNIDROID_CONFIG_PATH`
  selects the IMAGES only. `config.data_dir()` — which decides `runtime/`,
  `accounts.json` and the scratch — follows where the engine is RUN from. So
  `manager.py` in the repo uses the repo's runtime and accounts even with
  `OMNIDROID_CONFIG_PATH=%LOCALAPPDATA%\OmniExec\paths.json`. Roblox only
  exists in the APP's image set (offset `arceusremote`); the repo's
  `C:\Users\berat\OmniImages` base has **no Roblox at all**, which is why a
  launch there dies with `no_kiosk_reply`.
* **Measure commit with `-accel whpx`.** Without it QEMU reserves a ~1 GB TCG
  translation buffer and every per-instance number is wrong by that much.
* **RAM is not the wall any more; commit is.** Watch
  `GlobalMemoryStatusEx().ullAvailPageFile`. At 5 farming + 1 gaming the box
  was at **60198 / 65207 MB of commit** — 92% — while RAM still had 4.7 GB.
* **Python output is buffered when redirected.** Run long probes with `-u` or
  the log stays empty and looks like a hang.
* **`find_window` needs the pid now.** The window is renamed to
  `omni: <account>`, so a title-only search cannot find it.

## PICK UP HERE — 2026-08-17 (later): farming density

**Nothing deployed yet at the time of writing this section.** All measured on
this box with real PS99 launches through the executor's own argv (auto-login,
auto-join, `in_world: true` every run).

### The bug under everything: a live instance read as DEAD

`instance_live()` asked QMP with a **0.25 s** budget. QMP is served by QEMU's
main loop and a guest running a game keeps it busy — a live farming instance
answered `query-name` in **3 s**. So `list` said stopped, `stop` could not
stop it, and **the memory governor exited with "QEMU process is gone" after one
shrink**. That is the whole "it always uses the `-m` even when idle" symptom,
and it left a 3.2 GB QEMU orphaned with its ports held.

Liveness is now the pid's **creation time**, recorded at spawn
(`run.json: pid_started`). Microseconds, cannot be starved, definitive both
ways. QMP is a fallback only, at 4 s.

⚠ **Fixing this turned ON a governor that had never really run** — and the
governor as shipped was harmful (below). Do not ship one without the other.

### Memory: a working-set CEILING, not a periodic trim

`EmptyWorkingSet` on a timer never converges. Measured, every 30 s against a
live instance: host RSS stuck at 1–37 MB, **adb stopped answering within 12 s**,
the client killed, and the guest never recovered even after the trims stopped.

A hard working-set maximum (`SetProcessWorkingSetSizeEx`,
`QUOTA_LIMITS_HARDWS_MAX_ENABLE`) does the job properly:

```
ceiling   hostRSS  client  adb
(none)       3417  alive   0.05s
650           650  alive   0.04s
500           500  alive   0.10s
384           384  alive   0.10s   <- 152% of a guest core, 0.01 MB/s off disk
300           300  DEAD    0.04s
```

The faults are **soft** (standby list), which is why the guest stays fast at
8.9× less resident memory. The governor searches for the ceiling rather than
being told one — the floor is a property of THE GAME.

### CPU: rendering is not the cost, the game is

SurfaceFlinger is **6.7%** of a guest core against the client's **148%**. So
"only render on demand" saves ~7%: blanking the display did cut host CPU 150%
→ 32%, but only because it killed the client. The lever is a job-object hard
cap: **160.9% → 49.9%** at a 50% ceiling, client alive, adb 0.06 s.

**Per instance: 3417 MB → 384 MB, 161% → 50% of a core.**

### Launches: 257 s → 126 s cold, 101 s warm

The 120 s post-squeeze survival wait was 47% of the launch and is gone — the
governor already watches the client, so it starts *before* that wait instead of
after and records a death into run.json. The warm pool removes the boot itself
(**34.7 s → 0.07 s**); it works, it just has to be filled (`pool start`, or the
app's Keep-warm toggle).

### Why not 30 instances: it is DISK, twice over — and `doctor` now costs it

The governors made RAM and CPU cheap, which moved the wall. Per farming
instance at `-m 3072` on this box: **ram 42, cpu 48, disk 13, commit 9**.

Commit, measured on a paused QEMU — **and you must pass `-accel whpx`**:

```
-m 1024 whpx, -display none    1065 MB   (+41)
-m 2048 whpx, -display none    2092 MB   (+44)
-m 3072 whpx, -display none    3117 MB   (+45)
-m 3072 whpx + gtk,gl=on       3258 MB  (+186)
```

Without `-accel whpx` the same probe reads **+1070 MB** — that is TCG's default
~1 GB translation buffer, an artifact of the probe. I got this wrong once; the
constant now carries the numbers and the warning.

`memory-backend-ram,reserve=off` would move guest RAM off the commit limit
entirely. **It is not in this build** (`Property 'memory-backend-ram.reserve'
not found`), so commit == `-m` and QEMU offers no way out.

So `-m` is the only lever in code, and `omnidroid doctor` now reports the
ladder:

| `-m` | instances | wall |
|---|---|---|
| 3072 | 9 | commit |
| 2048 | 13 | commit |
| 1536 | 13 | **disk** |
| 1024 | 13 | **disk** |

Below 2048 the scratch reserve takes over, so shrinking the guest stops
helping. `capacity_shortfall(30)` costs the goal: at `-m 2048`, **35 GB short
on commit, 31 GB short on disk** — and since the pagefile lives on the same
volume, both are the same shopping list.

**To actually reach 30 on this box: free ~66-90 GB and set the pagefile to
~96 GB.** There is room to find — measured on C: (953 GB, 30 GB free):
`C:\Users\berat\.lmstudio` **90 GB**, Videos 30, Downloads 28.5, Documents
25.6, `C:\Riot Games` 31.7, `C:\XboxGames` 24.6. Nothing was deleted; that is
the user's call.

⚠ **STILL UNMEASURED: whether PS99 actually farms at `-m 2048`.** The ladder
above assumes it does. `lean.GUEST_MEM_FLOOR_MB` raises PS99 to 3072 because
2048 was measured to OOM — but that was before zram and before the working-set
ceiling. Re-measure before promising 13 instances.

## PICK UP HERE — 2026-08-17

The window. **Nothing was deployed** — no new app version, no images pushed,
the server untouched. Everything below was run on this Windows box.

### The window is on screen for the boot now

It used to open, get hidden, and only come back when the launch finished — so
the first thing the user ever saw was Roblox already running, and the minute
before that looked like a frozen app. `qemu_proc.place_window` decides at spawn
now: **present** (gaming — somebody is watching), **hide** (farming — fifty at
a time, and fifty windows is not a product), or **leave alone** (`--gpu
window`, the debug hatch). Measured on a real boot:

```
t+0.0s   no window yet
t+3.1s   window exists, hidden, 640x505   title 'QEMU (omni-HezMi_ImYu)'
t+3.5s   window VISIBLE,        640x505   (GTK shows it)
t+3.7s   window VISIBLE,       1280x800   title 'omni: HezMi_ImYu'
```

**3.7 s**, against ~133 s before — which is when `start` returned and the app
finally called `view`. `OMNI_HIDE_BOOT_WINDOW=1` / config
`qemu.hide_boot_window` puts the old behaviour back.

### The aspect ratio is held LIVE, and there are two halves to that

1. **`keep-aspect-ratio=on`** on the gtk display, so QEMU letterboxes instead
   of stretching. Verified in the pixels, not inferred: grabbing the client
   area at 1400x500 gives left/right columns a constant 25.0, at 700x800 the
   top/bottom rows do, at 1280x800 neither does.
   ⚠ **The suboption is not in `-display help`** (that text is hand-maintained;
   it lives in the QAPI schema) and **QEMU refuses an unknown suboption rather
   than ignoring it** — a wrong guess costs the boot, not the chrome. Checked
   against the shipped binary; a test re-asks, with a control.
2. **`hostwin.aspect_lock`**, a detached `_windowlock` process that corrects
   the window's *client* area **while you are still dragging it**, not when you
   let go. A resize runs inside `DefWindowProc`'s modal loop, so an outside
   `SetWindowPos` "should" lose the race — measured against a real modal drag,
   it does not:

   | corrector | samples off-ratio >2% | final ratio |
   |---|---|---|
   | none | 76/246 (31%) | 1.98 — 24% off |
   | every 8 ms | 2/246 (1%) | 1.600 |

   Idle cost **0.16% of one core**. The drag axis is LATCHED for the drag;
   re-deciding it per frame makes the lock and the drag argue on an edge drag.
   All three kinds measured: corner 880x550→1482x926 (0/188 off), bottom edge
   →1264x790, right edge →1200x750, each keeping the dimension being dragged.
   The guest is still told once, a second after the drag stops, because QEMU
   coalesces and the lock stops as soon as the shape is right.

   With the guest up, `wm size` read `Physical size: 1280x800` against a
   1000x624 window — same 16:10, uniform scale, nothing distorted.

**Client pixels, not window pixels.** QEMU hands the guest its DRAWING AREA
size, so sizing the window to 1280x800 gives the guest 1264x761 — the frame is
16x39 on this host. Everything in this path is now the client area.

### Our name and our icon, on QEMU's own window

`WM_SETTEXT`/`WM_SETICON` are marshalled between processes, so QEMU's
`QEMU (omni-<account>)` + QEMU logo become `omni: <account>` +
`omnidroid/assets/omni-icon.png`. No patched build, no bar stacked on top.

That PNG has been in the tree since 2026-08-16 with **nothing consuming it** —
it was added for the `QEMU_WINDOW_ICON` env var no build reads. `LoadImageW`
cannot read a PNG and this repo ships no `.ico`, but
**`CreateIconFromResourceEx` takes the PNG bytes directly**, so there is no
conversion, no generated `.ico` and no temp file.

⚠ **RENAMING THE WINDOW BROKE FINDING IT.** `find_window` matched the identity
as a title SUBSTRING and our title (`omni: farm3`) does not contain the
identity (`omni-farm3`). Falling back to the pid is necessary and **not
sufficient**: a QEMU process owns half a dozen windows —

```
gdkWindowToplevel       the only one that is ever the guest's
NVOpenGLPbuffer         'NVOGLDC invisible', 1914x994, invisible
GDI+ Hook Window Class  1x1
GdkDisplayChange        0x0
Default IME / MSCTFIME  0x0
```

— and the real one does not exist until **t+0.22 s**, while `find_window` runs
~50 ms after spawn. A naive pid fallback returned the NVIDIA pbuffer, and
everything downstream styled, renamed, resized and watched a window nothing is
drawn in while the real one sat on screen at 640x505 wearing QEMU's name. A
pid-only candidate now has to LOOK like a guest window (top-level, not a decoy
class, ≥64x64 client), which restores "wait for the real window".

### Three things that were built on a QEMU that does not exist

Both were fixed here, and the phantom is now labelled in the source so it
cannot be trusted again:

* **`window-close=off` is back.** It was removed because "our patched QEMU asks
  'Stop this instance?' on the X" — it does not, so the X killed the guest
  instantly with no prompt. That is a launch lost to one mis-click, on a window
  that now sits there for the whole boot. The cost is an X that does nothing;
  the window is managed from the app instead (Hide / Stop).
* **`QEMU_WINDOW_*` is inert** and now says so at both sites. Do not add
  behaviour behind those names.
* **The window's title and icon** were supposed to come from that build too.
  They come from `WM_SETTEXT`/`WM_SETICON` now — see above — and the icon
  asset that was sitting unused is the one being used.

### The bridge

`account_status` now reports `native_window` / `window_visible` /
`window_client` / `has_vnc`, and the app uses them:

* the viewer button is a **View/Hide toggle** — `engine_hide` had existed in
  `main.py` since it was written and **nothing had ever called it**, which was
  survivable only while the window appeared at the end of a launch;
* a running GPU instance's row said `VNC 18001 · ADB 15001` for a port nothing
  is listening on (QEMU serves no VNC beside a GL context). It now reads
  `Window 1280×800 · ADB 15001`.

`view` branches on `boot_shows_in_a_window` rather than
`boot_has_hidden_window`: the second is False on precisely the boots that need
the window path, and an already-open window is raised rather than re-set-up
(re-applying the saved geometry would yank a window the user had moved).

### Still open here

* **The X does nothing now.** That is the deliberate trade above, but the
  honest fix is a close that ASKS. It cannot be done from outside the process
  (Windows will not let one process handle another's `WM_CLOSE` without
  injecting a DLL), so it needs either a patched QEMU or a decision that the
  app's Hide button is discoverable enough.
* **Not run on macOS or Linux.** `place_window`'s present path is win32-only
  and returns a reason elsewhere; the window comes up wherever QEMU put it.

## PICK UP HERE — 2026-08-16 (evening)

Everything below this section still reads true; this is what changed after it.
**Nothing was deployed.** The app version is untouched, no images were pushed,
and the server is exactly as it was.

### 0. OPEN AND IMPORTANT: PS99 farming instances die ~60-90 s after launch

Every "healthy" farming reading this project has ever taken was sampled within
about a minute of the launch returning. Watching one for six minutes instead:

```
              t+0s    t+30s   t+60s   t+90s     last alive
GPU (auto)    alive   alive   alive   DEAD      t+60.14 s
software      alive   alive   alive   DEAD      t+60.19 s   <- control
```

**Identical to within 0.05 s across two completely different render paths.**
That is not a random crash; something on a timer kills it. The Roblox process
is simply gone — `pidof` empty, `screencap` solid black, the guest's
MemAvailable jumping ~591 MB -> ~2227 MB as its 1.6 GB is released — and
**nothing is written to the client log**, so there is no error code to chase.

What this rules out: it is NOT the renderer (both paths die identically), NOT
the scratch disk (that is fixed and the volume had 7+ GB free), NOT the 320x180
panel (these runs used 480x270).

**IT IS THE SQUEEZE.** Re-run with `OMNI_FARM_SKIP=<every step>`, everything
else identical:

```
              t+0s   t+30s  t+60s  t+90s  t+120s t+150s t+180s
squeezed      alive  alive  alive  DEAD
NO squeeze    alive  alive  alive  alive  alive  alive  alive
```

Seven minutes, all 15 samples, still alive. So the squeeze is the cause.

**But it is not one lever — it is HEADROOM.** Bisecting `lmkd` (the obvious
suspect: it is configured `kill_heaviest_task=true`, and Roblox is by far the
heaviest task) only bought 30 s, and the memory trace says why:

```
t+0    MemAvailable 1264 MB
t+30                1215 MB
t+60                1058 MB
t+90                 808 MB     <- falling steadily
t+120  DEAD          2287 MB    <- the game's ~1.5 GB released
```

**PS99 is still growing when the squeeze runs**, and the squeeze takes away the
headroom that was hiding it. `wait_for_game_settled` declares the client
"settled" on two samples within 4% — which PS99 satisfies while it is still
climbing. So the squeeze fires early, and the client dies about a minute later.

**And it is NOT simple exhaustion either.** Full squeeze at `--mem 4096`:

```
t+0    MemAvailable 2048 MB      t+90   1856 MB
t+30                2019 MB      t+121  1704 MB
t+60                2010 MB      t+151  1604 MB
                                 t+181  DEAD
```

It lived 2.5x longer than at 3 GB — and died with **1604 MB still
available**. So memory is not the wall; more headroom only buys time. The
squeeze is doing something that kills a client which still has 1.6 GB in front
of it.

Run matrix so far, all PS99, all farming, all in-world at t+0:

| configuration | last seen alive |
|---|---|
| full squeeze, 3 GB, GPU | t+60 |
| full squeeze, 3 GB, software (control) | t+60 |
| full squeeze minus `lmkd`, 3 GB | t+90 |
| full squeeze, **4 GB** | t+151 |
| **whole squeeze skipped**, 3 GB | **t+421, still alive** |

That reframes it: the levers below are not individually guilty in a simple
way, and turning them off one at a time keeps giving 30-second answers because
each one only shifts the deadline.

**Read `ro.lmk.*` on a FRESH boot before doing any more lmkd bisecting.** They
read `true` after a squeezed run, but that is our own step setting them —
whether the Bliss base already ships them true is unknown, and if it does then
`OMNI_FARM_SKIP=lmkd` never disabled anything and that whole result means
something different.

Old suspect list, kept because bisecting them is still how you would confirm
any residual effect:

* **`ro.lmk.kill_heaviest_task=true`** is the prime suspect. Roblox IS the
  heaviest task in the guest by a wide margin, and lmkd is explicitly
  configured here to kill the heaviest one under pressure.
* `am send-trim-memory RUNNING_CRITICAL`
* the `background` cpuset (the game is moved off `top-app`)
* `dumpsys deviceidle force-idle`

The bisect knob already exists and is the right tool:
`OMNI_FARM_SKIP=lmkd,doze,trimmemory,cpuset` and friends. A run with the WHOLE
squeeze skipped is the first fork — if it still dies at ~60-90 s the cause is
outside this engine (game-side or network), and if it survives, bisect the four
above.

**This is the single most important open item.** "The game should perform as
intended, it shouldn't crash" is the one hard requirement farming has, and a
farming instance that dies after a minute meets none of it. Note also that the
launch's own `in_world: true` is measured DURING that first minute, so it is
not evidence the instance is healthy.

### 0a. THE BIG ONE: farming's CPU is llvmpipe, and the GPU halves it

Everything this project believed about farming's cost was wrong, including
what an earlier part of this session wrote down. Attributing CPU per THREAD
(`/proc/<pid>/task/*/stat`, 20 s, PS99, in-world) instead of per process:

```
software (--gpu headless)          GPU (hidden GL window)
  llvmpipe-1      52.8%              (gone)
  llvmpipe-0      51.8%              (gone)
  HttpClient      11.1%              FunctionMarshal  18.3%
  FunctionMarshal 10.0%               RBX Worker A    17.3%
   RBX Worker A    6.8%               RBX Worker B    20.0%
  ---------------------              ---------------------
  TOTAL          141.1%              TOTAL            72.3%
```

**Three quarters of a software farming instance's CPU is software GL**, not
arm64 translation. The translated game code (` RBX Worker *`) is a minority.

This also explains two null results that had been misread as "there is no
render lever": the fps cap throttles Roblox's task scheduler and the panel
changes its pixel count, and **neither reaches the rasteriser's per-frame
work**. The lever was never "render less" — it is "render somewhere else".

`MODES["farming"]["gpu"]` is now `GPU_AUTO`. Since CPU is what decides how
many instances a host holds, this roughly doubles the ceiling. The settle also
got faster (67 s vs 116-148 s).

* **`auto`, not `window`.** An explicit `window` means "I want to see it", so
  `_hide_window_if_wanted` leaves it visible — verified, `--gpu window` really
  does put a farming window on screen. `auto` opens one only because this host
  has no other GL route, then hides it.
* **It degrades, it does not fail.** A real headless farm box has no window
  server; `auto` finds nothing and falls back to software.
* **Cost:** no VNC on a GL boot, so `capture`/`autocap` are unavailable and
  `omnidroid view` uses the embedded window. **`screenshot` goes via adb and
  works** — verified against a GPU farming instance.
* **Untested: GPU contention with many concurrent instances.** Only one Roblox
  cookie was live, so one in-world instance is the only data point. Take that
  number before promising a fleet size.

**Cookie state, 2026-08-15 evening: only `admn1b12farm3` is live.** `farm4`
started the session working (it ran the gaming benchmark) and returned HTTP 401
later the same evening; `farm2` was already expired. Re-login both before any
multi-instance work.

### 0. The thing that was killing instances: QEMU's scratch

Instances were dying three minutes in, reproducibly, with a **zero-byte
`qemu.log`** and no Windows error report. It had been diagnosed twice as a
guest crash. It is not.

An ephemeral boot runs `snapshot=on`, so QEMU keeps the guest's writes in a
temporary overlay — and puts it wherever libc says, which is `%TEMP%`.
Measured on PS99: **1.3 GB per instance**, and they **leak**, because a QEMU
that dies rather than exits never unlinks its own file. This box had 3.7 GB of
them across three sessions, the oldest two days old. With the volume down to
4 GB the next launch ran it out — and QEMU could not write down why, because
writing the log needed the disk that had just gone.

Fixed: overlays go to `<data dir>/scratch` (`TMP`/`TEMP`/`TMPDIR` on QEMU's
child env), leaked ones are reaped on every boot and every pool tick, and
`doctor` reports `scratch_dir` / `scratch_free_mb` / `scratch_fits_instances`.
Verified against real QEMU: overlays land in the scratch dir, `%TEMP%` stays
empty, a clean stop still lets QEMU unlink its own.

**Plan capacity off the DISK, not only the RAM.** At ~1.3 GB scratch plus
2.2–3.2 GB host RSS per instance, this box (7–8 GB free) holds **three**
instances before the disk stops it, while `list` shows the rest healthy right
up until they vanish. `%TEMP%` here also still holds a **13.76 GB `zulasetup`**
directory from an unrelated installer — deleting that is the single biggest
thing that would raise the instance ceiling on this machine.

### 1. Warm boot: the pool is the answer, and it is FASTER than snapshots

Researched properly this time, and reproduced locally on QEMU 11.0.50:

| host | save/restore of VM state today | evidence |
|---|---|---|
| **Windows / WHPX** | **No.** `migrate`, `savevm` and QMP `migrate` all refuse with the same blocker. `-accel tcg` on the same binary snapshots fine, so the machinery works — WHPX is fenced off. | reproduced here |
| **macOS / HVF (arm64)** | **Yes** — no `migrate_add_blocker` in any HVF file; ARM HVF registers vmstate. Not confirmed end to end; verify on the Mac. | QEMU master source |
| **Linux / KVM** | **Yes**, reference implementation | — |

Google's Android Emulator gets Quick Boot on Windows by **commenting the
blocker out** of their QEMU fork (their source says so in a comment; the
string is absent from their shipped binary). That is the only route, and it
needs a patched build.

**But it would be slower than what we already have.** A `loadvm` has to read
~2.2 GB of guest RAM off disk. The pool hands over a slot in **0.08 s**. The
snapshot's value would be capacity (parking idle instances to disk) and
surviving a host reboot — not latency.

So the pool is the answer on all three platforms, and it now:

* **resyncs the guest clock on adoption** — `resync_guest_clock` had exactly
  one call site, the warm-restore branch, which is unreachable on Windows. A
  live slot's clock normally ticks fine; the case a desktop has is the host
  SLEEPING, and Roblox rejects a skewed clock **identically to a dead cookie**.
  Measured on a real adoption: `guest clock resynced (was -18s behind host)`.
* **releases a claimed slot when adoption throws.** `cmd_start` catches
  everything and boots normally, which fixes the launch and abandons the slot:
  one exception left `adopted.json` on disk permanently, so a live ready
  instance was never handed out again while `pool status` reported the pool
  full. Found by running it.
* **takes `--place`.** `mem` is part of the slot key, so a pool warmed at
  farming's 2048 is *invisible* to a PS99 launch resolving to 3072 — every
  launch cold-boots while `pool status` says slots are ready.

```
pool fill --size 1 --mode farming --place 8737899170    slot ready in 34.8 s
start <acct> --place 8737899170 --mode farming          guest RAM raised to 3072
                                                        guest clock resynced
                                                        took slot _pool0, no boot
```

### 2. Farming's memory floor is a property of the GAME

`MODES["farming"]["mem"]` is 2048; PS99 is OOM-killed there and needs 3072.
That was never a farming constant set too low — it is a per-game number with
no home. `lean.GUEST_MEM_FLOOR_MB` is the home; `guest_mem_floor_mb` only ever
**raises**, so gaming's host autoscaling is untouched and `--mem` still wins.
An unmeasured place gets the default and may OOM.

### 3. Measured on PS99 (place 8737899170), x86 base, this box

| | farming | gaming |
|---|---|---|
| boot | 35 s (+116 s squeeze-settle) | 60 s |
| host RSS | **3.24 GB** at `--mem 3072` | 4.58 GB at 4096/4 vCPU |
| guest at rest | 1.19 GB available, 925 MB swapped | — |
| game PSS / RSS | 1302 / 1095 MB | — |
| guest CPU | **168% of 200%, 20% idle** — CPU-bound, not memory-bound | — |
| reached the world | **yes** (`in_world: true`) | game-side **Error 773** (teleport restricted) |
| rendering | headless, zero host cost | GPU via hidden GL window (frames verified changing) |
| view toggle | VNC framebuffer pulls a full 1280x800 frame | window reparented into our viewer |
| screenshot | works headless, on demand | works |

**~400 MB per instance is not reachable on Windows and the reason is not the
balloon.** Host RSS tracks `-m` almost exactly (3072 → 3.24 GB) because QEMU
has no `madvise` there, and PS99 itself needs ~1.5 GB resident in-world. The
400 MB figure came from a login screen, not a loaded game.

### 4. Still open

* **The GL renderer mask stays OFF.** `wrap.<pkg>` is mutually exclusive with
  the arm64 translator on the x86 base (seccomp kills the bridge's `mount`).
  The two viable routes are image-side — `export` in zygote's init rc, or
  `setenv()` from OmniBootstrap — and both need the APK/bake toolchain, which
  is Mac-only (`omni-exec-android` is an empty folder on Windows).
  **Note this is defence in depth, not a blocker: farming already reaches the
  PS99 world headless without Roblox objecting.**
* **`--quality minimal` was measured and REMOVED.** Four runs on PS99: its
  320x180 panel kills the client every time it is applied (process gone,
  black screen, the guest's 1.6 GB released) — including after the sequence
  was folded to resize only once, which is what ruled out "a second mid-session
  `wm size`" as the cause. And 3 fps idles the same as 5 fps, because the guest
  is CPU-bound on arm64 translation, not fill-bound. The profile is gone from
  `QUALITY_PROFILES`; farming stays at `low`. **There is no render lever left
  on this base** — the ones with room are `-m` and free scratch disk.
* **The Mac was unreachable all session.** ProtonVPN's kill switch blocks LAN,
  and the VPN is not optional here (Roblox is blocked on this network without
  it). Enable "Allow LAN connections" in ProtonVPN to reach `192.168.0.30`.
  No arm base is registered on Windows, so arm was never exercised either.
* Gaming's `probe_client_join` runs ~1 s after delivery, long before a client
  could have joined, so `in_world: false` on a gaming launch means nothing.

## PICK UP HERE — 2026-08-16

The 2026-08-15 evening list is below and still worth reading; this section is
what changed after it.

> **Nothing in this session was committed.** `omnidroid` on Windows carries
> the previous session's uncommitted evening work (~77 modified tracked files,
> plus untracked `hostwin.py`/`embedview.py`/`migfile.py`/`consent.py`/
> `awake.py`/`qmpsession.py`/`timings.py`), and committing "my" files would
> have swept all of that into one commit under a message that did not describe
> it. **New this session, all untracked:** `omnidroid/pool.py`,
> `omnidroid/glmask.py`, `omnidroid/netmtu.py`, `tests/test_pool.py`,
> `tests/test_gl_mask.py`, `tests/test_guest_mtu.py`,
> `docs/windows-ram-discard.md`. Modified: `engine.py`, `farming.py`,
> `qemu_proc.py`, `tests/test_farming_apply.py`,
> `tests/test_qemu_footprint.py`, `tests/test_gaming_apply.py`,
> `tests/test_warm_boot_policy.py`, `MODES.md`, `CHANGELOG.md`,
> `FOOTPRINT.md`. Decide how to land that lot before adding more.
>
> **Eight of those pre-existing modified files are wholly CRLF in the working
> tree while LF in the index** — `omnidroid/{bases,vncview,warmboot}.py` and
> `tests/test_{farming_mode,gaming_mode,headless_gpu,qemu_accepts_devices,warmboot}.py`.
> That is the line-endings gotcha this document already lists, and it has
> already happened: each will diff as a whole-file rewrite. Normalise them
> (`data.replace(b"\r\n", b"\n")`) before committing or the real changes are
> unreviewable; `git ls-files --eol <path>` tells you which.
> `tests/test_warm_boot_policy.py` was in that state and has been normalised.

### 1. SOLVED: instant boots on Windows — the warm POOL

```
omnidroid pool start --size 2 --mode gaming    # background manager
omnidroid pool fill  --size 1 --mode gaming    # boot now, in this process
omnidroid pool status / omnidroid pool stop
```

Measured on PS99, x86 base, gaming 2048 MB / 2 vCPU:

```
pool fill                 slot ready in 58.8 s
start <account> --place   warm pool: took slot _pool0
                          timings.stages.boot = 0.082 s     (was 47-190 s)
                          session delivered   = 7.6 s
```

A slot is an instance booted to the **account-free ready point** — the state
`bake_entry` used to freeze. Instances are diskless (`snapshot=on`), so every
account on one offset boots identical disks; the session broadcast is what
makes an instance somebody's, and it arrives long after boot. Nothing is
serialised, which is why this works where the warm CACHE cannot (WHPX blocks
migration; §9b).

New: `omnidroid/pool.py`, `pool`/`_poolmgr` subcommands, `tests/test_pool.py`
(15 tests). `MODES.md` has the design notes — the three traps are that
adoption must COPY `run.json` (Windows will not move a directory with an open
handle in it), must keep the slot's `identity` verbatim (or the adopted
instance reads as dead), and must claim with `O_EXCL` (or two accounts land on
one guest).

### 2. SOLVED: `--mode farming` reaches the PS99 world

Screenshot-verified in-world: the Roblox top bar, PS99's live player
leaderboard (real usernames, ranks and diamond counts), the game's chat
scrolling, and its own teleport logic running. The instance was **already
squeezed** at that point (display override 480x270), which is the state
farming exists to produce.

```
omnidroid start admn1b12farm3 --place 8737899170 --mode farming --mem 3072
  guest MTU 1420 (from this host's egress interface)
  session -> kiosk: place 8737899170, joined
  the game has finished loading (2252 MB resident) — squeezing now
  balloon: not inflating — this host cannot return the pages
  boot 155 s;  guest 2.9 GB, 587 MB available, 341 MB swap free
  game PSS 2.1 GB / RSS 1.5 GB;  host RSS 3239 MB
```

It took four separate things, and only two of them were bugs in this engine:

| | |
|---|---|
| the squeeze ran BEFORE the client loaded | moved to after (§2 below) |
| the balloon squeezed the guest for no host gain | skipped where the host cannot reclaim |
| Roblox blocked on this network (GoodbyeDPI mangled the game's UDP) | **the VPN** — not ours to fix, but it is why every join said 279 |
| the guest's MTU (1500) did not fit the VPN's (1420) | `virtio-net-pci,host_mtu` (§2b) |

**PS99 needs `--mem 3072`.** At the shipped 2048 the client is OOM-killed —
measured three times, under gaming tuning with nothing else in the way. The
farming defaults are right for a light place and wrong for this one; that is a
per-GAME property (see `FOOTPRINT.md`).

### 2a. Farming: the SQUEEZE is what stops PS99 loading — not memory

The last session's conclusion ("the translator cannot survive swapping") was
right about the mechanism and wrong about the trigger. Re-measured today, all
on PS99, all screenshot- and `dumpsys meminfo`-verified:

| run | guest | tuning | outcome |
|---|---|---|---|
| `--mode farming` (as shipped: 2048, balloon 896) | 830 MB | full squeeze | **session delivery itself timed out** (`pm path` no answer in 45 s, twice) |
| farming, `--balloon 0` | 2048 MB, 1.2 GB free | full squeeze | translator abort at 11 s, game never came back |
| **gaming, `--mem 2048 --smp 2`** (control) | same | gaming tune | **zero aborts**, game grew to 1476 MB and was OOM-killed |
| farming, `--mem 3072 --balloon 0` | 3 GB, 1.8 GB free | full squeeze | game alive, PSS flat ~400 MB, **engine deadlocked in `futex_wait`**, guest 200% idle |
| farming, 3072, `--quality balanced` | 3 GB | full squeeze | same stall — **the 5 fps tick cap is not it** |
| **farming, 3072, `OMNI_FARM_SKIP=<all steps>`** | 3 GB | none | **PSS 1173 MB at 111 s, `Connection accepted from` — it loads** |

So: not the balloon, not memory, not the tick cap, not `smp 2`. **The farming
squeeze is the blocker**, and the control proves the same guest size and vCPU
count load the place fine with gaming's tuning.

Two facts worth carrying forward:

* **Swapping hard does NOT break the translator.** The gaming control ran its
  zram to `SwapFree: 0.2 MB` with zero aborts. The previous session's
  swappiness fix was real but the explanation was not.
* **The engine deadlocks rather than crawls.** `debuggerd -j` on a stalled
  client: Roblox's `Main` and its single ` RBX Worker A` both parked in
  `futex_wait` (syscall 202, NULL timeout) with the guest 200% idle. It is not
  slow — it is waiting for something that never comes.

**Done — the squeeze now runs after the client has loaded.** It used to run in
`_ensure_booted`, i.e. BEFORE the session is delivered and therefore before the
client had been told which place to load, while every lever in it exists to
make a JOINED, IDLE instance cheap. It moved to `settle_density_instance()`,
called from `cmd_start` after delivery and after the client's memory stops
growing (`wait_for_game_settled`; `OMNI_SETTLE_TIMEOUT` bounds the wait). A
density launch is minutes rather than seconds now and reports it as
`timings.stages.density_settled`. The bisect knob stays:
`OMNI_FARM_SKIP=display,packages,zram,swappiness,lmkd,quiesce,doze,trimmemory,cpuset`.

**Also done: no balloon on a host that cannot take the pages back.** Measured
guest 830 MB / host 2190 MB — the inflate was never a host saving on Windows
and it cost the guest enough to time out the session handover. `-m` is the
lever there. An explicit `--balloon` still wins; `OMNI_FORCE_BALLOON=1`
overrides.

### 2b. The join failures were the NETWORK, not the engine — and the MTU is now set

`Error Code: 279` on every cold join turned out to be Roblox being blocked on
this network: the machine was reaching it through GoodbyeDPI, which passes
HTTPS and mangles the UDP the game server needs. Switching to a VPN got the
client to `Connection accepted from 128.116.13.34|59036` — a real Roblox game
server — for the first time.

**And then it disconnected with `Error Code: 277`, which was a second, real
bug in this engine.** QEMU's user networking hands the guest **1500** and
sends its packets out through the host's stack, which on ProtonVPN is
**1420**. TCP survives (MSS negotiation); **UDP does not**, and Roblox's
gameplay traffic is UDP. So: assets load, the server connects, and the moment
the world streams the connection dies.

Fixed: `omnidroid/netmtu.py` probes the egress interface's MTU and
`virtio-net-pci,host_mtu=N` hands it to the guest (virtio has a feature bit
for it, so the guest kernel does the rest). Only applied below 1500;
`OMNI_GUEST_MTU` / config `network.mtu` override. Two traps are written up in
the CHANGELOG — the adapter's LINK mtu reads 65535 on a TUN driver while the
IP interface is 1420, and a `probe=` default argument binds at definition time.

**277 now has two causes needing opposite fixes**: the asset CDN DNS-blocked
(§8b, fixed by Private DNS) and this one. Tell them apart by whether the
game's own loading art appears — if it does, assets are fine and the problem
is the game-server path.

**Note for whoever tests next: switching to the VPN invalidated
`admn1b12farm2`'s Roblox cookie (HTTP 401 at the preflight).** `farm3` and
`farm4` are still live. A big IP/region change is enough to make Roblox drop a
session, so expect to re-`login` accounts after changing how the host reaches
the internet.

`probe_client_join()` now reads the client's own log after a density launch
and says so: `ok: true` only ever meant "the session was delivered and the
game launched", and a client sitting on Error 279 satisfied every check the
engine had.

### 3. Farming undetectability: `wrap.` cannot work on the x86 base

`MESA_GL_RENDERER_OVERRIDE` via the `wrap.<pkg>` property is implemented
(`omnidroid/glmask.py`), the property sticks, and **the client dies three
seconds later**:

```
Cause: seccomp prevented call to disallowed x86_64 system call 165   (mount)
  #01 libnativebridge.so (PreInitializeNativeBridge+1204)
```

That mount is the arm64 translator setting itself up. Zygote does it before
installing the app's seccomp filter on the ordinary fork path; the
`invokeWith` path re-execs, so the filter is already on. Roblox is arm64-only,
so on this base every launch goes through the bridge — `wrap.` and translation
are mutually exclusive here. **Default OFF**, and a boot that does not want it
clears the property. The two routes that could still work are image-side and
written up in `glmask.py`: `export` in zygote's init rc, or `setenv()` from
OmniBootstrap (which already runs inside the game process).

### 3b. Diagnosing a client that will not get into a place

The three failure modes look identical from outside the guest and need
completely different fixes. Tell them apart by WHERE it stops:

| what you see | what it is |
|---|---|
| black/plain Roblox splash forever, guest **idle** | the client is wedged — check `debuggerd -j $(pidof com.roblox.client)` for threads parked in `futex_wait`, and whether the farming squeeze ran before it loaded |
| Roblox splash, then **Error 277**, no game art | the ASSET CDN is DNS-blocked (§8b) — `ensure_private_dns` |
| **the game's own loading art appears**, then 277/279 | assets are fine; the GAME-SERVER path is broken — the MTU (§2b) or a DPI/VPN issue |
| in-world, then `Error Code: 773` | not an error in this stack at all — a game-side teleport restriction |

`omnidroid screenshot` answers the "which one" question in one command, and
`probe_client_join()` reports it in `start --json` for a density launch.

### 4. 30 instances on 32 GB: the game is the wall, not the hypervisor

`docs/windows-ram-discard.md` has the `DiscardVirtualMemory` patch for
`ram_block_discard_range()`, written and reviewed but **not built** — a QEMU
build tree does not fit in this box's 6.3 GiB of free disk. It also has the
measurement that matters more: **one PS99 client needs ~1.5 GB resident
in-world** (PSS 1018 → 1476 MB, then killed in a 2048 MB guest). Even with a
perfect discard, that is ~15 instances on 32 GB, not 30. The ~400 MB figure
this target came from was measured at the LOGIN SCREEN. 30 instances needs a
game whose in-world working set is ~700 MB — measure the intended place before
promising a fleet size for it.

---

## Previous list — open actions, 2026-08-15 (evening)

The 2026-08-15 evening session went after four things: instant boots, maximum
speed at high resolution, two modes instead of five, and headless-QEMU-with-a-
VNC-viewer that keeps the GPU. **Three landed. The fourth is not achievable on
this hardware and the reason is now measured rather than suspected** — see §9.

### 1. SOLVED: GPU rendering with no QEMU window on screen

The requirement was "GPU, and I only ever see MY viewer". On Windows QEMU will
not give a GL context without a window, and will not serve VNC beside one (both
re-measured; §9a). The way out is not to fight either fact:

```
the window is opened (QEMU needs it) -> hidden immediately -> `omnidroid view`
reparents it INTO our own Tk window with SetParent
```

Measured, all on PS99: a hidden window **keeps rendering** (303 frames / 30 s
invisible); hiding at spawn does **not** break the boot; and embedded in our
viewer the guest ran at **58 fps** (702 frames / 12.1 s). No copy, no encode, no
decode, and input goes straight into `usb-tablet`/`usb-kbd` instead of being
synthesised from RFB — lower latency than VNC could ever be.

New: `omnidroid/hostwin.py` (hide/keep-hidden/show), `omnidroid/embedview.py`
(the reparenting viewer), `omnidroid view` picks it automatically, and
`_embedview` is its hidden subcommand.

**Two things to know:**

* **GTK re-shows the window during early boot.** One hide at spawn was undone
  by the time the guest had joined. `hostwin.keep_hidden()` re-hides for the
  length of a boot and then stops; after that a single hide sticks.
* **A FORCE-killed viewer destroys the window and blinds the guest.** Windows
  destroys a child window with its parent, and QEMU does not make a new one:
  the instance stays alive on adb and renders nothing (`totalFrames = 0`).
  Closing the viewer with its X is clean and hands the window back hidden.
  `omnidroid view` detects the destroyed-window state and says so rather than
  opening an empty viewer. **If this proves annoying in practice, the fix is to
  stop reparenting and instead keep QEMU's window top-level, borderless, and
  positioned over the viewer's client area — safe, but fiddlier z-order.**

Linux does not need any of this (`egl-headless` presents there, so GPU +
windowless + VNC all work at once) and macOS has no virgl yet, so both keep the
plain VNC viewer. **The viewer is the same on all three; only what it attaches
to differs.**

### 1b. Guest panels above the base's native mode do not boot

`--panel` sets the guest display, and it works downward. Upward it costs and
buys nothing: `--panel 1080p` on the x86 base took **3.3+ minutes without
reaching adbd** (0.8 min for the same image at 1280x800), with QEMU alive and
**zero scanout errors in its log** — and when the guest did come up it was
**still 1280x800**, confirmed by `screencap` returning a 1280x800 image. The
guest ignores a mode its panel does not carry, after a long stall trying. The
base's native panel is 1280x800 and that is the ceiling until a base ships a
larger mode.

### 2. There is no warm-boot cache on Windows either, and it is the hypervisor

`omnidroid start` cold-boots in 47–102 s and cannot be made to snapshot:
**QEMU/WHPX registers a migration blocker at CPU realize time.**

```
warm bake failed (migration State blocked due to non-migratable CPUID feature
support,dirty memory tracking support, and XSAVE/XRSTOR support)
```

Measured against a real booted instance (§9b), after the two *other* blockers
on the path had been removed. `_warm_cache_allowed()` now refuses the whole
mechanism under WHPX instead of paying a guest stop + two staged overlays + a
refused migration on every launch.

**To get instant boots on Windows the options are:** a warm POOL (keep N
instances pre-booted to the ready point and hand the session to one — works
under WHPX because nothing is serialised), or a different hypervisor. The pool
is the recommended next feature; the engine is already shaped for it, since
`bake_entry` already proves an account-free "ready point" exists.

### 3. Farming will not reach ~400 MB on Windows, and cannot

Measured on PS99: guest game PSS 508 MB, **host RSS 2198 MB per instance**. The
balloon works (guest really does hand the pages back — `capped at 897 MB`), but
**QEMU on Windows has no `madvise`**, so the host never gets them. The lever
that works there is `-m` itself, not the balloon. On Linux the balloon,
free-page-reporting and KSM all decommit for real. See §9c and `MODES.md`.

### 4. Farming does not finish loading PS99 — bisected to memory (open)

A farming instance boots, joins and stays alive (~48% of one core, answering
adb) and then sits on the Roblox splash forever. Bisected by running it:

| suspect | test | result |
|---|---|---|
| the 5 fps tick cap | `--quality balanced` | **not it** |
| the 480x270 display | `--guest-display native` (new flag) | **not it** |
| the package trim | read the list | not it (no WebView; game is in KEEP_ALWAYS) |
| doze | read the sequence | not it (game whitelisted before force-idle) |
| **memory** | `--mem 4096 --balloon 3072` | not it — **and it gave the answer**: Roblox aborted inside `libndk_translation` (`Cannot process signal 11`) with 2.1 GB free |

**The blocker is the arm64 TRANSLATOR, not memory.** Farming is the only mode
that swaps hard (`swappiness 100`, zram), and evicting translated code pages
makes `libndk_translation` take a SIGSEGV its host-signal handler cannot
process. `swappiness_x86: 10` took aborts 1 → 0 and got the client past the
black splash to Roblox's loading screen.

Turning zram off as well was measured to be WORSE — same aborts (0), more free
RAM, and Roblox OOM-killed three times over. zram is what lets a 2 GB guest
hold this game; keep it, just stop swapping eagerly. See
`omnidroid/MODES.md` and `omnidroid/farming.py`.

**Still open:** the client reaches the loading screen and not yet the world.

**If confirmed, the farming cap is a per-GAME property rather than a tuning
constant.** The mode then needs either a much larger cap for big places or a
documented list of what actually fits in 896 MB, and the "30+ instances" target
has to be stated per game.

Three adb timeouts came out of `start` as tracebacks along the way, all on
farming, all now handled as a class (`adb.adb_soft`, a 45 s + retry kiosk
probe, and `deliver_session` finally enforcing the "never raises" promise its
docstring always made).

### Fixed on the way, all measured

| | |
|---|---|
| Five modes | ✅ two — `gaming` + `farming`; `playable`/`hard`/`brutal` alias to `gaming` |
| GPU/VNC policy was implicit and wrong | ✅ `--gpu auto\|headless\|window\|off`, in the CLI and the app |
| `-vnc` dropped on EVERY GL boot | ✅ dropped only for a WINDOWED GL boot (`blocks_vnc`) — egl-headless keeps its viewer |
| Farming `smp 1` timed out the session handover on x86 | ✅ `smp_x86: 2` |
| That timeout was a raw traceback out of `cmd_start` | ✅ a timeout is a result now; budget 45 s → 120 s |
| "balloon driver missing?" on a working balloon | ✅ 30 s → 90 s, and a moving balloon says so |
| `omnidroid measure` printed `-` for host RSS on Windows | ✅ `ps` → GetProcessMemoryInfo |
| Warm cache silently off (7.4 GiB free vs a hardcoded 10 GiB reserve) | ✅ configurable + says why it skipped |
| QEMU cannot migrate to a file on Windows | ✅ `omnidroid/migfile.py` relays over a loopback socket |
| arm firmware never found on Windows (`edk2-aarch64-code.fd`) | ✅ looks beside the resolved QEMU |
| `--panel` / guest resolution not settable | ✅ `--panel 1080p`, density scales with it |

### Still true from the previous session

The Mac items (Xcode CLT, `ro.hardware.egl=mesa`, publishing `app-mac`) and the
omnidroid lineage reconciliation are untouched and still open — see the
2026-08-15 (morning) list in git history.

---

## TL;DR — what changed this session

| | State |
|---|---|
| x86 base boots to a lock screen ("swipe up") | ✅ **fixed** — boots straight into the game |
| `no_kiosk_reply` / no auto-login | ✅ **fixed** — cookie installed, place joined |
| Guest rendered in software on a 4060 | ✅ **fixed 2026-08-15** — `playable` renders on the GPU now; see §8a. (This row read "off by default, on in `--mode gaming`" — which was the bug: nothing in the product ever passed `--mode gaming`.) |
| Root-only tunes silently skipped on x86 | ✅ **fixed** — Roblox CPU 209% → 103% |
| Kiosk restarting the game on a ~45 s loop | ✅ **fixed** — it was the Play Store |
| "No live session" / scripts never ran in-game | ✅ **fixed** — verified `return 6*7` → `42` |
| Anyone could run scripts in anyone's session | ✅ **fixed** — owner-only, tested across two machines |
| Cookies stuck on one machine | ✅ **fixed** — encrypted in Mongo, follow the user |
| No login / no licensing in the app | ✅ **built** — register with a key, sign in, plan enforced |
| "Running on Mac mini" | ✅ **live** — verified both directions |
| Backend deployed | ✅ live on the VPS |
| Rebuilt images uploaded | ✅ live and server-verified |
| Auto-update on launch (images + app) | ✅ **built**, both halves verified end to end |
| Install button did nothing | ✅ **fixed** in 1.0.8 |
| Fresh PC never installed QEMU/adb — setup deadlocked | ✅ **fixed** in 1.0.11 — see §0 |
| App crashed on launch on a fresh PC (Mark-of-the-Web) | ✅ **fixed** in 1.0.12 — see §0b |
| Shipped as a .zip, which caused that | ✅ **installer built and published** — see §0c |
| "Add account" crashed the app (unable to set cookie) | ✅ **fixed** in 1.0.13 — see §0d |
| **"3 fps, unplayable"** — every session rendered on the CPU | ✅ **fixed** in 1.0.14 — 3.2 → 13.6-16.5 fps, see §8 |
| **Game loaded forever, then "Error 277"** | ✅ **fixed** in 1.0.14 — the asset CDN is DNS-blocked, see §8b |
| `cpuset: SKIPPED` on every launch | ✅ **fixed** — it was `not su` against a valid `""` |
| Update needed two clicks and only checked at startup | ✅ **fixed** in 1.0.14 — see §8c |
| Mac had no one-command setup | ✅ **built** — `scripts/setup-macos.sh`, see §8d |

---

## 0. First install on a fresh PC — it never worked

Reported as "it doesn't install QEMU automatically". It was worse than that:
**a machine that was not a dev box could not get past the setup screen at
all**, and no image was ever downloaded.

Three independent faults, each sufficient on its own:

1. **A deadlock.** `BootstrapView` only called `bootstrap_start()` when
   `qemu_ok` was already true (`maybeStart`: `if (s && !s.ready && s.qemu_ok…)`).
   Nothing in the app ever installed QEMU — the engine's `ensure_qemu()` fires
   only when an INSTANCE is started, which is unreachable from that screen. So
   `qemu_ok` could never become true, and the screen sat there forever showing
   a hint that *promised* an automatic install. The Re-check button could only
   ever return the same answer.
2. **The app and the engine disagreed about where QEMU is.**
   `engine_ready()` used `shutil.which` — PATH only — while omnidroid's
   `qemu_bin()` on Windows **deliberately never consults PATH**. Wrong in both
   directions: a PATH-only QEMU reported ready to an engine that could not run
   it, and a QEMU installed where the engine *does* look reported
   not-installed forever. This is why the bug was invisible here — this box has
   QEMU in `C:\Program Files\qemu` and adb in the Android SDK, both on PATH.
3. **Even if it had fired, it could not have worked.** The weilnetz installer
   is manifested `requireAdministrator`, so an unelevated `CreateProcess` does
   not run it and fail — it **never starts**, raising `WinError 740`. Verified
   by hand. `ensure_qemu()` used `shell=True`, which only moved that failure
   into cmd.exe and made it look like a broken download.

**adb was never considered at all.** `omnidroid/adb.py` shells the BARE NAME
`"adb"`, so a machine without Android platform-tools fails every guest command
at CreateProcess.

### What it does now

`bootstrap.ensure_tools()` runs as **phase 0 of first boot**, before the 4 GB
of images (which are useless without a QEMU to boot them):

| step | admin? | notes |
|---|---|---|
| adb | **no** | plain zip from `adb-win` (302 → Google platform-tools), unpacked and flattened |
| QEMU — portable | **no** | `qemu-portable-win`, 77 MB, sha256-verified. **This is the live path** |
| QEMU — vendor installer | **yes** | fallback only: NSIS `/S /D=…` through `ShellExecuteEx "runas"`. Used when the tools channel is unreachable |
| WHPX | **yes** | `DISM /Enable-Feature /FeatureName:HypervisorPlatform`, then a restart |

**In practice a fresh machine now sees NO prompt at all** unless WHPX is off.
Measured end to end against the live server, unelevated, with `run_elevated`
hard-asserted never to be called: **both tools installed in 15 s.**

**One UAC prompt, not two.** WHPX can only be probed *with* a working QEMU, so
on a machine that has none we cannot know whether the feature needs enabling —
and DISM's enable is idempotent (exit 0 = already on, 3010 = done, restart).
So when QEMU needs installing, both run in a single elevated `cmd /c`. The
second prompt only exists for the case the batch cannot cover: QEMU was
already present, so nothing needed elevation, and WHPX turns out to be off
only once there is a QEMU to ask.

**Where the tools live: `%LOCALAPPDATA%\OmniExec\{qemu,platform-tools}`.**
Not `<exe dir>/qemu`, which is what omnidroid's `QEMU_DIR` resolves to — the
app's own updater renames the entire app directory aside and copies the new
build in (`updates.py apply_staged_app`), so a 200 MB QEMU installed there is
**destroyed by every app update**. It is also under Program Files for anyone
who installs there, i.e. unwritable. The runtime dir is user-writable with no
elevation, survives updates, and sits beside the images.

The engine finds them because `configure_engine()` writes the resolved QEMU
directory into `paths.json` as `qemu.dir` (which `qemu_bin()` already consults
first) and prepends the adb directory to `PATH`, which every engine subprocess
inherits. `omnidroid/config.py` also now honours **`OMNI_QEMU_DIR`** ahead of
the config, so a subprocess still resolves QEMU if `paths.json` is stale.

### The portable QEMU (`qemu-portable-win`, channel `tools`)

Built by `omni-backend/scripts/build-qemu-portable.py` from an installed
Windows QEMU. 1.19 GB → **204 MB staged, 77 MB zipped**, reproducible (same
sha256 across rebuilds). Pinned at **11.0.50** — the exact build every
measurement in this document was taken against — not the 11.1.0 the vendor
installer currently ships.

Pruning is a **deny-list on purpose**. An allow-list of "the firmware an x86
guest needs" boots fine on the machine that wrote it and fails on a customer's
six weeks later, because QEMU loads option ROMs lazily and *by name*
(`efi-virtio.rom` per NIC, `vgabios-*.bin` per display model, `kvmvapic.bin`,
`linuxboot_dma.bin`, …). So `share/` keeps everything except what cannot
possibly apply: docs, desktop icons, and the other-architecture UEFI images —
which is where the size is anyway (the arm/aarch64/riscv/loongarch edk2 blobs
alone are 288 MB of the 346 MB tree). Of the ~58 system emulators, only the
three the engine actually invokes ship (`grep qemu_bin(` in `engine.py`).

**It is in its own `tools` channel, and that is load-bearing.** It is a
sha256'd artifact with a `dest`, so to any client older than the `kind: "tool"`
rule it is indistinguishable from a base image: it would enter the first-boot
download plan, be fetched a second time, and — far worse — make every
already-installed machine report un-ready forever, because readiness is "the
plan is empty". Shipping it in `stable` would have broken every 1.0.8/1.0.9
client the moment it went live. Two independent guards now: the channel, and
`plan_downloads` skipping `kind in ("app", "tool")`.

### Verified

```
adb        downloaded, unpacked, flattened, `adb version` -> 1.0.41,
           on PATH via configure_engine, NO elevation          (clean runtime dir)
qemu       vendor installer: one UAC prompt -> exit 0 in 31 s -> 1.17 GB,
           all three binaries 11.1.0, find_qemu picks it up, WHPX probe -> True
declined   ERROR_CANCELLED(1223) -> a clear message, not a traceback
portable   BOOTED A REAL INSTANCE: ok/delivered/played true, 64.9 s,
           Roblox rendering the place at 1280x800 (screenshotted)
e2e        fresh runtime + PATH stripped, UNELEVATED, live server:
           both tools installed in 15 s, ZERO elevation prompts,
           paths.json -> the new qemu, adb on PATH, image plan correct
frozen     omni-exec.exe --doctor reports the new fields from the shipped exe
```

**One bug shipped in 1.0.10 and was caught by that e2e run, not by a unit
test:** `ensure_tools()` looked up the tools channel, found the portable build,
then handed `install_qemu_windows()` the *original* manifest (`None`). It
recomputed a plan that could not see the portable build and fell back to
downloading the 197 MB installer and prompting for administrator — on a machine
that needed neither. Fixed in **1.0.11**, with a regression test. The lesson is
the usual one: the mock knew what it was told.

`omni-exec.exe --doctor` is new: it prints where this install found (or failed
to find) QEMU and adb plus the hypervisor verdict, as JSON, from the shipped
binary. The app is a GUI-subsystem binary, so short of launching it there was
previously no way to tell which bootstrap code a given exe carried.

**Caveat on testing:** Windows re-injects `ProgramFiles` into every child
process, so `find_qemu`'s system-QEMU fallback **cannot be simulated
cross-process** — overriding it in `subprocess(env=…)` is silently ignored.
Test that branch in-process (as `tests/test_tools_install.py` does) or you will
conclude the override works when it does not.

**`app-win 1.0.14` is live** (superseded blobs deleted from the
server). **Users already stuck on 1.0.8 must re-download** — the update banner
lives in the app shell, which is behind the `ready` gate, so a machine stuck on
the setup screen never sees it. Fresh downloads are fine.

---

## 0b. The app would not start at all on a fresh PC — Mark-of-the-Web

A second, separate failure, reported from a fresh machine after §0 shipped:

```
RuntimeError: Failed to resolve Python.Runtime.Loader.Initialize from
C:\Users\...\Downloads\omni-exec\omni-exec\_internal\pythonnet\runtime\Python.Runtime.dll
```

**Windows marks every file extracted from a downloaded .zip** with a
`Zone.Identifier` alternate data stream recording the Internet zone. The .NET
Framework assembly loader then refuses to load `Python.Runtime.dll`,
`clr_loader` cannot resolve its entry point, and pywebview's WinForms backend
dies on import — before one line of this program's own code runs. Every fix in
§0 was therefore unreachable on the machines that needed them most.

Reproduced exactly: putting that one stream on that one DLL in a working venv
produced the identical error, and removing it cured it. It is structurally
invisible in development, because a build made locally was never downloaded and
so is never marked.

Two fixes, both shipped:

* **`main._unblock_app_files()`** (1.0.12) strips the mark from the install
  tree at startup, before anything touches the CLR. One stat on a normal
  launch; a full sweep only when that probe comes back marked. It clears the
  WHOLE tree, because `Python.Runtime.dll` pulls in ~100 netstandard facades
  beside it and each is refused on the same grounds — clearing one file only
  moves the error along.
* **An installer** (below), which is the real answer: files an installer writes
  are never marked, so the failure cannot arise at all.

Verified by marking a real build exactly as Explorer marks an extraction: the
old build would not start, 1.0.12 came up with the window title "Omni Executor"
and the marks cleared.

## 0c. The installer — `OmniExecutorSetup.exe`

**Download link (permanent):** `http://72.62.59.232/omni/dist/blob/setup-win`

Built by `.\build-windows.ps1 -Installer` from `installer.py` +
`OmniExecutorSetup.spec`. **12.4 MB, one file.**

It is a **stub**: it fetches the current `app-win` build from the same dist API
the app's own updater uses, and sha256-verifies it with the same
`bootstrap.download_blob`. So it does **not** need rebuilding for each app
release — publish `app-win` and the installer already out there picks it up.

| | |
|---|---|
| installs to | `%LOCALAPPDATA%\Programs\OmniExecutor` — **per-user, no administrator** |
| creates | Start Menu + Desktop shortcuts, and an Apps & features entry (HKCU) |
| keeps a copy of | itself, as the uninstaller, so it survives the user clearing Downloads |
| uninstall leaves | `%LOCALAPPDATA%\OmniExec` alone — several GB of images and the user's accounts; deleting that because they removed the launcher would be indefensible |
| flags | `--silent`, `--no-launch`, `--uninstall` |

Verified end to end: install → 95 MB / 1176 files, **no `Zone.Identifier` on any
installed file**, both shortcuts resolve, the Apps & features entry is correct,
and the installed app launches ("Omni Executor"). Uninstall → directory, both
shortcuts and the registry key all gone, runtime data kept — including the
self-delete of the uninstaller that is running from the directory it removes.

Publishing it: it is `setup-win` in the **`tools` channel** (not `stable`), for
the same reason as `qemu-portable-win` — it is a hashed artifact with a `dest`,
so an older client seeing it in `stable` would treat it as a base image.

```bash
cd omni-executor && .\build-windows.ps1 -SkipFrontend -Installer -DistPath dist-setup
cp dist-setup/OmniExecutorSetup.exe ../omni-backend/dist/blobs/
cd ../omni-backend && python scripts/push-images.py setup-win   # re-hash + upload
python scripts/deploy.py
```

### SmartScreen — the remaining friction, and it costs money to fix

The installer and the app are **unsigned**, so a downloaded
`OmniExecutorSetup.exe` carries the mark and **SmartScreen intercepts it**:
"Windows protected your PC". Confirmed here — a scripted `Start-Process` on the
downloaded file simply hangs on that dialog, and the earlier zip build could not
be launched at all for the same reason.

The user must click **More info → Run anyway** once (or Properties → Unblock).
Verified: after `Unblock-File` the installer runs to exit code 0 and installs
perfectly. **Say so on the download page** — it is normal for unsigned software
but it looks alarming.

The only real fix is Authenticode signing: an OV certificate (~$200-400/yr; EV
buys instant reputation and costs more), then a `signtool sign /fd sha256 /tr
<timestamp-url>` step over `omni-exec.exe` and `OmniExecutorSetup.exe` in
`build-windows.ps1`. Not done — it needs a purchased certificate, which is a
decision rather than a task.


## 0d. "Add account" took the whole app down

Reported with a raw PyInstaller crash dialog and a wall of chromedriver stack
addresses:

```
WebDriverException: Message: unable to set cookie
  (Session info: chrome=151.0.7922.138)
  ... accounts.py line 563, in capture_login_from_cookie
  ... selenium/webdriver/remote/webdriver.py line 799, in add_cookie
```

**Two separate faults.**

**It should never have been a crash.** The contract in `accounts.py` is a
RESULT DICT — `cmd_login` and `_capture_and_save_account` both read `r["ok"]`
and turn a falsy one into a clean `fail(error, message)`. Every failure path
honoured that except the browser calls, so one `WebDriverException` ended the
process. In the frozen GUI build that is the "Unhandled exception in script"
dialog. Both capture functions now return a dict for every failure, with a
`WebDriverException` backstop around each.

**And the cookie call needed hardening.** `add_cookie` is a WebDriver call
bound to the CURRENT DOCUMENT's origin, so it reports that bare message
whenever the browser is not genuinely on roblox.com — a DNS failure, a captive
portal, a proxy, anything that leaves Chrome on `chrome-error://chromewebdata/`.
**That is why it does not reproduce on a healthy machine**: probed here against
Chrome 151 with five cookie variants on two pages, and every single one
succeeded. So the fix is not a different cookie spec:

* check the origin first and say plainly that **the browser never reached
  roblox.com** — the thing the user can actually act on;
* then fall back to **CDP `Network.setCookie`**, which writes straight to the
  network stack and does not care which document is loaded.

Messages are de-duplicated too (`_wd_reason`): chromedriver's text already
begins `"Message:"` and `WebDriverException.__str__` prepends another, so the
naive interpolation read `"Message: Message: unable to set cookie"` with 20
lines of addresses behind it.

`tests/test_login_failures.py` covers all of it. **Fixed in 1.0.13.**


## 1. The x86 base: lock screen and auto-login

Both were IMAGE problems, not code. The shipped base carried a kiosk built
before the `SET_SESSION` contract (no `SessionReceiver`, signed with a key
rotated after that base was cut, so `pm install -r` is refused), and its
`/data` had the lock screen enabled.

**Fixed by `omnidroid/tools/rebuild_x86_base.py`** — one builder boot that
swaps the `/system` kiosk, provisions `/data` (lock screen off, kiosk as
device owner + HOME, game package pinned), and `qemu-img commit`s both deltas
back over the shipped images.

Why *commit* and not *convert*: the arceus offset is itself a thin overlay of
`data-template-8g.qcow2`. Flattening it would turn a 750 MB delta into a
multi-gigabyte download for no gain — and copying it elsewhere breaks its
**relative** backing reference (`Could not open backing file`, which is exactly
how the first attempt failed).

Verified on the rebuilt image:

```
omnidroid start admn1b12farm3 --place 606849621
  ok: true, delivered: true, played: true          (was: no_kiosk_reply)
  OmniKiosk:     device owner: keyguard disabled = true
  OmniKiosk:     launching com.roblox.client (boot)
  OmniKiosk:     joined place 606849621 (host session)
  OmniBootstrap: session cookie installed (1202 chars)
  boot 1.1 min
```

The kiosk now also disables the keyguard itself (`MainActivity.dismissKeyguard`,
three layers) so an image whose `/data` predates this still boots into the game.

**Re-running it:** `python omnidroid/tools/rebuild_x86_base.py` with the
product's env. It keeps `.bak` copies of both images — a bad bake is one `mv`
from undone. It refuses to bake if the lock screen is still on.

## 2. Performance: what was actually wrong

Two separate things, and the bigger one was not the GPU.

### 2a. The root-only tunes were skipping on a guest that IS root

`resolve_su()` looks for an **`su` binary**. The x86 Bliss base has none — but
its **adbd runs as uid 0** and can write anywhere. So every root-gated step
reported "no root on this instance" and skipped, on a guest where all of them
work. Checked by hand before changing anything:

```
$ adb shell id -u                                      -> 0
$ adb shell 'echo x > /data/data/com.roblox.client/…'  -> WRITE_OK
```

The biggest of those is the Roblox `ClientAppSettings` tune, which this repo
already measured at roughly **half the host CPU per instance** — skipped on
every x86 launch since the base existed. Measured on one live instance, same
place, same image:

```
com.roblox.client   209% CPU   before
com.roblox.client   103% CPU   after
```

`resolve_root_shell()` now returns `""` (adbd is already root), an su path, or
None; `root_shell()` runs a script by whichever route exists. That also unlocked
swappiness and the gaming tune-up's native-resolution step.

Three traps on the way, all now in code comments: `shlex.quote` is needed in
**both** root paths (adb re-joins argv and the guest shell re-parses it — the
first heredoc silently vanished and still exited 0); `""` is a valid root mode
so every gate had to become `is None` rather than falsy; and a **negative**
probe must not be cached, because "no root" is usually "asked before adb was
up".

### 2b. GPU rendering is exclusive with VNC

QEMU says it outright:

```
qemu: -vnc 127.0.0.1:12101: Display vnc is incompatible with the GL context
```

omnidroid appended `-vnc` unconditionally, and that one line explains both GPU
failures: `--mode gaming` **exited on startup** instead of booting, and
`egl-headless` did not error but published a framebuffer VNC could never be fed
from — the black viewer. `vnc_args()` now drops `-vnc` on a GL boot and keeps it
everywhere else.

With that, `--mode gaming` gives a real GPU-accelerated window:

```
before   GLES: Mesa, llvmpipe
gaming   GLES: Mesa, virgl (NVIDIA GeForce RTX 4060/PCIe/SSE2), OpenGL ES 3.2
```

Note **ES 3.2 and native GL** — better than the headless path, which went
through ANGLE and exposed only ES 2.0.

**Headless GL stays OFF by default** (`qemu.headless_gl` / `OMNI_HEADLESS_GL=1`
to enable). It costs the viewer, and the viewer is the product's only window
into a headless instance. Worth enabling for farming nobody watches.

A GL boot gives up `omnidroid view` and capture.py/autocap, which read that
framebuffer. `omnidroid screenshot` is unaffected — it goes through adb.

Boot time is unchanged either way — four alternating boots, `timings.stages.boot`:
47.8 s / 52.3 s (GL on) vs 36.2 s / 58.8 s (off). Overlapping noise.

**Diagnosing "black viewer":** blackness alone proves nothing — the kiosk draws
solid black by design and Roblox renders black while loading a place. The
reliable signals are the RFB **update count** and an `adb exec-out screencap`
taken at the same instant. `omnidroid screenshot` will NOT tell you: it reads
adb, not VNC.

### The ceiling

> **Superseded by §8 (2026-08-15), which measured all of this.** The two
> paragraphs below were right about the ceiling and wrong about where the
> product sat under it: every session was rendering in software, not just the
> ones nobody had opted into GPU for. Kept as written, because "the GPU works,
> it is just off by default" was the belief that let 3 fps go unexplained for
> a week.

**Near-native is not reachable on x86, and it is not a tuning problem.** Roblox
ships arm64-only, so every instruction runs through `libndk_translation`. No
flag removes that. The machine that can do near-native is the **Mac mini** — M1
runs the arm64 build natively under HVF, no translation at all.

**On the Mac the GPU path does nothing yet.** Homebrew's QEMU has no
`virtio-gpu-gl-pci` and no `egl-headless` (there is no `virglrenderer` formula),
so the capability probe degrades to software. It needs a QEMU built with
`--enable-opengl --enable-virglrenderer`; no code change.

## 3. Accounts, licensing, and the privacy wall

**Register** takes email + password + a license key, consumed in the same
transaction that creates the user. **Sign-in** takes email + password. Plans
are `30_day` / `90_day` / `lifetime` (the older `1_month`/`3_month` keys still
redeem). Mint more with `node scripts/seed-keys.js [plan] [count]` — sign-up
needs a key, an admin needs an account, and an account needs a key, so the
first keys cannot come from the admin endpoint.

Keys currently unused — **queried from the live database 2026-08-14**, 12 of
15. An earlier revision of this table was already wrong: it listed
`OMNI-R8HJ-MLH7-UPPX` as available when it had been redeemed, and omitted
three lifetime keys.

| Code | Plan | Note |
|---|---|---|
| `OMNI-5EQH-UJKE-MGH9` | 30 days | seeded |
| `OMNI-ACUT-25BQ-7FRB` | 30 days | seeded |
| `OMNI-9J4S-C7HL-46GG` | 90 days | seeded |
| `OMNI-XGEF-YGU7-5C65` | 90 days | seeded |
| `OMNI-97US-7AJW-H2X7` | Lifetime | for berat |
| `OMNI-CG62-K8M3-CKGV` | Lifetime | testing account creation |
| `OMNI-DAAJ-FQ3M-NVR5` | Lifetime | testing account creation |
| `OMNI-SWPA-G6H6-EXC7` | Lifetime | testing account creation |
| `OMNI-PN44-YS25-2XWK` | Lifetime | testing account creation |
| `OMNI-LVMV-B7ZR-3WYF` | Lifetime | test |
| `OMNI-4JUU-FSS6-QRDZ` | Lifetime | test |
| `OMNI-YZC2-L84H-ZSRL` | Lifetime | test |

Spent: `OMNI-WDG4-2X56-L37G`, `OMNI-RNTH-RDES-7WYF`, `OMNI-R8HJ-MLH7-UPPX`.

**Do not trust this table** — it is stale the moment anyone signs up. Ask the
database, which is where the answer actually lives. Note `NODE_ENV=production`:
without it the scripts load `.env.development.local`, find no `DB_URI`, and die
claiming the variable is missing.

```bash
cd /root/omni-backend && NODE_ENV=production node -e "
import('./backend/src/database/mongodb.js').then(async m => {
  await m.default();
  const K = (await import('./backend/src/models/licenseKey.model.js')).default;
  for (const k of await K.find({ status: 'unused' }).lean())
    console.log(k.code, k.plan, k.note || '');
  process.exit(0); })"
```

To see what is still unused rather than trusting this table, query
`licensekeys` for `status: "unused"` — a redeemed key stays in the collection.

The two test accounts are `omni-primary@omni.test` and `omni-second@omni.test`,
both `hunter22`, both lifetime — delete them when you are done with them.

**Cookies live against the user, encrypted** (AES-256-GCM). Signing in on
another machine brings the same Roblox accounts with you — verified: the Mac
pulled `admn1b12farm2` and `admn1b12farm3`, byte-identical, having never held
them.

> **`ACCOUNT_ENC_KEY` must be identical everywhere that shares the database.**
> It is now set in `.env.development.local`, `.env.production.local`, and on the
> VPS. It is deliberately independent of `JWT_SECRET`: they rotate for different
> reasons, and a JWT rotation must not make every stored cookie undecryptable.
> (It did, once, this session — dev and prod had different JWT secrets.)

**The wall.** `submit` / `status` / read-result require a JWT, an active plan,
and that the channel be an account *that user owns*. The in-game poller has no
credential, so it exchanges its channel for a session token at `/omni/exec/claim`,
granted only while the owner's device holds a live running lease.

## 4. Auto-update

Every launch asks what is out of date — base images, Roblox offsets, and the
app itself — on a background thread, so a slow or unreachable server costs
nothing visible. A banner appears above the content when something is stale;
the full control is in Settings → Updates.

|  | compared by | why |
|---|---|---|
| base image / offsets | **sha256** | the offset was rebuilt IN PLACE — its version string is the Roblox build, not the bake, so only the hash changes |
| the app | **version** | a one-dir PyInstaller build is a whole tree, and from source there is no build to hash |

Applying is one click, never automatic, and refused while an instance is
running: a live QEMU holds the base image open, and restarting the app drops
the presence lease your other machines read.

**The app replaces itself** by staging the new build and launching the STAGED
copy with `--apply-update <dir> <pid>`. That copy waits for the old process to
exit, renames the old directory aside, copies itself in, restores the backup if
anything fails, and relaunches. A running executable cannot overwrite itself,
which is why the swap happens from the other side.

Verified end to end here: a 1.0.0 build found 1.0.1, downloaded, verified,
swapped, relaunched, hashed identical to the staged tree, backup cleaned up.
The runtime half too — 4 GB of rebuilt images downloaded, verified and placed
in 336 s, after which the check reports clean.

**Offline is reported as offline**, never as "up to date": a machine that could
not ask is not a machine that is current.

**Machines with their own images are left alone.** Both dev boxes point the
engine at a hand-made images dir (`OMNI_IMAGES_DIR`), and downloading into the
runtime dir would not change a byte the engine boots. Those installs say so
instead of offering an update that would do nothing.

### Publishing a release

Two things can be published and they have DIFFERENT cadences. Publishing
the app is routine; publishing the installer is rare, because the installer
is a stub that resolves whatever `app-win` currently is at run time.

**The app** — every release:

```bash
# 1. bump APP_VERSION in omni-executor/updates.py, then build somewhere fresh
cd omni-executor && .\build-windows.ps1 -SkipFrontend -DistPath dist-<version>
# 2. zip it, record size + sha256, bump app.version in the registry
cd ../omni-backend && node scripts/push-app.mjs win <version> ../omni-executor/dist-<version>/omni-exec
# 3. upload it and restart the server
python scripts/push-images.py app-win
# 4. delete the superseded blob it names as unreferenced
python scripts/vps.py run "rm -f /root/omni-backend/dist/blobs/omni-exec-win-<old>.zip"
```

**The installer** — only when `installer.py` itself changes, NOT per release:

```bash
cd omni-executor && .\build-windows.ps1 -SkipFrontend -Installer -DistPath dist-setup
cp dist-setup/OmniExecutorSetup.exe ../omni-backend/dist/blobs/
cd ../omni-backend && python scripts/push-images.py setup-win   # re-hashes and uploads
python scripts/deploy.py                                        # serve the new registry
```

`-DistPath dist-<version>`, not `dist`: PyInstaller deletes and recreates its
output dir, so anything holding it open (an Explorer window, a shell whose cwd
is inside, a running copy) fails the build — and a build disturbed that way
still LOOKS launchable while missing the frontend. That is how 1.0.6 shipped
broken; `push-app.mjs` now refuses a build under 60 MB or missing the exe or
`_internal/frontend/dist/index.html`.


The version must go up — the updater refuses a build that is not newer than the
one running. **`app-win 1.0.14` is live**, built from `dist-1014/`. **`app-mac`
has never been published**, so Macs see "no update" rather than a broken offer;
build one with `OmniExecutor.spec` and `push-app.mjs mac <version> <path-to-.app>`.

Two traps, both hit for real:

* **Publishing an app build made every already-installed client re-run first
  boot.** Readiness compared every manifest entry against `installed.json`, and
  an app build is never recorded there. Readiness now asks `plan_downloads()`,
  the one place that knows what is installable. If you add another artifact
  `kind`, teach `plan_downloads` about it and nothing else.
* **1.0.6 shipped broken** — 31 MB with zero frontend files, because a running
  `omni-exec.exe` disturbed PyInstaller's COLLECT step and the result still
  looks launchable. `push-app.mjs` now refuses a build missing the exe or
  `_internal/frontend/dist/index.html`, or under 60 MB. **Close every
  `omni-exec.exe` before building**, or build with `-DistPath dist-new`.

### The Install button

It must actually install. Two bugs made "1.0.7 is available" a dead end:

* the banner button called `onOpenSettings` in **every** state, so a button
  reading "Install" only switched tabs — and did nothing visible when Settings
  was already open;
* the restart **refused while any instance was running**. That guard was wrong:
  QEMU is spawned DETACHED, so VMs outlive the app. Verified — the swap ran with
  an instance up, its pid unchanged afterwards, and the installed tree came out
  hashing identical to the new build. Since something is usually running, the
  button was refusing nearly always.

The **runtime** update still requires everything stopped; those image files are
open by a live QEMU. That distinction is real.

## 5. The game restarting on a loop — it was the Play Store

Reported as "the kiosk restarts the game from 0, logs in, closes and reopens
continuously". Not the kiosk.

The **Play Store self-updates inside the guest**, and every update REPLACES
packages the game depends on. Android kills a process whose dependency is
replaced, so Roblox was killed and relaunched every ~45 s. Traced live:

```
I ActivityManager: Killing 5717:com.google.android.gms … due to installPackageLI
I ActivityManager: Process com.android.vending (pid 6276) has died
W ActivityManager: Rescheduling restart … for mem-pressure-event
```

Disabling `com.android.vending`: **zero kills over the next 100 s**, same Roblox
pid throughout, on the instance that had been cycling. Confirmed again from a
clean boot — pid unchanged across a 90 s watch.

**GMS itself is kept.** Play Integrity lives in GMS, not in the store, and it is
the store that does the updating — so this is the narrow cut, not "drop Google".

It runs on **every boot** (`quiet_the_store`), not from `TRIM_PACKAGES`. That
list only executes inside `provision_settings`, which is dead on the product
path: instances are ephemeral and boot from a pre-provisioned /data with
`first_boot_done` already set, so the store is enabled again every launch. Same
reasoning as `assert_kiosk_game` beside it.

It catches `SystemExit` as well as `Exception`: adb's `_require_adb_port` calls
`fail()`, which `sys.exit`s, and a best-effort tune-up must never end the
process — adding it to the boot tail took down three unrelated test suites until
it did.

## 6. In-game execution — working

Two bugs, both mine, either enough to make it impossible.

**Claim needed a POST.** In-game, the only HTTP call guaranteed to exist is
`game:HttpGet`. A POST needs an executor-provided
`syn.request`/`http.request`/`request`, and this executor exposes none — so
`claim()` returned false forever and the script sat in its claim loop, **never
reaching the polling code below it**. Symptom: jobs queued, `lastPollMsAgo:
null`, and the editor saying "No live session" over a plainly loaded game. The
old poller only ever needed `HttpGet`; requiring more was the regression. Claim
now answers GET too, and `/report` is a GET twin of the result post so output
comes back without a POST.

**Claim also demanded a fresh running lease**, which only the desktop app's
heartbeat renews — so a CLI launch, or the app closed, or before the first sync,
could not be claimed at all. It now requires only that the account exist.

The wall is unchanged and is where it belongs: **submit** needs a JWT, an active
plan, and ownership of the channel. A poll token alone drains a queue only the
owner can fill.

Verified end to end from a clean boot, poller connecting by itself:

```
return 6*7                     -> 42
return tostring(game.PlaceId)  -> 606849621
return 1+1                     -> 2
```

## 7. Presence

`running.heartbeatAt` is renewed every 25 s and read as stale after 90 — a
lease, not a flag, because a machine that crashes never sends "stopped". The
list shows **Running** on the machine holding it and **Running on ⟨device⟩**
everywhere else. Verified live: the Mac showed `admn1b12farm2 -> Running on
Berat` while Windows ran it.

## 8. "3 fps and unplayable" — it was two faults, and the frame rate was the smaller one

Reported as the emulator being too slow. Measured 2026-08-15, and the report
was exact: **3.2 fps**, reproduced on the first try.

### 8a. Every session rendered on the CPU

`playable` is DEFAULT_MODE and what the app launches, and it booted **headless**.
That is the whole bug: **without a host window QEMU has no GL context**, so
virglrenderer cannot run, and Mesa falls back to llvmpipe. A 3D game was being
drawn pixel by pixel on the CPU that was already paying arm64 translation,
while an RTX 4060 sat idle. Only `--mode gaming` ever asked for a window, and
nothing in the product ever passed it.

Measured at 1280x800, one account, one place, frame counts off
`dumpsys SurfaceFlinger --timestats`, with the render confirmed by screenshot
at the moment of measurement:

```
before   GLES: Mesa, llvmpipe                     95 frames / 30.1 s ->  3.2 fps
after    GLES: Mesa, virgl (RTX 4060/PCIe/SSE2)  407-496 / 30.0 s   -> 13.6-16.5 fps
```

`playable` now asks for the window. `--no-window` / `OMNI_NO_WINDOW=1`
suppresses it — and now suppresses the *native* window as well as the VNC
viewer, so one flag means one thing. The window is also what removes input
latency: host events go straight into `usb-tablet`/`usb-kbd` instead of a VNC
round trip through framebuffer encode, decode and synthesised input.

**Measuring frame rate is easy to get wrong.** Three readings of 53, 44 and
59 fps were all bogus — a disconnected client's overlay and a loading screen,
both of which animate happily. Screenshot at the same instant as the sample,
every time, and check the client is actually in-world.

Three more things came out of the same pass:

* **`cpuset: SKIPPED` on every launch, forever.** `build_pin_game_step` tested
  `not su`, but `""` is a VALID root mode (adbd runs as uid 0 on the x86 base).
  The one latency-critical step — putting the game on the `top-app` scheduler
  set — never ran. Now `su is None`. This is gap 5 from the last handoff; it
  was not the stale `run.json`.
* **A GL boot came up 640x480.** `virtio-gpu-gl-pci` offers a mode list that
  starts there and Android takes the first entry, so `wm size reset` cannot fix
  it. The panel actually requested is recorded at spawn and set explicitly.
* **The quality profile now follows what the boot can draw.** `high` is level 10
  with post-FX on — written for a renderer with a GPU. With one it is nearly
  free (16.5 vs 16.4 for `balanced`); without one it is being asked of the CPU,
  so a software boot steps down. An explicit `--quality` still wins.

### The ceiling, re-measured

**Near-native is still not reachable on x86, and it is still not tuning.**

* **`--smp 6` is catastrophic on WHPX**: 5.6 min to boot (vs 0.8) and 0.6 fps,
  with QEMU using **83% of one host core**. `WHPX_SMP_CEIL = 4` is right. At
  smp 4 QEMU uses ~1.4 of 16 host cores — the host is not the limit.
* **Extra CPU features buy nothing.** `-cpu qemu64,+sse4.2,+avx2,+aes,…` boots
  cleanly (unlike `-cpu host`) and measured 14.4 fps against 14.2. Reverted.
* **640x480 gave 19 fps against 14 at 1280x800** — a 4.2x pixel cut for 33%
  more frames. That is CPU-bound, not fill-bound: `libndk_translation` is the
  wall, and no flag removes it.

The machine that can do near-native is still the **Mac mini** — M1 runs the
arm64 build natively under HVF. Verified this session: `arm64 native (no
translation): abilist=arm64-v8a OK`, and it boots in **18.8 s** against 55-83 s
here. Its QEMU still has no virglrenderer, so it renders in software (ANGLE +
SwiftShader) — see §8d.

### 8b. "Loads forever, then Error 277" — the asset CDN is DNS-blocked

A separate fault, and the one that actually made sessions unusable. The client
logged in, joined, never finished loading the world, and minutes later showed
**"Disconnected (Error Code: 277)"**. logcat had hundreds of:

```
HttpError: DnsResolve   Could not resolve host: fts.rbxcdn.com
MeshContentProvider failed to process ... because 'could not fetch'
```

while `google.com`, `roblox.com` and `cloudflare.com` all resolved in the same
guest. So it is not slirp's DNS relay failing, which is what it looks like.

The relay was faithfully forwarding to the host's resolvers, and the failure is
upstream of the host entirely:

| resolver | `fts.rbxcdn.com` | `t0.rbxcdn.com` |
|---|---|---|
| ISP (Türk Telekom) | FAIL | FAIL |
| 8.8.8.8 over UDP:53 | FAIL | FAIL |
| 1.1.1.1 over UDP:53 | FAIL | FAIL |
| Cloudflare over **DoH** | 2.22.89.53 | 2.20.134.200 |

Failing against *every* resolver while succeeding over HTTPS is **transparent
interception on the network path** — the ISP-level Roblox block, applied to the
asset CDN. Changing which resolver the guest is told about cannot fix it.

**Android Private DNS can.** `private_dns_mode=hostname` is DNS-over-TLS on
TCP:853 with the certificate pinned to the resolver's hostname, so an
interceptor can neither read nor forge it. `ensure_private_dns` sets it on
**every boot and every mode** (a farming instance that cannot fetch assets is
one that never gets into the game) and verifies with a `t0.rbxcdn.com` canary.
DnsResolve errors went from hundreds to **zero**, and the world streams.

It must be `hostname`, not `opportunistic`: opportunistic upgrades the resolver
it was already handed and accepts any certificate, so the interceptor still
wins. Override with `network.private_dns` or `OMNI_PRIVATE_DNS`; `off` disables.

**Ordering trap:** the kiosk launches Roblox at boot, so a client that starts
before Private DNS is up caches the failures and shows "Connection error" on
the splash — seen on the Mac. Force-stop the game and re-deliver the session;
do not conclude the DNS fix failed.

### 8c. The app updates itself now

Two behaviours, split by what the user is in the middle of:

| | |
|---|---|
| **at launch** | nobody is doing anything yet, so a new build downloads, swaps and relaunches by itself |
| **while open** | the download still happens on its own; the **restart** is offered — "Version X is ready. Restart to update." with one button — because this process holds the presence lease and the editor buffer |

The check is a loop now (every 30 min), not a one-shot at startup, so a release
published while someone has the app open is no longer invisible to exactly the
people who leave it open. `autoUpdate` in settings turns both halves off.

**The guard that matters:** an automatic swap ends by relaunching, which runs
the same code again. A build that cannot actually replace the running one would
download, swap, come back as the old version and do it again forever, with the
window vanishing every 30 seconds. So an automatic apply is attempted **once
per version per machine**, and the receipt is written **before** the attempt —
a swap that takes the process down does not come back to write it. Still on the
old version next launch? The banner offers it and a human decides.

### 8d. The Mac

`omni-executor/scripts/setup-macos.sh` takes a Mac from nothing to running:
Homebrew, qemu, adb, node, python, the venv, the pip deps and the React build,
then reports what it found. Idempotent and keyed on the COMMAND rather than
`brew list`, so "run it again" is safe advice. `--check` reports only; `--run`
launches the app after. It deliberately does not download base images — the app
already does that with resume and sha256.

Run on the Mac this session: node was missing (so `frontend/dist` was stale and
nobody would have known), deps installed, frontend built, app launched and
confirmed in front on the Aqua session.

**The engine boots there now** — the last handoff said it could not. 18.8 s,
`ok/delivered/played: true`, `arm64 native (no translation)`.

Two Mac-specific findings:

* **The arm base's default offset was `arceusae`, which has no kiosk** —
  `reason: kiosk_missing`, so no session could ever be delivered. `arceusremote`
  has it and is now the default (`omnidroid offset default arceusremote`).
* **GPU rendering on macOS — what it actually takes.** `scripts/setup-macos.sh
  --gpu` automates the host half; the rest of this is why it looks like that.

  All four ways of getting a virgl-capable QEMU were checked on 2026-08-15:

  | | |
  |---|---|
  | Homebrew core `qemu` | no `virtio-gpu-gl-pci`; `-display help` lists only `none/curses/cocoa/dbus` |
  | `knazarov/qemu-virgl` | coexists with core qemu, but pins QEMU to a **2021** revision and its test-image resource 404s, so the formula cannot install. Its `libangle` / `libepoxy-angle` / `virglrenderer` formulae ARE the hard macOS-specific part and are what `--gpu` reuses |
  | `startergo/…-kosmickrisp` | current QEMU and bottled, but the formula is named `qemu`, so Homebrew **replaces** the working one rather than installing beside it |
  | UTM.app (prebuilt cask) | ships `virglrenderer.1.framework` + `EGL.framework`, but its QEMU is a Mach-O **shared library** that UTM `dlopen`s — not an executable, so anything that spawns a QEMU *process* cannot use it |

  So `--gpu` installs the three dependency formulae and builds a matching QEMU
  against them into `~/Library/Application Support/OmniExec/qemu-gl`, pointing
  the engine at it with config `qemu.dir`. **The system qemu is never touched**
  — that is what makes it safe to try and trivial to undo.

  **Two things still block it on this Mac**, and neither is code:

  1. **Xcode Command Line Tools are too old for macOS 26**, so Homebrew refuses
     every source build. Fixing it needs a password and a GUI (Software Update,
     or `sudo xcode-select --install`), so the script reports it precisely
     instead of failing obscurely. Note the check is deliberately *not* a
     pre-check: a dry-run of a bottled formula never invokes a compiler and
     comes back clean on a machine that cannot build anything — measured.
  2. **The arm base ships `ro.hardware.egl=angle`**, which routes the guest's
     GL to ANGLE → SwiftShader (software) no matter what the host offers. The
     image *does* have Mesa (`/vendor/lib64/egl/libEGL_mesa.so`) and the
     virtio-gpu render node (`/sys/class/drm/renderD128`), so the pieces are
     there — but `ro.*` properties are immutable after init (verified: `setprop`
     as root fails), so this needs the base rebuilt with
     `ro.hardware.egl=mesa`. The one-line check:

     ```
     adb shell dumpsys SurfaceFlinger | grep GLES:
     # want "Mesa, virgl"; "ANGLE ... SwiftShader" means still software
     ```

  Two latent bugs were found and fixed while working this out, both of which
  would have fired on the first Mac to get a GL-capable QEMU (omnidroid
  `eb4bdc9`, ported to the Mac lineage as `e5704ca`):

  * **macOS needs `gl=es`, not `gl=on`.** macOS deprecated OpenGL for Metal, so
    a macOS QEMU does GL through ANGLE, which speaks OpenGL ES. `default_display`
    hardcoded `gl=on` and `tests/test_gpu_display.py` *asserted* it. The visible
    result would have been a correctly-installed virgl QEMU that did not work.
  * **`uses_gl_context` only matched the literal `gl=on`**, so a `cocoa,gl=es`
    boot would have been read as non-GL, kept `-vnc`, and QEMU would have
    refused to start — the same one-line failure that made `--mode gaming` exit
    before `vnc_args` existed. The Mac lineage was worse: it appended `-vnc`
    unconditionally and had no `vnc_args` at all.

  `playable` still asks for the window on macOS regardless, because a native
  `cocoa` window is the input-latency win even without GL, and
  `default_display` degrades cleanly when there is no GL to be had.

### 8e. Should the executor be rewritten out of Python?

**No.** Measured on the frozen build:

| | |
|---|---|
| bare interpreter startup | 39 ms |
| + import the whole engine | 136 ms |
| full CLI round trip | 171 ms |
| **`omni-exec.exe --omnidroid version --json`** | **267 ms** |

Against a 55-83 s boot that is **0.3-0.5%**, and during gameplay Python is not
in the loop at all — QEMU is spawned detached and the game runs in the guest. A
rewrite in Rust/Go/C# would save ~200 ms per engine call and would not move the
frame rate by one frame.

There *is* one real cost, and it is the contract rather than the language: the
executor talks to the engine by **spawning a process per call**, and the
Instances tab polls `list` every 4 s. Each poll pays the full 267 ms to re-read
a JSON file — about 6.7% of a core, continuously, forever. The fix is to stop
spawning a process per call (`main.py` is already Python and could `import
omnidroid` in-process for read-only commands), not to change languages.

---

## 9. "Instant boots, native speed, headless with a viewer" — what is and is not possible

Everything in this section was measured on the Windows host (i7-13700F, RTX
4060, 32 GB) against **Pet Simulator 99, place `8737899170`** (universe
`15502302041`), on 2026-08-15.

### 9a. Headless + VNC + GPU: pick two

The product wants all three. QEMU gives any two.

**Fact 1 — a GL window and a VNC server are mutually exclusive.** Re-verified
this session on QEMU 11.0.50:

```
$ qemu-system-x86_64 ... -device virtio-gpu-gl-pci -display gtk,gl=on -vnc 127.0.0.1:98
qemu: -vnc 127.0.0.1:98: Display vnc is incompatible with the GL context
```

Same for `sdl,gl=on`, `gtk,gl=es`, `gtk,gl=core`. It is NOT refused for
`egl-headless` — that display exists to be paired with vnc/spice, and QEMU's
manual says so. **The old code conflated the two**, dropping `-vnc` for any GL
context at all, so every GPU-accelerated boot ran with no VNC server and the
missing server was reported as "the viewer is black". `blocks_vnc()` now scopes
the drop to the windowed case, and a real QEMU is constructed in
`tests/test_qemu_accepts_devices.py` to prove the headless GPU + VNC command
line is accepted.

**Fact 2 — `egl-headless` renders but does not present on Windows.** Three
boots, plain / `blob=true,hostmem=512M` / without the forced `video=` mode:

```
dmesg       [drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1203 (command 0x103)
timestats   totalFrames = 0        (displayOnTime 16-20 s)
screencap   solid black
VNC         connects, 1 update, mean brightness 0.0
```

`0x103` = `VIRTIO_GPU_CMD_SET_SCANOUT`, `0x1203` =
`VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID`. The guest's GL was healthy:
`GLES: Mesa, virgl (ANGLE (NVIDIA, NVIDIA GeForce RTX 4060), OpenGL ES 2.0` and
**no GL errors in logcat at all** — the only errors were audio and thermal HAL
noise. So the guest draws on the GPU and QEMU refuses the buffer it offers for
scanout. Most likely egl-headless advertises dmabuf support the Windows host
cannot honour; the cause matters less than the measurement.

Note the ES version, which is a second reason not to want this path on Windows
even if it presented: `egl-headless` goes through ANGLE and exposes **ES 2.0**,
while the windowed `gtk,gl=on` path goes through WGL and exposes **ES 3.2**.

**The resulting numbers, at 1280x800, in-world, screenshot-verified:**

| | fps | VNC viewer |
|---|---|---|
| `--gpu auto` (window, virgl, RTX 4060) | **24.2** (728 frames / 30.1 s) and **44.8** (1346 / 30.1 s) on two in-world runs | ✗ |
| `--gpu headless` (llvmpipe) | 3.2 | ✓ |

`auto` is the default and takes the frames. This is a real trade and it is
surfaced as one — in `--gpu`, and in the app's Launch panel as "Graphics".

**And that is not where it ended.** A QEMU GL window hidden with
`ShowWindow(SW_HIDE)` **keeps rendering** -- 303 frames in 30 s while invisible
-- so "GPU + nothing on screen" was already reachable; the only missing piece
was a way to watch it. Copying the pixels out was the obvious answer and the
wrong one (VNC does not exist on that boot, `screendump` returns "no surface",
and `screenrecord` over adb costs a guest-side H.264 encode on the CPU that is
already the bottleneck). **Reparenting costs none of it:** `SetParent` makes
QEMU's window a child of our Tk window, so the same GL surface is composited by
Windows into our frame. Measured embedded: **702 frames / 12.1 s = 58 fps**,
with native input. See §1 in PICK UP HERE and `omnidroid/embedview.py`.

### 9b. Instant boots: three blockers, and only two were fixable

The warm-boot cache (bake a booted, account-free machine once; restore it in
seconds) has existed in the code for weeks and had **never produced a single
entry on Windows**. Three independent reasons, found in this order:

1. **Disk, silently.** `has_room()` wanted `projected + 10 GiB`; the box has
   7.4 GiB free of 930 GiB. It returned a bare `False` and printed nothing, so
   every launch cold-booted with no clue why. The reserve is configurable now
   (`qemu.warm_reserve_gb` / `OMNI_WARM_RESERVE_GB`) and `room_report()` gives a
   skipped bake the numbers to print.
2. **QEMU cannot migrate to a FILE on Windows.** With the disk check relaxed,
   the bake failed with:

   ```
   "error-desc": "Failed to set FD nonblocking: Input/output error"
   ```

   Windows supports non-blocking I/O on sockets, never on file handles, so the
   moment QEMU wraps the destination file in a QIOChannel it fails — below any
   capability. `mapped-ram` alone failed the same way and
   `mapped-ram`+`multifd` **killed the QEMU process**. The same guest migrated
   to `tcp:127.0.0.1:<port>` in 0.2 s, so `omnidroid/migfile.py` now puts a
   loopback socket in the middle and does the file I/O itself (host listens for
   a bake, QEMU listens for a restore). `file:` + mapped-ram + multifd is kept
   for Linux/macOS, where it is both supported and better (sparse file, parallel
   read); the transport used is recorded in the entry's `meta.json`, since the
   two formats are not interchangeable.

   A unit test firing a genuine RST at that relay caught it writing 1.6 MB of a
   2.9 MB payload and reporting success — a truncated entry that would have
   failed a restore weeks later, on someone else's machine. `_shortfall()`
   cross-checks bytes written against `query-migrate`'s `ram.transferred`.

3. **WHPX blocks migration outright, and this one is final.**

   ```
   warm bake failed (migration State blocked due to non-migratable CPUID
   feature support,dirty memory tracking support, and XSAVE/XRSTOR support)
   ```

   That string is verbatim from `target/i386/whpx/whpx-all.c`. Windows
   Hypervisor Platform exposes no readback of guest CPUID state, no dirty-page
   log and no XSAVE area, so there is nothing for QEMU to serialise. No
   capability, transport, flag or disk changes it.

`_warm_cache_allowed()` refuses under a non-migratable accelerator, so the cost
is not paid on every launch any more. **The cache still works on KVM and HVF** —
the Mac and any Linux host get it, and the transport work above makes it
correct there too.

**What is left for Windows boot time:** a warm POOL. Keep N instances
pre-booted to the ready point (no cookie, no account — exactly the state
`bake_entry` freezes today) and deliver the session to one on launch. Nothing
is serialised, so WHPX cannot object.

### 9c. Farming and the ~400 MB target

Measured on PS99, `--mode farming`, after the two x86 fixes below:

| | |
|---|---|
| boot to joined | 102 s |
| balloon | `guest capped at 897 MB (target 896 MB)` |
| guest MemTotal / MemAvailable | 1450 MB / 470 MB |
| Roblox PSS / RSS in-guest | 508 MB / 743 MB |
| **host RSS (the QEMU process)** | **2198 MB** |
| renderer | llvmpipe (farming defaults to `--gpu headless`) |
| VNC framebuffer | mean brightness 208.5 — **content, not black** |

**The balloon does its job and the host does not benefit**, because QEMU on
Windows has no `madvise`: `ram_block_discard_range` fails, so the pages the
guest returns are never released to the host. (free-page-reporting is already
dropped on Windows for the same reason — it logged ~925 failed discards a
minute and reclaimed nothing.) So the host pays the full `-m` plus overhead,
and the only lever that moves it on Windows is `-m` itself.

"~400 MB, like the desktop Roblox app" is a comparison to the **game process**,
and the game process here is 508 MB PSS — in the same neighbourhood. An
*instance* is that game plus a whole Android plus QEMU, and on Windows that
floor is ~2.2 GB. The ~400-900 MB per-instance story needs Linux, where the
balloon, free-page-reporting and KSM all work.

**Two farming bugs fixed getting to that measurement:**

* `smp 1` could not carry an x86 farming boot through the session handover:
  the ordered `am broadcast` did not return within 45 s and `adb`'s
  `TimeoutExpired` came out of `cmd_start` as a traceback. Farming takes
  `smp_x86: 2` now (arm stays at 1 — it runs Roblox natively),
  `kiosk_broadcast` treats a timeout as a result, and the budget is 120 s.
* The balloon was declared missing when it was working: at 30 s a 2048→896 MB
  inflation read 1805 MB and printed "guest balloon driver missing?"; the same
  guest read 938 MB a minute later and reached target. 90 s now, and a balloon
  that is still moving says so rather than blaming the guest kernel.

### 9d. Resolution, and one idea that did not work

`--panel WxH|720p|800p|1080p|1440p` (config `qemu.panel`, env `OMNI_PANEL`)
sets the guest display, and `gaming.density_for_panel()` scales the guest's dpi
with it so a bigger panel does not shrink Roblox's on-screen controls.

**`video=Virtual-1:<mode>` was tried and reverted.** On a windowed GL boot the
guest takes the first mode the virtio GPU offers and comes up 640x480, which
Android papers over with a `wm size` OVERRIDE — so the compositor renders the
full 1280x800 and scales it down onto a quarter-size scanout. The kernel's own
`video=` override is the textbook fix and it: changed **nothing** on the
headless paths (already at the panel size), and **hung the windowed GL boot** —
five minutes without reaching adbd, QEMU alive and `running` over QMP, the
guest stuck before userspace. It is kept behind `qemu.force_video_mode` /
`OMNI_FORCE_VIDEO_MODE=1` and defaults off.

Worth remembering when reading fps numbers: at these resolutions the guest is
**CPU-bound on arm64 translation, not fill-bound**. 640x480 gave 19 fps against
14 at 1280x800 — a 4.2x pixel cut for 33% more frames.

### 9e. Modes: five became two

`gaming` (was `playable`) and `farming`. `playable` was `gaming` without a
window and there is no window any more; `hard`/`brutal` were fixed RAM tiers
that `--mem`/`--smp` already express. All three retired names are still
ACCEPTED and resolve to `gaming` — omni-executor persists the mode in its
settings and 1.0.14 ships `"mode": "playable"`, so rejecting them would break
every launch from a client that has not updated. `MODES.md` has the full table.

---

## Multi-device acceptance — what actually passed

Run it yourself: `python omni-executor/tests/acceptance_multidevice.py`.

**Test A — same Omni account, two machines.** 9/9 on Windows, 9/9 on the Mac.
Cookies followed the user, presence named the other device, each machine could
submit to its own accounts and to nothing else.

**Test B — sign out, sign in as someone else, same machine.** 3/3 on the Mac
after the fixes below. user1 signed in and pulled farm2+farm3; signing out
purged them from the machine; user2 signed in and saw *only* farm4, on disk and
in the cloud. From Windows, user1 could not execute in, or even read the status
of, farm4: `403 not_your_account`.

### Two real bugs Test B found

1. **A second user on one machine inherited the first user's accounts.** The
   local `accounts.json` is per-machine; ownership is per-user; sync pushed
   everything it found on disk. user2's first sync uploaded every one of
   user1's cookies into user2's cloud account. Fixed: the executor records
   which Omni user each local account belongs to (`cloud-owners.json` beside
   the store) and pushes only its own; signing out drops what that user pulled.
2. **Deleting an account did not delete it.** omnidroid's legacy-cookie
   migration runs on every read, so a removed account came straight back from
   `cookies/<label>.json` — `remove_account()` reported success and the record
   reappeared for the next user. Fixed with a one-shot marker.

Both have regression tests.

---

## Unfinished / known gaps

1. ~~Image upload.~~ **Done.** Both rebuilt images are live and server-verified:

   | artifact | bytes | sha256 |
   |---|---|---|
   | `base-x86` | 3,028,930,560 | `dbad36bc1bb7a787…` |
   | `offset-arceus-x86` | 996,016,128 | `7d16e35793c78f6f…` |

   Each was uploaded to `<name>.part`, hashed **by the server**, and only then
   moved over the old one — so the old image is replaced rather than
   accumulating, and a dropped connection can never leave a truncated blob being
   served as real. (3.8 GB at ~0.9 MiB/s took ~70 minutes.) The arm blobs are
   untouched and still current; the two `handoff` channel blobs
   (`omnidroid-bundle.tar` 1.0 GB, `arceus-STATIC-REMOTE-v2.apk` 143 MB) are
   last session's transfer artifacts and can be deleted whenever you want the
   1.1 GB back — nothing in the stable manifest points at them.
2. ~~In-game script execution.~~ **Working** — see §6. The black screen that
   made it look hopeless was Roblox loading a place, plus the Play Store killing
   the client mid-load (§5).
3. ~~The Mac could not boot an instance.~~ **Fixed, 2026-08-15.** It boots in
   **8.8–27 s** (warm restore) with `ok/delivered/played: true` and
   `arm64 native (no translation)`. Two things were wrong and neither was the
   boot path: its default offset was `arceusae`, which carries **no kiosk**
   (`reason: kiosk_missing`, so no session could ever be delivered) — the
   default is now `arceusremote`; and the Roblox asset CDN was DNS-blocked
   (§8b), which is what made the client sit forever and then disconnect.
4. **omnidroid's two lineages have diverged, and still need a deliberate
   merge.** Measured 2026-08-15: **40 commits only on the Mac's
   `slice-b-config-path`, 20 only on `slice-c-x86-offsets`**, and ~950 lines
   apart across `engine.py`/`qemu_proc.py`. A `git cherry-pick` of one commit
   conflicted across **8 hunks in two files** — exactly the shape where an
   automated merge silently picks a side. It was aborted, and the fixes were
   hand-ported instead (see the pickup list at the top).

   What changed in your favour: **nothing is uncommitted any more.** The Mac's
   ~17 dirty files are committed (`dd7e50c`), tarred to
   `~/omni-backups/omnidroid-wip-*.tar.gz` *and* pushed to GitHub as
   `origin/slice-b-config-path`. Windows still carries ~60 uncommitted tracked
   files — same rule as before: commit only named files there, never
   `checkout`/`stash`/`reset`/`add -A`.
5. ~~`cpuset: SKIPPED` still.~~ **Fixed, 2026-08-15 — and the diagnosis in the
   previous revision of this document was wrong.** It was not the stale
   `run.json` handle. `gaming.build_pin_game_step` tested `not su`, and `""` is
   a VALID root mode (adbd runs as uid 0 on the x86 base), so it returned None
   on every single launch. Now `su is None`; a live boot reports
   `cpuset: game on top-app` and `/proc/<pid>/cgroup` confirms `3:cpuset:/top-app`.
6. ~~The GL path comes up 640x480.~~ **Fixed, 2026-08-15.** `wm size reset`
   could never have fixed it: the virtio GPU offers a mode list that *starts*
   at 640x480 and Android takes the first entry, so 640x480 IS the physical
   size as far as the guest is concerned. The panel actually requested is
   recorded at spawn (`run.json.gl_panel`) and set explicitly, with the
   matching 160 dpi. A boot now reports `1280x800 (GL panel)`.
7. ~~Your desktop copy is still an old build.~~ **Done, 2026-08-15.**
   `C:\Users\berat\Desktop\Omni Executor\` (73 MB, ~1.0.4) has been **deleted** —
   it held nothing but build output and an auto-generated default
   `configs/paths.json` with no bases and no accounts, checked before removing
   it. The stale `dist\` (1.0.4), `dist-new\` (1.0.8), `dist-1012\` and
   PyInstaller's `build\` went with it: 415 MB freed in total.

   **There is no hand-deployed copy any more, and that is the point.** The app
   is installed like any other program, by the installer (§0c):

   | | |
   |---|---|
   | installed at | `%LOCALAPPDATA%\Programs\OmniExecutor` — **1.0.13**, and it will offer 1.0.14 |
   | installed by | `http://72.62.59.232/omni/dist/blob/setup-win` |
   | updates | in-app banner / Settings → Updates, **or** re-run the installer |

   Re-running the installer over an existing install is the supported repair:
   it closes the running copy, replaces the directory (restoring the old one if
   the copy fails), and keeps `%LOCALAPPDATA%\OmniExec` — the accounts and the
   7.2 GB of images — untouched.

   Verified that the server offers 1.0.14 to a **1.0.13**
   client alike, so any older install can still reach it by itself. The one
   exception is a machine stuck on the first-boot screen: the update banner
   lives in the app shell, behind the `ready` gate, so it never sees one. Those
   need the installer.

   What is left in `omni-executor\` is current, not stale: `dist-1013\` (the
   published build) and `dist-setup\` (the installer). There is also an EMPTY
   `dist-1011\` — 0 bytes, every file already gone, held open by a stale
   Windows directory handle. It is `.gitignore`d (`dist-*/`) and disappears on
   the next reboot.

---

## Layout and running things

| What | Where |
|---|---|
| Repos (must be siblings) | `Omni Apps/{omnidroid, omni-executor, omni-backend}` |
| Installed app (Windows) | `%LOCALAPPDATA%\Programs\OmniExecutor` — put there by the installer |
| Product runtime (images, accounts, paths.json) | `%LOCALAPPDATA%\OmniExec\` |
| Mac runtime | `~/Desktop/Omni Apps/omnidroid` (images in `~/OmniImages`) |
| VPS app | `/root/omni-backend`, pm2 `omni-backend` |

```bash
# engine, by hand
export OMNIDROID_CONFIG_PATH="C:/Users/berat/AppData/Local/OmniExec/paths.json"
export OMNI_DATA_DIR="C:/Users/berat/AppData/Local/OmniExec"
export OMNI_IMAGES_DIR="$OMNI_DATA_DIR/images"
export PYTHONPATH="C:/Users/berat/Desktop/Omni Apps/omnidroid"
# only if this box lacks a system QEMU/adb (the app installs its own):
#   export OMNI_QEMU_DIR="$OMNI_DATA_DIR/qemu"
#   export PATH="$OMNI_DATA_DIR/platform-tools:$PATH"
omni-executor/.venv/Scripts/python.exe -m omnidroid start <account> --place <id> --json

# backend
python scripts/deploy.py          # tar the tracked tree, install, restart, health-check
node --test --test-concurrency=1 backend/tests/*.test.js   # serial, or Arcjet 429s it
python scripts/vps.py run "<cmd>" # anything on the server (paramiko; ssh here has no key)
python scripts/push-images.py base-x86 offset-arceus-x86

# the Mac needs Homebrew on PATH over ssh
ssh berat@192.168.0.30 'export PATH=/opt/homebrew/bin:$PATH; ...'

# the Mac, end to end (its own venv; omnidroid needs no deps of its own)
ssh berat@192.168.0.30
cd ~/Desktop/"Omni Apps"/omni-executor
./scripts/setup-macos.sh --check          # report only
./scripts/setup-macos.sh                  # install everything
./scripts/setup-macos.sh --gpu            # + build a GPU QEMU (slow; see §8d)
cd ../omnidroid && PYTHONPATH=. ../omni-executor/.venv/bin/python -m omnidroid \
    start <account> --place <id> --no-window --json
```

**`--no-window` is required for any engine command driven over ssh to the
Mac.** `_host_has_gui()` returns True unconditionally on macOS (there is
always a window server), but an ssh session is not part of the logged-in Aqua
session, so `-display cocoa` has nothing to attach to and QEMU exits. From the
GUI app it inherits the right session and the window works.

**Two adb rules on the Mac, both learned the hard way.** Never let adb inherit
a pipe over ssh — its forked server holds it open and the read never ends;
redirect to a file and `cat` the file. And `adb devices` reporting
`unauthorized` after a guest reboot is not fixable from adb: the
authorization does not survive a guest-initiated reboot, and the "Allow USB
debugging?" dialog needs input the headless path cannot give it. Restart the
instance instead.

## Git

Everything is committed and pushed, on **both** machines. State at the end of
2026-08-15:

| Repo | Machine | Branch | HEAD |
|---|---|---|---|
| omnidroid | Windows | `slice-c-x86-offsets` | `eb4bdc9` gl=es + vnc/GL exclusivity |
| omnidroid | **Mac** | `slice-b-config-path` | `e5704ca` ported gl=es + vnc_args |
| omni-executor | both | `slice-c-windows-exe` | `1086311` macOS `--gpu` phase |
| omni-backend | Windows | `slice-c-win-artifacts` | `5f9de44` release app-win 1.0.14 |

Nothing is merged to `main` — that is a decision waiting for you, and for
omnidroid it needs the lineage reconciliation in the pickup list.

**The Mac has no GitHub credentials** (`could not read Username for
'https://github.com'`). Its commits were relayed to the remote from Windows:

```bash
cd omnidroid
git remote add macmini "berat@192.168.0.30:Desktop/Omni Apps/omnidroid"
git fetch macmini slice-b-config-path
git push origin FETCH_HEAD:refs/heads/slice-b-config-path
git remote remove macmini
```

**omnidroid on Windows still carries ~60 uncommitted tracked files** —
pre-existing WIP the bundle shipped with. Never
`git checkout`/`stash`/`reset`/`add -A` there; commit only named files.

**The Mac's WIP is no longer uncommitted** (that was ~17 files). It is
`dd7e50c`, plus a tarball and a full patch in `~/omni-backups/`. Committing it
before syncing was deliberate: a dirty tree plus a cherry-pick over the same
files is exactly how work disappears.

**Watch the line endings when scripting edits.** A Python rewrite of
`qemu_proc.py` with `io.open(p, "w")` flipped the whole file to CRLF and turned
a 150-line diff into 2493 lines. Use `newline=""` on read *and* write, or
normalise with `data.replace(b"\r\n", b"\n")` before committing; `git ls-files
--eol <path>` tells you what you actually have.

## Test baselines

- **omnidroid**: **11 failed / 932 passed** on 2026-08-16 (was 19/814, and
  18/904 at the start of that session). The 11 are the same environmental
  class as always — POSIX file modes on Windows (`0o666 != 0600`), arm-only
  fixtures on an x86 host, Windows path separators in a qemu-img argv
  assertion, an unreadable-root simulation that needs POSIX permissions.

  Seven of the eight that went away were tests encoding contracts this
  session deliberately changed, and each was rewritten to assert the NEW
  contract with the measurement that forced it (the boot no longer squeezes;
  the balloon is skipped where the host cannot reclaim). One was a real
  robustness bug the tests found: a new boot-tail step that could `SystemExit`
  into the boot path — the same trap `quiet_the_store` documents, and the fix
  is the same `except (Exception, SystemExit)`.

  `tests/test_kiosk_boot_app.py`'s two failures were confirmed pre-existing by
  disabling this session's boot-tail addition and re-running: they fail either
  way.

  **Run it with a config that has a base registered, or collection dies**, not
  with a test failure but with `INTERNALERROR ... SystemExit: error: no base
  image is registered` — `tests/test_qemu_accepts_devices.py` calls
  `load_config()` at import time. And **copy the config fresh each run**: the
  engine WRITES BACK to `OMNIDROID_CONFIG_PATH` and has been seen to drop the
  base registration, so the second run fails for a reason the first one caused.

  ```bash
  cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
  cd omnidroid && OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
    OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
    ../omni-executor/.venv/Scripts/python.exe -m pytest tests/ -q
  ```

  To tell your own breakage from the baseline, diff the `FAILED` lines against
  the 19 rather than counting them — one new failure in a list of 20 is
  invisible otherwise. That is how the `gl=on` assertion in
  `test_gpu_display.py` was caught.
- **omni-executor**: **172 passed, 1 failed** (was 118/1) —
  `test_resume_after_interruption_uses_range`, flaky on identical code
  including on `HEAD`.
- **omni-backend**: **82/82 pass — but only with `--test-concurrency=1`**:

  ```bash
  node --test --test-concurrency=1 backend/tests/*.test.js
  ```

  Run concurrently, the files race each other into **Arcjet's rate limiter** and
  fail with 429s that look like real assertion failures (`expected 404, actual
  429`). Every suite in isolation passes. `npm test`'s quoted glob also does not
  expand on Windows and silently runs **zero** tests.

## Gotchas that cost real time

**Newest — from the performance/GPU work (§8)**

00a. **A frame-rate number means nothing without a screenshot taken at the same
    instant.** Three readings of 53, 44 and 59 fps were all bogus — a
    disconnected client's "Error 277" overlay and a loading-screen animation,
    both of which animate happily at 60 Hz while the game renders nothing.
    Verify the client is in-world, every single time.
00b. **`""` is a VALID root mode, so every gate on it must be `is None`.** On
    the x86 base adbd runs as uid 0 with no `su` binary, so `resolve_root_shell`
    returns `""` and `if not su:` reads it as "no root". That one falsy test
    skipped the top-app cpuset pin on every launch since it was written, and
    the previous revision of this document blamed something else entirely.
00c. **`ro.*` guest properties cannot be changed on a running instance**, root
    or not — `setprop ro.hardware.egl mesa` fails outright. They are baked at
    init, so anything that depends on one is an image rebuild.
00d. **A `brew install --dry-run` of a BOTTLED formula never invokes a
    compiler**, so it passes on a machine whose Command Line Tools are too
    outdated to build anything. Do not use it as a can-I-build probe; attempt
    the real install and read the reason from the log.
00e. **macOS takes `gl=es`, never `gl=on`** (ANGLE → Metal; OpenGL is
    deprecated there). And whatever checks "is this a GL boot" must match
    `gl=` generally — a check that knows only `gl=on` leaves `-vnc` on a
    `gl=es` command line, and QEMU refuses that pair and exits.
00f. **Writing a Python file with `io.open(p, "w")` on Windows converts LF to
    CRLF**, turning a 150-line diff into a 2493-line one. Use `newline=""` on
    both read and write.

**From the fresh-install failure (§0)**

0a. **The QEMU installer never STARTS unelevated.** It is manifested
    `requireAdministrator`, so `CreateProcess` raises `WinError 740` rather
    than running and failing. `shell=True` only relocates that into cmd.exe.
    Elevation is a SHELL verb — it needs `ShellExecuteEx` with `"runas"`, and
    `SEE_MASK_NOCLOSEPROCESS` to get a waitable handle at all (plain
    `ShellExecuteW` hands back an HINSTANCE and no process).
0b. **NSIS `/D=` must be LAST and UNQUOTED**, even for a path with spaces.
    Quoting it — the reflex for every other program — makes the installer
    silently use its DEFAULT location, which then reads as "the download went
    to the wrong place".
0c. **Windows re-injects `ProgramFiles` into child processes.** Overriding it
    via `subprocess(env=…)` is silently ignored, so a "fresh machine"
    simulation that relies on it is testing nothing. Verified: both
    `dict(os.environ)` and an explicitly uppercased dict come back as the real
    path in the child.
0d. **Never install tools into the app directory.** `updates.py
    apply_staged_app` renames the whole app dir aside and copies the new build
    in, so anything living there is destroyed on every app update.
0e. **`shutil.which` is the wrong question on Windows.** omnidroid's
    `qemu_bin()` deliberately never consults PATH there, so any app-side check
    that uses `which` will disagree with the engine — in both directions.

**From chasing the "too slow" / restart / no-session reports**

1. **QEMU refuses `-vnc` together with a GL context**, and says so in one line.
   That single incompatibility caused `--mode gaming` to exit on startup AND
   `egl-headless` to publish a black framebuffer. If a GPU path misbehaves, read
   `runtime/<name>/qemu.log` first — QEMU had already explained itself.
2. **`omnidroid screenshot` does NOT read VNC.** It shells `adb screencap`, so
   it looks identical whether the viewer works or not. The viewer path is
   `vncview.RFBClient` (and `connect()` only handshakes — `run()` is the receive
   loop, on its own thread). An entire wrong diagnosis came from testing the
   wrong path.
3. **`resolve_su` means "is there an su BINARY", not "am I root".** On the x86
   base adbd runs as uid 0 with no `su` anywhere, so every root-gated tune
   skipped on a guest that could do all of them. Use `resolve_root_shell`.
4. **A negative capability probe must not be cached.** "No root" is usually
   "asked before adb was up".
5. **The Play Store self-updates and kills the game.** Any package replace kills
   dependent processes. `com.android.vending` is disabled on every boot.
6. **A running `omni-exec.exe` silently truncates a PyInstaller build.** The
   result still launches and is missing the frontend. Build with `-DistPath
   dist-new` and let `push-app.mjs` verify it.
7. **Runtime port/pid bookkeeping goes stale after a failed start.**
   `runtime/<name>/run.json` can name a different `adb_port` than the live QEMU
   actually forwards, so `build_acct()` hands you a handle that talks to
   nothing. `adb devices` is the truth. This is what still blocks the cpuset
   pin (gap 5).

**From earlier this session**

8. **`adb` forks a server daemon that inherits your pipes.** With
   `capture_output=True` that daemon holds them open for its whole life, so
   `subprocess.run`'s timeout path — kill the child, then `communicate()` again
   to reap — blocks *forever*, timeout or no timeout. Measured: `adb_connect`
   sat 12+ minutes against a dead QEMU with a 15 s timeout set. `omnidroid/adb.py`
   now hands adb temp files, never pipes. **If you shell out to adb anywhere
   else, do the same** (an `adb devices` over ssh hung this session for the same
   reason).
9. **`wait_for_boot` used to poll a dead QEMU for its full timeout** (25 min on
   a first boot) while QEMU's own one-line explanation sat unread in `qemu.log`.
   It now bails when the pid is gone and prints the log tail.
10. **Restarting the framework (`stop; start`) drops adb into `offline`, and it
   stays there** — `adb connect` on a known endpoint just says "already
   connected". A command run then returns the string `adb: device offline`,
   which reads like output. That silently no-op'd every `/data` provisioning
   step in the first base rebuild while reporting success.
11. **HTTP header values are latin-1.** The Mac is "Berat’ın Mac mini";
   `http.client` raised before the request left the machine. Device names are
   percent-encoded now.
12. **`argparse` %-formats help strings** — a literal `%LOCALAPPDATA%` is a
   `ValueError` at import.
13. **Two 4 vCPU guests + a 3 GB upload starved a boot** to 17 s of guest CPU in
   7 minutes. If a boot is slow, check whether the guest is *idle*.

**Still true from before**

14. Closed loopback ports **time out** on this Windows host instead of refusing.
15. `Get-WindowsOptionalFeature`/DISM need elevation; ask QEMU about WHPX instead.
16. WHPX *is* Hyper-V — you cannot "turn Hyper-V off to go faster".
17. `chrome.exe --version` prints nothing on Windows (GUI subsystem binary).
18. PowerShell 5.1: no `$IsWindows`; native stderr becomes ErrorRecords (judge by
    `$LASTEXITCODE`); variable names are case-insensitive.
19. **PyInstaller deletes and recreates its output dir** — any handle on it
    (an Explorer window, a shell whose cwd is inside) fails the build. Use
    `-DistPath <fresh>`.
20. **PyInstaller DOES find a plain `import x` inside a function** — verified
    with a control build that bundled `accountsync`/`cloud`/`bootstrap` without
    any hiddenimports entry. What it cannot resolve unaided is a dependency that
    is imported conditionally AND ships native binaries or data: selenium (whose
    Selenium Manager is an executable), tkinter and PIL. Those are the
    hiddenimports that matter, and `tests/test_packaging.py` asserts BOTH specs
    declare the same set — the macOS spec had been missing them for months.
21. `OMNIDROID_SELF_ARGV` is required, or detached children relaunch the GUI.

## Performance notes (measured, counter-intuitive)

- **Autoscaling to host capacity made boots 6.5x SLOWER** under WHPX. Capped to
  4 vCPU / 4096 MB on Windows (`WHPX_SMP_CEIL`, `WHPX_MEM_CEIL_MB`).
- **`-cpu host` is WORSE** than `qemu64` here, despite the guest translating
  arm64. Three runs, zero completions. Do not re-enable on reasoning alone.
- **Warm boot cannot work on Windows.** QEMU refuses the snapshot under WHPX
  (no dirty-memory tracking, no migratable CPU state), and now also refuses it
  under virgl ("virgl is not yet migratable"). It degrades cleanly.

Added 2026-08-15, all at 1280x800 on one account and place, screenshot-verified
in-world (see §8):

- **A host window is worth 5x, and it is not about the window.** 3.2 fps
  headless (`Mesa, llvmpipe`) vs 13.6-16.5 fps windowed (`Mesa, virgl, RTX
  4060`). Without a window QEMU has no GL context, so virglrenderer cannot run
  at all. This was the whole of "3 fps and unplayable".
- **`--smp 6` on WHPX is catastrophic, not merely unhelpful**: 5.6 min to boot
  against 0.8, and **0.6 fps**, with QEMU using 83% of ONE host core. At smp 4
  QEMU uses ~1.4 of 16 host cores, so the host is nowhere near the limit —
  `libndk_translation` is. `WHPX_SMP_CEIL = 4` is correct; leave it.
- **Extra CPU features buy nothing.** `-cpu qemu64,+ssse3,+sse4.1,+sse4.2,
  +popcnt,+aes,+xsave,+avx,+avx2,+bmi1,+bmi2,+fma,+f16c,+movbe` boots cleanly
  (unlike `-cpu host`) and measured 14.4 fps against 14.2. Reverted. The
  translator does not appear to use them.
- **Lower resolution barely helps, which is the diagnosis.** 640x480 gave 19
  fps against 14 at 1280x800 — a 4.2x pixel cut for 33% more frames. That is
  CPU-bound, not fill-bound.
- **The quality profile is nearly free once there IS a GPU** — 16.5 fps at
  `high` (level 10, post-FX on) vs 16.4 at `balanced`. It is only expensive on
  a software renderer, which is why the default now steps down there.
- **`free-page-reporting` is negative on Windows**: 925 failed
  `ram_block_discard_range` calls and 78 KB of qemu.log in ~60 s, reclaiming
  nothing, because on Windows the discard IS the reclaim. Dropped there; the
  log is now 3 lines.
- **The Mac is the fast machine, and by a wide margin on the CPU side.** 8.8-27 s
  to boot against 55-83 s here, `arm64 native (no translation)`. It renders in
  software today — see the pickup list for what that would take to change.
