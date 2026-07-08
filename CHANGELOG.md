# Changelog

> **Resuming from a fresh session? Read `HANDOFF.md` first**, then `PLAN.md`,
> then this file, then `git log`. Entries are newest-first.

All notable base-image and manager changes. Bases are immutable and
versioned; each new base is flattened self-contained (no backing file).

## Manager — 2026-07-09 — `omni view`: live VNC viewer (self-contained + native)

New `omni view <account> [--start]` opens a LIVE window onto an instance —
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
end via `omni create/start/install/stop`.

**DONE & verified on arm:** silent-of-*console* aside (see below), an account
boots the provisioned kiosk in ~15–40 s; kiosk is HOME and auto-launches the
app; **device-owner Lock Task fully blocks the status bar AND the swipe-down
Quick-Settings panel** (verified: swipe-from-top does nothing); app renders
arm64-native; `omni stop` powers off cleanly via the arm path. Device-owner
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
- **`omni ksm [status|on|off] [--aggressive]`** — drives
  `/sys/kernel/mm/ksm/*`, prints stats + MB deduped; clean no-op message
  on Windows. `list --stats` shows per-instance `ksm-merged` MB on Linux.
- **`omni bench-ksm`** — Phase 8 measurement scaffold (Linux-guarded):
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

**New manager command:** `omni update-kiosk [--apk ...]` — ship a new kiosk
launcher in a new base version (reuses the generalized base-builder), then
`omni update-all` rolls it out (per-account data preserved).

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

**Performance modes** — `omni start <name> --mode playable|hard|brutal`.
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
- `omni test-apk <name> --apk <apk> [--mode hard] [--window] [--reuse]` —
  one-shot: ensure a FRESH session with no app pre-baked (v3 dev base +
  kiosk), install the APK, let the kiosk launch it, emit one JSON line:
  `{account, base, mode, package, installed, launched, foreground, pid,
  adb_port, qmp_port, adb_serial, ok}`. Headless by default.
- `omni screenshot <name> [--out path]` — pull a framebuffer screenshot
  (true colors, works headless); prints JSON `{ok, path}`.
- `omni logcat <name> [--tag T] [--clear]` — read/clear guest logcat.
- `omni adb <name> -- <args>` — arbitrary adb (existing).
  An agent scripts: `test-apk` → parse JSON → `screenshot`/`logcat`/`adb`
  against the reported `adb_serial`.

## Manager — 2026-07-06 — base migration, QEMU auto-install, exe, prod updates

**Base migration (update accounts to a newer base, keeping their data).**
An account = a disposable `system.qcow2` overlay on a shared base + an
independent `data.qcow2` (all logins/settings/apps). Migration recreates
only the overlay against the new base; `data.qcow2` is never touched.
- `omni update-base <name> [--to vN]` — migrate one account.
- `omni update-all [--to vN] [--skip-current]` — migrate every account.
- Each migration re-provisions (idempotent): applies the new base's kiosk/
  HOME/settings without erasing data. **Verified:** alice v1→v3 kept a
  `/sdcard` marker file + installed Roblox, gained the v3 kiosk, and
  auto-launched Roblox. All 5 accounts migrated v1/v2→v3, data preserved.

**Production pre-installed-game update (no data loss for any user).**
- `omni rebuild-base --game <apk>` — boots a throwaway builder on the
  current base, bakes/replaces the game as a `/system/app` system app
  (`/system/app/OmniGame/OmniGame.apk`, correct SELinux context),
  **extracts the APK's native `.so` libs into `lib/<abi>`** (a `/system/app`
  APK is NOT auto-extracted like a `/data` install, so an ARM game would
  crash at load without this — libndk still translates the ARM libs),
  flattens to a new self-contained base version, registers it, makes it
  current.
- Roll out to everyone: `omni update-all` → each account's overlay repoints
  to the new base (new game) while its `data.qcow2` (per-account login/
  saves) is preserved. So updating the pre-installed APK reaches all users
  without erasing data.
- `provision_settings` sets the kiosk's target game from the base's
  pre-installed game (production) or the adb-installed game (dev).

**Dev vs production mode switch.**
- `omni bases` — list registered bases (marks current) + any pre-installed
  game per base.
- `omni use-base <tag>` — set the default base for new accounts (e.g. a dev
  base with no game vs a production base with the game baked in).
- Dev workflow: base without game; `omni install <acct> <apk>` per account.
  Production workflow: game baked in base via `rebuild-base`; every account
  gets it.

**QEMU auto-install on first use (not bundled in the exe).**
- `qemu_bin()` resolves the QEMU executable: config `qemu.dir` → local
  `./qemu` (auto-installed) → PATH.
- `ensure_qemu()` runs before any command that needs QEMU; if QEMU is not
  resolvable it downloads a portable Windows QEMU installer and silently
  installs it into `./qemu` (NSIS `/S /D=`), no global install. Overridable
  via config `qemu.download_url`. No-op when QEMU is already present.
- `omni qemu-info [--install]` — show/repair QEMU resolution.

**Single Windows exe.**
- `build-exe.ps1` → `dist/omni.exe` (PyInstaller onefile, ~9.5 MB, stdlib
  only). Ships next to `configs/`; `accounts/`, `work/`, `qemu/` are created
  beside it. The exe's CLI is identical to `python omni.py …`, so external
  scripts call it the same way. QEMU is NOT inside the exe — fetched on
  first use. Verified: `omni.exe list` and `omni.exe qemu-info` work.

## base-v4 — 2026-07-06 — PRODUCTION base (game pre-installed)

`v3 + Roblox baked as a `/system/app` system app` with its 11 arm64 `.so`
libs extracted into `lib/arm64`. Built via `omni rebuild-base --game
roblox.apk`. **Verified:** a brand-new account on v4 (`prod2`) boots
straight into Roblox — pre-installed system app, kiosk auto-launches it,
renders via libndk — with NO adb install and no manual steps.

This is the production lineage. `current_base` is kept at **v3 (dev
default)**; switch to production with `omni use-base v4`. Dev accounts (v3,
game via `omni install`) and production accounts (v4, game pre-installed)
coexist. Updating the pre-installed game for everyone: `omni rebuild-base
--game <newapk>` (→ v5) then `omni update-all` — each account's overlay
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
