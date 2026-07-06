# Omnidroid — Kiosk Game Launcher & Multi-Account Manager: PLAN

> **Resuming? Read `HANDOFF.md` first** (current state), then this file
> (history/decisions), then `CHANGELOG.md`, then `git log`.
> **Status (2026-07-06): Phases 0–7 complete + many extras; current base = v5.**
> This document is the original plan plus appended per-phase results; the
> "draft" line below is historical.

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

> **STATUS (2026-07-06): Phases 0–7 COMPLETE. Phases 8–9 optional/open.**
> Current base = **v5** (dev, Lock-Task kiosk + RAM trims). Full details in
> CHANGELOG.md and HANDOFF.md. Quick status per phase:
> - Phase 0 Housekeeping — ✅ DONE
> - Phase 1 Prove pipeline (ARM bridge, /data-on-2nd-disk gate, root) — ✅ DONE
> - Phase 2 Manager skeleton — ✅ DONE (detached start, watchdog, list, etc.)
> - Phase 3 Per-account data split — ✅ DONE (absorbed into Phase 2)
> - Phase 4 Silent boot + custom loading screen — ✅ DONE (→ base-v2; silent
>   boot later moved fully host-side in base-v3: -vga none/console=null)
> - Phase 5 Kiosk launcher APK (auto-launch, no-apk, close→shutdown) — ✅ DONE
> - Phase 6 Base update pipeline (`update-base`/`update-all`/`update-kiosk`) — ✅ DONE
> - Phase 7 Production mode (game baked as /system/app, libs extracted) — ✅ DONE (base-v4)
> - **Plus (beyond original plan):** black-wallpaper seam fix + host-side
>   silent boot (base-v3); performance modes playable/hard/brutal + --headless;
>   VirGL color fix; dev/test harness (test-apk/screenshot/logcat, JSON);
>   single `omni.exe` + auto-download portable QEMU; **Lock Task Mode
>   device-owner lockdown + RAM trims (base-v5)**.
> - Phase 8 Linux/KVM+KSM port — ⬜ OPEN (optional; for concurrency beyond ~7).
> - Phase 9 Deeper RAM/boot trimming (low_ram/zram/services) — ⬜ OPTIONAL
>   (app-level RAM trims already done in v5; deeper service trims are risky,
>   low payoff — boot is dominated by system_server/zygote).

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

---

## Phase 0 + Phase 1 results (2026-07-05) — COMPLETE

**Phase 0:** `base-v1.qcow2` moved to `C:\Users\berat\OmniImages\` (immutable). `configs/paths.json`, `.gitignore` (`*.qcow2` etc.), git repo initialized.

**Phase 1 verification results (all on throwaway overlay):**

| Check | Result |
|---|---|
| /data on second disk (**the gate**) | ✅ **PASSED** — `DATA=vdb` kernel param mounts `/dev/block/vdb` as `/data`; verified `mount` shows `/dev/block/vdb on /data type ext4`. Two-disk architecture is confirmed viable. |
| ARM translation | ✅ `ro.dalvik.vm.native.bridge=libndk_translation.so`, abilist `x86_64,arm64-v8a,x86,armeabi-v7a,armeabi`. Game-launch check pending (need game APK). |
| Root | ✅ Image has KernelSU; **`adb root` restarts adbd as root** (works on this user build). Manager gets root without any in-guest APK privileges. |
| Shutdown chain | ✅ `adb shell svc power shutdown` → guest powers off → QEMU process exits cleanly. (Plain ACPI/QMP `system_powerdown` is IGNORED by Android — manager must use adb shutdown first, QMP `quit` as fallback.) |
| adb over TCP | ✅ Guest adbd listens on 5555 by build default (`ro.adb.secure=0`, no auth); works via `hostfwd=tcp:127.0.0.1:PORT-:5555`. Survives blank /data. |
| **Direct kernel boot** | ✅ Kernel + initrd extracted to host (`work/kernel`, `work/initrd.img`). QEMU `-kernel/-initrd/-append "... SRC=/android-2024-10-11 DATA=vdb ..."` boots fine. **GRUB is bypassed entirely** — no GRUB menu to hide, and per-account kernel params (DATA disk) come from the manager, zero base-image edits. |
| Idle RAM (no game) | Guest uses ~1.5 GB of its 4 GB; QEMU host process ≈ 4.3 GB resident (full allocation) + ~0.2-0.5 GB overhead. Windows does not share pages between instances. Game measurement pending. |
| Storage after 1st boot | System overlay: 776 MiB. Data disk (8 G virtual): 478 MiB. Base shared: 6.17 GiB. |

**Architecture updates locked in from findings:**
1. Boot via `-kernel/-initrd/-append` (files in `images/` next to each base version, extracted once per base update). GRUB/bootloader screens no longer exist in the boot path. §3's GRUB row is obsolete.
2. Manager shutdown order: `adb shell svc power shutdown` → wait → QMP `quit`. `adb root` immediately after connect, always.
3. Serial console (`-serial file:` + `console=ttyS0` appended after `console=tty0`) stays in dev-mode boots for diagnosability; drop from production boots (keeps `quiet`).
4. First boot of a fresh data disk takes many minutes (first-boot dexopt, WHPX): manager must allow ≥15 min timeout for *first* boot of an account, ~2-4 min for subsequent boots.
5. WHPX quirks learned: QMP `screendump` returns garbage (don't trust it); warm reboots are suspect — manager should always cold-start instances.

---

## Phase 2 results (2026-07-05) — COMPLETE

**Manager:** `manager/omni.py` — `create / start / resume / stop / list [--stats] / install / run-app / adb`. QEMU spawns fully detached (PID+ports in `accounts/<n>/run.json`); `start` returns immediately (`--wait` opts in); `stop` = in-guest `svc power shutdown` → 90 s → QMP `quit` → kill. `list` verifies liveness by PID (ctypes OpenProcess — never `os.kill(pid,0)` on Windows, it terminates the target).

**Critical bug found & fixed:** the Bliss initrd only *mounts* the `DATA=` device — a blank disk leaves Android with no `/data` and it hangs before adbd (this, not WHPX, explained the "first boot hang"). Data disks are now copies of a formatted-empty ext4 template: `OmniImages/data-template-8g.qcow2` (1.5 MB). With a formatted disk, **first boot is ~2–3 min, not ~15** (dexopt runs in background after boot).

**Two accounts side by side, ARM game running (Roblox, arm64-v8a-only APK):**

| Metric | alice | bob |
|---|---|---|
| `/data` device | `/dev/block/vdb` ✅ | `/dev/block/vdb` ✅ |
| Native bridge | libndk ✅ | libndk ✅ |
| Game installed via adb + running foreground | ✅ `ActivityNativeMain` | ✅ `ActivityNativeMain` |
| Game PSS (login screen) | 963 MB | 957 MB |
| Guest RAM used (of 4096 MB) | 2289 MB | 2225 MB |
| QEMU host-resident | 4297 MB | 4298 MB |

**Measured capacity (replaces the void 2 GB guess):** on Windows a QEMU instance costs its **full `-m` allocation + ~0.2 GB** resident once the guest touches its pages (no page sharing on WHPX). Guest actually uses ~2.3 GB with the game at the login screen (more in real gameplay — remeasure in-game).
- **Windows, 32 GB host, `-m 4096` (current): ~6 concurrent instances** (≈27 GB usable ÷ 4.3 GB).
- Windows, `-m 3072` (looks safe at menu; validate in gameplay): ~8 instances.
- **Linux + KVM + KSM (projection, measure in Phase 8):** 30–40 % dedup of identical system/game code pages → effective ~2.6–3.0 GB/instance → **~9–11 instances** at `-m 4096`, 12–16 at `-m 3072`; more after the Phase 9 low_ram/zram work.

Phase 3 (per-account data split) was absorbed into Phase 2 — built into the manager and verified on both accounts. Login isolation (two different game accounts) needs the user's credentials: log in inside each window, then we confirm settings persist independently across restarts.

---

## Color-swap investigation (2026-07-05)

**Symptom:** QEMU window shows red↔blue swapped (user-confirmed visually). **Quantified:** built a pixel comparator (host window capture vs Android `screencap`, scoring only saturated pixels). Every host capture is a *clean* R/B swap (swapped-error ≈ 0 across 1300+ pixels on Roblox gameplay + home-selector icons) — not a true negative. Android's own framebuffer is always correct (screencap correct), so rendering/translation are fine; only QEMU's virtio-gpu → host presentation swaps.

**Tested (all on throwaway overlay), R/B swap PERSISTS in every case:**

| Config | Result |
|---|---|
| `-device virtio-vga -display sdl` (user's original) | swapped |
| `-display sdl,gl=on` | swapped |
| `-display gtk` | swapped |
| `HWC=drm_minigbm GRALLOC=minigbm_arcvm` | swapped |
| `GRALLOC=minigbm_gbm_mesa` | swapped |
| `-device virtio-gpu-pci` | swapped |
| `-vga std` | **display never inits** (Bliss has no std-VGA path; window stuck 720×400, adb offline) — not viable |

**Conclusion:** not fixable by Android gralloc/HWC flags, display backend, or virtio device variant. The swap is in **QEMU 11.0.50 (dev snapshot `v11.0.0-12631-g54e84cdc7a`) virtio-gpu presentation on Windows**. `virtio-vga` is *required* (only device Bliss drives), so we cannot switch it away. Most likely real fix = **stable QEMU build** (qemu.weilnetz.de/w64) — the Phase-1 note already flagged the dev-snapshot as a risk. Pending user decision on QEMU build vs deferring (swap is cosmetic to the SDL window; pipeline/game logic unaffected).

**DECISION (user, 2026-07-05): DEFER.** Continue to Phase 4/5; revisit color at the kiosk-display phase. Verification of colors meanwhile uses Android `screencap` (shows true colors) as ground truth, not the swapped host window.

---

## Phase 4 results (2026-07-05) — silent boot + custom loading screen — COMPLETE

**Silent boot** (all host-side, no image edits): GRUB already absent (direct kernel boot). Production boot append now adds `quiet loglevel=0 vt.global_cursor_default=0 SETUPWIZARD=0` (in `omni.py` `qemu_command`, non-dev branch). Dev boots keep serial console for diagnosis.

**Custom loading screen:** replaced `/system/media/bootanimation.zip` (system-as-root: `/` = `/dev/loop0` = system.img, ro; `mount -o remount,rw /` makes it writable, writes captured by the qcow2 overlay → will bake into base-v2). Original kept as `bootanimation.zip.orig`.
- Tooling: `tools/make_bootanimation.py` (packs frames → **STORED** zip, desc.txt first — deflate would silently fail to play), `tools/gen_placeholder_frames.ps1` (System.Drawing placeholder: rotating arc + pulsing "LOADING"). Placeholder art in `assets/loading/`; **user swaps `assets/loading/frames/part0/*.png` for their own art, re-runs the two tools.**
- **Verified true-color** via `screencap` of the live `bootanimation` binary: blue arc at two different rotation angles across frames (= it animates), correct colors (not R/B-swapped, because guest screencap is ground truth). Boot-time `bootanim` service ran clean (no zip errors in logcat).
- Note: adbd on WHPX only becomes reachable at ~boot_completed, so the boot-window animation can't be caught via adb screencap; the live-binary method is the reliable in-guest proof. Host-window view during real boot will show it R/B-swapped until the QEMU-build color fix.

**No lock screen:** `/data` settings (`locksettings set-disabled true`, `lockscreen.disabled=1`, `device_provisioned=1`, `user_setup_complete=1`). These are per-`/data`, so wired into the manager as `provision_settings()`, run once on each account's first boot (create/start/resume). Existing alice/bob need a one-time apply.

**ARM translation:** `ro.dalvik.vm.native.bridge=libndk_translation.so` still OK on the modified system (bootanimation is a media asset — no libs/props touched). Full game-launch regression deferred to Phase 5 on the flattened base-v2.

**base-v2 NOT flattened yet (deliberate):** the `work/base-builder-system.qcow2` overlay (on base-v1) holds the bootanimation change and is preserved. Phase 5 adds the kiosk APK to the same overlay, then it flattens to `base-v2.qcow2` once — avoids writing a 6 GB base twice.

---

## Phase 5 results (2026-07-05) — kiosk launcher APK — TESTS PASSED (pre-flatten checkpoint)

**Cleanup:** killed the leftover non-project QEMU `omniagent`/`overlay.qcow2` (PID 7096, user's own manual experiment) — freed **1.46 GB** and ~5311 s of accumulated CPU. That process WAS skewing earlier measurements slightly; real per-instance headroom is marginally better than recorded. alice/bob unaffected.

**Kiosk APK** (`launcher/`, `com.omni.kiosk`, ~12 KB, built Gradle-free via `launcher/build.ps1`: aapt2 → javac → d8 → apksigner; JDK 21 + build-tools 36.0.0). Registered as HOME (`MAIN`/`HOME`/`DEFAULT`), fullscreen immersive black. Reads game package from `Settings.Global omni_game_package` (manager sets it on `install`); dev fallback = first launchable non-system app **excluding a denylist** of base preinstalled apps (opencamera/termux/amaze/kernelsu/keymapper) — without the denylist it wrongly grabbed Open Camera.

**Manager additions:** `omni kioskify <name>` (install kiosk, `set-home-activity`, `pm disable-user` the 3 Bliss launchers), `omni watch <name> --grace N` (host watchdog), `omni install` now records package + pushes `omni_game_package`, and uses `--no-incremental` (Bliss rejects incremental sessions; adb was falling back to streamed with a scary trace).

**Four behaviors, all verified on account `charlie` (fresh, base-v1 + kiosk):**

| Requirement | Result |
|---|---|
| "no apk found" black screen when game absent | ✅ configured game not installed → centered "no apk found" on black |
| Auto-launch game on boot | ✅ cold boot → silent boot → loading screen → kiosk → **Roblox foreground & rendering** (screencap confirms arm64 game via libndk), zero intervention |
| Instantly launch a newly adb-installed APK | ✅ `omni install charlie roblox.apk` → kiosk logcat `PACKAGE_ADDED … launching com.roblox.client (new apk installed)` → foreground |
| Shut down when game closes | ✅ see edge test below |

**Shutdown-edge test — the critical distinction between "game closed" and "game blipped":**

The kiosk app NEVER decides shutdown. The **host watchdog** (`omni watch`) owns it, and it keys on **process death, never foreground**. State machine: `WAITING → RUNNING` (pidof game present) `→ GRACE` (pidof empty) `→ shutdown` only after `--grace` seconds of *consecutive* absence; any reappearance returns to RUNNING; adb hiccups count as "unknown" and never advance the grace timer.

- **Blip (must stay alive):** launched kiosk over the running game so the game fully **left the foreground** (top activity became `com.omni.kiosk`) while its **process stayed alive (pid 5331)**. Watchdog held `RUNNING`, never entered GRACE. Waited 25 s (> 20 s grace) → **instance still up** (`boot_completed=1`). Proves foreground change / dialog / ad / webview / loading does not trigger shutdown.
- **Real close (must shut down):** `am force-stop com.roblox.client` → pidof empty → log: `RUNNING → GRACE`, countdown `3/6/9/12/15/18/21s`, then `gone for 21s >= 20s - shutting instance down` → in-guest `svc power shutdown` → **QEMU exited clean**.

Grace default 20 s (tune per game via `--grace`; a heavy game with long black-screen transitions can go higher — but those keep the process alive anyway, so grace mainly covers crash-relaunch races).

**ARM translation:** `libndk_translation.so` present; the arm64-only Roblox launches and renders on the kiosk instance. Kiosk is a HOME app + media/settings only — no `/system` libs or bridge props touched.

**AWAITING USER REVIEW before flatten** (per instruction "stop and show shutdown-edge results before flattening"). Next: add kiosk to the base-builder overlay as the system default HOME, flatten overlay → `base-v2.qcow2`, then full ARM game-launch regression on a fresh account created on base-v2.

---

## base-v2 built + verified (2026-07-05) — Phase 4/5 COMPLETE

**Step 1 — "Viewing full screen" suppressed:** `provision_settings()` now also sets `secure immersive_mode_confirmations=confirmed` (per-`/data`, first boot). Confirmed absent on base-v2 run.

**Step 2 — base-v2 flattened:** into the base-builder overlay (which already held the Phase 4 loading screen) I installed the kiosk as a **regular `/system/app`** (`/system/app/OmniKiosk/OmniKiosk.apk`, context `u:object_r:system_file:s0` matching real system apps; `/system/app` not `priv-app` to avoid the privileged-permission allowlist requirement), removed the 9 MB bootanimation `.orig` backup, then `qemu-img convert -c` → **`base-v2.qcow2`, self-contained (no backing file), 2.74 GiB**. Kernel/initrd copied as `base-v2.kernel`/`.initrd.img` (unchanged from v1 — they live on a different partition than the modified system.img). `configs/paths.json`: `current_base=v2`. `provision_settings()` sets kiosk as HOME + disables Bliss launchers when the kiosk package is present (so v2 accounts self-configure; v1 accounts skip cleanly).

**Step 3 — brand-new account `dave` from base-v2, full run (zero intervention):**

| Check | Result |
|---|---|
| Silent boot → custom loading screen → kiosk | ✅ boot ~0.5 min, no console text |
| Kiosk auto-launches Roblox on boot | ✅ Roblox foreground at t≈3 s after boot, no intervention |
| ARM translation | ✅ `ro.dalvik.vm.native.bridge=libndk_translation.so`, abilist has `arm64-v8a`; arm64-only Roblox **renders** (login screen screencap) |
| "Viewing full screen" message | ✅ does NOT appear (`immersive_mode_confirmations=confirmed`) |
| Lock screen | ✅ does NOT appear (`locksettings get-disabled = true`) |
| Kiosk is a base-v2 system app | ✅ `pm path com.omni.kiosk = /system/app/OmniKiosk/OmniKiosk.apk` |
| Default HOME | ✅ `com.omni.kiosk/.MainActivity` |
| Close game → clean shutdown | ✅ watchdog `RUNNING→GRACE`, countdown 3..21 s, `svc power shutdown`, **QEMU exited clean** |

**Isolation preserved:** `base-v1.qcow2` still 6.17 GiB (never written). Each account has its own independent `data.qcow2` (alice 1776 / bob 1455 / charlie 2667 / dave 2176 MB — all different). Overlays back the correct base (alice/bob/charlie→v1, dave→v2). alice/bob still running throughout, untouched.

**Constraints honored:** only media (bootanimation), a HOME app (kiosk in /system/app), and `/data` settings changed. No `/system` libraries or native-bridge props touched — verified by libndk + Roblox rendering on the flattened base-v2.
