# Omnidroid — Handoff / Resume Brief

## How to resume (read in this order, before doing anything)
1. **HANDOFF.md** (this file) — the whole picture.
2. **PLAN.md** — original plan + per-phase results/decisions.
3. **CHANGELOG.md** — what each base version and manager change delivered.
4. **git log** — commit-by-commit history.

Then: `python manager/omni.py list` and `python manager/omni.py bases` to see
live state. The repo is self-describing; you do NOT need the prior chat.

---

## What this project is
A **kiosk game-launcher + multi-account manager** on top of a **Bliss OS
16.9.7** image (Android 13, x86_64, with **libndk ARM translation** so an
ARM-only game runs on x86), run in **QEMU on Windows**. Each "account" is an
isolated Android instance that boots straight into a single game (silent
boot → custom loading screen → game), fully locked down (no status bar, no
launcher, no escape), and powers off when the game closes. Shipped as a
single **`omni.exe`** that **auto-downloads a portable QEMU on first use**
(QEMU is NOT bundled in the exe, NOT installed globally).

**Test game:** Roblox (`com.roblox.client`, arm64-v8a only) at
`C:\Users\berat\Downloads\roblox.apk`. It uses its own account system (no
Google sign-in), but **GApps/GMS are kept** (it may use Play Integrity).

## Environment / paths
- Host: Windows 11, **i7-13700F (16c/24t), 31.8 GB RAM**. QEMU 11.0 (dev
  snapshot) on PATH; adb (platform-tools) on PATH; Python 3.14; JDK 21 +
  Android build-tools 36 (for the kiosk APK).
- **Disk images live OUTSIDE the repo** at `C:\Users\berat\OmniImages\`
  (`base-vN.qcow2` + `base-vN.kernel` + `base-vN.initrd.img`, plus
  `data-template-8g.qcow2`). `*.qcow2/*.img` are gitignored and must never
  be committed.
- Repo: `C:\Users\berat\Desktop\Omni Apps\omnidroid\`.
  - `manager/omni.py` — the manager (the whole product logic).
  - `launcher/` — the kiosk APK source (`com.omni.kiosk`) + Gradle-free
    `build.ps1` (aapt2→javac→d8→apksigner). Output `launcher/build/omni-kiosk.apk`.
  - `configs/paths.json` — images dir, base registry, `current_base`,
    `base_game`, qemu defaults.
  - `tools/` — `make_bootanimation.py` (STORED-zip packer),
    `gen_placeholder_frames.ps1` (placeholder loading animation).
  - `assets/loading/` — loading-screen frames + `bootanimation.zip`.
  - `build-exe.ps1` — builds `dist/omni.exe` (PyInstaller onefile).
  - `accounts/` (gitignored) — per-account overlay+data+state.

## Architecture
- **Two-disk design (the core invariant).** Each account = a cheap
  `system.qcow2` **overlay** on the shared immutable base + an **independent
  `data.qcow2`** holding Android `/data` (all logins/settings/installed
  apps). Base updates recreate ONLY the disposable overlay; **`data.qcow2`
  is NEVER touched**, so per-account data always survives. `/data` is mounted
  from the 2nd virtio disk via the `DATA=vdb` kernel param.
- **Direct kernel boot** (`-kernel/-initrd/-append`, files extracted per
  base) — **no GRUB**, no bootloader menu; the manager sets per-account
  kernel params. Data disks must be pre-formatted ext4 (Bliss initrd mounts
  but never formats `DATA=`), hence the `data-template-8g.qcow2`.
- **Silent boot (production profile, all host-side):** `-vga none` +
  `virtio-gpu-pci` (no VGA text device → no SeaBIOS/iPXE firmware text) +
  `console=null` (discards the Bliss initrd script text) +
  `quiet loglevel=0`, NIC `romfile=` (no iPXE ROM). Black from power-on to
  the loading screen. Dev/builder boots use `virtio-vga` + serial for
  debugging and run **headless** (no window) so a dexopt-busy window can't
  look frozen.
- **Kiosk APK = system HOME app** (`/system/app/OmniKiosk`). Auto-launches
  the game on boot, shows "no apk found" on black if absent, launches a
  newly adb-installed APK immediately, sets a solid-black wallpaper, and
  drives **Lock Task Mode**: as **device owner** (`dpm set-device-owner`
  during provisioning) it whitelists [kiosk, game], `setLockTaskFeatures(NONE)`,
  `setStatusBarDisabled(true)`, `startLockTask()` — status bar, Quick-Settings
  pull-down, notifications, and nav gestures are fully blocked while the game
  runs. (Immersive mode alone only hides the bar — it swipes back.)
- **Shutdown watchdog (host-side is the decider).** `omni watch` polls
  `pidof <game>` via adb; state machine WAITING→RUNNING→GRACE→shutdown. Only
  **process death** (gone for `--grace` secs of consecutive polls) triggers
  shutdown — NOT foreground changes (ads/dialogs/loading keep the process
  alive). Shutdown = in-guest `svc power shutdown` (KernelSU root) →
  QMP `system_powerdown`/`quit` → kill. Never relies on the guest.
- **Provisioning (`provision_settings`, per-`/data`, idempotent):** disables
  lock screen, marks setup complete, suppresses immersive confirmation,
  reverts any stale `/data` kiosk override (`uninstall-system-updates`), sets
  kiosk as HOME + disables Bliss launchers, sets device owner (lockdown),
  sets black wallpaper, sets the game package, disables 23 unneeded apps
  (`TRIM_PACKAGES`: v5's 7 + 16 Tier-1 trims of 2026-07-06) + zeroes
  animations (RAM trim), disables the setup wizard. Existing accounts get
  new trims on their next re-provision (`update-all`/`update-base`).

## Base versions (immutable, versioned, self-contained after flatten)
| Base | Contents | Role |
|---|---|---|
| v1 | raw Bliss 16.9.7 + libndk | archival |
| v2 | v1 + custom loading screen + kiosk system app | superseded |
| v3 | v2 + black wallpaper + host-side silent boot | superseded (dev lineage) |
| **v4** | v3 + Roblox pre-installed as `/system/app` (libs extracted) | **PRODUCTION** (old kiosk, pre-lockdown) |
| **v5** | v3 + Lock-Task kiosk + RAM trims | **current DEV base** (no baked game) |

- `current_base` = **v5** (dev). `omni bases` lists them; `omni use-base <tag>`
  sets the default for NEW accounts (dev v5 vs a production base).
- **Dev vs production:** dev base (v5) has the kiosk but no game — install
  per-account via `omni install` (game lives in that account's `/data`).
  Production base has the game baked as `/system/app` (a `/system/app` APK
  needs its native `.so` libs extracted into `lib/<abi>` or an ARM game
  crashes at load — `rebuild-base` does this automatically; libndk translates).
- **v4 is production but on the OLD (pre-lockdown) kiosk.** To get a
  production base WITH the lockdown: `omni rebuild-base --game <apk>` (it
  builds on current=v5, so the new production base inherits the lock-task
  kiosk), then `omni update-all`.
- **Current fleet:** accounts alice, bob, charlie, dave, erin — all on **v5**,
  all DeviceOwner (lockdown active), data preserved through every migration.

## omni CLI (identical whether `python manager/omni.py …` or `omni.exe …`)
Instance lifecycle:
- `create <name>` — new account on current base; provisions (headless first
  boot). Ex: `omni create alice`
- `start <name> [--mode ...] [--gpu ...] [--mem MB] [--headless] [--wait] [--dev]`
  — detached boot; returns immediately (`--wait` blocks). Ex:
  `omni start alice --mode playable`
- `resume <name>` — attach to a running instance, wait for boot, run checks.
- `stop <name>` — graceful shutdown chain (adb → QMP → kill).
- `list [--stats]` — accounts, base, ports, running PID (+ RAM with --stats).
Apps / control:
- `install <name> <apk>` — adb-install a game into `/data` (dev), record +
  set it as the kiosk's target. Ex: `omni install alice roblox.apk`
- `run-app <name> <pkg>` — launch a package.
- `watch <name> [--grace N] [--package P]` — host shutdown watchdog.
- `adb <name> -- <args...>` — arbitrary adb against that instance. Ex:
  `omni adb alice -- shell getprop ro.dalvik.vm.native.bridge`
- `kioskify <name> [--apk ...]` — (legacy) install kiosk into a running
  instance + set HOME. Prefer baking via the base now.
Bases / rollout:
- `bases` — list bases + current + per-base pre-installed game.
- `use-base <tag>` — set default base for new accounts (dev/prod switch).
- `update-base <name> [--to vN] [--no-reprovision]` — migrate ONE account's
  overlay to a base, keep its data, re-provision.
- `update-all [--to vN] [--skip-current]` — migrate ALL accounts (data kept).
- `rebuild-base --game <apk>` — bake/replace the pre-installed game as a
  `/system/app` system app in a NEW base version (extracts native libs),
  make current. Then `update-all` rolls it out. Ex:
  `omni rebuild-base --game roblox.apk`
- `update-kiosk [--apk ...]` — ship a new kiosk launcher in a NEW base
  version (uses `launcher/build/omni-kiosk.apk` by default). Then `update-all`.
QEMU / platform:
- `qemu-info [--install]` — show resolved QEMU path / trigger auto-install.
- `start --accel <str>` — override the auto-detected hypervisor
  (Windows→`whpx,kernel-irqchip=off`, Linux→`kvm` + `-machine mem-merge=on`
  so KSM can dedup guest pages).
- `ksm [status|on|off] [--aggressive]` — Linux KSM control via
  `/sys/kernel/mm/ksm/*` (prints stats + MB deduped). Clean no-op message
  on Windows.
- `bench-ksm [--mode brutal] [--floor-mb N] [--apk ...]` — Linux-only
  Phase 8 measurement: adds identical headless instances one at a time,
  waits for `pages_sharing` to plateau, records the **marginal drop in
  host MemAvailable** per instance (RSS double-counts KSM-shared pages).
  Stops at the RAM floor, never a count cap. **Scaffold — first run on
  the future Linux box is its test.**

## Performance modes (`--mode`, per-instance; counts NEVER capped)
- **playable** (default): VirGL (`virtio-gpu-gl` + `-display sdl,gl=on`),
  4 GB, 4 vCPU — **correct colors + GPU accel**, smooth, few instances.
- **hard**: software rendering (`virtio-gpu-pci`), 3 GB, 4 vCPU — more
  instances; **host window shows R/B swapped** (use `--gpu virgl` to fix).
- **brutal**: **headless** (no window), software, 2 GB, 2 vCPU — max instances.
- Overrides: `--gpu virgl|software`, `--mem MB`, `--headless` (any mode).
- **VirGL graceful fallback:** if host GL init fails, `start` detects the
  immediate QEMU exit and relaunches in software automatically.

## Color fix (settled)
The R/B swap (blue↔orange) is in QEMU's **software** virtio-gpu→SDL blit on
this build; gralloc/display/device tweaks don't fix it (one breaks boot).
**VirGL fixes it** (host OpenGL) AND adds GPU accel — baked into `playable`.
So: **correct colors under VirGL/playable only.** Software modes still swap,
but `brutal` is headless (no window) so it's moot. A stable QEMU build would
likely fix software too (deferred).

## Honest host limits (measured, RAM-bound; CPU never the limit)
- Per software instance w/ Roblox running: **~3.2 GB resident** (the game
  uses ~1.9 GB, so lowering `-m` below ~3 GB gives little and risks OOM).
- **Safe concurrent software instances before "Display output is not active"
  starvation:** ~**4–5** with your normal apps open (VALORANT/Discord/Opera/
  WSL → ~13–19 GB free); ~**7–8** with them closed (~25–27 GB free).
- Rule: keep ~2 GB OS headroom; stop when free RAM nears ~3 GB.
- v5 RAM trims save **~285 MB/instance** (~2289→~2004 MB guest with Roblox).
- **Tier-1 trims round 2 (2026-07-06): −119 MB more guest-used**
  (1789→1670 MB, fixed T+480 s protocol, Roblox at login; 206→191 guest
  processes). **Host RSS did NOT move** (3195 MB at `-m 3072`): on WHPX
  the guest page cache expands into freed RAM, so QEMU touches ~its full
  allocation regardless. Guest trims = in-guest headroom (safer at
  brutal's 2 GB), NOT host RAM. Host-side levers: lower `-m`,
  virtio-balloon (proposed, unapproved), or Linux+KSM.
- **Boot time ~35 s cold, unchanged by trims** (trimmed apps aren't on the
  boot-critical path — they save RAM, not boot time).

## Dev / testing harness (scriptable, headless, JSON)
- `omni test-apk <name> --apk <apk> [--mode hard] [--window] [--reuse]` —
  one-shot: FRESH v5 dev session (kiosk, no baked game) → headless boot →
  install → kiosk auto-launches → emit ONE JSON line:
  `{account, base, mode, package, installed, launched, foreground, pid,
  adb_port, qmp_port, adb_serial, ok}`.
- `omni screenshot <name> [--out path]` — framebuffer PNG (true colors, works
  headless); prints JSON `{ok, path}`.
- `omni logcat <name> [--tag T] [--clear]` — read/clear guest logcat.
- Agent pattern: run `test-apk` → parse JSON → drive `screenshot`/`logcat`/
  `adb` against the reported `adb_serial`. Works from `omni.exe` identically.

## Settled decisions (do NOT re-litigate)
- **COLD BOOT every instance, every time — never snapshot/resume.** Chosen
  for stability over the ~35 s speed win. Snapshots rejected: stale in-game
  sessions, shared/duplicated device identity across restored instances, and
  brittleness across base/QEMU updates. ~35 s cold boot is accepted.
- VirGL for correct color (not a QEMU downgrade/upgrade right now).
- GApps/GMS kept (Play Integrity risk); GApps removal is out of scope.
- Provisioning/builder boots are headless by design.
- **On Windows/WHPX, package trims buy IN-GUEST headroom only — never
  host RAM or more instances** (guest page cache expands into freed RAM;
  QEMU touches ~its full `-m` and Windows shares nothing). Measured
  2026-07-06: −119 MB guest-used, host RSS unchanged. Host-RAM density
  is a **Linux/KSM concern — this is the documented reason the Linux
  port matters.** Consequently **virtio-balloon (Tier 2) is REJECTED**
  (user, 2026-07-06): lmkd-kills-the-game risk not worth it on a host
  where RSS won't drop anyway. Tier 3 (low_ram/zram-resize/telephony
  disables) stays documented-only. Don't re-propose these on Windows.

## HARD CONSTRAINTS (every change must honor)
1. **Never touch `/system` libraries or ARM native-bridge props**
   (`ro.dalvik.vm.native.bridge`, libndk files). Only media assets, the HOME
   app, `/data` settings, and host-side QEMU flags may change.
2. **Every base change ends with the regression check:**
   `getprop ro.dalvik.vm.native.bridge` == `libndk_translation.so` AND
   **Roblox actually launches and renders** (screencap).
3. **Preserve base/data isolation:** bases immutable once referenced;
   `data.qcow2` never touched on base updates; the original image is never
   booted writable.

## Open / optional items (nothing required)
- **Linux/KVM + KSM port — HOST-SIDE CODE PREP DONE (2026-07-06), hardware
  pending.** The manager is Linux-ready without a Linux host ever having
  run it: accel auto-detect (WHPX/KVM) + `--accel` override, `-machine
  mem-merge=on` on KVM, `/dev/kvm` preflight warnings, `omni ksm`,
  `omni bench-ksm` (measurement scaffold), per-platform `images_dir`
  (`configs/paths.json` maps windows→`C:/Users/berat/OmniImages`,
  linux→`~/OmniImages`), KSM-aware `list --stats`. Windows verified
  unaffected (QEMU cmdline byte-identical; fresh-account regression run).
  Still TODO on real hardware: KVM+KSM bench, ARM-translation + full kiosk
  parity gates, cross-platform account portability test.
  **Honest expectation for the planned first Linux box (Ubuntu 24.04
  laptop, ~8 GB RAM): it PROVES the port works; it does NOT unlock scale.**
  ~6 GB usable after Ubuntu ≈ **3–4 brutal instances even with KSM** —
  FEWER than the ~7 the 32 GB Windows host runs. Scaling past Windows
  needs a high-RAM Linux machine later; don't oversell the laptop numbers.
- ~~RAM proposals~~ **DECIDED 2026-07-06: Tier 2 (virtio-balloon)
  REJECTED, Tier 3 documented-only** (see Settled decisions). Windows
  optimization work is CLOSED; host-RAM density comes from Linux/KSM.
- **Deeper boot-service trimming** — risky, low payoff (boot is dominated by
  system_server/zygote, not the trimmed apps). Only behind the regression check.
- **Production base with lockdown** — build via `rebuild-base --game` on v5
  when a production deployment is needed (v4 predates the lockdown kiosk).
