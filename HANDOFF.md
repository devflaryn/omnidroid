# Omnidroid — Handoff / Resume Brief

## How to resume (read in this order, before doing anything)
1. **HANDOFF.md** (this file) — the whole picture.
2. **PLAN.md** — original plan + per-phase results/decisions.
3. **CHANGELOG.md** — what each base version and manager change delivered.
4. **git log** — commit-by-commit history.

(**HOWTO.md** is the user/GUI-facing usage guide — full command
reference incl. the `--json` schemas the GUI depends on.)

Then: `python manager/omni.py list` and `python manager/omni.py bases` to see
live state. The repo is self-describing; you do NOT need the prior chat.

---

## What this project is
A **kiosk game-launcher + multi-account manager** on top of a **Bliss OS
16.9.7** image (Android 13, x86_64, with **libndk ARM translation** so an
ARM-only game runs on x86), run in **QEMU** — cross-platform: same CLI and
behavior on Windows (WHPX) and Linux (KVM+KSM); only packaging differs.
Each "account" is an isolated Android instance that boots straight into a
single game (silent boot → custom loading screen → game), fully locked
down (no status bar, no launcher, no escape), and powers off when the game
closes. **ALL instances run HEADLESS, always** (no host window; each
instance exposes a **localhost-only VNC attach point** on its vnc_port —
wired 2026-07-06). Shipped as **`omnidroid`**:
- **Windows `omnidroid.exe`** — fully portable: `setup` (or first use)
  downloads a **portable QEMU into ./qemu only**. Nothing is ever
  installed to the host system (no global install/registry/PATH).
- **Linux `omnidroid`** (ELF, built ON the Linux box via
  `build-linux.sh` — PyInstaller can't cross-build) — uses **system QEMU**
  (`sudo apt install qemu-system-x86 qemu-utils android-tools-adb`);
  `setup` preflights qemu / `/dev/kvm` / KSM with exact fix commands.

**Naming (updated 2026-07-07, supersedes the 2026-07-06 split):** the
engine/CLI is named **`omnidroid`** (formerly `qemu-manager` — renamed
at the user's request; no `qemu-manager` references should remain). A
separate GUI app (built in another session) drives this engine via the
`--json` CLI (see "GUI contract" below and **HOWTO.md**, the detailed
usage guide).

**Test game:** Roblox (`com.roblox.client`, arm64-v8a only) at
`C:\Users\berat\Downloads\roblox.apk`. It uses its own account system (no
Google sign-in), but **GApps/GMS are kept** (it may use Play Integrity).

## Environment / paths
- Host: Windows 11, **i7-13700F (16c/24t), 31.8 GB RAM**. QEMU 11.0 (dev
  snapshot) on PATH; adb (platform-tools) on PATH; Python 3.14; JDK 21 +
  Android build-tools 36 (for the kiosk APK).
- **Disk images live in `images/` inside the checkout** (committed;
  `.gitignore` un-ignores `images/*`). **Two canonical bases** (2026-07-09):
  - `base_x86.qcow2` + `base_x86.kernel` + `base_x86.initrd.img`
    (Bliss x86_64; renamed from `base-v5.*` — version now lives in the
    config entry's `version`/`changelog`, not the filename), plus
    `data-template-8g.qcow2` (formatted-empty ext4 /data template);
  - `base_arm.qcow2` + `base_arm_system.qcow2` + `base_arm_data.qcow2` +
    `base_arm_efivars.fd` (LineageOS 23.2 arm64/UEFI provisioned pair).
  The host architecture selects the base at runtime (x86_64 → x86/WHPX/KVM,
  arm64 Mac → arm/HVF); the other architecture's files sit in the same
  folder and are harmlessly ignored.
- Repo: `C:\Users\berat\Desktop\Omni Apps\omnidroid\`.
  - `manager/omni.py` — the manager (the whole product logic).
  - `launcher/` — the kiosk APK source (`com.omni.kiosk`) + Gradle-free
    `build.ps1` (aapt2→javac→d8→apksigner). Output `launcher/build/omni-kiosk.apk`.
  - `configs/paths.json` — images dir, base registry, `current_base`,
    `base_game`, qemu defaults.
  - `tools/` — `make_bootanimation.py` (STORED-zip packer),
    `gen_placeholder_frames.ps1` (placeholder loading animation).
  - `assets/loading/` — loading-screen frames + `bootanimation.zip`.
  - `build-exe.ps1` — builds `dist/omnidroid.exe` on Windows;
    `build-linux.sh` — builds `dist/omnidroid` ON Linux (two-build
    process; PyInstaller cannot cross-build).
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

## ARM64 / Apple Silicon (proof-of-life, 2026-07-08)
**Status: PROOF-OF-LIFE ONLY. `base_arm` NOT built yet** — that is its own
next session. Everything below is a Mac Mini (Apple Silicon, arm64) finding;
the entire x86 product above (bases v1–v5, accounts, manager) is **untouched**.
Goal of the eventual arm lane: a `base_arm.qcow2` running **arm64 Android
natively under HVF with NO translation layer** (the target app is arm64-native,
so libndk is irrelevant here), carrying the same kiosk features as x86.

- **Result: arm64 Android boots fully under QEMU/HVF on this Mac.** Reached a
  booted, provisioned home screen; verified over adb: `ro.product.cpu.abilist=
  arm64-v8a`, `uname -m=aarch64`, Android 16, kernel Linux 6.12.81 under `-accel
  hvf -cpu host`. **No translation layer involved** — arm64 guest on arm64 host.
- **Viable base source:** **`jqssun/android-lineage-qemu`** — LineageOS **23.2**
  (Android 16), **arm64-v8a**, the **`virtio_arm64only`** build. Prebuilt qcow2,
  actively maintained, purpose-built for QEMU `virt` + HVF, documents a Magisk
  root path. Release asset used: `UTM-VM-lineage-23.2-*-virtio_arm64only.zip`
  (unzips to `LineageOS_on_arm64.utm/Data/` = `vda.qcow2` + `vdb.qcow2` +
  `efi_vars.fd`). Repo: https://github.com/jqssun/android-lineage-qemu
  (alternatives evaluated & ranked lower: AOSP Cuttlefish — Google-official but
  macOS/HVF host support poorly documented; Bliss OS arm64 — x86/virgl-centric).
- **Working boot command/flags:** saved as **`tools/arm64/boot_arm64.sh`**
  (portable; takes the VM Data dir + optional VNC display N). Essence:
  `qemu-system-aarch64 -machine virt -accel hvf -cpu host -smp 4 -m 4096`,
  **EDK2 pflash** (`edk2-aarch64-code.fd`, ships with brew qemu) + `efi_vars.fd`,
  two **`virtio-blk-pci`** disks (vda=system, vdb=data — same 2-disk shape as
  x86), **`virtio-gpu-pci`**, **`-display none -vnc 127.0.0.1:N`** (headless +
  localhost VNC, same model as x86), **`hostfwd tcp:127.0.0.1:5555`** for adb.
  Toolchain: `brew install qemu android-platform-tools`; `sysctl kern.hv_support`=1.
- **Kiosk primitives — all confirmed present on the arm image:**
  - **Custom HOME app:** `cmd package set-home-activity` works; HOME reassignable.
  - **Device-owner / Lock Task:** `dpm set-device-owner`/`set-active-admin`
    present; features `android.software.device_admin` + `managed_users`; **0
    accounts** on device (the usual DO blocker); SELinux **Enforcing**.
  - **Root:** `adb root` is gated by LineageOS's `persist.sys.root_access`
    (shell can't set it; needs the su-addon dev-menu toggle) — use one of the
    two root paths below.
- **⚠️ Two build-time notes for the `base_arm` session (don't relearn these):**
  1. **Assign device-owner DURING first-boot provisioning, NOT after.** On this
     booted instance `device_provisioned=1`/`user_setup_complete=1` already, so
     a post-hoc `dpm set-device-owner` is blocked. Provision DO on a fresh/wiped
     base before setup completes (same as the x86 flow).
  2. **Pick a root path:** LineageOS **su-addon** (enables the "Root access"
     Developer-options toggle → `persist.sys.root_access`) **or** the
     maintainer's documented **Magisk boot-image patch** (`boot_arm64only.img`).
- **Quirk:** adbd on this image starts in **trade-in mode** and refuses `shell:`
  until the setup wizard is finished — complete first-boot setup (via VNC) before
  expecting `adb shell`.
- **HARD CONSTRAINT (arm lane):** VNC stays **localhost-only** here too — same
  no-auth-so-127.0.0.1-only rule as constraint #4 below.

### base_arm BUILD STATUS (2026-07-08, session 2) — WORKING kiosk, silent-boot/root PENDING
The arm base is **built, registered, and functional** for create/boot/kiosk/
lockdown/shutdown. What remains (silent boot, custom loading animation, root)
is blocked on read-only-system editing and is a **surfaced decision**, below.

- **images_dir (macOS `~/OmniImages`) holds the arm base set:**
  - `base_arm.qcow2` — pristine LineageOS 23.2 system (5 GiB virt), **shared
    immutable backing**.
  - `base_arm_system.qcow2` — provisioned overlay (~7 MB, backed by
    base_arm.qcow2). **Holds the `/metadata` FBE keys** — see matched-pair note.
  - `base_arm_data.qcow2` — provisioned `/data` (~1 GB): kiosk installed,
    **device-owner set**, kiosk is HOME, lockscreen off, adb key authorized.
  - `base_arm_efivars.fd` — provisioned UEFI vars.
  - Config entry `bases.arm` (type `arm-uefi`); `current_base` stays `v5`
    (x86) — the arm base is chosen by **host arch**, not current_base.
- **FBE MATCHED PAIR (the load-bearing fact).** `/data` is file-based-encrypted;
  keys live in `/metadata`, a partition on the **vda system overlay**. So the
  system overlay and `/data` are ONE unit: a fresh overlay + provisioned `/data`
  → `init_user0_failed` (recovery); a truncated `/data` copy →
  `set_policy_failed:/data/misc`. An account therefore **copies the provisioned
  trio** (system overlay + data + efivars); the overlay keeps its qcow2 backing
  to the shared base_arm.qcow2, so only the ~1 GB `/data` + tiny overlay are
  per-account. Capture templates only from a **fully powered-off** guest (use
  `qemu-img convert` for /data) — copying while QEMU still writes corrupts it.
- **Device-owner provisioning recipe (no root needed):** boot fresh data →
  complete the first-boot wizard (adbd is in trade-in mode until then) →
  enable USB debugging + authorize adb (one-time, GUI) → `settings put global
  device_provisioned 0` → `dpm set-device-owner …/OmniDeviceAdminReceiver`
  (succeeds: 0 accounts) → set HOME + game + disable LineageOS launcher +
  lockscreen off. Bake this into the data template once; accounts just copy it.
- **VERIFIED via the engine** (`omni create armtest` → `start` → `install
  test_arm64.apk` → `stop`): boots to kiosk in ~15–40 s; kiosk is HOME and
  auto-launches Roblox; **swipe-down from the top does NOTHING** (Lock Task
  kills the status bar + Quick-Settings panel); Roblox renders arm64-native
  (~0.5–2% jank); `omni stop` powers off cleanly via `reboot -p`.
- **DEFERRED by user (2026-07-08): shipping base_arm as-is.** Phase C (silent
  boot, custom loading animation, root) is intentional future work, not an
  unfinished task — the base is shipped as a functional kiosk without them.
  When revisited, no approach was pre-chosen; the options are in the decision
  note below. Details of what's deferred and why:
- **Phase C (read-only-system edits) — the deferred work.** Today the
  visible boot is NOT silent: TianoCore UEFI splash → GRUB menu (8 s countdown)
  → scrolling kernel console → LineageOS boot animation → kiosk. Making it
  silent (edit grub.cfg: `timeout=0`, `quiet console=ttynull`, drop
  `console=tty0`), swapping the **custom loading animation**
  (`/product/media/bootanimation.zip`), and **root** all require writing the
  read-only vda. Blockers on this Mac: user build (no `adb root`; `adb root` is
  gated by `persist.sys.root_access`), and macOS has **no qemu-nbd/libguestfs**
  (`Kernel /dev/nbdN support not available`) to edit the qcow2 offline. Only
  on-macOS route: boot **LineageOS Recovery** (root context, shown in the GRUB
  menu) to mount partitions rw and edit grub.cfg + bootanimation (and/or install
  **Magisk** via the maintainer's `boot_arm64only.img` patch for runtime root).
  dm-verity/AVB is OFF ("AVB is not enabled" in dmesg), so edits won't trip
  verity. This is a real sub-project with brick risk — **do it in its own
  session once the user picks the approach.**
- **Build the kiosk APK on this host:** `launcher/build.sh` (macOS/Linux
  counterpart of build.ps1). Boot the base by hand with
  `tools/arm64/boot_arm64.sh <Data dir> [vnc N]`.
- **Live viewer:** `omni view <account> [--start]` opens a real-time window
  (screen + mouse + keyboard) on the account's localhost `vnc_port`, and
  returns the terminal immediately (viewer runs detached; output →
  `accounts/<name>/viewer.log`).
  - **Default = self-contained cross-platform viewer** (`manager/vncview.py`:
    Tkinter window + a minimal pure-Python RFB client — Raw/CopyRect/
    DesktopSize, 32bpp BGRX pixel format so colours are correct on any QEMU
    build). Identical on Windows/macOS/Linux; no OS screen-sharing app. Deps:
    tkinter + Pillow (`pip install pillow`; Linux also `apt install
    python3-tk`). The frozen exe bundles them via the `--hidden-import`
    flags in build-exe.ps1 / build-linux.sh (vncview is imported lazily by
    name, so those flags are REQUIRED for freezing). Verified against a live
    instance: framebuffer decode is pixel-identical to a QMP screendump, and
    injected pointer events reach the guest (`getevent` shows ABS_MT_*/
    BTN_MOUSE).
  - **`--native`** instead uses the OS/native client. macOS launches built-in
    **Screen Sharing.app BY PATH** (`/System/Applications/Utilities/Screen
    Sharing.app`) — NOT `open vnc://`, because the `vnc://` scheme is commonly
    hijacked by a third-party handler (RealVNC here) that silently opens the
    wrong app / nothing. `--viewer 'cmd {host}::{port}'` or config
    `qemu.vnc_viewer` (`{host}/{port}/{url}/{display}`) force a specific
    client (implies --native); Linux tries TigerVNC/remmina/gvncviewer,
    Windows vncviewer.exe then the shell handler.
  - No password anywhere (localhost, no auth — connect/proceed).

## CLI (identical: `python manager/omni.py …` == `omnidroid(.exe) …`)
Setup:
- **Blank-deployment bootstrap (2026-07-06):** the exe can be dropped
  into ANY folder — every command self-creates `configs/paths.json`
  (default template incl. `default_src`) next to it. A complete
  canonical `base_x86.qcow2/.kernel/.initrd.img` triple appearing in
  images_dir is **auto-registered on the next command** (tag `x86`;
  current_base = `x86` when unset), as are legacy versioned
  `base-vN.*` triples and the `base_arm` provisioned pair — this is
  the hook the future server download uses.
  With no usable base, every base-needing command (create/start/
  update-*/rebuild-base) exits CLEANLY with the exact copy-these-files
  message (never a traceback; `--json` gets `{"ok":false,"error":…}`).
- `setup` — first-run, idempotent. Windows: creates folders + downloads
  portable QEMU into ./qemu (self-contained, never touches the host
  system). Linux: creates `~/OmniImages`, preflights system QEMU /
  `/dev/kvm` / KSM with exact fix commands. JSON report + the exact
  missing-file list when the base isn't there yet.
- `doctor [--json]` — readiness check: per-file base/template presence
  (full missing paths), QEMU/adb resolution, `ready` verdict. Exit 0 =
  ready, 1 = not. The GUI can gate its UI on this.
Instance lifecycle:
- `create <name>` — new account on current base; provisions (headless first
  boot). Ex: `omni create alice`
- `start <name> [--mode ...] [--mem MB] [--accel A] [--wait] [--dev]`
  — detached HEADLESS boot; returns immediately (`--wait` blocks). Ex:
  `omni start alice --mode playable`
- `resume <name>` — attach to a running instance, wait for boot, run checks.
- `stop <name> [--timeout S]` — explicit POWER-OFF chain (adb → QMP →
  kill), every step hard-bounded; reports the `method` used. A viewer
  disconnect must NOT call this (see GUI contract).
- `remove <name> [--timeout S]` — **DESTRUCTIVE**: stop if running →
  delete `accounts/<name>/` (overlay + data.qcow2 + state) → ports
  freed. Guardrails: exact `[A-Za-z0-9_-]+` name only; resolved target
  asserted inside `accounts/` (structurally cannot touch bases/images).
- `list [--stats]` — accounts, base, ports (adb/qmp/vnc), running PID
  (+ RAM with --stats).
- **`--json` (create/start/stop/remove/list) — the GUI contract:**
  stdout carries EXACTLY one JSON payload (progress → stderr); errors
  become `{"ok":false,"error":…}` + exit 1. `start --json` returns
  pid + adb/qmp/vnc ports immediately. Full schemas in HOWTO.md §5.
- **Port scheme (invariant):** one shared index i per account →
  adb `16001+i`, qmp `17001+i`, vnc `18001+i` — **all three WIRED**,
  all bound 127.0.0.1 only. Ranges 1000 apart → the three can never
  collide below 1000 instances (also asserted at spawn). Old accounts
  are backfilled automatically on load.
- **VNC (wired 2026-07-06):** QEMU's built-in VNC server runs on every
  instance's vnc_port (QEMU display = port − 5900), **127.0.0.1 only,
  no auth** (safe ONLY because of the bind — HARD CONSTRAINT 4).
  Always on: an idle listener does no framebuffer encoding (measured
  host RSS unchanged). Instance stays `-display none`; VNC is an
  attach point, never a window. Note: on this QEMU dev snapshot the
  VNC framebuffer shows the known host-side **R/B swap** (guest/
  screencap are true-color; see Color note below).
- **GUI contract — disconnect vs shutdown:** a VNC/adb client
  disconnecting is a NO-OP; instances keep running headless (that is
  the default state, designed for hours-long unwatched runs). Power-off
  happens only via explicit `stop`/`remove` or the `watch` watchdog
  when the game closes.
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
- `update-all [--to vN] [--fast|--full] [--skip-current]` — migrate ALL
  accounts (data kept). **AUTO picks per account:**
  - **FAST (default when the base's game package is unchanged for that
    account, or the account has a dev-installed game):** discard + fresh
    overlay on the new base — a metadata-only qemu-img op, **no boot, no
    re-provision; 6 accounts measured in 0.2 s** (100+ ≈ seconds). This is
    how OS/game/kiosk updates ship (they live in /system → the overlay).
  - **FULL (auto when the base game package changes; force with
    `--full`):** boot + idempotent re-provision — required whenever
    provisioned **/data** state must change (kiosk target game, lockdown
    policies, TRIM_PACKAGES updates).
  - Never edit an existing base in place (corrupts overlays): every
    update = NEW immutable base vN+1, then repoint.
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
**ALL instances are HEADLESS, always** (`-display none`; no host window
anywhere — verified no code path opens one). Modes are pure RAM/CPU tiers:
- **playable** (default): 4 GB, 4 vCPU.
- **hard**: 3 GB, 4 vCPU — more instances.
- **brutal**: 2 GB, 2 vCPU — max instances.
- Override: `--mem MB`. (`--gpu`/`--headless` flags and the VirGL path
  were REMOVED with headless-always — a GL window can't exist.)
- View/control: `omni screenshot` (true colors) + adb, or any VNC
  viewer at `127.0.0.1:<vnc_port>` (18001+i, localhost-only, wired
  2026-07-06 — colors R/B-swapped on this QEMU build, see Color note).

## Color note
The old R/B swap was in QEMU's **software virtio-gpu→SDL window blit**
only. Guest rendering and `screencap`/screenshots were ALWAYS true-color.
**Confirmed 2026-07-06: the wired VNC path swaps too** — VNC framebuffer
mean RGB was exactly R↔B-mirrored vs screencap ground truth (46.4/51.8/
52.6 vs 52.6/51.8/46.4, Roblox login). Same host-side presentation bug
family on this QEMU dev snapshot; **cosmetic only** (input + view work).
Fix candidates if the GUI needs true color: stable QEMU build (flagged
since Phase 1) or swap channels in the viewer. NEVER touch gralloc
(guest is fine; one gralloc tweak even breaks boot).

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
- ~~VirGL for correct color~~ — MOOT since headless-always (2026-07-06):
  no host window exists; VirGL/`--gpu`/`--headless` flags removed.
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
4. **VNC stays localhost-only.** The per-instance VNC server has NO auth —
   that is safe ONLY because it binds 127.0.0.1. **Never bind VNC to a
   network interface without adding authentication (and preferably TLS/
   tunneling) in the same change** — a future "remote viewing" feature
   must not silently expose every instance's screen+input to the LAN.
5. **Destructive ops are confined to `accounts/`:** `remove` (the only
   one) must keep its structural guardrails — exact-name match, resolved
   path asserted under `accounts/`, never able to touch a base file or
   the images dir.

## Open / optional items (nothing required)
- **Server base updates (INTENDED FLOW — networking NOT implemented; the
  fast path was built to support it).** In production, omnidroid will
  detect an update on the user's server and download a new base qcow2.
  The local flow is already in place and verified:
  1. a new base triple (+ `.kernel`/`.initrd.img`) lands in the images
     dir (today out-of-band; later downloaded). Note: `rebuild-base`/
     `update-kiosk` still emit **versioned** `base-vN.*` files — bases
     are immutable while overlays reference them, so a rebuild can never
     overwrite `base_x86.qcow2` in place; promoting a build to the
     canonical versionless name is a release/rename step,
  2. it is AUTO-REGISTERED on the next command (2026-07-06; `src` from
     config `default_src`) — manual registration no longer needed,
     though `use-base` still switches the default explicitly,
  3. `update-all` — AUTO takes the FAST overlay-repoint for a pure
     system/game swap: **all accounts on the new base in seconds, no
     boots, data untouched** (measured 0.2 s for 6 accounts).
  Bases stay immutable: an update is always a NEW versioned file +
  repoint, never an in-place edit (in-place would corrupt every overlay).
- **QEMU delivery (DEFERRED decision, 2026-07-09).** The QEMU resolution +
  auto-install MECHANISM is done and proven: on Windows QEMU resolves ONLY
  from the product dir (config `qemu.dir` → `./qemu`), **never PATH/global**;
  if missing it downloads into `./qemu` with a hard socket + installer timeout
  and a clear error (never hangs). What is NOT decided is the *delivery
  source*. There is deliberately **no hardcoded download URL** anymore — the
  old pinned public URL (weilnetz) rotted and started 404ing, so
  `DEFAULT_QEMU_URL = None`; `ensure_qemu` reads `qemu.download_url` from
  config and, when unset, exits with an actionable message (populate `./qemu`
  or set the URL). **INTENDED production answer:** host a portable QEMU on the
  user's own server/CDN — the **SAME delivery path as the base-image
  download** above — so QEMU + base are one consistent "download from my
  server" story once that server exists. Until then: keep `./qemu` populated
  from a portable copy (the product-dir model works today). To wire it later,
  just set `qemu.download_url` (and/or ship QEMU inside the product); no code
  change needed.
- ~~Local VNC view/control~~ — **DONE 2026-07-06** (localhost-only, always
  on, R/B-swapped colors on this QEMU build — cosmetic; see Color note).
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
