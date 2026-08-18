# Our own QEMU, and what it unlocks

Design, 2026-08-19. Five problems are open at once — the window flickers when
resized, the X does nothing, 30 farming instances would need a 100 GB
pagefile, the kiosk guesses which app to launch, and a retired tkinter viewer
is still in the tree. Four of the five turn out to have the same answer, and
the answer is already half-written on this disk.

---

## 1. What is wrong today

| # | Symptom | Where it actually lives |
|---|---|---|
| 1 | The gaming window flickers hard while dragging a corner | `hostwin._aspect_watch` (`hostwin.py:1674`) corrects the window **reactively, from a separate process**, up to 125x/s during Windows' modal size loop |
| 2 | Clicking the X does nothing | QEMU is spawned `window-close=off` (`qemu_proc.py:366`); the only close prompt ever written (`windowbar._ask_close`) has no caller |
| 3 | 30 farming instances need ~92-120 GB of commit | commit charge tracks `-m` at 1:1 and `memory-backend-file` is not registered in the shipped Windows QEMU (`qemu_proc.py:2607`) |
| 4 | An instance that actually farms dies at 10-15 minutes | it runs at ~700 MB of guest headroom; the fix is a bigger `-m`, which today costs ~2 instances per 512 MB |
| 5 | The kiosk sometimes pins the wrong app and then refuses the game | `omni_game_package` is written **after** `MainActivity.onResume` already resolved-and-launched a guess (`tests/test_kiosk_boot_app.py:5-45`) |

Problems 1 and 2 share a root cause that `hostwin.py:1678-1683` states outright:
one process cannot handle another's `WM_SIZING`, and cannot intercept another's
`WM_CLOSE`, without injecting a DLL. Every workaround built on top of that
constraint — the Tk strip, the polling lock — is a way of losing more slowly.

Problem 3's root cause is stated just as plainly at `qemu_proc.py:2601`:
*"COMMIT 3072 MB = -m, and NOTHING reduces it."* True of the shipped binary.

## 2. The decision

**The product ships its own QEMU.** Not a vendor build we style from outside —
a build we compile from a patch series this repo owns.

This is less of a leap than it reads. `C:\qemubuild` is already QEMU 11.1.0
with five omni patches applied, and `qemu-system-x86_64.exe` was already built
from it on 2026-08-16. The patches are **uncommitted working-tree
modifications in a directory outside any repo** — one stray `git checkout` and
they are gone, and there is no way to reproduce them on the Mac. That is the
first thing this design fixes.

Everything the window needs then becomes in-process and trivial: `WM_SIZING`
is handled inside the drag loop instead of chasing it, and `WM_CLOSE` can ask
before it acts.

## 3. Sub-project E — omni-qemu

### 3a. The patch series

`qemu-patches/` in this repo, numbered, `git format-patch` shaped, applied by
`tools/build-qemu.py` against a pinned upstream tag. It cannot live under
`qemu/` — that directory is the downloaded binary bundle and is gitignored
(`.gitignore:49`), which is precisely the accident that left the current
patches unversioned. Five patches exist in the tree at `C:\qemubuild` and are
lifted as-is:

| patch | file | what |
|---|---|---|
| `0001-omni-window-icon` | `ui/gtk.c` | `QEMU_WINDOW_ICON` |
| `0002-omni-aspect-lock` | `ui/gtk.c`, `include/ui/gtk.h`, `ui/gtk-gl-area.c` | `QEMU_WINDOW_LOCK_ASPECT` -> a `WM_SIZING` GDK filter on Windows, `GDK_HINT_ASPECT` elsewhere |
| `0003-omni-panel-pin` | `ui/gtk.c` | `QEMU_WINDOW_PANEL` — stop the guest adopting the window's startup size as its panel |
| `0004-omni-confirm-close` | `ui/gtk.c` | `QEMU_WINDOW_CONFIRM_CLOSE` |
| `0005-omni-win32-discard` | `system/physmem.c` | `DiscardVirtualMemory` as the `_WIN32` arm of `ram_block_discard_range` |

Two are new:

| patch | what |
|---|---|
| `0006-omni-win32-memory-backend-file` | `backends/hostmem-file.c` + `system/physmem.c` + `backends/meson.build` — guest RAM from a mapped file on Windows |
| `0007-omni-win32-punch-hole` | `FSCTL_SET_ZERO_DATA` as the file-backed arm of the discard path, so a discard reclaims the *disk* as well as the RAM |

### 3b. The close prompt becomes three options

`0004` currently offers *Cancel / Stop instance*. It becomes exactly what was
asked for:

```
   This machine is running.
     [ Shut down the machine ]   power off, everything inside is lost
     [ Hide the viewer ]         keep it running, show it again from the app
     [ Cancel ]                  default
```

*Hide* is `gtk_widget_hide` on QEMU's own window — the VM keeps running, the
GL context is untouched, and the engine learns about it the same way it learns
about `_view_hide` today (`run.json: window_visible`). Because the dialog is
in-process, the window's real X is finally load-bearing instead of inert, and
`window-close=off` comes off the gtk flag list.

### 3c. `memory-backend-file` on Windows — the measured part

`hostmem-file.c` is excluded on Windows in `backends/meson.build:13`, and
`file_ram_alloc` sits under `#if defined(CONFIG_POSIX)` in `physmem.c:1542`.
The port replaces `mmap` with `CreateFileMapping` + `MapViewOfFile`.

Two probes were built with msys2 mingw64 and run on the target host before
this design was written (`commit_probe.c`, `whpx_probe.c`; results in
`docs/superpowers/runbooks/2026-08-19-windows-ram-backing.md`):

```
3 GiB of guest RAM              system commit delta
  VirtualAlloc(MEM_COMMIT)              +3078 MB     <- what QEMU does today
  CreateFileMapping + MapViewOfFile        +12 MB
```

and, on the question `docs/windows-ram-discard.md` called *"the single question
that decides whether the patch works at all"*:

```
WHvMapGpaRange(file-backed)              -> 0x00000000  OK
after host touch 256M                    -> commit +0 MB
FSCTL_SET_ZERO_DATA while GPA-mapped     -> 1 (err 0)
readback[0]                              -> 0x00        hole visible through the live mapping
file EOF=3072 MB  allocated=0 MB
```

**WHPX does not pin the range.** Pages can be punched back out while the guest
is mapped, and the guest reads zeroes — which is what `MADV_DONTNEED` promises
and what every reclaim path in QEMU is written against.

Consequence, per farming instance: commit **4065 MB -> ~1000 MB** (QEMU's own
private allocations). Resident RAM is unchanged at 384 MB, because the
working-set governor (`runtime.cap_working_set`) already delivers that.

The RAM files live in the existing scratch directory (`qemu_proc.scratch_dir`),
which is already reaped, already size-checked, and already redirectable via
`qemu.scratch_dir` — so nothing new has to be invented to place them, budget
them, or clean them up after a crash.

### 3d. `free-page-reporting` comes back on Windows

It was dropped there (`qemu_proc.py:1485`) because every discard returned
`-ENOSYS`: 925 failures a minute, 78 KB of log, nothing reclaimed. With `0005`
and `0007` the discard succeeds, so the guest reporting a freed page now
punches a hole in the RAM file. That is what keeps the file's *allocated* size
near the guest's **live** set instead of everything it has ever touched, and it
is the difference between a fleet that fits on this disk and one that does not.

### 3e. Building it

`tools/build-qemu.py` — apply patches to a pinned tag, configure, build,
prune, stage. Targets `x86_64-softmmu` **and** `aarch64-softmmu` (the existing
build tree has only the first, so the ARM base has no patched binary at all
today). It replaces the pinned 11.0.50 portable bundle that
`omni-backend/scripts/build-qemu-portable.py` produces, and reuses that
script's deny-list pruning — the reasoning there (option ROMs are loaded
lazily and by name, so an allow-list boots on the machine that wrote it and
fails six weeks later on a customer's) still holds.

On macOS the same series is applied on the Mac, into a private prefix, with
`qemu.dir` pointed at it. Never over the Homebrew `qemu` formula.

### 3f. What is deleted

* `_windowlock`, `_aspect_watch`, `aspect_lock`, `run_aspect_lock`,
  `_initial_fit`, the `ASPECT_*` constants, and the engine's window-lock pid
  plumbing. The flicker is not tuned down; its mechanism is removed.
* `window-close=off` from `_WINDOW_FLAGS`.
* `_apply_window_env`'s "not implemented" comments — the env vars start
  meaning something.

`aspect_fit`/`aspect_is_close` stay as pure functions with their tests: the
same arithmetic now lives in C, and the Python is the readable specification
of it.

## 4. Sub-project A — farming density

Depends on E.

1. Guest RAM onto `memory-backend-file` in the scratch dir, for the density
   profile first. Gaming follows once it is proven.
2. Re-enable `free-page-reporting=on` on Windows.
3. **Raise `-m`.** This is the point of the whole exercise. Instances that
   genuinely farm die at 892-931 s from ~700 MB of headroom, while stuck ones
   at ~1250 MB never die; the lever has always been `-m 3584`/`4096`
   (`lean.GUEST_MEM_FLOOR_MB`) and it has always cost ~2 instances per 512 MB.
   With commit off the table it costs disk instead, and disk is the thing
   punch-hole reclaims.
4. Serialise `cmd_start` the way `pool_boot_slot` already is.
5. Surface `client_is_playing` in `list` and the app row. Four of six
   instances once reported a perfect 384 MB and 50% of a core while farming
   nothing; a fleet number that counts those is not a fleet number.
6. Re-run the capacity ladder and write the honest figure into `MODES.md`.

**30 instances is a projection, not a promise.** RAM (384 MB x 30 ~ 12 GB),
commit (~30 GB, inside the pagefile that already exists) and CPU (30 x 50% of
a core) all fit. Disk is the open one: the sparse RAM files plus 1.3 GB of
scratch each land near the free space on this host, and how near depends
entirely on how much free-page-reporting claws back. That gets measured, not
assumed.

## 5. Sub-project B — gaming quality

Depends on E.

The aspect lock and the quit prompt arrive with E. What is left is
verification and the Mac:

* frames over 30 s per platform via `dumpsys SurfaceFlinger --timestats`;
  the guest really on `Mesa, virgl` and never `ANGLE ... SwiftShader`.
* Input stays passthrough — `usb-tablet` + `usb-kbd`, no pointer-lock layer.
  The work is removing latency that exists, not adding a layer.
* macOS: `gl=es` (never `gl=on`), a virgl-capable QEMU in a private prefix,
  and an arm base rebuilt with `ro.hardware.egl=mesa` — `ro.*` is immutable
  after init, so this one is an image rebuild, not a `setprop`.
* `macgpu.readiness()` is written and tested but **has no production caller**.
  Wire it into `doctor`.

## 6. Sub-project C — kiosk stability and silent boot

Independent of E.

**The race is the bug.** `MainActivity.onResume` resolves a game package and
Lock-Task-pins it; `omni_game_package` is written later; `/data` is ephemeral
so every boot is a first boot with the setting unset. The recorded failure
pinned Magisk and then refused Roblox as a Lock Task violation. Fixes:

1. Bake `omni_game_package` into the image so it is set before the kiosk ever
   runs, and make the dev-mode "first launchable non-system app" fallback
   **wait** rather than guess — a kiosk with nothing to launch shows the
   loading screen, which is the correct state, not an error.
2. Replace the `launchedThisBoot` bool with a bounded retry: a deep link that
   does not resolve gets tried again with backoff instead of parking until a
   human taps.
3. Force-stop Roblox before every session hand-off, so a live process cannot
   join as the previous account.

**Boot cosmetics** — most of this already exists and needs verifying on both
bases rather than building: `quiet loglevel=0 console=null
vt.global_cursor_default=0` on x86 (`qemu_proc.py:1834`), the same plus a
hidden GRUB menu on arm (`engine.py:3337`), `setStatusBarDisabled(true)` +
`LOCK_TASK_FEATURE_NONE`, three-layer keyguard dismissal, a 1x1 black
wallpaper, and `install_no_console_default()` for host-side console flashes.
The deliverable is a checked list per base, not new code — plus whatever the
check finds missing.

**Full-disk access is already automatic**: `consent.py:77` issues
`appops set <pkg> MANAGE_EXTERNAL_STORAGE allow`, which needs shell's
`MANAGE_APP_OPS_MODES`, not root. It is re-applied every boot because app-ops
do not survive ephemeral `/data`. A root fallback is added for the case where
adbd is not uid 0.

## 7. Sub-project D — remove the tkinter viewer

Independent. Small.

`RFBClient` moves out of `vncview.py` into `omnidroid/rfb.py` — pure protocol,
no tkinter, no Pillow — because `capture.py:280` is built on it and
screenshot/capture/autocap must keep working. Then `run_viewer`,
`windowbar.py`, and the `_vncview` / `_windowbar` / `_windowlock` subcommands
go, along with `--hidden-import tkinter` / `PIL` in `build-exe.ps1` and
`build-linux.sh`. No tkinter remains in the package.

The retired-but-shipped state is worth naming: `windowbar.py` is 636 lines
with 705 lines of tests, and `engine._spawn_window_bar` has had **zero
production callers** since `cmd_view` hardcoded `bar_ok = False`
(`engine.py:7239`). Its docstrings also describe an `apply_chrome` that no
longer exists, so anyone reviving it would get two title bars.

## 8. What gets measured

Nothing here is settled by argument.

| claim | how it is checked |
|---|---|
| file-backed RAM removes the commit | marginal system commit per instance, `-m 4096`, six live instances |
| punch-hole tracks the live set | the RAM file's *allocated* size over a 30-minute farm |
| the resize is smooth | drag a corner for 10 s, screen-record it, and count `SetWindowPos` calls (target: zero from outside the process) |
| the three close outcomes | by hand, on a live instance, all three |
| the guest is really on the GPU | `dumpsys SurfaceFlinger \| grep GLES:` -> `Mesa, virgl` |
| the fleet number | the capacity ladder re-run at the new `-m`, on this host |
| an instance is really farming | `client_is_playing` — USER time, not "the process exists" |

Two standing rules from earlier sessions apply. **Verify the frozen build, not
the source** — the engine is frozen in from a sibling checkout at build time,
so "the source is fixed" and "the shipped exe is fixed" are different claims.
And **do not re-open a probabilistic failure on one lucky run**; PS99 at
`-m 2048` survived once in five.

## 9. Non-goals

* **Resolution above the base's native panel.** `--panel 1080p` on the x86
  base took 3.3+ minutes without reaching adbd and came up 1280x800 anyway.
  The window scales; anything sharper is an image change.
* **Frame-rate parity with a native client on x86.** Roblox ships arm64 only,
  so every instruction goes through `libndk_translation`. No display flag
  touches that. The arm base under HVF does not have this ceiling.
* **Pointer lock and key mapping.** Each is its own feature with its own UI.
* **Warm boot on Windows.** WHPX registers a migration blocker; the warm pool
  is the answer there and already works.
* **KSM.** Linux-only. There is no Windows equivalent and the file-backed path
  does not create one.

## 10. Sequencing

```
E  omni-qemu ............ patch series, build, ship        <- everything below waits on this
|- A  density ........... file-backed RAM, bigger -m, honest fleet number
\- B  gaming ............ verify; then macOS virgl + image rebuild
C  kiosk ................ parallel with E
D  tkinter removal ...... parallel with E
```

E is first because A and B are both downstream of it and because it is the
piece that is closest to done — five patches written, one binary already
built, and the two hardest unknowns now measured rather than assumed.
