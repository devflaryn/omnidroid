# Omnidroid — Kiosk Game Launcher & Multi-Account Manager: PLAN

**Status:** draft for approval — no code has been written yet.
**Date:** 2026-07-05

## Confirmed inputs

- Base image: Bliss OS 16.9.7 (Android 13, x86_64) qcow2, **with libndk ARM translation** — the game is ARM-only and runs through the native bridge. Preserving this is a hard constraint on every step.
- `base.qcow2` currently in this project folder is **already a copy** (original stored elsewhere). It still gets treated as our immutable "base-v1": never booted writable, never modified in place.
- Game uses **its own account system** (no Google sign-in) → Google Play Services is **not required for login**. We keep GApps in the base for now anyway (removing it is a later RAM-optimization phase, and stripping it carelessly can destabilize the image).
- Host: **32 GB RAM** Windows 11 (QEMU 11.0 + adb 36.0 already installed and verified). Linux target later, same project.
- Manager v1: **simple CLI** (Python).

## Open questions (answer whenever — they don't block Phase 0–1)

1. **Your current known-good QEMU command line** for booting this image on Windows. I need it to inherit your working display/accel flags (especially graphics: `virtio-vga-gl` vs software rendering) instead of guessing.
2. Does the Bliss install have **root (su)** available? (I can check via adb in Phase 1 — affects how the in-guest shutdown works; there's a host-side fallback either way.)

---

## 1. Software to install

### Windows (mostly done already)

| Software | Version | Status / how |
|---|---|---|
| QEMU | 11.0 (installed) | ✅ Already installed. Note: it's a dev snapshot (`v11.0.0-12631-g...`); if we hit oddities, install the latest **stable** release from https://qemu.weilnetz.de/w64/ |
| adb (platform-tools) | 36.0 (installed) | ✅ Already installed |
| Windows Hypervisor Platform | Windows feature | Needed for `-accel whpx` (hardware acceleration). Enable: *Settings → Optional Features → More Windows features → Windows Hypervisor Platform*, or `dism /online /enable-feature /featurename:HypervisorPlatform /all` then reboot |
| Python | 3.12+ | `winget install Python.Python.3.12` — the manager CLI is Python |
| JDK | 17 (Temurin) | `winget install EclipseAdoptium.Temurin.17.JDK` — required to build the launcher APK |
| Android command-line tools | latest (or Android Studio) | For building the kiosk APK: `sdkmanager "build-tools;34.0.0" "platforms;android-33"`. Android Studio is fine too if you prefer an IDE |

### Linux (when we get there; Ubuntu 24.04 assumed — adjust per distro)

| Software | Version | How |
|---|---|---|
| QEMU + tools | ≥ 8.2 (distro) | `sudo apt install qemu-system-x86 qemu-utils` |
| KVM | kernel built-in | `sudo apt install cpu-checker && kvm-ok`; add user to `kvm` group |
| KSM tuning | ksmtuned | `sudo apt install ksmtuned` (or manager writes `/sys/kernel/mm/ksm/*` directly) |
| adb | android-tools | `sudo apt install android-tools-adb` |
| Python | 3.12 | distro default |

The manager code is one Python codebase; platform differences (accel flags, KSM, paths, sockets) live in a small platform layer.

---

## 2. Architecture

### Disk layout — the key design decision

Each account gets **two disks**, not one:

```
images/                          <- OUTSIDE the git repo (large binaries)
  base-v1.qcow2                  <- immutable, shared by all accounts (today's base.qcow2)
  base-v2.qcow2                  <- future updates; old versions kept until no account uses them

accounts/<name>/
  system.qcow2                   <- qcow2 OVERLAY backed by base-vN  (disposable, recreatable)
  data.qcow2                     <- standalone disk holding Android /data (precious, independent)
  account.json                   <- ports, base version, game package, state
```

- `system.qcow2` is created with `qemu-img create -f qcow2 -b base-vN.qcow2 -F qcow2 system.qcow2` — costs ~0 bytes until written. It absorbs stray system-partition writes and is **throwaway**.
- `data.qcow2` is a small independent disk mounted as Android's `/data` (Bliss/Android-x86 supports pointing `/data` at a partition via the `DATA=` kernel parameter / labeled data partition). All game accounts, logins, and settings live here.

**Why two disks:** qcow2 overlays are corrupted if their backing file changes. By keeping the precious data OFF the overlay chain, a base update = "recreate the cheap system overlay against base-v2" while `data.qcow2` is untouched. This is what makes requirement #5 safe (details in §5).

### The manager (host side)

Python CLI, working name `omni`:

```
omni create <name>            # make overlay + data disk + account.json
omni start <name> [--dev]     # boot instance: free ports, QEMU flags, adb connect
omni stop <name>              # graceful ACPI shutdown via QMP, then hard-kill fallback
omni list                     # accounts + running state + RAM use
omni install <name> game.apk  # adb install into a running instance (dev mode)
omni update-base <new.qcow2>  # register base-v(N+1), migrate accounts (see §5)
```

Per instance the manager assigns: an adb port (host 5555+i forwarded to guest 5555 via QEMU user networking — adb-over-TCP enabled once in the base), and a QMP control socket (QEMU's JSON control channel) for clean shutdown and monitoring. It also runs a **watchdog**: polls `adb shell pidof <game>`; when the game process is gone (and the guest didn't shut itself down), it powers the instance off.

Windows accel: `-accel whpx,kernel-irqchip=off`. Linux: `-accel kvm` + `-machine mem-merge=on` (KSM sharing, default on) + manager enables KSM system-wide.

---

## 3. Removing every boot screen (without touching ARM translation)

All changes below are made in a **base-editing session**: boot a temporary overlay on top of the base, make changes, verify, then bless that overlay chain as the next base version (§5). The original base file is never opened read-write.

| Screen | How it's removed |
|---|---|
| **GRUB menu** | Edit the grub config inside the image: `timeout=0`, hidden menu style. We only change timeout/visibility — the menu entry's kernel line keeps **all existing Bliss flags untouched** (some Bliss boot options interact with hardware/features; we append, never remove). |
| **Kernel/initrd console text** | Append `quiet loglevel=0 vt.global_cursor_default=0` to the existing kernel command line. |
| **Android boot animation → your custom loading screen** | Replace `bootanimation.zip` (in `/system/media` or `/product/media`) with a custom one built from your PNG frames (`desc.txt` + frame folders, zip with **store** compression). This is the sanctioned hook for a custom loading screen — plays from early boot until the launcher is up. |
| **Lock screen** | One-time in base: `adb shell locksettings set-disabled true` (+ `settings put secure lockscreen.disabled 1`). |
| **Launcher / taskbar / status bar** | Our own kiosk APK becomes the **only HOME app** (Bliss launcher + taskbar packages disabled via `pm disable-user`). The kiosk app runs fullscreen-immersive on a black background. Phase 2 hardening: register it as **device owner** (`dpm set-device-owner`) and use Android's Lock Task ("kiosk") mode, which hides the status bar and blocks system gestures at the OS level. |

**Why ARM translation survives:** libndk lives in `/system` libraries plus `ro.dalvik.vm.native.bridge` system properties. Everything above is a config/media/userspace change — we never reflash or rebuild `/system`, never touch native-bridge props or `libndk_translation` files, never remove ARM support pieces. Additionally, **every phase ends with the same regression check**: `getprop ro.dalvik.vm.native.bridge` reports libndk, and the ARM game (or a small ARM-only test APK) installs and runs.

---

## 4. Kiosk launcher APK design

Small Kotlin app (`com.omni.kiosk`), minSdk 26 (Android 8+ per your game), one fullscreen activity, black background.

- **HOME app:** intent filters `MAIN` + `HOME` + `DEFAULT` → Android boots straight into it; no launcher/taskbar exists anymore.
- **On start/resume:** read the configured game package (a config file the manager pushes via adb; in dev mode, "any user-installed app" counts). If installed → `startActivity(launchIntentFor(game))` immediately. If not → stay on the black screen showing **"no apk found"**.
- **New APK via adb:** listens for `PACKAGE_ADDED` broadcasts → launches the new package the moment installation completes.
- **Game-closed detection:** because the kiosk is HOME, when the game exits or crashes, Android returns to the kiosk → its `onResume` fires with state "game was running". That triggers shutdown. Guard: if the game ran under ~15 s (crash loop), show an error screen instead of relaunch/shutdown-looping.
- **Shutdown path (two layers):**
  1. In-guest: kiosk invokes shutdown (`su -c svc power shutdown` if the image has root; or via device-owner privileges). QEMU is started with `-no-reboot`-style semantics so guest poweroff ends the process.
  2. Host fallback: the manager's watchdog notices the game pid is gone / guest halted and issues QMP `system_powerdown`, then kills QEMU after a timeout. So shutdown works even if the in-guest path is unavailable.

Dev vs production: dev mode = game installed per-account via `omni install` (freely uninstall/reinstall). Production = game baked into the base as a system app (`/system/priv-app` or `/product/app` with its ARM native libs intact — installed through a base-edit session, again without touching the bridge).

---

## 5. Base updates without corrupting overlays

Rules that make this safe:

1. **Bases are immutable and versioned.** `base-v1.qcow2` is never written after accounts exist on it.
2. **To build v2:** create a fresh overlay on v1 → boot it → apply updates (Bliss update, game update, tweaks) → verify ARM translation + game launch → flatten it (`qemu-img convert` merges base+overlay into a standalone `base-v2.qcow2`).
3. **To migrate an account:** delete/recreate its cheap `system.qcow2` overlay pointing at v2. Its `data.qcow2` (all logins, saves, settings) is not on the overlay chain, so it survives untouched. `account.json` records which base version each account uses, so migration can be gradual and reversible (v1 stays on disk until nobody references it).
4. `omni update-base` automates 2–3 and refuses to delete a base that any account still references.

This is exactly "update the base once, every account gets it" — with zero risk to account data because data was never stored in the overlay.

---

## 6. RAM / storage estimates (32 GB host)

Per instance, before any RAM-optimization phase: Bliss 13 idle uses ~1.2–1.8 GB; we allot **2 GB** per VM (3 GB in dev for comfort) + ~0.3 GB QEMU overhead.

| | Windows 11 + WHPX | Linux + KVM + KSM |
|---|---|---|
| Effective RAM per instance | ~2.3 GB (no page sharing) | ~1.3–1.7 GB after KSM warms up (identical Android system pages merged across instances; 30–50 % savings is typical for clone VMs) |
| Usable host RAM (leave ~6 GB for OS/desktop) | ~26 GB | ~28 GB (server, lighter desktop) |
| **Realistic concurrent instances** | **~9–11** | **~16–20** (more after the later low_ram/zram/service-trim phase — 25+ is plausible) |

Storage: base ~6.2 GiB **shared once**; per account, system overlay typically 0.2–1 GiB + data disk (dev mode with game installed per-account: ~1–3 GiB depending on the game; production with game in base: ~0.1–0.5 GiB of settings/saves). So 50 stored accounts ≈ 6 GiB + 50 × (their data), not 50 × 6 GiB.

These are estimates; Phase 1 includes measuring the real idle footprint of your image and revising this table.

---

## 7. Ordered task list (execute together, copy-first at every step)

**Phase 0 — Housekeeping (10 min)**
1. Create `images/` outside the project (e.g. `C:\Users\berat\OmniImages\`), move `base.qcow2` there as `base-v1.qcow2`.
2. Create `configs/paths.json` pointing at it; `git init`; `.gitignore` for `*.qcow2`, `*.img`, `accounts/`.

**Phase 1 — Prove the pipeline on a throwaway copy**
3. Create a throwaway overlay on base-v1; boot it in QEMU with WHPX using your current known-good flags (→ open question 1).
4. Verify: adb connects; `getprop ro.dalvik.vm.native.bridge` shows libndk; install the game APK; it runs. Record baseline RAM use and boot time. Check for root (→ open question 2).

**Phase 2 — Manager skeleton (Windows)**
5. `omni create/start/stop/list` with overlay creation, port allocation, QMP shutdown, adb auto-connect. Two accounts booting side by side.

**Phase 3 — Per-account data split**
6. Add `data.qcow2` per account + `DATA=` boot param (this is the step most specific to Bliss internals — tested on throwaway copies until right). Verify: two accounts log into two different game accounts, settings persist independently, deleting a system overlay loses nothing.

**Phase 4 — Silent boot + custom loading screen (base-edit session → base-v2)**
7. GRUB timeout 0 + quiet kernel flags; custom `bootanimation.zip` from your artwork; lock screen disabled. Regression check: ARM game still runs.

**Phase 5 — Kiosk launcher APK**
8. Build `com.omni.kiosk`, install as HOME, disable Bliss launcher/taskbar. Test the full loop: boot → loading screen → game auto-launch → close game → instance powers off. Test "no apk found" + adb-install-triggers-launch (dev mode).

**Phase 6 — Base update pipeline**
9. Implement `omni update-base`; prove an existing account keeps its login/settings across a base update.

**Phase 7 — Production mode**
10. Bake the game into the base as a system app; verify per-account logins still isolate via data disks.

**Phase 8 — Linux port**
11. KVM flags, KSM enablement, multi-instance load test; measure real KSM savings and update §6 numbers.

**Phase 9 (later, as you specified) — RAM optimization**
12. `ro.config.low_ram`, zram tuning, disabling unneeded services/GApps — each behind the same ARM-translation regression check.

---

## Approved amendments (2026-07-05)

1. **Canonical Windows QEMU profile** (user's exact working command — display/accel flags inherited verbatim, only additions allowed):
   `qemu-system-x86_64 -machine q35,accel=whpx,kernel-irqchip=off -cpu qemu64 -smp 4 -m 4096 -drive file=<disk>,format=qcow2,if=virtio -device virtio-vga -display sdl -device qemu-xhci -device usb-kbd -device usb-tablet -netdev user,id=net0 -device virtio-net-pci,netdev=net0`
   Additions per instance: `hostfwd=tcp:127.0.0.1:<adb_port>-:5555` on the netdev, a QMP socket, and the second (data) drive. Linux profile: identical except `accel=kvm` + KSM enabled; never swap GPU (`virtio-vga`) or display backend semantics.
2. **Root is unknown** → detect via adb in Phase 1. Shutdown must work both ways:
   - *With root:* kiosk app runs `su -c reboot -p` / `svc power shutdown` when the game closes.
   - *Without root:* **host watchdog (concrete design):** manager polls `adb shell dumpsys activity activities` every ~2 s and parses the resumed/top activity package. State machine: `WAITING_FOR_GAME` → (game becomes top) → `GAME_RUNNING` → (game no longer top AND process absent per `pidof`) → send QMP `system_powerdown` → if QEMU still alive after 20 s grace → QMP `quit`/kill.
3. **/data-on-second-disk is a Phase 1 GATE.** Prove Bliss mounts `/data` from a second virtio disk (`DATA=` kernel param or equivalent) before building anything on the two-disk design. If it does not work cleanly: STOP and present the fallback (data-in-overlay + `qemu-img rebase` on base updates) for an explicit decision.
4. **All instance-count/RAM figures are placeholders until measured.** Early phase: boot one instance with the real game running, record actual guest+QEMU memory use, recompute Windows and Linux+KSM capacity for the 32 GB host.
