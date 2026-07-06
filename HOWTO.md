# qemu-manager — How to Use

Complete usage guide for the **qemu-manager** engine (`qemu-manager.exe`
on Windows, `qemu-manager` ELF on Linux; identical to
`python manager/omni.py …` from a checkout). This is the engine the
**omnidroid.exe** GUI drives — the GUI is a separate app; everything the
GUI does goes through the commands documented here, so a human at a
terminal can do all of it too.

Companion docs: `HANDOFF.md` (project state / architecture),
`CHANGELOG.md` (history), `PLAN.md` (original plan + phase results).

---

## 1. What it is

A **multi-account manager for a Bliss OS (Android 13, x86_64) image**
running under QEMU, with libndk ARM translation so ARM-only games run on
x86. Each *account* is a fully isolated Android instance that:

- boots **silently** (no firmware text, no boot menus) into a custom
  loading screen, then a locked-down kiosk that **auto-launches one game**;
- is **fully locked down** (Lock Task Mode as device owner: no status
  bar, no quick settings, no navigation escape);
- **powers itself off when the game closes** (host-side watchdog);
- runs **HEADLESS, always** — there is never a host window. Viewing an
  instance is optional, via its private localhost-only VNC port.

### The two-disk design (why account data is safe)

```
OmniImages/  (outside the repo)          accounts/<name>/   (per account)
  base-vN.qcow2      <- immutable,        system.qcow2  <- disposable overlay
  base-vN.kernel        shared by         data.qcow2    <- Android /data:
  base-vN.initrd.img    all accounts                       logins, saves,
  data-template-8g.qcow2                                   installed apps
                                          account.json  <- ports, base, state
```

- `system.qcow2` is a copy-on-write overlay on the shared base. It is
  **throwaway**: base updates just recreate it against the new base.
- `data.qcow2` holds all per-account state and is **never touched by any
  base operation**. Deleting it (only `remove` does) deletes the account's
  game data forever.

---

## 2. First-run setup

### Windows

```
qemu-manager.exe setup
```

Idempotent; also runs implicitly on first use. Creates the folder layout
and downloads a **portable QEMU into ./qemu only** — nothing is installed
to the host system (no registry, no PATH, no global install). You also
need `adb` (Android platform-tools) on PATH.

### Linux

```
sudo apt install qemu-system-x86 qemu-utils android-tools-adb
./qemu-manager setup
```

Uses system QEMU. `setup` preflights QEMU, `/dev/kvm` access, and KSM,
printing exact fix commands for anything missing (e.g.
`sudo usermod -aG kvm $USER`). The Linux binary is built ON a Linux box
via `build-linux.sh` (PyInstaller cannot cross-build).

### Fresh deployment: drop the exe anywhere

`qemu-manager.exe` is designed to be copied into any folder and just
work — **every command self-bootstraps** `configs/paths.json` (and
`accounts/`, `./qemu` on Windows) next to the exe on first use. What it
cannot invent is the base image. Until the base files exist, every
command that needs one fails with a clean message telling you exactly
what to copy where (no crashes), and `--json` callers get
`{"ok": false, "error": …}`.

Make the install ready by copying these files into the images dir
(`configs/paths.json` → `images_dir`; Windows default
`C:/Users/berat/OmniImages`, Linux `~/OmniImages`):

```
base-vN.qcow2            the immutable Bliss OS system image (e.g. base-v5.qcow2)
base-vN.kernel           its extracted kernel
base-vN.initrd.img       its extracted initrd
data-template-8g.qcow2   formatted-empty ext4 /data template
```

Complete `base-vN` triples are **auto-registered on the next command**
(current base = highest version if none was set) — no manual config
editing. This is also the hook for the future server download: a new
base landing in `images_dir` registers itself the same way.

### Verify readiness: `doctor`

```
qemu-manager doctor [--json]
```

Reports the config path, images dir, registered bases, per-file
presence (exact missing paths), data template, QEMU/adb resolution, and
an overall `ready` verdict. Exit 0 = ready to create/boot; exit 1 = the
report says precisely what is missing and where to put it.

---

## 3. Quickstart

```bash
qemu-manager create alice            # one-time: ~3-15 min (first boot + provisioning)
qemu-manager start alice             # cold boot, headless, detached (~35 s to game)
qemu-manager list --stats            # who is running, ports, RAM
qemu-manager watch alice             # host watchdog: powers off when the game closes
qemu-manager stop alice              # explicit power-off (adb -> QMP -> kill)
qemu-manager remove alice            # DESTRUCTIVE: delete the account + its data
```

On a **dev base** (no game baked in) install the game per account once:

```bash
qemu-manager install alice roblox.apk   # kiosk auto-launches it immediately
```

To watch a running instance, point any VNC viewer at
`127.0.0.1:<vnc_port>` (e.g. `127.0.0.1:18001` for the first account).
Closing the viewer does nothing to the instance — see §6.

---

## 4. Ports: the fixed per-account scheme

Every account gets one index `i` (0-based, assigned at create, reused
after remove) and three localhost-only ports derived from it:

| Channel | Port      | Purpose |
|---------|-----------|---------|
| adb     | `16001+i` | Android debug bridge (`adb -s 127.0.0.1:<port>`) |
| QMP     | `17001+i` | QEMU control socket (shutdown, monitoring) |
| VNC     | `18001+i` | View/control attach point (RFB), **127.0.0.1 only** |

The ranges are 1000 apart, so the three channels can never collide below
1000 instances (RAM runs out long before that). All three are printed by
`list` and `start`, and appear in every JSON payload.

---

## 5. Command reference

Below, `omni` stands for `qemu-manager(.exe)` or `python manager/omni.py`.

`--json` (on `create`, `start`, `stop`, `remove`, `list`) prints **exactly
one machine-readable JSON line on stdout**; all progress/log text moves to
stderr. Errors become `{"ok": false, "error": "..."}` on stdout with exit
code 1. This is the GUI contract — parse stdout, ignore stderr, check
`ok` + exit code.

### Lifecycle

#### `omni create <name> [--no-provision] [--json]`
Creates the account (overlay + data disk from the ext4 template + port
allocation), then boots once to provision: lock screen off, kiosk set as
HOME + device owner (Lock Task lockdown), unneeded packages disabled
(RAM trims), black wallpaper, setup wizard suppressed. First boot runs
Android's one-time dexopt — allow **~3–15 min**. Ends powered off.
- `--no-provision`: create disks only; the first `start` provisions.
- Name must match `[A-Za-z0-9_-]+`.
- JSON: `{"name", "base", "adb_port", "qmp_port", "vnc_port",
  "vnc_host": "127.0.0.1", "provisioned": true|false, "ok": true}`

#### `omni start <name> [--mode M] [--mem MB] [--accel A] [--wait] [--timeout S] [--dev] [--json]`
Cold-boots the instance **headless and detached** — the command returns
immediately; the VM is not tied to the calling process (PID recorded in
`accounts/<name>/run.json`). Boot to game ≈ 35 s.
- `--mode playable|hard|brutal` — RAM/CPU tier (see §7). Default playable.
- `--mem MB` — override guest RAM.
- `--wait` — block until `sys.boot_completed=1` (adb readiness — nothing
  display-dependent), then verify the ARM bridge. Hard timeout: 360 s
  normal boot, 1500 s first boot, or `--timeout`.
- `--accel` — override hypervisor (auto: Windows→WHPX, Linux→KVM).
- `--dev` — builder profile (serial log, virtio-vga) — still headless.
- JSON (immediate): `{"name", "pid", "mode", "headless": true,
  "adb_port", "qmp_port", "vnc_port", "vnc_host", "adb_serial",
  "first_boot", "ok": true}`.
  With `--wait` adds `"booted": true|false, "native_bridge_ok": true|false`.
- Starting an already-running account is an error (exit 1).

#### `omni stop <name> [--timeout S] [--json]`
**Explicit power-off** (this is NOT what a viewer disconnect should call —
see §6). Graceful chain, every step hard-bounded: in-guest
`svc power shutdown` (10 s adb timeout) → wait up to `--timeout` (default
90 s) for QEMU to exit → QMP `quit` (+5 s) → hard kill (+2 s).
- JSON: `{"name", "was_running", "stopped", "method":
  "not-running"|"powerdown"|"qmp-quit"|"killed"|"kill-failed", "ok"}`
- Stopping a stopped instance is fine: `method: "not-running", ok: true`.

#### `omni remove <name> [--timeout S] [--json]`  — DESTRUCTIVE
Deletes the account: stop if running (same bounded chain as `stop`; if it
somehow cannot be stopped, remove **refuses** and deletes nothing) →
delete `accounts/<name>/` (system overlay + **data.qcow2** + state) →
ports are freed (the index is reused by the next `create`).
- **The account's game data is gone forever.** Bases and other accounts
  are untouched — structurally: the resolved delete path must be inside
  `accounts/`, the name must match `[A-Za-z0-9_-]+` exactly (no globs, no
  partial matches, no paths), and the images dir is explicitly checked to
  not be inside the target. The command cannot delete anything else.
- JSON: `{"name", "removed": true, "was_running",
  "freed_ports": {"adb", "qmp", "vnc"}, "ok": true}`

#### `omni list [--stats] [--json]`
All accounts with base, ports, and state. `--stats` adds host RSS MB,
guest-used MB (via adb, 8 s timeout per instance), and on Linux
KSM-merged MB.
- JSON: array of `{"name", "base", "running", "pid", "mode",
  "adb_port", "qmp_port", "vnc_port", "vnc_host", "adb_serial",
  "game_package"}` (+ `"started"` epoch when running; + `"host_rss_mb"`,
  `"guest_used_mb"` with `--stats`).

#### `omni resume <name>`
Attach to an already-running instance: wait for boot, run the post-boot
checks. Useful to track a first boot started detached.

### Game / in-guest control

#### `omni install <name> <apk>`
`adb install` a game into the account's `/data` (dev workflow), record it,
and set it as the kiosk's launch target — the kiosk launches it the moment
the install completes.

#### `omni run-app <name> <package>` / `omni adb <name> -- <args...>`
Launch a package / run any adb command against that instance
(e.g. `omni adb alice -- shell getprop ro.dalvik.vm.native.bridge`).

#### `omni watch <name> [--grace N] [--package P]`
The host-side shutdown watchdog: polls the game's **process** (never
foreground state — ads/dialogs/loading don't kill it) and powers the
instance off after the process is gone `--grace` s (default 20). This is
the production "game closed → machine off" path.

#### `omni screenshot <name> [--out path]` / `omni logcat <name> [--tag T] [--clear]`
True-color framebuffer PNG (works headless; JSON `{ok, path}`) / guest
logcat.

#### `omni test-apk <name> --apk <apk> [--mode M] [--reuse]`
Dev harness: fresh session → headless boot → install → kiosk launches →
one JSON line with the outcome (`installed/launched/foreground/ok`, ports,
serial).

### Bases / updates

#### `omni bases` / `omni use-base <tag>`
List registered bases (current marked, pre-installed game shown) / set the
default base for **new** accounts (dev base = no game, install per
account; production base = game baked in).

#### `omni update-base <name> [--to vN]` / `omni update-all [--to vN] [--fast|--full] [--skip-current]`
Migrate one/all accounts to a base **keeping their data**. `update-all`
auto-picks per account:
- **FAST** (base game unchanged for that account): recreate the overlay
  against the new base — no boot, seconds for a whole fleet.
- **FULL** (base game changed, or forced with `--full`): boot +
  idempotent re-provision — needed whenever provisioned `/data` state
  must change (kiosk target, lockdown policies, package-trim updates).

#### `omni rebuild-base --game <apk>` / `omni update-kiosk [--apk ...]`
Build a NEW immutable base version with the game baked as a `/system/app`
(native libs extracted) / with a new kiosk build. Then `update-all` rolls
it out. Bases are never edited in place.

### Platform

#### `omni setup` / `omni doctor [--json]` / `omni qemu-info [--install]`
First-run setup (see §2) / readiness check — what's present/missing in
images_dir, QEMU/adb resolution, `ready` verdict, exit 0/1 (see §2) /
show or repair QEMU resolution.

#### `omni ksm [status|on|off] [--aggressive]` / `omni bench-ksm ...`
Linux only (clean no-op on Windows): control kernel samepage merging /
measure real instances-per-GB with KSM.

---

## 6. Viewing instances: VNC (and the disconnect vs shutdown rule)

Every instance runs QEMU's built-in VNC server on its `vnc_port`, bound to
**127.0.0.1 only**. Connect any RFB viewer (TigerVNC, RealVNC, noVNC via
a local websockify, the omnidroid GUI) to `127.0.0.1:<vnc_port>`.
For viewers that take a display number instead of a port, the display is
`vnc_port − 5900` (e.g. 18001 → `:12101`).

- **Always available, costs nothing idle.** With no viewer connected the
  server does no framebuffer encoding — instances designed to run for
  hours unwatched pay nothing for it. No enable/disable step exists or is
  needed; connect whenever you want to look, disconnect whenever done.
- **Disconnect ≠ shutdown (the GUI contract).** Closing a viewer merely
  closes a socket: the instance keeps running headless — that is the
  default, expected state. Powering an instance OFF is only ever an
  explicit act: `omni stop` (or the `watch` watchdog when the game
  closes). A GUI must never call `stop` on viewer close.
- Input works over VNC (QEMU exposes usb-kbd/usb-tablet); the guest kiosk
  lockdown still applies — you see and control exactly what the locked
  kiosk allows.
- `omni screenshot` remains the scriptable no-viewer alternative.

### SECURITY — hard rule

**The VNC server has no authentication. That is safe ONLY because it is
bound to 127.0.0.1** — it is unreachable from any network interface.
**Never bind VNC to a non-loopback address without adding authentication
(and ideally TLS) in the same change.** Any future "remote viewing"
feature must tunnel (SSH port-forward, VPN) or add RFB auth — exposing
`-vnc 0.0.0.0:…` as-is would hand every instance's screen and input to
the LAN. (Also recorded as HARD CONSTRAINT #4 in HANDOFF.md.)

---

## 7. Performance modes and capacity

Modes are pure RAM/CPU tiers (all headless; counts NEVER capped — host
free RAM decides):

| Mode       | Guest RAM | vCPUs | Use |
|------------|-----------|-------|-----|
| `playable` | 4 GB      | 4     | default, most comfortable |
| `hard`     | 3 GB      | 4     | more instances |
| `brutal`   | 2 GB      | 2     | max instances |

Measured on the 32 GB Windows host (Roblox running): ~3.2 GB host-resident
per instance; **~4–5 concurrent** with normal desktop apps open, **~7–8**
with them closed. Keep ~2 GB OS headroom; stop adding when free RAM nears
~3 GB. On Windows/WHPX guest-side trims do NOT reduce host RSS (QEMU
touches its full `-m`); host-RAM density is what the Linux/KSM port is
for. Boot is ~35 s cold; instances always cold-boot (no snapshots — by
design, do not re-add).

---

## 8. Typical workflows

### Dev: test a game build on a fresh instance
```bash
qemu-manager test-apk t1 --apk mygame.apk --mode hard
# -> one JSON line; then poke it:
qemu-manager screenshot t1
qemu-manager logcat t1 --tag OmniKiosk
qemu-manager adb t1 -- shell dumpsys activity activities
qemu-manager remove t1 --json
```

### Production: ship a game update to every account
```bash
qemu-manager rebuild-base --game newgame.apk   # new immutable base vN+1
qemu-manager update-all                        # data preserved; FAST/FULL auto
```

### GUI: manage an account end to end (all JSON)
```bash
qemu-manager create p1 --json
qemu-manager start p1 --json                   # returns pid + ports at once
# ... GUI connects viewer to 127.0.0.1:<vnc_port> whenever asked,
#     disconnects freely; instance keeps running ...
qemu-manager list --stats --json               # live dashboard
qemu-manager stop p1 --json                    # explicit power-off only
qemu-manager remove p1 --json                  # explicit delete only
```

---

## 9. Troubleshooting

- **"no base image is registered" / "base assets missing"** — the
  install has no usable base yet. The error lists the exact files and
  the full images_dir path; copy them in and re-run (they register
  automatically). `qemu-manager doctor` shows what's still missing.
- **First boot seems stuck** — it is dexopt: allow up to 15 min once per
  account. `omni resume <name>` shows progress phases. Dev boots write
  `accounts/<name>/serial.log`; every boot writes `qemu.log`.
- **`start` fails immediately / PID dies at once** — read
  `accounts/<name>/qemu.log`. Common: another process took one of the
  account's three ports; WHPX not enabled (Windows Hypervisor Platform
  feature); `/dev/kvm` missing (Linux — `setup` prints the fix).
- **adb can't connect** — `adb kill-server`, then any omni command
  (they auto-`adb connect 127.0.0.1:<port>`).
- **VNC viewer can't connect** — instance running? (`omni list`.) Viewer
  must target `127.0.0.1` (it is never reachable from other machines —
  that is intentional; see §6).
- **Instance won't die** — `omni stop` escalates automatically and
  reports `method`; `kill-failed` (never observed) means investigate the
  QEMU process manually.
- **Game doesn't launch on boot** — dev base with no game installed shows
  "no apk found" on black: `omni install <name> <apk>`. Check
  `omni logcat <name> --tag OmniKiosk`.
- **Never do these:** edit a base file in place (corrupts every overlay
  on it); touch `/system` libs or the ARM bridge props
  (`ro.dalvik.vm.native.bridge`); commit `*.qcow2`; bind VNC beyond
  localhost (§6).

---

## 10. Invariants (summary of the rules everything above relies on)

1. Bases are **immutable** once referenced; updates are always a new
   versioned base + overlay repoint.
2. `data.qcow2` is **never touched** by base operations; only `remove`
   deletes it, and `remove` can only ever delete inside `accounts/`.
3. Instances are **always headless**; VNC is a localhost-only attach
   point, never a window, never network-exposed without auth.
4. Every base change ends with the regression check: libndk bridge prop
   intact + the game launches and renders.
5. Cold boot only; no snapshots.
