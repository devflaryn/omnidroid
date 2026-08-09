#!/usr/bin/env python3
"""omni — multi-account Bliss OS instance manager.

Each account = a cheap qcow2 overlay on the shared immutable base (system)
plus an independent data disk (Android /data). Instances boot via direct
kernel boot (-kernel/-initrd/-append): no GRUB, per-account kernel params.

Usage (full guide: HOWTO.md; --json on create/start/stop/remove/list
emits exactly one machine-readable line on stdout — the GUI contract):
  python omni.py create <name> [--no-provision] [--json]
  python omni.py start  <name> [--mode M] [--wait] [--json]
  python omni.py stop   <name> [--timeout SECS] [--json]
  python omni.py remove <name> [--json]              # DESTRUCTIVE
  python omni.py list   [--stats] [--json]
  python omni.py install <name> <apk> [package]
  python omni.py run-app <name> <package>
  python omni.py capture <name> [--duration S] [--package PKG] [--json]
  python omni.py adb    <name> -- <adb args...>
"""
import argparse
import contextlib
import json
import os
import platform
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

from omnidroid import config
from omnidroid import farming
from omnidroid import gaming
from omnidroid import lean
from omnidroid.config import (
    REPO, CONFIG_PATH, QEMU_DIR,
    IS_WINDOWS, IS_LINUX, IS_MACOS, HOST_ARCH, IS_ARM64_HOST,
    images_dir, qemu_bin, qemu_system_name,
)

# Data-store root (accounts.json, accounts/, logs/, runtime/). Defaults to
# REPO; relocatable via OMNI_DATA_DIR (see omnidroid.config.data_dir()).
# Captured once at import time (intentional: the data root doesn't need to
# move mid-process) -- contrast with the cookie store, which re-resolves
# per call via _store_root() so tests that flip OMNI_DATA_DIR at runtime
# see it take effect.
ACCOUNTS_DIR = config.data_dir() / "accounts"


def _store_root():
    """Root dir for the cookie store (accounts.json), re-resolved per call
    so tests/tools that flip OMNI_DATA_DIR at runtime see it take effect."""
    return config.data_dir()


FIRST_BOOT_TIMEOUT = 1500   # first boot runs full dexopt; be patient
NORMAL_BOOT_TIMEOUT = 360

# Portable QEMU installer URL. Intentionally NOT hardcoded to any third-party
# URL: pinned public URLs rot (the old weilnetz pin started 404ing). Set it in
# config ("qemu": {"download_url": ...}) when a delivery source exists. The
# INTENDED production answer (deferred — see HANDOFF "QEMU delivery") is to host
# a portable QEMU on the user's own server/CDN, the SAME path as the base-image
# download, so QEMU + base are one "download from my server" story. Until then,
# populate ./qemu from a portable copy (the product-dir model already works).
# The NSIS installer supports silent install via /S /D=<dir> (no global install).
DEFAULT_QEMU_URL = None

# Kernel SRC= param for auto-registered bases (all Bliss 16.9.7 lineage
# bases v1..v5 use this). Overridable per config ("default_src") and per
# base ("src") — a future downloaded base can carry its own.
DEFAULT_SRC = "/android-2024-10-11"

# ---------- base types ----------
# "x86-bliss"  : Bliss OS x86_64, direct kernel boot (-kernel/-initrd + SRC=/
#                DATA= append). Cheap disposable overlay on a shared immutable
#                base; kiosk baked in /system; provisioning writes /data.
# "arm-uefi"   : LineageOS arm64, UEFI/GRUB disk boot (EDK2 pflash + GPT vda).
#                Runs NATIVELY under HVF on Apple Silicon — no translation.
#                /data is file-based-encrypted (FBE) with keys in /metadata
#                (a partition on the vda system overlay), so the system
#                overlay and the /data disk are a MATCHED PAIR captured
#                together at provisioning time — a fresh overlay against a
#                provisioned /data fails with init_user0_failed. An account
#                therefore copies the provisioned (system-overlay, data,
#                efivars) trio rather than provisioning on first boot.
from omnidroid.bases import *  # noqa: F401,F403
from omnidroid.bases import (_truthy_env, _debug_boot_requested,
                             _no_warm_requested,
                             _select_base_tag, _next_base_tag)  # noqa: F401
from omnidroid import offsets as offsets_mod








# ---------- dual-use bases ----------
# There is no separate dev base and no dev-mode gate. Every shipped base is
# DUAL-USE: the same image that ships to production is the one omni-agent
# debugs on. What used to be "the dev base" is now three independent things —
# see omnidroid/bases.py for the full split:
#
#   root     baked into the shipped image (Magisk-patched boot), always there.
#   hiding   baked into the shipped /data and re-enforced on EVERY boot
#            (Zygisk + Enforce DenyList), so production looks unrooted.
#   toolkit  the devkit disk (frida-server + omni-* scripts), attached as vdc
#            ONLY on a `--debug` boot.
#
# So "debug" is a per-BOOT option, not a base and not an account property. The
# same account boots production on one run and debug on the next, off one image.














# ---------- canonical arch tokens (contract omnidroid-api.md v1 §2) ----------
# The frozen arch enum both clients code against is "x86" | "arm". It maps
# base type x86-bliss->"x86", arm-uefi->"arm"; host amd64/x86_64->"x86",
# arm64/aarch64->"arm".




def host_arch_token():
    """Canonical arch token for THIS host: 'arm' on arm64, else 'x86'."""
    return "arm" if IS_ARM64_HOST else "x86"


# arm-uefi base default filenames in images_dir (a future downloaded base
# can override any of these in its config entry).

# x86-bliss base canonical filenames in images_dir — mirrors the base_arm
# scheme exactly: versionless filename, version tracked INSIDE the config
# entry ("version" + "changelog"). Legacy base-vN.* triples are still
# auto-registered so old deployments keep working.

# The devkit disk — the attachable debug toolkit, built host-side (rootless,
# cross-platform via `mke2fs -d`) as an ext4 image carrying the frida-server for
# that arch, Magisk (apk + magiskboot), the omni-* device scripts, and a
# manifest. It belongs to no base entry: `omnidroid start --debug` attaches it as vdc
# and the guest mounts it read-only at /mnt/omni-devkit, copying the toolkit to
# /data/local/tmp to execute it (/mnt is a noexec tmpfs). See _devkit_* and
# `omnidroid build-devkit`.
#
# Root-state markers embedded in a base's human-readable `notes`; kept as
# constants because the notes must never contradict the entry's `rooted` flag.
#
# frida-server is pinned for the devkit (android-<arch>). The base runs its
# guest arch natively (arm64 under HVF/KVM, x86_64 under WHPX/KVM), so no
# translation. Override with `omnidroid build-devkit --frida-version`. The hidden
# frida port is intentionally NOT the well-known 27042.
DEFAULT_FRIDA_VERSION = "17.15.4"
DEFAULT_FRIDA_PORT = 27142

# EDK2 aarch64 firmware CODE (read-only); resolved from the QEMU install.
# On macOS/brew it ships inside the qemu Cellar; overridable via config
# qemu.arm_edk2_code.




# Config bootstrapped on a blank deployment (exe dropped into a new
# folder): any command self-creates this, then base files are copied (or
# later: downloaded) into images_dir and auto-registered.
DEFAULT_CONFIG = {
    "images_dir": "images",
    "current_base": None,
    "data_template": "data-template-8g.qcow2",
    "default_src": DEFAULT_SRC,
    "bases": {},
    "qemu": {"mem_mb": 4096, "smp": 4, "data_disk_size": "8G",
             "adb_port_start": 16001, "qmp_port_start": 17001,
             "vnc_port_start": 18001},
    "notes": ("Base images are immutable once any account references "
              "them. Never commit image files."),
}


# ---------- machine-readable output (the GUI contract) ----------
# Relocated to omnidroid/output.py (Task 2 extraction); re-exported below so
# every `engine.<name>` caller keeps resolving.
from omnidroid.output import emit_json, enable_json_mode, fail, redact_token
from omnidroid import output   # for output._JSON_MODE single-source reads
from omnidroid.output import _JSON_MODE  # noqa: F401  (facade re-export)


# ---------- config / account state ----------

def ensure_config():
    """Bootstrap configs/paths.json on a blank deployment so the exe can
    be dropped into any folder and every command just works (base files
    are then copied — later: downloaded — into images_dir)."""
    if CONFIG_PATH.exists():
        return False
    CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
    CONFIG_PATH.write_text(json.dumps(DEFAULT_CONFIG, indent=2))
    print(f"[config] created default config: {CONFIG_PATH}")
    return True


def read_config():
    """Plain JSON read, no base validation (safe to call before QEMU or
    any base exists). Self-bootstraps a default config."""
    ensure_config()
    try:
        return json.loads(CONFIG_PATH.read_text())
    except json.JSONDecodeError as e:
        sys.exit(f"error: {CONFIG_PATH} is not valid JSON ({e}). Fix or "
                 f"delete it (a default will be recreated).")










def load_config():
    """Config for commands that NEED a bootable base. Never tracebacks on
    a fresh/incomplete install: auto-registers base files that appeared in
    images_dir, and otherwise exits with the exact copy-these-files help."""
    cfg, _ = autoregister_bases()
    cfg["images_dir"] = images_dir(cfg)   # normalized for callers (OMNI_IMAGES_DIR wins)
    images = Path(cfg["images_dir"])
    # Host architecture selects the base (arm64 -> arm-uefi; x86 -> current).
    tag = effective_base_tag(cfg)
    cfg["_effective_base"] = tag
    bases = cfg.get("bases") or {}
    if not tag or tag not in bases:
        sys.exit("error: no base image is registered - cannot create or "
                 "boot instances." + base_setup_help(images, cfg))
    missing = base_missing_files(images, bases[tag])
    if missing:
        sys.exit(f"error: base '{tag}' is registered but its files are "
                 f"missing:\n  " + "\n  ".join(missing)
                 + base_setup_help(images, cfg))
    return cfg


# ---------- qemu auto-install ----------

def _qemu_present():
    import shutil
    p = qemu_bin(qemu_system_name())
    return Path(p).exists() or shutil.which(p) is not None


def ensure_qemu():
    """Install QEMU into QEMU_DIR on first use if it is not already
    resolvable (config dir, local dir, or PATH). Not bundled in the exe —
    downloaded on demand. No-op when QEMU is already available."""
    if _qemu_present():
        return
    if IS_MACOS:
        # macOS policy: SYSTEM QEMU only (Homebrew), like Linux.
        sys.exit("QEMU not found. Install it with Homebrew:\n"
                 "  brew install qemu android-platform-tools\n"
                 "then re-run (see: omnidroid setup)")
    if not IS_WINDOWS:
        # Linux policy: SYSTEM QEMU only (no portable download).
        sys.exit("QEMU not found. Install the system packages:\n"
                 "  sudo apt install qemu-system-x86 qemu-utils "
                 "android-tools-adb\nthen re-run (see: omnidroid setup)")
    import urllib.request
    url = (read_config().get("qemu", {}).get("download_url")
           or DEFAULT_QEMU_URL)
    if not url:
        # No delivery source configured (and none hardcoded, by design). Give a
        # clear, actionable message rather than reaching for a rotting default.
        sys.exit(
            f"QEMU not found in the product dir and no download source is "
            f"configured.\nEither place a portable QEMU in:\n  {QEMU_DIR}\n"
            f"or set 'qemu.download_url' (and/or 'qemu.dir') in "
            f"configs/paths.json.\n(Production: host portable QEMU on your "
            f"server/CDN — same delivery path as the base image; see HANDOFF "
            f"\"QEMU delivery\".)")
    QEMU_DIR.mkdir(parents=True, exist_ok=True)
    installer = QEMU_DIR / "qemu-setup.exe"
    print(f"[qemu] not found; downloading portable QEMU into the product "
          f"dir {QEMU_DIR}")
    print(f"[qemu] from {url} (one-time, ~150 MB)")
    # Hard socket timeout so a stalled/blackholed connection can NEVER hang the
    # engine: a clear error is raised instead. Stream to disk (no whole file in
    # RAM). The download lands ONLY in QEMU_DIR — never a global/system path.
    try:
        with urllib.request.urlopen(url, timeout=60) as resp, \
                open(installer, "wb") as f:
            shutil.copyfileobj(resp, f)
    except Exception as e:
        installer.unlink(missing_ok=True)
        sys.exit(f"[qemu] download failed or timed out ({e}). Set "
                 f"'qemu.dir' in configs/paths.json to a QEMU install, or "
                 f"place a portable QEMU in {QEMU_DIR} manually.")
    print("[qemu] installing silently into the product dir (no global "
          "install)...")
    # NSIS silent install into QEMU_DIR; /D must be last and unquoted. Bounded
    # so a wedged installer can't hang either.
    try:
        r = subprocess.run(f'"{installer}" /S /D={QEMU_DIR}', shell=True,
                           timeout=300)
    except subprocess.TimeoutExpired:
        installer.unlink(missing_ok=True)
        sys.exit(f"[qemu] installer timed out after 300s. Place a portable "
                 f"QEMU in {QEMU_DIR} manually or set 'qemu.dir'.")
    installer.unlink(missing_ok=True)
    if not _qemu_present():
        sys.exit(f"[qemu] auto-install failed (exit {r.returncode}). "
                 f"Install QEMU into {QEMU_DIR} manually or set qemu.dir in "
                 f"configs/paths.json")
    print(f"[qemu] ready: {qemu_bin(qemu_system_name())}")


def account_dir(name):
    return ACCOUNTS_DIR / name


def _base_tag_for_mode(rec, cfg=None):
    """Resolve a store record's base MODE to a cfg base TAG (a key into
    cfg["bases"]). The store and the engine speak different vocabularies for
    "base": the store tracks a coarse mode, the engine needs the exact
    registered base entry to boot/introspect. `rec` need only carry a "base"
    key (a full record or a list_accounts entry both work); pass `cfg` to avoid
    a re-read when resolving many accounts at once (e.g. all_accounts).

    There is only one mode now — every shipped base is dual-use, so a legacy
    "dev" record resolves to the same production base as everything else."""
    cfg = cfg if cfg is not None else read_config()
    bases = cfg.get("bases") or {}
    tag = effective_base_tag(cfg)
    if tag and tag in bases:
        return tag
    for t, b in bases.items():
        if base_type(b) == BASE_TYPE_ARM:
            return t
    return ARM_BASE_TAG


def _booted_with_native_window(name):
    """Did this instance's CURRENT boot open a QEMU window on the host?

    Recorded by spawn_qemu into run.json (see command_opens_a_window), not
    recomputed here — the capability probe is host state that can change
    between the spawn and this call, and the only answer that matters is what
    the running process actually did. Unknown reads as False, which keeps the
    VNC viewer: an extra window is a nuisance, a missing one is a black box."""
    try:
        run = json.loads((runtime_dir(name) / "run.json").read_text())
        return bool(run.get("native_window"))
    except Exception:  # noqa: BLE001 — no run.json, unreadable, or older format
        return False


def _want_vnc_viewer(native_window, explicit_window, json_mode, no_window):
    """Whether `omnidroid start` should also spawn the built-in Tk/RFB viewer.

    The instance always RUNS a VNC server — screenshot, autocap and the
    omnidroid-input skill attach to it in every mode. This decides only
    whether to put a second viewer WINDOW on screen, and the gaming case is
    why it exists: a native QEMU window plus the VNC viewer means two windows
    onto one instance, and the VNC one is the laggier of the two, so it is the
    one a user would click and then judge the mode by.

    Order matters: --no-window is absolute, then an explicit --window (the
    user asked for the viewer; do not second-guess), then the native window
    stands it down, then today's rule (on interactively, off under --json)."""
    if no_window:
        return False
    if explicit_window:
        return True
    if native_window:
        return False
    return not json_mode


def load_account(name):
    """Build a runtime HANDLE for `name` -- identity from the central store
    (omnidroid/accounts.py), live ports (and, if running, the exact base tag
    and whether it was a debug boot) from runtime/<name>/run.json. Reads NO
    per-account folder: the ~14 running-instance commands only ever need
    name/base/ports/debug/game_package, all of which live in one of those two
    places now."""
    from omnidroid import accounts as _acc
    rec = _acc.get_account(_store_root(), name)
    run_path = runtime_dir(name) / "run.json"
    run = None
    if run_path.exists():
        try:
            run = json.loads(run_path.read_text())
        except Exception:  # noqa: BLE001
            run = None
    if rec is None and run is None:
        sys.exit(f"error: no such account '{name}'")

    cfg = read_config()
    if run and run.get("base"):
        base_tag = run["base"]
    else:
        base_tag = _base_tag_for_mode(rec)

    acct = {
        "name": name,
        "base": base_tag,
        "ephemeral": True,
        # Per-BOOT, not per-account: true only if the live boot attached the
        # devkit disk. An account that is not running is never "debug".
        "debug": bool((run or {}).get("debug")),
        # Which Roblox version the LIVE boot picked, not what the default is
        # now — the default can be changed while an instance is running.
        "offset": (run or {}).get("offset"),
        "data_image": (run or {}).get("data_image"),
        "game_package": ROBLOX_PACKAGE,
        "first_boot_done": True,
    }
    if run:
        for key in ("adb_port", "qmp_port", "vnc_port"):
            if run.get(key) is not None:
                acct[key] = run[key]
    return acct


def save_account(acct):
    d = account_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    (d / "account.json").write_text(json.dumps(acct, indent=2))


def all_accounts():
    """Every STORE-listed account (omnidroid/accounts.py), joined with live
    running state (runtime/*/run.json via running_instances()). Replaces the
    old accounts/*/account.json folder scan: identity now lives in the
    central cookie store, not per-account folders. A non-running account's
    handle carries no port keys at all (nothing has allocated them yet) --
    callers that need a port must check running state first."""
    from omnidroid import accounts as _acc
    cfg = read_config()
    running = {i["name"]: i for i in running_instances()}
    out, seen = [], set()
    for entry in _acc.list_accounts(_store_root()):
        name = entry["username"]
        seen.add(name)
        r = running.get(name)
        # `entry` (from list_accounts) already carries the base MODE, so pass
        # it straight to _base_tag_for_mode -- no need to re-fetch the full
        # record per account -- and reuse the cfg read above.
        base_tag = r["base"] if (r and r.get("base")) else _base_tag_for_mode(
            entry, cfg)
        acct = {"name": name, "base": base_tag, "ephemeral": True,
                "debug": bool(r.get("debug")) if r else False,
                "offset": (r or {}).get("offset"),
                "data_image": (r or {}).get("data_image"),
                "game_package": ROBLOX_PACKAGE, "first_boot_done": True}
        if r:
            for k in ("adb_port", "qmp_port", "vnc_port"):
                if r.get(k) is not None:
                    acct[k] = r[k]
        out.append(acct)
    # Union: running instances NOT registered in the store (temp build/bench
    # instances) must still appear so running-instance safety guards see them.
    for name, r in running.items():
        if name in seen:
            continue
        base_tag = r.get("base") or _base_tag_for_mode(None, cfg)
        acct = {"name": name, "base": base_tag, "ephemeral": True,
                "debug": bool(r.get("debug")),
                "offset": r.get("offset"), "data_image": r.get("data_image"),
                "game_package": ROBLOX_PACKAGE, "first_boot_done": True}
        for k in ("adb_port", "qmp_port", "vnc_port"):
            if r.get(k) is not None:
                acct[k] = r[k]
        out.append(acct)
    return sorted(out, key=lambda a: a["name"])


# ---------- ports / process / instance tracking ----------
from omnidroid.runtime import *  # noqa: F401,F403
from omnidroid.runtime import (_reserve_ports, _wipe_runtime,
                               _claimed_port_indices, _launch_lock)  # noqa: F401


# ---------- KSM (Linux kernel samepage merging) ----------
from omnidroid.ksm import *  # noqa: F401,F403
from omnidroid.ksm import _ksm_wait_settle  # noqa: F401


# ---------- adb / qmp ----------

from omnidroid.adb import *  # noqa: F401,F403
from omnidroid.adb import _require_adb_port, _pidof, _foreground  # noqa: F401


from omnidroid.qemu_proc import *  # noqa: F401,F403
from omnidroid.qemu_proc import (_gl_window_requested, _assert_port_triple,
                                 _refresh_ephemeral_efivars)  # noqa: F401


# ---------- boot waiting with visible progress ----------

def wait_for_boot(acct, timeout, label, first_boot=False):
    """Poll until sys.boot_completed=1, printing honest progress lines."""
    # arm's serial.log is written under runtime_dir (see qemu_command_arm);
    # x86's stays under account_dir (qemu_command/x86 is untouched — Task 4
    # retires x86 instances along with the overlay-disk model).
    d = runtime_dir(acct["name"]) if acct_base_is_arm(acct) \
        else account_dir(acct["name"])
    serial_log = d / "serial.log"
    start = time.time()
    phase = "starting QEMU"
    last_print = 0.0
    last_change = time.time()
    adbd_seen = False
    initrd_found = False
    while time.time() - start < timeout:
        elapsed = time.time() - start

        if not initrd_found and serial_log.exists():
            try:
                if "Found at" in serial_log.read_text(errors="ignore"):
                    initrd_found = True
            except OSError:
                pass
        adb_connect(acct)
        if adb_getprop(acct, "sys.boot_completed") == "1":
            print(f"[{label}] boot completed after {elapsed/60:.1f} min")
            return True
        try:
            if adb(acct, "get-state").stdout.strip() == "device":
                adbd_seen = True
        except subprocess.TimeoutExpired:
            pass

        if adbd_seen and first_boot:
            new_phase = ("Android first boot: app optimization (dexopt), "
                         "one-time, ~15 min")
        elif adbd_seen:
            new_phase = "Android booting (adbd up)"
        elif initrd_found:
            new_phase = "initrd found OS; Android starting (adbd not up yet)"
        else:
            new_phase = "starting QEMU"

        if new_phase != phase:
            last_change = time.time()
        stalled = time.time() - last_change > 600 and not adbd_seen
        if stalled:
            new_phase += ("  [WARNING: no progress signal for 10+ min - "
                          f"check {d / 'serial.log'} and qemu.log]")
        if new_phase != phase or elapsed - last_print >= 15:
            phase = new_phase
            last_print = elapsed
            print(f"[{label}] {elapsed/60:.1f} min - {phase}", flush=True)
        time.sleep(5)
    print(f"[{label}] TIMED OUT after {timeout/60:.0f} min")
    return False


def post_boot(acct, label):
    """Post-boot sanity. x86/Bliss: adb root (KernelSU) + verify the libndk
    ARM native-bridge is present (its whole reason to exist). arm/LineageOS
    runs the app arm64-NATIVE (no bridge) and is a user build (no adb root),
    so there the check is that the CPU ABI is arm64-v8a instead."""
    if acct_base_is_arm(acct):
        adb_connect(acct)
        abi = adb_getprop(acct, "ro.product.cpu.abilist")
        ok = "arm64-v8a" in abi
        print(f"[{label}] arm64 native (no translation): abilist={abi or '?'}"
              f" {'OK' if ok else '*** NOT arm64-v8a ***'}")
        return ok
    adb(acct, "root")
    time.sleep(3)
    adb_connect(acct)
    bridge = adb_getprop(acct, "ro.dalvik.vm.native.bridge")
    ok = bridge == "libndk_translation.so"
    print(f"[{label}] native bridge: {bridge or '<unreadable>'}"
          f" {'OK' if ok else '*** ARM TRANSLATION CHECK FAILED ***'}")
    return ok


def provision_settings(acct, label):
    """One-time per-account /data settings: kill the lock screen and mark
    setup complete so boot goes straight to HOME. Idempotent."""
    adb(acct, "root")
    time.sleep(2)
    adb_connect(acct)
    for args in (
        ("shell", "locksettings", "set-disabled", "true"),
        ("shell", "settings", "put", "secure", "lockscreen.disabled", "1"),
        ("shell", "settings", "put", "global", "device_provisioned", "1"),
        ("shell", "settings", "put", "secure", "user_setup_complete", "1"),
        # Suppress the "Viewing full screen / swipe down to exit" immersive
        # confirmation so it never appears when the kiosk/game hides bars.
        ("shell", "settings", "put", "secure",
         "immersive_mode_confirmations", "confirmed"),
    ):
        try:
            adb(acct, *args, timeout=10)
        except Exception:
            pass
    # If the kiosk is present (system app in base-v2), make it the only
    # HOME: set default launcher + disable Bliss launchers/taskbar. All
    # per-/data and reversible; skipped cleanly on bases without it.
    r = adb(acct, "shell", "pm", "path", "com.omni.kiosk", timeout=10)
    if "package:" in (r.stdout or ""):
        adb(acct, "shell", "cmd", "package", "set-home-activity",
            "--user", "0", "com.omni.kiosk/.MainActivity", timeout=15)
        for pkg in BLISS_HOME_PACKAGES:
            adb(acct, "shell", "pm", "disable-user", "--user", "0", pkg,
                timeout=15)
        # Launch the kiosk once now so it sets the solid-black wallpaper
        # into /data before the first production boot (no wallpaper flash).
        adb(acct, "shell", "am", "start", "-n",
            "com.omni.kiosk/.MainActivity", timeout=15)
        time.sleep(3)
        print(f"[{label}] kiosk set as HOME, Bliss launchers disabled, "
              f"black wallpaper applied")
    # Which game the kiosk should launch: an adb-installed one (dev) takes
    # precedence, else the base's pre-installed system-app game (production).
    game = acct.get("game_package")
    if not game:
        try:
            game = read_config().get("base_game", {}).get(acct["base"])
        except Exception:
            game = None
    if game:
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", game, timeout=10)
        print(f"[{label}] kiosk game package = {game}")

    lockdown_and_trim(acct, label)

    print(f"[{label}] provisioned /data settings (lock screen off, "
          f"immersive confirmed, setup complete)")


# Packages a single-game kiosk never needs. Disabling frees RAM and trims
# boot (all `pm disable-user` = per-/data, reversible, no /system touched).
# GMS + Play Store are intentionally KEPT (the game may use Play Integrity).
TRIM_PACKAGES = (
    "com.google.android.setupwizard",         # setup wizard + its notification
    "com.google.android.googlequicksearchbox",  # Assistant/search (~215 MB)
    "com.google.android.apps.restore",        # device restore
    "org.blissroms.aboutbliss",               # Bliss about app
    "net.sourceforge.opencamera",             # preinstalled camera
    "com.termux",                             # preinstalled terminal
    "com.amaze.filemanager",                  # preinstalled file manager
    # Tier-1 trims (2026-07-06, measured -119 MB guest-used w/ Roblox).
    # Persistent/running services a single-game kiosk never needs:
    "org.omnirom.omnijaws",                   # weather service (was running)
    "org.lineageos.updater",                  # OTA updater (persistent)
    "com.android.touch.gestures",             # Bliss gestures (persistent;
                                              # Lock Task blocks them anyway)
    # Boot-spawned apps that idle in cached state (page-touch avoidance):
    "com.farmerbb.taskbar",                   # taskbar main pkg
    "io.chaldeaprjkt.gamespace",              # game overlay
    "player.phonograph.plus",                 # music player
    "com.android.deskclock",
    "com.android.dialer",                     # dialer UI (telephony svc kept)
    "com.android.contacts",
    "com.android.messaging",
    "com.google.android.projection.gearhead",  # Android Auto
    "com.google.android.gm.exchange",
    "com.google.android.syncadapters.calendar",
    "com.android.printspooler",
    "com.android.imsserviceentitlement",
    "com.android.cellbroadcastreceiver.module",
    # Deliberately KEPT: GMS/Play Store (Play Integrity), latin IME (login
    # typing), Settings (FallbackHome), managedprovisioning (device owner),
    # networkstack / com.android.phone / media provider (stability).
)


def lockdown_and_trim(acct, label):
    """Kiosk lockdown (device-owner Lock Task) + boot/RAM trims. All
    per-/data: sets the kiosk as device owner so it can fully disable the
    status bar / Quick-Settings pull-down / nav gestures, kills the setup
    wizard, disables unneeded apps, and zeroes animations."""
    # If an older kiosk was ever adb-installed into /data (e.g. via
    # 'kioskify'), that copy shadows the base's /system kiosk and may lack
    # the device-admin receiver -> "Unknown admin". Revert to the system
    # kiosk first so device owner can be set.
    adb(acct, "shell", "cmd", "package", "uninstall-system-updates",
        "com.omni.kiosk", timeout=30)
    # Device owner: enables the kiosk's Lock Task Mode. Works only on a
    # device with no added accounts (kiosk accounts have none). Idempotent-
    # ish: ignore "already set" failures.
    r = adb(acct, "shell", "dpm", "set-device-owner",
            "com.omni.kiosk/.OmniDeviceAdminReceiver", timeout=20)
    out = (r.stdout + r.stderr)
    if "Success" in out:
        print(f"[{label}] kiosk is DEVICE OWNER (Lock Task lockdown active)")
    elif "already" in out.lower() or "not allowed" in out.lower():
        print(f"[{label}] device owner already set / present")
    else:
        print(f"[{label}] NOTE: could not set device owner: "
              f"{out.strip()[:120]}")
    # Trim unneeded packages (RAM + boot).
    for pkg in TRIM_PACKAGES:
        try:
            adb(acct, "shell", "pm", "disable-user", "--user", "0", pkg,
                timeout=15)
        except Exception:
            pass
    # Zero UI animations (snappier, tiny boot win).
    for k in ("window_animation_scale", "transition_animation_scale",
              "animator_duration_scale"):
        adb(acct, "shell", "settings", "put", "global", k, "0", timeout=10)
    print(f"[{label}] trimmed {len(TRIM_PACKAGES)} unneeded packages, "
          f"animations off")


# ---------- commands ----------

def make_overlay(system_path, base_disk):
    """(Re)create a cheap qcow2 system overlay backed by base_disk. The
    overlay only absorbs disposable system-partition COW writes, so it is
    safe to delete and recreate against a different base (account data
    lives on the separate data.qcow2, never in this overlay)."""
    Path(system_path).unlink(missing_ok=True)
    subprocess.run([qemu_bin("qemu-img"), "create", "-f", "qcow2",
                    "-b", str(base_disk), "-F", "qcow2",
                    str(system_path)], check=True, capture_output=True)


def resolve_launch_offset(cfg, tag, requested=None, allow_none=False,
                          label=None):
    """Which Roblox OFFSET this launch boots — the whole version selection.

    Returns (offset_name, data_image_filename); both None means "boot the
    base's own /data", which on a clean base is a Roblox-less instance.

    The failure modes are separated deliberately, because they need different
    answers from the caller:

      unknown    the user named a version that is not baked -> hard error,
                 listing what IS baked. Silently falling back to the default
                 here would run the wrong Roblox under the right name, which
                 is the single most expensive way to be wrong.
      ambiguous  several offsets, none marked default -> hard error asking for
                 `omnidroid offset default <name>`.
      none       nothing baked at all -> hard error UNLESS allow_none (the
                 `--apk` / `--offset none` paths supply their own build).
    """
    base = (cfg.get("bases") or {}).get(tag) or {}
    name, entry, why = offsets_mod.resolve_offset(base, requested)
    known = list(offsets_mod.offsets_of(base))
    if why == "unknown":
        fail("no_offset",
             f"no Roblox offset '{requested}' on base '{tag}'. Baked: "
             f"{known or 'none'}. Bake one with `omnidroid offset create "
             f"{requested} --apk <path>`.")
    if why == "ambiguous":
        fail("no_default_offset",
             f"base '{tag}' has {len(known)} offsets ({', '.join(known)}) and "
             f"no default. Pick one for this launch with `--offset <name>`, "
             f"or set it once with `omnidroid offset default <name>`.")
    if name is None:
        if not allow_none:
            fail("no_offset",
                 f"base '{tag}' has NO Roblox baked (the base ships clean). "
                 f"Bake a version first: `omnidroid offset create <name> "
                 f"--apk <roblox.apk>` — or pass `--apk <path>` to install a "
                 f"build for this launch only, or `--offset none` to boot a "
                 f"deliberately game-less instance.")
        return None, None
    img = offsets_mod.offset_data_image(base, name)
    images = Path(cfg["images_dir"])
    if not (images / img).exists():
        fail("no_offset",
             f"offset '{name}' is registered on base '{tag}' but its image is "
             f"missing: {images / img}. Re-bake it (`omnidroid offset create "
             f"{name} --apk <path>`) or drop it (`omnidroid offset remove "
             f"{name}`).")
    if label:
        print(f"[{label}] roblox offset: {name}"
              + (f" ({entry.get('version_name')})" if entry.get("version_name")
                 else "")
              + ("" if why == "explicit" else "  [default]"))
    return name, img


def build_acct(name, cfg, debug=False, offset=None, allow_no_offset=False,
               label=None):
    """Build the EPHEMERAL launch handle for `name`: resolves the base tag,
    allocates a fresh port triple, and stages a per-boot efivars copy into
    runtime_dir(name) -- but writes NO account.json and creates NO overlays.
    Ephemeral instances boot the shared base templates directly (snapshot=on,
    see qemu_command_arm), so there is nothing per-account to persist; the
    handle is pure launch state (name/base/ports/debug), same key shape as
    load_account()'s but always carrying ports since this is what actually
    reserves them.

    This is the LAUNCH counterpart to load_account(): load_account reads an
    identity that may or may not be running; build_acct allocates a fresh
    instance to run. arm-only by design (the product is arm).

    `debug` is a per-BOOT flag: it flows to spawn_qemu to attach the devkit
    disk (vdc). It does NOT change the base — production and debug boot the
    exact same dual-use image.

    `offset` is the per-BOOT Roblox VERSION (see omnidroid/offsets.py). It is
    likewise not a property of the account: there is exactly one account
    identity and it can be launched on any baked version. None means "the
    base's default offset"."""
    if not re.fullmatch(r"[A-Za-z0-9_-]+", name):
        fail("bad_name",
             f"instance/username must be [A-Za-z0-9_-]+ (got '{name}')")
    tag = _select_base_tag(cfg, arch="arm")
    base = cfg["bases"][tag]
    if base_type(base) != BASE_TYPE_ARM:
        fail("arch_boundary",
             f"instances are arm-only; base '{tag}' is {arch_of_base(base)}")
    # Resolved BEFORE any port is reserved: a launch that names a version this
    # host has not baked must fail having allocated nothing, the same rule
    # cmd_start already follows for a missing cookie.
    off_name, data_image = resolve_launch_offset(
        cfg, tag, offset, allow_none=allow_no_offset, label=label)
    ensure_qemu()
    # Hold the launch lock across allocate + reserve ONLY (tiny critical
    # section): a lock alone isn't enough (allocate-then-release before spawn
    # would let a concurrent launcher see the same slot free), so the
    # reservation write happens BEFORE the lock releases -- see
    # _launch_lock/_reserve_ports docstrings.
    with _launch_lock():
        adb_port, qmp_port, vnc_port = allocate_ports(cfg)
        _reserve_ports(name, adb_port, qmp_port, vnc_port)
    d = runtime_dir(name)
    d.mkdir(parents=True, exist_ok=True)
    import shutil
    images = Path(cfg["images_dir"])
    efi_tmpl = images / base.get("efivars", ARM_BASE_EFIVARS)
    if not efi_tmpl.exists():
        fail("no_base", f"arm base efivars template missing: {efi_tmpl}")
    shutil.copyfile(efi_tmpl, d / "efivars.fd")
    return {"name": name, "base": tag,
            "adb_port": adb_port, "qmp_port": qmp_port, "vnc_port": vnc_port,
            "ephemeral": True, "debug": bool(debug),
            "offset": off_name, "data_image": data_image,
            "game_package": ROBLOX_PACKAGE, "first_boot_done": True}




def _make_persistent_arm_account(name, cfg, tag=None):
    """Create a PERSISTENT (non-ephemeral) thin arm account on base `tag`
    (default: the config's current/effective base): COW overlays of the
    base's already-provisioned system+data pair, plus a per-account efivars
    copy -- the same disk layout `cmd_create`'s arm branch used to produce
    (see the deleted `_create_arm`'s non-ephemeral path). Already-provisioned
    (kiosk, device-owner, HOME baked into the template), so first_boot_done
    is set immediately; no boot happens here.

    This exists ONLY for base-build/maintenance flows that need a real,
    on-disk overlay to boot and write into -- update_kiosk_arm's throwaway
    capture account, cmd_test_apk's disposable dev harness. It is NOT part
    of the product's login/start path, which is fully ephemeral via
    build_acct() and writes nothing to disk (see build_acct's docstring).
    arm-only, matching build_acct."""
    tag = tag or _select_base_tag(cfg)
    base = cfg["bases"][tag]
    if base_type(base) != BASE_TYPE_ARM:
        fail("arch_boundary", f"base '{tag}' is not arm-uefi")
    if not re.fullmatch(r"[A-Za-z0-9_-]+", name):
        fail("bad_name", f"account name must be [A-Za-z0-9_-]+ (got '{name}')")
    d = account_dir(name)
    if (d / "account.json").exists():
        fail("engine_error", f"account '{name}' already exists")
    ensure_qemu()
    d.mkdir(parents=True, exist_ok=True)
    adb_port, qmp_port, vnc_port = allocate_ports(cfg)
    acct = {"name": name, "base": tag,
            "adb_port": adb_port, "qmp_port": qmp_port, "vnc_port": vnc_port,
            "first_boot_done": True, "created": time.time()}
    import shutil
    images = Path(cfg["images_dir"])
    efi_tmpl = images / base.get("efivars", ARM_BASE_EFIVARS)
    if not efi_tmpl.exists():
        fail("no_base", f"arm base efivars template missing: {efi_tmpl}")
    make_overlay(d / "system.qcow2", images / base["system"])
    make_overlay(d / "data.qcow2", images / base["data"])
    shutil.copyfile(efi_tmpl, d / "efivars.fd")
    acct["game_package"] = ROBLOX_PACKAGE
    save_account(acct)
    print(f"[create {name}] arm64 disks ready (provisioned pair copied from "
          f"{base['system']}+{base['data']}); adb {adb_port}, qmp {qmp_port}, "
          f"vnc {vnc_port}")
    # Return the handle we just built -- NOT load_account(name), which is now
    # store-based and would not find this persistent folder-only build account
    # (it has no store entry). This handle carries real per-account overlays,
    # so `ephemeral` is absent/false -> qemu_command_arm boots the overlays,
    # not the shared snapshot=on templates. Base-build only (update_kiosk_arm,
    # cmd_test_apk); the product path never comes here.
    return acct


def _load_persistent_arm_account(name):
    """Load an existing persistent (folder-backed) base-build account's handle
    from accounts/<name>/account.json. The base-build/maintenance counterpart
    to load_account() -- which is store-based (for ephemeral product accounts)
    and cannot see a folder-only build account. Used for the `test-apk --reuse`
    path. Backfills game_package for older folder records."""
    p = account_dir(name) / "account.json"
    if not p.exists():
        fail("no_account",
             f"no such build account '{name}' (looked for {p})")
    acct = json.loads(p.read_text())
    acct.setdefault("game_package", ROBLOX_PACKAGE)
    return acct


# Magisk's su on this all-read-only LineageOS lives in Magisk's own tmpfs, NOT
# in $PATH, so a bare `su` fails ("inaccessible or not found"). Probe the known
# spots. The dev /data template pre-grants shell (Forever), so a granted su
# returns uid 0 with no prompt.
SU_CANDIDATES = ("/debug_ramdisk/su", "/sbin/su", "su")


def resolve_su(acct):
    """Return the working Magisk su path in the guest ('/debug_ramdisk/su' etc.)
    or None if root is unavailable — NOT granted / not rooted / not present.

    CRUCIAL: on a rooted base whose /data has NOT pre-granted the shell, the very
    first `su` request pops MagiskSU's approval dialog and BLOCKS until someone
    taps it — so the adb call hangs and times out. That must read as 'no su',
    never as an exception that propagates into the boot flow (it would crash
    every production boot). So a timeout/error on a candidate is swallowed and
    treated as 'this candidate did not grant'."""
    for cand in SU_CANDIDATES:
        # Must go through `sh -c` (matches _devkit_activate's invocation):
        # MagiskSU's getopt permutes argv, so a bare trailing `-u` (as in
        # `su 0 id -u`) is misread as an unrecognized su OPTION (usage/exit 2)
        # instead of being passed to `id`.
        try:
            r = adb(acct, "shell", f"{cand} 0 sh -c {shlex.quote('id -u')}",
                    timeout=8)
        except Exception:  # noqa: BLE001 — a prompting su hangs -> timeout; not root
            continue
        if (r.stdout or "").strip().splitlines()[-1:] == ["0"]:
            return cand
    return None


def _magisk_pkg(acct, su=None):
    """The installed Magisk manager app's package name, or None. Magisk can be
    installed under its default 'com.topjohnwu.magisk' OR a hidden/random
    ('repackaged') package with no 'magisk' in the name — so if the name scan
    fails we ask the Magisk daemon itself (root) for its stored requester package.
    Best-effort; never raises."""
    try:
        r = adb(acct, "shell", "pm", "list", "packages", timeout=10)
    except Exception:
        r = None
    pkgs = [ln.split(":", 1)[1].strip() for ln in ((r.stdout if r else "") or "").splitlines()
            if ln.startswith("package:")]
    if "com.topjohnwu.magisk" in pkgs:
        return "com.topjohnwu.magisk"
    for p in pkgs:
        if "magisk" in p.lower():
            return p
    # Hidden/repackaged: the daemon stores the manager package as 'requester'.
    su = su or resolve_su(acct)
    if su:
        q = 'magisk --sqlite "SELECT value FROM strings WHERE key=\'requester\'"'
        try:
            rr = adb(acct, "shell", f"{su} 0 sh -c {shlex.quote(q)}", timeout=15)
            for ln in ((rr.stdout or "")).splitlines():
                ln = ln.strip()
                # rows print as: value=<pkg>
                if ln.startswith("value=") and "." in ln[6:]:
                    cand = ln[6:].strip()
                    if cand in pkgs or not pkgs:
                        return cand
        except Exception:
            pass
    return None


def resolve_game_package(acct, cfg=None):
    """Which package the kiosk should treat as THE GAME, or None.

    An adb-installed game on the account handle wins (that is what `--apk`
    and dev/test flows set); otherwise the base's own pre-installed game from
    `base_game` in the config. Never raises on a missing/!dict config — the
    caller degrades to leaving the setting alone."""
    game = acct.get("game_package")
    if game:
        return game
    try:
        return (cfg or {}).get("base_game", {}).get(acct["base"])
    except Exception:  # noqa: BLE001 — no config, wrong shape, unreadable
        return None


def assert_kiosk_game(acct, cfg, label):
    """Tell the kiosk what the game is, on EVERY boot, then re-front it.

    THE BUG THIS EXISTS FOR (traced live 2026-08-06 on the rooted arm base).
    The kiosk's resolveGamePackage() falls back to "the first launchable
    NON-SYSTEM app" when Settings.Global `omni_game_package` is unset. The
    setting was only ever written by deliver_session (whose own comment says
    it is for "a LATER REBOOT") and by provision_settings (which never runs on
    the arm bases). Instances are EPHEMERAL, so /data is discarded at
    power-off and that later reboot never inherits it: every boot came up with
    the setting unset. Observed:

        02:15:40.882  OmniKiosk: launching com.topjohnwu.magisk (boot)
        02:15:43.274  settings put global omni_game_package com.roblox.client
        02:15:44.904  E ActivityTaskManager: Attempted Lock Task Mode violation
                         r=...com.roblox.client/.ActivityProtocolLaunch

    Before the dual-use change that fallback found nothing to pick. Rooting
    production installed the Magisk MANAGER as a launchable non-system app, so
    the guess started landing on it — and the kiosk then whitelisted and
    PINNED Magisk under Lock Task, which is why the real game's launch was
    refused afterwards. Roblox itself is a SYSTEM app here
    (/product/app/Roblox/Roblox.apk) and can never win that scan, so the
    setting is the only thing that can select it.

    Writing it before the session arrives, and then re-fronting the kiosk so
    it re-runs launchGame() (which re-whitelists and re-pins the right
    package), is the fix at the cause rather than at the symptom."""
    game = resolve_game_package(acct, cfg)
    if game:
        try:
            adb(acct, "shell", "settings", "put", "global",
                "omni_game_package", game, timeout=15)
            print(f"[{label}] kiosk game package = {game}")
        except Exception as e:  # noqa: BLE001 — never fail a boot over this
            print(f"[{label}] could not set omni_game_package: {e}")
    # Best-effort, like every other post-boot assertion here: an instance that
    # could not be re-fronted must still end up booted and reachable, so this
    # reports and returns rather than propagating into the boot.
    try:
        return _assert_kiosk_foreground(acct, label)
    except Exception as e:  # noqa: BLE001
        print(f"[{label}] kiosk UI: could not re-front ({e}); the instance is "
              f"up — check `omnidroid screenshot` to see what is on screen")
        return {"kiosk_foreground": False, "reason": "error"}


def _assert_kiosk_foreground(acct, label):
    """Make the KIOSK the visible UI on a dev instance: keep it as HOME and
    foreground it, and stop the Magisk manager app so its 'additional setup' /
    root screen doesn't sit on top of the kiosk at boot. All plain adb shell
    (no su needed); best-effort, reversible, idempotent. The dev /data is a copy
    of the provisioned production /data, so the kiosk IS installed + set as HOME —
    this just re-asserts it after the rooted dev boot. Returns a small dict."""
    kiosk = KIOSK_PACKAGE
    r = adb(acct, "shell", "pm", "path", kiosk, timeout=10)
    if "package:" not in (r.stdout or ""):
        return {"kiosk_foreground": False, "reason": "kiosk_not_installed"}
    # Re-assert kiosk as HOME (harmless if already), stop the Magisk app if it is
    # foregrounding, then bring the kiosk to the front.
    try:
        adb(acct, "shell", "cmd", "package", "set-home-activity",
            "--user", "0", f"{kiosk}/.MainActivity", timeout=15)
    except Exception:
        pass
    mpkg = _magisk_pkg(acct)
    if mpkg:
        try:
            adb(acct, "shell", "am", "force-stop", mpkg, timeout=15)
        except Exception:
            pass
    try:
        adb(acct, "shell", "am", "start", "-n", f"{kiosk}/.MainActivity", timeout=15)
    except Exception:
        pass
    print(f"[{label}] dev UI: kiosk foregrounded"
          + (f", Magisk app ({mpkg}) stopped" if mpkg else "") + ".")
    return {"kiosk_foreground": True, "magisk_pkg": mpkg}


def _devkit_activate(acct, label):
    """Activate the devkit disk after a DEBUG boot: mount vdc read-only and
    stage the omni-* tools into an exec-capable dir (/data/local/tmp/omni-devkit).
    Needs Magisk root (su) — the tools all run as root. Best-effort: on an
    unrooted base it explains what to do and returns without failing the start.
    Only called on a `--debug` boot (the caller gates it), so the vdc disk is
    present. Returns a small status dict."""
    # Root via Magisk su (the arm base is a 'user' build — `adb root` is NOT
    # available; root comes only from the patched-boot Magisk daemon). The
    # shipped /data pre-grants shell, so this is headless (no su prompt).
    su = resolve_su(acct)
    if not su:
        print(f"[{label}] devkit: Magisk root not available (su denied/missing). "
              f"The base is not rooted — frida can't attach and hiding is off. "
              f"Build the rooted image with: omnidroid root-base <tag>")
        return {"activated": False, "reason": "no_root", "devkit_disk": True}
    # Mount vdc ro and copy the scripts + manifest to an exec-capable dir. (The
    # frida-server binary is read from the mount by omni-fridad; /mnt is noexec
    # so we never exec directly from the mount.)
    script = (
        f"mkdir -p {DEVKIT_MOUNT} {DEVKIT_WORK} && "
        f"{{ grep -q ' {DEVKIT_MOUNT} ' /proc/mounts || "
        f"mount -o ro /dev/block/vdc {DEVKIT_MOUNT}; }}; "
        f"cp {DEVKIT_MOUNT}/omni-* {DEVKIT_WORK}/ 2>/dev/null; "
        f"cp {DEVKIT_MOUNT}/manifest.json {DEVKIT_WORK}/ 2>/dev/null; "
        f"chmod 755 {DEVKIT_WORK}/omni-* 2>/dev/null; "
        # Populate the Magisk app-managed env (/data/adb/magisk) if it's missing.
        # The offline --patch-boot roots the device (magiskd + su work) but never
        # fills /data/adb/magisk — that's normally the Magisk app's Direct-Install
        # step. Without it the app pops "Requires additional setup / reboot" and
        # its own env_check() fails. The devkit /bin carries the exact matching
        # arm64 binaries + util_functions.sh/boot_patch.sh (same build that
        # patched the boot), so copy them in (Magisk's fix_env behaviour).
        # Idempotent: only runs when the required files are absent. Copy-only
        # (/mnt is noexec, but we never exec from it).
        f"MBIN=/data/adb/magisk; "
        f"NEED='busybox magiskboot magiskinit magiskpolicy util_functions.sh boot_patch.sh'; "
        f"envok=1; for f in $NEED; do [ -f $MBIN/$f ] || envok=0; done; "
        f"if [ $envok = 0 ] && [ -d {DEVKIT_MOUNT}/bin ]; then "
        f"rm -rf $MBIN/* 2>/dev/null; mkdir -p $MBIN; chmod 700 /data/adb; "
        f"cp -af {DEVKIT_MOUNT}/bin/. $MBIN/ && chmod -R 755 $MBIN "
        f"&& chown -R 0:0 $MBIN && echo MAGISK_ENV_FIXED || echo MAGISK_ENV_FAIL; "
        f"fi; "
        f"[ -f {DEVKIT_WORK}/manifest.json ] && echo ACTIVATED || echo NO_MANIFEST"
    )
    r = adb(acct, "shell", f"{su} 0 sh -c {shlex.quote(script)}", timeout=45)
    out = (r.stdout or "") + (r.stderr or "")
    if "ACTIVATED" in out:
        if "MAGISK_ENV_FIXED" in out:
            print(f"[{label}] devkit: populated Magisk env (/data/adb/magisk) "
                  f"from the devkit — the Magisk app's 'Requires additional "
                  f"setup' prompt is now cleared (no reboot needed).")
        elif "MAGISK_ENV_FAIL" in out:
            print(f"[{label}] devkit: WARNING could not populate /data/adb/magisk "
                  f"(the Magisk app may still ask for 'additional setup').")
        print(f"[{label}] devkit: activated (root {su}, vdc -> {DEVKIT_MOUNT}, "
              f"tools in {DEVKIT_WORK}). frida: omni-fridad; hide: omni-hide <pkg>.")
        # Dev boots to the SAME kiosk as production: re-assert the kiosk as the
        # foreground HOME and stop the Magisk app from sitting on top (the reported
        # "Magisk screen instead of kiosk"). Switch back any time with
        # `omnidroid dev-ui <name> --show magisk`.
        kiosk_ui = _assert_kiosk_foreground(acct, label)
        return {"activated": True, "su": su, "mount": DEVKIT_MOUNT,
                "work": DEVKIT_WORK,
                "magisk_env_fixed": "MAGISK_ENV_FIXED" in out,
                "kiosk_ui": kiosk_ui}
    print(f"[{label}] devkit: activation incomplete:\n{out.strip()[-800:]}")
    return {"activated": False, "reason": "mount_failed", "detail": out.strip()[-400:]}


# The game package. The shipped base is rooted, so it MUST be on the Magisk
# DenyList in production or Roblox's own root/Magisk detection would see through
# the device. _enforce_hiding adds it on every boot.
GAME_PACKAGE = "com.roblox.client"


def _enforce_hiding(acct, label):
    """Re-enforce Magisk hiding so the game sees an UNROOTED device — run on
    EVERY boot, production included. This is what makes a rooted shipped base
    safe to ship: the device is rooted, but hidden.

    Needs only `su` and the `magisk` applet, BOTH of which live in the rooted
    boot — NO devkit disk required, so it runs on a plain production boot with
    no vdc attached. Idempotent. On an UNROOTED base (no su) it is a logged
    no-op and never fails the boot, so an unrooted deployment still works.

    What it asserts (mirrors omni-magisk-setup + omni-hide, minus anything that
    needs the devkit files):
      * Zygisk + Enforce DenyList ON (the DenyList only unmounts Magisk for a
        listed app when these are on);
      * the game package ON the DenyList;
      * the classic root/verified-boot prop 'tells' normalized via resetprop.
    """
    su = resolve_su(acct)
    if not su:
        # Unrooted base (Phase-1 deployment, or root not yet built). Not an
        # error: production simply runs without root/hiding until the rooted
        # image lands. Say so once, quietly.
        print(f"[{label}] hiding: base is not rooted (no su) — skipping "
              f"DenyList/prop enforcement. This is fine for an unrooted "
              f"deployment; build the rooted image to enable it.")
        return {"enforced": False, "reason": "not_rooted"}
    pkg = acct.get("game_package") or GAME_PACKAGE
    # One root shell does everything. `magisk` (like su) lives in Magisk's own
    # tmpfs, never on $PATH, so probe the known locations. resetprop -n on each
    # prop is idempotent (it no-ops when already at the wanted value).
    # Shamiko (module id zygisk_shamiko), if installed + enabled, does the
    # hiding — and it REQUIRES DenyList ENFORCEMENT to be OFF (it reads the list
    # itself). Without Shamiko, Magisk's own Enforce-DenyList does the hiding, so
    # it must be ON. Either way: Zygisk on + the game ON the list. So the
    # enforce-flag is the ONLY thing that flips on Shamiko's presence.
    script = (
        'M=""; for m in /debug_ramdisk/magisk /sbin/magisk magisk; do '
        '"$m" -v >/dev/null 2>&1 && { M="$m"; break; }; done; '
        '[ -z "$M" ] && { echo NO_MAGISK; exit 0; }; '
        'SH=/data/adb/modules/zygisk_shamiko; '
        'if [ -d "$SH" ] && [ ! -f "$SH/disable" ]; then EN=0; echo SHAMIKO; '
        'else EN=1; fi; '
        '"$M" --sqlite "REPLACE INTO settings (key,value) VALUES(\'zygisk\',1)" >/dev/null 2>&1; '
        '"$M" --sqlite "REPLACE INTO settings (key,value) VALUES(\'denylist\',$EN)" >/dev/null 2>&1; '
        # Add to the DenyList, then CONFIRM membership — `add` returns non-zero
        # when the package is already listed (baked into the rooted /data), which
        # is success, not failure. So trust `denylist ls`, not add's exit code.
        f'"$M" --denylist add {shlex.quote(pkg)} >/dev/null 2>&1; '
        f'"$M" --denylist ls 2>/dev/null | grep -q {shlex.quote(pkg)} && echo DENY_OK || echo DENY_FAIL; '
        'for kv in ro.build.tags=release-keys ro.build.type=user '
        'ro.boot.verifiedbootstate=green ro.boot.flash.locked=1 '
        'ro.boot.veritymode=enforcing ro.boot.vbmeta.device_state=locked '
        'ro.boot.warranty_bit=0 ro.warranty_bit=0 ro.debuggable=0; do '
        'k=${kv%%=*}; v=${kv#*=}; "$M" resetprop -n "$k" "$v" >/dev/null 2>&1; done; '
        'echo HIDING_DONE'
    )
    r = adb(acct, "shell", f"{su} 0 sh -c {shlex.quote(script)}", timeout=30)
    out = (r.stdout or "") + (r.stderr or "")
    if "NO_MAGISK" in out:
        print(f"[{label}] hiding: su works but the `magisk` applet was not "
              f"found — boot is rooted by something other than Magisk? "
              f"DenyList hiding not applied.")
        return {"enforced": False, "reason": "no_magisk"}
    ok = "HIDING_DONE" in out
    deny_ok = "DENY_OK" in out
    shamiko = "SHAMIKO" in out
    hider = "Shamiko" if shamiko else "Enforce-DenyList"
    print(f"[{label}] hiding: {hider} + Zygisk, {pkg} "
          f"{'on DenyList' if deny_ok else 'DenyList add FAILED'}, "
          f"root/verified-boot props normalized.")
    return {"enforced": ok, "denylist": deny_ok, "package": pkg,
            "shamiko": shamiko}


_BOOTSTRAP_LOGIN_MARKER = "OmniBootstrap: session cookie installed"


ROBLOX_AUTH_URL = "https://users.roblox.com/v1/users/authenticated"


def _curl_json(url, cookie, timeout=15):
    """GET <url> carrying <cookie> as .ROBLOSECURITY. Returns (code, body);
    code is None when the CHECK ITSELF could not run.

    curl rather than urllib deliberately: the Python builds this ships on do
    not reliably carry a system CA store (the macOS python.org build fails
    EVERY https call with CERTIFICATE_VERIFY_FAILED), while curl works on every
    host we run. The engine already shells out to adb/qemu, so this is in
    keeping with the surrounding code.
    """
    argv = ["curl", "-s", "--max-time", str(timeout),
            "-H", f"Cookie: .ROBLOSECURITY={cookie}",
            "-H", "User-Agent: Roblox/Android",
            "-w", "\nHTTP_CODE=%{http_code}", url]
    try:
        r = subprocess.run(argv, capture_output=True, text=True,
                           timeout=timeout + 5)
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return None, ""
    out = r.stdout or ""
    m = re.search(r"HTTP_CODE=(\d+)\s*$", out)
    if not m:
        return None, out
    return int(m.group(1)), out[:m.start()]


def validate_roblox_cookie(cookie, timeout=15):
    """Ask Roblox whether <cookie> is still a live session. TRI-STATE:

        ok True  -> authenticated (also returns user_id/username)
        ok False -> Roblox definitively rejected it (401) => abort loudly
        ok None  -> the check could not run => callers MUST fail open

    Why this exists: the OmniBootstrap logcat marker only proves the APK
    INJECTED the cookie, never that Roblox ACCEPTED it. A dead cookie injects
    perfectly, emits the marker, and lands on the Sign In page -- so
    _await_bootstrap_login passes and `start` reports success against a login
    screen. Checking on the HOST, before boot, turns that silent 40s lie into
    an immediate, accurate failure.

    The cookie is never echoed back in the result (results get printed/JSON'd).
    """
    if not cookie:
        return {"ok": False, "error": "cookie_invalid",
                "detail": "no session cookie saved for this account"}
    code, body = _curl_json(ROBLOX_AUTH_URL, cookie, timeout=timeout)
    if code is None:
        return {"ok": None, "error": "check_unavailable",
                "detail": "could not reach Roblox (no network, no curl, or timeout)"}
    if code == 401:
        return {"ok": False, "error": "cookie_invalid",
                "detail": "Roblox says this session is not authenticated (HTTP 401). "
                          "The cookie is expired or was invalidated -- sign the "
                          "account in again to save a fresh one."}
    if code != 200:
        return {"ok": None, "error": "check_unavailable",
                "detail": f"Roblox returned HTTP {code}; says nothing about the cookie"}
    try:
        data = json.loads(body)
    except (ValueError, TypeError):
        return {"ok": None, "error": "check_unavailable",
                "detail": "unparseable response (captive portal / proxy?)"}
    if not isinstance(data, dict) or "id" not in data:
        return {"ok": None, "error": "check_unavailable",
                "detail": "unexpected response shape"}
    return {"ok": True, "user_id": data.get("id"),
            "username": data.get("name")}


def _await_bootstrap_login(acct, timeout=25):
    """Poll guest logcat for the OmniBootstrap login marker. A dev --apk
    install can silently be a plain/stock Roblox build that cannot read the
    delivered session cookie -- it "delivers" fine but lands on a Sign In
    page. This is the LOUD check that catches that: condition-based poll
    (dump + check, repeat), not a single fixed sleep, so it returns as soon
    as the marker shows up instead of always paying the full timeout."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            out = adb(acct, "logcat", "-d", timeout=15).stdout or ""
        except Exception:
            out = ""
        if _BOOTSTRAP_LOGIN_MARKER in out:
            return True
        time.sleep(2)
    return False


def _start_timings_stages(has_apk):
    """The stage names `cmd_start` marks, in order. Declared separately from
    the marking itself so the emitted contract is testable without a boot."""
    stages = ["boot"]
    if has_apk:
        stages.append("apk_install")
    stages += ["session_delivered", "game_foreground"]
    return stages


def cmd_start(args):
    """Boot an instance, deliver its saved Roblox session, and land either
    INSIDE a place (if one is set) or on the account's home screen, logged in,
    with no menu and no simulated taps — the product's whole point. Rejects if
    the instance is already running (one live instance per username).

    Identical on the dev and production bases: same kiosk, same session
    broadcast, same roblox:// join. The dev base only differs in what is
    additionally available (frida/Magisk + always-on screenshots)."""
    from omnidroid.runtime import reconcile_runtime
    reconcile_runtime()
    ensure_qemu()
    cfg = load_config()
    debug = _debug_boot_requested(args)
    no_warm = _no_warm_requested(args)
    if getattr(args, "apk", None):
        # --apk installs a custom Roblox build for testing. This works on EVERY
        # base (root is available on all of them now) and is INDEPENDENT of
        # --debug: it needs adb/pm install, not the frida devkit. Do not force a
        # debug boot and do not gate it — see the apk-swap-all-bases note.
        if not Path(args.apk).exists():
            return fail("bad_apk",
                        f"--apk path not found: {args.apk}")
    label = f"start {args.name}"
    json_mode = getattr(args, "json", False)

    if running_pid(args.name):
        sys.exit(f"error: '{args.name}' is already running")

    # Resolve the session BEFORE creating anything on disk. store_session() and
    # resolve_token()/account_cookie() are pure reads of the central store —
    # neither needs an instance directory to exist, so this is safe to do
    # ahead of build_acct(). That ordering is load-bearing, not cosmetic:
    # "the only way to create a profile is to log in" means a brand-new name
    # with no saved cookie and no override must fail HERE, before a real
    # instance (efivars + ports) gets allocated for it.
    place_override = (_validate_place_id(args.place)
                      if getattr(args, "place", None) is not None else None)
    sess = store_session(args.name, args, place_override=place_override)
    # --token/--token-file/--token-stdin (or the account's own saved cookie)
    # still wins over whatever store_session() found, same priority order as
    # resolve_token() always documented.
    tok = resolve_token(args)
    if tok:
        sess["token"] = tok
    if not sess.get("token") and not getattr(args, "no_token", False):
        return fail("no_token",
                    f"no saved Roblox account '{args.name}'. Sign it in first: "
                    f"`omnidroid login` (saves the account under its username), then "
                    f"`omnidroid start {args.name}`. (Or override with "
                    f"--token-file <file>, or --no-token to land on Roblox's "
                    f"own login screen.) No instance was created for "
                    f"'{args.name}'.")
    # A place is OPTIONAL: with one set, this is a JOIN; without one, it's a
    # HOME boot — logged in via the delivered cookie, no deep link, no join.
    is_join = bool(sess.get("place_id"))

    # PREFLIGHT: is the cookie still a live session? The in-guest OmniBootstrap
    # marker only proves the APK INJECTED it, never that Roblox ACCEPTED it —
    # a dead cookie injects fine, emits the marker, and lands on the Sign In
    # page, so the post-boot probe passes and we report success against a login
    # screen (observed 2026-07-20). Ask Roblox HERE, before spending a ~40s
    # boot, and fail with an accurate reason instead of a confident lie.
    # FAILS OPEN: ok is None when the check itself could not run (offline host,
    # no curl, Roblox 5xx) — never block a boot on our own blindness.
    if sess.get("token") and not getattr(args, "no_cookie_check", False):
        chk = validate_roblox_cookie(sess["token"])
        if chk["ok"] is False:
            return fail("cookie_invalid",
                        f"{chk['detail']} No instance was created for "
                        f"'{args.name}'. (Skip this check with "
                        f"--no-cookie-check.)")
        if chk["ok"] is None:
            print(f"[{label}] cookie preflight skipped: {chk['detail']}")
        else:
            print(f"[{label}] cookie preflight: live session "
                  f"(user {chk.get('username')} / {chk.get('user_id')})")

    # Only NOW do we know a session is deliverable (a real token, or the
    # explicit --no-token escape hatch) — build the launch handle. Nothing is
    # persisted here: the store owns the account's cookie (via `omnidroid login`)
    # and its default place (via `omnidroid session --place`); --place above is a
    # one-off override for THIS launch only.
    # WHICH Roblox: the named offset, else the base's default. `--apk` and
    # `--offset none` are the two ways to say "boot the clean base", because
    # both supply (or deliberately omit) the build themselves.
    want_offset = getattr(args, "offset", None)
    if getattr(args, "no_offset", False):
        want_offset = offsets_mod.NO_OFFSET
    acct = build_acct(args.name, cfg, debug=debug, offset=want_offset,
                      allow_no_offset=bool(getattr(args, "apk", None)),
                      label=label)

    from omnidroid.timings import Timings
    timings = Timings()
    booted, first = _ensure_booted(acct, cfg, label,
                                   timeout=getattr(args, "timeout", None),
                                   accel=getattr(args, "accel", None),
                                   mode_name=getattr(args, "mode", None),
                                   mem=getattr(args, "mem", None),
                                   smp=getattr(args, "smp", None),
                                   balloon=getattr(args, "balloon", None),
                                   quality=getattr(args, "quality", None),
                                   debug=debug, no_warm=no_warm)
    timings.mark("boot")
    result = {"name": args.name, "place_id": sess.get("place_id"),
              "deeplink": roblox_deeplink(sess), "first_boot": first,
              "arch": acct_arch(acct), "debug": bool(debug),
              "offset": acct.get("offset"),
              "adb_port": acct["adb_port"], "vnc_port": acct["vnc_port"],
              "session": public_session(sess)}
    if not booted:
        result.update({"ok": False, "booted": False, "error": "boot_timeout"})
        result["timings"] = timings.as_dict()
        if json_mode:
            emit_json(result)
        sys.exit(1)

    # --apk: install a custom Roblox build on ANY freshly-booted base BEFORE
    # any session is delivered. A failed install must not hand the account a
    # session it can't actually run -- abort here instead.
    if getattr(args, "apk", None):
        ir = _install_apk(acct, args.apk, label)
        if not ir.get("ok"):
            result.update({"booted": True, "ok": False,
                           "error": ir.get("error", "apk_install_failed"),
                           "detail": ir.get("detail")})
            result["timings"] = timings.as_dict()
            if json_mode:
                emit_json(result)
            sys.exit(1)
        timings.mark("apk_install")

    # start ALWAYS launches: the kiosk joins when the session carries a place,
    # else lands on home (logged in). play=is_join was the home-mode bug --
    # play=False told the kiosk not to launch at all, so home never appeared.
    status = deliver_session(acct, label, sess, play=True)
    timings.mark("session_delivered")
    result.update({"booted": True, "ok": bool(status.get("delivered")),
                   **{k: v for k, v in status.items() if k != "kiosk"}})
    result["kiosk"] = status.get("kiosk")

    # Only now does the game process exist — the kiosk launches it in response
    # to the broadcast above. Pinning it to the top-app cpuset any earlier
    # finds no pid and silently does nothing (see gaming.build_pin_game_step).
    # EVERY performance-profile mode gets the pin, not just `gaming`: the
    # latency-critical scheduler set is what makes an instance feel like a
    # game, and `playable` is the mode a human and the AI actually use.
    if resolve_mode(read_config(), getattr(args, "mode", None)).get(
            "profile", "performance") == "performance":
        pin_game_to_top_app(acct, label)
    timings.mark("game_foreground")

    # LOUD failure check (only when a custom --apk was installed): deliver_session
    # reporting "delivered" only means the cookie broadcast reached the app -- it
    # says nothing about whether the app could actually USE it. A plain/stock
    # Roblox APK (no OmniBootstrap) silently lands on a Sign In page while
    # everything above reports success. Only probe when --apk was used and
    # delivery itself succeeded -- never on the trusted/baked path, and never
    # when delivery already failed (that error is the real one).
    if getattr(args, "apk", None) and result.get("ok"):
        if not _await_bootstrap_login(acct):
            result["ok"] = False
            result["error"] = "not_logged_in"
            print(f"[{label}] the installed APK logged in NO ONE (no "
                  f"'{_BOOTSTRAP_LOGIN_MARKER}' in logcat within 25s) -- a "
                  f"plain/stock Roblox cannot read the session cookie; "
                  f"build a login-capable APK via omni-agent's "
                  f"inject_session_bootstrap.")
            result["timings"] = timings.as_dict()
            if json_mode:
                emit_json(result)
            sys.exit(1)

    # Open a live WINDOW onto this instance so you can watch/play it, and so two
    # `omnidroid start` runs give two accounts side by side. Each viewer is its own
    # detached process bound to this instance's own VNC port, so N windows for N
    # accounts just work. Default ON for interactive use; suppressed by
    # --no-window and by --json (a machine/automation caller drives via capture).
    want_window = _want_vnc_viewer(
        native_window=_booted_with_native_window(args.name),
        explicit_window=getattr(args, "window", False),
        json_mode=json_mode,
        no_window=getattr(args, "no_window", False))
    viewer_pid = None
    if result["ok"] and want_window:
        try:
            if _wait_for_vnc("127.0.0.1", acct["vnc_port"], timeout=10):
                title = (f"omni: {args.name}  (place {sess['place_id']})"
                         if is_join else f"omni: {args.name}  (home)")
                viewer_pid = _spawn_builtin_viewer(
                    args.name, "127.0.0.1", acct["vnc_port"], title).pid
                result["viewer_pid"] = viewer_pid
        except Exception as e:  # noqa: BLE001 — a window failure must not fail start
            print(f"[{label}] could not open a window: {e}")

    result["timings"] = timings.as_dict()
    if json_mode:
        emit_json(result)
    elif result["ok"]:
        where = (f"window opened (pid {viewer_pid})" if viewer_pid
                 else f"vnc 127.0.0.1:{acct['vnc_port']} to watch")
        if is_join:
            print(f"[{label}] joined place {sess['place_id']} — {where}")
        else:
            print(f"[{label}] logged in on home — {where}")
    else:
        print(f"[{label}] session NOT applied: "
              f"{status.get('reason')} {status.get('detail', '')}")
        sys.exit(1)


def _shutdown(acct, label, timeout=90):
    """Graceful in-guest shutdown, then QMP quit fallback. Host-side
    fallback is mandatory: never rely on the guest self-killing.
    Returns the method that brought the instance down:
    'not-running' | 'powerdown' | 'qmp-quit' | 'killed' | 'kill-failed'.
    Every path is hard-bounded: timeout + 5s QMP wait + 2s kill wait."""
    name = acct["name"]
    pid = running_pid(name)
    if not pid:
        print(f"[{label}] not running")
        return "not-running"
    # In-guest power-off. x86/Bliss uses KernelSU root `svc power shutdown`.
    # arm/LineageOS is a user build (no adb root) and its ACPI powerdown does
    # NOT halt an idle guest (proof-of-life finding), so use `reboot -p` (the
    # guest-side path verified to power the arm image off reliably).
    try:
        if acct_base_is_arm(acct):
            adb(acct, "shell", "reboot", "-p", timeout=10)
        else:
            adb(acct, "shell", "svc", "power", "shutdown", timeout=10)
        print(f"[{label}] sent in-guest shutdown, waiting for QEMU exit...")
    except Exception:
        print(f"[{label}] adb unreachable, using QMP fallback")
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not pid_alive(pid):
            print(f"[{label}] instance is down (clean)")
            return "powerdown"
        time.sleep(3)
    print(f"[{label}] guest did not power off in {timeout}s - QMP quit")
    qmp(acct, "quit")
    time.sleep(5)
    if not pid_alive(pid):
        return "qmp-quit"
    print(f"[{label}] still alive — killing pid {pid}")
    if IS_WINDOWS:
        subprocess.run(["taskkill", "/PID", str(pid), "/F"],
                       capture_output=True)
    else:
        import os
        import signal
        os.kill(pid, signal.SIGKILL)
    time.sleep(2)
    return "killed" if not pid_alive(pid) else "kill-failed"


def cmd_stop(args):
    """Power the instance OFF. This is distinct from a viewer merely
    disconnecting: a VNC/adb disconnect is a no-op (the instance keeps
    running headless — the default); stop is the explicit power path.

    Diskless model: a successful power-off also wipes runtime/<name>/
    (efivars, run.json, qemu.log, autocap frames) -- that dir is the ONLY
    per-instance state an ephemeral account writes, so this IS the entire
    teardown. Skipped if the shutdown chain had to give up (kill-failed):
    the instance may still be alive, and wiping run.json would orphan a
    live QEMU process (running_pid would stop seeing it)."""
    acct = load_account(args.name)
    was_running = bool(running_pid(args.name))
    # Stop the always-on recorder first so it finalizes cleanly instead of
    # racing the VNC teardown as QEMU powers off.
    stop_autocap(args.name)
    method = _shutdown(acct, f"stop {args.name}", timeout=args.timeout)
    ok = method != "kill-failed"
    if ok:
        _wipe_runtime(args.name)
    if getattr(args, "json", False):
        emit_json({"name": args.name, "was_running": was_running,
                   "stopped": ok, "method": method,
                   "runtime_wiped": ok, "ok": ok})
    if not ok:
        sys.exit(1)


def _assert_deletable(path):
    """Guardrail for the ONLY destructive command (remove): the resolved
    target must live strictly inside accounts/ — structurally incapable
    of touching a base image, the images dir, or anything else. Resolve
    first so '..' or symlink tricks cannot escape; double-check that the
    images dir is not inside the deletion target (misconfig protection)."""
    p = Path(path).resolve()
    root = ACCOUNTS_DIR.resolve()
    if root not in p.parents:
        sys.exit(f"error: refusing to delete {p}: outside {root}")
    try:
        images = Path(images_dir(read_config())).resolve()
    except Exception:
        images = None
    if images and (images == p or p in images.parents):
        sys.exit(f"error: refusing to delete {p}: it contains the images "
                 f"dir {images}")
    return p


def cmd_remove(args):
    """DESTRUCTIVE: stop the instance if running, then delete its identity.

    Diskless model: an account IS a store entry (omnidroid/accounts.py) plus
    whatever it left in runtime/<name>/ while running. There is no
    per-account folder for a product account any more, so 'remove' deletes
    the store record and wipes runtime/<name>/ -- that's the entire
    footprint. A legacy accounts/<name>/ folder (base-build/maintenance
    accounts made by _make_persistent_arm_account et al, or leftovers from
    before this migration) is also rmtree'd here IF one happens to exist,
    but its absence is never an error."""
    name = args.name
    # Exact-name only: same charset create enforces; no globs, no partial
    # matches, and no path separators can ever reach the delete path.
    if not re.fullmatch(r"[A-Za-z0-9_-]+", name):
        sys.exit("error: account name must match [A-Za-z0-9_-]+ exactly "
                 "(no globs/partial names)")
    from omnidroid import accounts as _acc
    rec = _acc.get_account(_store_root(), name)
    has_runtime = runtime_dir(name).exists()
    legacy_dir = account_dir(name)
    has_legacy = legacy_dir.exists()
    if rec is None and not has_runtime and not has_legacy:
        sys.exit(f"error: no such account '{name}'")
    was_running = bool(running_pid(name))
    if was_running:
        # load_account needs either a store record or a live run.json to
        # build a handle -- both are guaranteed here since was_running=True
        # implies a live runtime/<name>/run.json.
        acct = load_account(name)
        stop_autocap(name)
        _shutdown(acct, f"remove {name}", timeout=args.timeout)
        if running_pid(name):
            sys.exit(f"error: '{name}' would not stop; NOT deleting")
    store_removed = _acc.remove_account(_store_root(), name)
    _wipe_runtime(name)
    legacy_removed = False
    if has_legacy:
        target = _assert_deletable(legacy_dir)
        import shutil
        for _ in range(10):
            try:
                shutil.rmtree(target)
                legacy_removed = True
                break
            except PermissionError:
                time.sleep(1)     # QEMU may still be releasing file handles
        else:
            sys.exit(f"error: could not delete {target} (files still locked)")
    print(f"[remove {name}] store entry {'cleared' if store_removed else '(none)'}"
          f", runtime/{name}/ wiped"
          + (f", legacy folder {target} deleted" if legacy_removed else ""))
    if getattr(args, "json", False):
        emit_json({"name": name, "removed": True,
                   "was_running": was_running,
                   "store_removed": store_removed,
                   "runtime_wiped": True,
                   "legacy_folder_removed": legacy_removed,
                   "ok": True})


# ---------- base migration (update accounts to a newer base) ----------

def migrate_account(name, cfg, target=None, reprovision=True):
    """Repoint an account's disposable system overlay onto `target` base
    (default: current). The account's data.qcow2 is NEVER touched, so all
    logins/settings/installed apps survive. Re-provisioning (idempotent)
    applies the new base's kiosk/HOME/settings. This is how a base update
    (e.g. an updated pre-installed game) reaches every account without
    erasing per-account data."""
    cfg = cfg or load_config()
    target = target or cfg["current_base"]
    acct = load_account(name)
    label = f"update {name}"
    # Overlay repoint is an x86-bliss mechanism only. arm-uefi accounts are
    # copies of a provisioned matched pair (system overlay + FBE /data) —
    # repointing their overlay would break FBE decrypt AND destroy the
    # provisioned state. Never cross the architecture boundary.
    if base_type(cfg["bases"][target]) == BASE_TYPE_ARM \
            or acct_base_is_arm(acct):
        print(f"[{label}] SKIP: arm-uefi bases/accounts don't migrate via "
              f"overlay repoint (matched-pair copies); nothing done")
        return
    if acct["base"] == target:
        print(f"[{label}] already on base {target}; re-provisioning")
    if running_pid(name):
        _shutdown(acct, label)
    d = account_dir(name)
    base = cfg["bases"][target]
    base_disk = Path(cfg["images_dir"]) / base["disk"]
    old = acct["base"]
    make_overlay(d / "system.qcow2", base_disk)      # data.qcow2 untouched
    acct["base"] = target
    save_account(acct)
    print(f"[{label}] overlay {old} -> {target}; data.qcow2 preserved")
    if not reprovision:
        return
    # Boot once (interactive/builder profile) and re-apply settings so the new
    # base's kiosk/system game/HOME take effect on the existing data disk.
    spawn_qemu(acct, cfg, interactive=True)
    first = not acct.get("first_boot_done")
    if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT if first
                         else NORMAL_BOOT_TIMEOUT, label, first_boot=first):
        print(f"[{label}] WARNING: boot timed out; leaving instance for "
              f"inspection")
        return
    post_boot(acct, label)
    provision_settings(acct, label)
    acct["first_boot_done"] = True
    save_account(acct)
    _shutdown(acct, label)
    print(f"[{label}] migrated to {target} and re-provisioned")


def cmd_update_base(args):
    cfg = load_config()
    target = args.to or cfg["current_base"]
    if target not in cfg["bases"]:
        fail("no_base", f"no base '{target}'. Known: {list(cfg['bases'])}")
    acct = load_account(args.name)
    # ARCH BOUNDARY (contract §7.2): arm-uefi accounts/bases are matched-pair
    # copies (FBE) — overlay repoint would break decrypt and destroy state.
    # Checked BEFORE ensure_qemu so the refusal never triggers a QEMU install.
    if base_type(cfg["bases"][target]) == BASE_TYPE_ARM \
            or acct_base_is_arm(acct):
        fail("arch_boundary",
             "arm-uefi accounts/bases do not migrate via overlay repoint "
             "(matched-pair copies); recreate from a provisioned pair")
    ensure_qemu()
    migrate_account(args.name, cfg, target=target,
                    reprovision=not args.no_reprovision)
    if getattr(args, "json", False):
        emit_json({"name": args.name, "migrated": True, "base": target,
                   "arch": acct_arch(acct), "ok": True})


# ---------- production base rebuild (update pre-installed game) ----------



# APK lib/<abi> dir -> Android system-app nativeLibraryDir name.
_ABI_TO_SYSLIB = {"arm64-v8a": "arm64", "armeabi-v7a": "arm",
                  "armeabi": "arm", "x86_64": "x86_64", "x86": "x86"}


def _bake_native_libs(acct, game_apk, appdir, label):
    """Extract native .so files from the APK and place them under
    <appdir>/lib/<abi> so a /system/app game can load them (required for
    ARM games, which libndk then translates)."""
    import zipfile
    import tempfile
    pushed = 0
    with zipfile.ZipFile(game_apk) as z:
        by_abi = {}
        for name in z.namelist():
            parts = name.split("/")
            if (len(parts) == 3 and parts[0] == "lib"
                    and name.endswith(".so")):
                by_abi.setdefault(parts[1], []).append(name)
        tmp = Path(tempfile.mkdtemp(prefix="omnilib_"))
        try:
            for abi, names in by_abi.items():
                syslib = _ABI_TO_SYSLIB.get(abi, abi)
                local = tmp / abi
                local.mkdir(parents=True, exist_ok=True)
                for name in names:
                    (local / Path(name).name).write_bytes(z.read(name))
                dst = f"{appdir}/lib/{syslib}"
                adb(acct, "shell", f"mkdir -p {dst}", timeout=15)
                adb(acct, "push", str(local) + "/.", dst + "/", timeout=300)
                adb(acct, "shell",
                    f"chmod 755 {dst}/*.so; "
                    f"chcon u:object_r:system_file:s0 {dst} {dst}/*.so",
                    timeout=30)
                pushed += len(names)
                print(f"[{label}]   libs {abi} -> lib/{syslib} "
                      f"({len(names)} .so)")
        finally:
            import shutil
            shutil.rmtree(tmp, ignore_errors=True)
    return pushed


def _build_next_base(cfg, mutate, notes, base_game=None):
    """Boot a throwaway builder on the current base, let `mutate(acct,label)`
    modify /system (remount rw already up to the caller), flatten to the
    next base version, register it, and make it current. Returns the tag."""
    import shutil
    cur = cfg["current_base"]
    nxt = _next_base_tag(cfg)
    label = f"build {cur}->{nxt}"
    images = Path(cfg["images_dir"])
    cur_disk = images / cfg["bases"][cur]["disk"]

    bname = "_builder"
    d = account_dir(bname)
    if d.exists():
        shutil.rmtree(d)
    d.mkdir(parents=True)
    adb_port, qmp_port, vnc_port = allocate_ports(cfg)
    acct = {"name": bname, "base": cur, "adb_port": adb_port,
            "qmp_port": qmp_port, "vnc_port": vnc_port,
            "first_boot_done": True}
    save_account(acct)
    make_overlay(d / "system.qcow2", cur_disk)
    shutil.copyfile(images / cfg["data_template"], d / "data.qcow2")

    try:
        print(f"[{label}] booting builder on {cur}")
        spawn_qemu(acct, cfg, interactive=True)
        if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT, label):
            sys.exit(f"[{label}] builder boot failed")
        adb(acct, "root")
        time.sleep(3)
        adb_connect(acct)
        adb(acct, "shell", "mount -o remount,rw /", timeout=20)
        mutate(acct, label)
        adb(acct, "shell", "sync", timeout=15)
        _shutdown(acct, label)

        newdisk = images / f"base-{nxt}.qcow2"
        print(f"[{label}] flattening overlay -> {newdisk} (self-contained)")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-O", "qcow2",
                        "-c", str(d / "system.qcow2"), str(newdisk)],
                       check=True)
        shutil.copyfile(images / cfg["bases"][cur]["kernel"],
                        images / f"base-{nxt}.kernel")
        shutil.copyfile(images / cfg["bases"][cur]["initrd"],
                        images / f"base-{nxt}.initrd.img")

        raw = read_config()
        raw["bases"][nxt] = {
            "disk": f"base-{nxt}.qcow2",
            "kernel": f"base-{nxt}.kernel",
            "initrd": f"base-{nxt}.initrd.img",
            "src": cfg["bases"][cur]["src"],
            "notes": notes}
        # Carry forward the prior base's pre-installed game, unless changed.
        prior_game = raw.get("base_game", {}).get(cur)
        if base_game:
            raw.setdefault("base_game", {})[nxt] = base_game
        elif prior_game:
            raw.setdefault("base_game", {})[nxt] = prior_game
        raw["current_base"] = nxt
        CONFIG_PATH.write_text(json.dumps(raw, indent=2))
        print(f"[{label}] base {nxt} built and current. "
              f"Roll out: omnidroid update-all")
    finally:
        if d.exists():
            shutil.rmtree(d, ignore_errors=True)
    return nxt


def rebuild_base(cfg, game_apk):
    """Bake/replace the pre-installed game APK as a /system/app system app in
    a NEW base version. Ephemeral instances pick it up on their next boot;
    `update-base` migrates an existing persistent account, keeping its
    data.qcow2."""
    pkg = apk_package_name(game_apk)
    if not pkg:
        sys.exit(f"could not read package name from {game_apk}")

    def mutate(acct, label):
        appdir = "/system/app/OmniGame"
        adb(acct, "push", game_apk, "/data/local/tmp/game.apk", timeout=600)
        r = adb(acct, "shell",
                f"rm -rf {appdir} && mkdir -p {appdir} && "
                f"cp /data/local/tmp/game.apk {appdir}/OmniGame.apk && "
                f"chmod 644 {appdir}/OmniGame.apk && "
                f"chcon u:object_r:system_file:s0 {appdir} && "
                f"chcon u:object_r:system_file:s0 {appdir}/OmniGame.apk && "
                f"rm /data/local/tmp/game.apk && echo BAKED", timeout=120)
        if "BAKED" not in r.stdout:
            sys.exit(f"[{label}] baking failed: {r.stdout}{r.stderr}")
        n = _bake_native_libs(acct, game_apk, appdir, label)
        print(f"[{label}] baked {pkg} as system app ({n} native libs)")

    return _build_next_base(cfg, mutate,
                            notes=f"pre-installed game {pkg} (system app)",
                            base_game=pkg)


def update_kiosk_base(cfg, kiosk_apk):
    """Replace the kiosk system app (/system/app/OmniKiosk) in a NEW base
    version — e.g. to ship a new launcher with Lock Task lockdown."""
    if not Path(kiosk_apk).exists():
        sys.exit(f"kiosk apk not found: {kiosk_apk}")

    def mutate(acct, label):
        appdir = "/system/app/OmniKiosk"
        adb(acct, "push", kiosk_apk, "/data/local/tmp/kiosk.apk",
            timeout=120)
        r = adb(acct, "shell",
                f"rm -rf {appdir} && mkdir -p {appdir} && "
                f"cp /data/local/tmp/kiosk.apk {appdir}/OmniKiosk.apk && "
                f"chmod 644 {appdir}/OmniKiosk.apk && "
                f"chcon u:object_r:system_file:s0 {appdir} && "
                f"chcon u:object_r:system_file:s0 {appdir}/OmniKiosk.apk && "
                f"rm /data/local/tmp/kiosk.apk && echo KIOSK_OK", timeout=60)
        if "KIOSK_OK" not in r.stdout:
            sys.exit(f"[{label}] kiosk swap failed: {r.stdout}{r.stderr}")
        print(f"[{label}] replaced kiosk system app")

    return _build_next_base(cfg, mutate,
                            notes="updated kiosk (Lock Task lockdown + "
                                  "boot/RAM trims)")


def cmd_rebuild_base(args):
    ensure_qemu()
    cfg = load_config()
    rebuild_base(cfg, args.game)


def update_kiosk_arm(cfg, kiosk_apk, tag, label=None):
    """Refresh an arm base's /data TEMPLATE with a new kiosk build.

    The arm bases do not carry the kiosk in /system like x86 does — it is an
    ordinary app inside the provisioned /data template (base_arm_data.qcow2 /
    base_arm_devdata.qcow2), and arm is a `user` build with no `adb root`, so
    update_kiosk_base()'s /system swap cannot work here.

    So: boot a throwaway account off the base, install the kiosk over adb, shut
    it down cleanly, and copy its /data back over the template. The template
    stays a matched FBE pair with the base's system image because the account's
    /metadata keys came from a copy of that very image.

    Without this, a FRESH account ships the kiosk build that was current when the
    template was last captured — so `omnidroid start` gets no_kiosk_reply until someone
    installs the new kiosk by hand.
    """
    kiosk_apk = Path(kiosk_apk)
    if not kiosk_apk.exists():
        return fail("engine_error", f"kiosk apk not found: {kiosk_apk}")
    base = (cfg.get("bases") or {}).get(tag)
    if not base:
        return fail("no_base", f"no base '{tag}'")
    if base_type(base) != BASE_TYPE_ARM:
        return fail("arch_boundary",
                    f"base '{tag}' is not arm; use `update-kiosk` without "
                    f"--base for the x86 /system flow")
    images = Path(cfg["images_dir"])
    tmpl = images / base["data"]
    label = label or f"update-kiosk {tag}"

    name = f"_kioskbuild_{int(time.time())}"
    live = [a["name"] for a in all_accounts() if running_pid(a["name"])]
    if live:
        return fail("instance_running",
                    f"stop running instances first ({', '.join(live)}): the "
                    f"template is rebuilt from a clean boot")
    print(f"[{label}] building a throwaway account '{name}' off base '{tag}'")
    acct = _make_persistent_arm_account(name, cfg, tag=tag)
    try:
        spawn_qemu(acct, cfg, interactive=False, mode=resolve_mode(cfg))
        if not wait_for_boot(acct, NORMAL_BOOT_TIMEOUT, label):
            return fail("boot_timeout",
                        f"the throwaway account did not boot; template "
                        f"unchanged")
        r = adb(acct, "install", "-r", "-g", "--no-incremental",
                str(kiosk_apk), timeout=300)
        out = (r.stdout or "") + (r.stderr or "")
        if "Success" not in out:
            return fail("install_failed",
                        f"kiosk install failed: {out.strip()[-300:]}")
        print(f"[{label}] installed {kiosk_apk.name}")
        # Deliberately do NOT launch the kiosk here. It would enter Lock Task
        # Mode, and lock task BLOCKS `reboot -p` — the guest then never powers
        # off, _shutdown escalates to SIGKILL, and the capture is refused. There
        # is nothing to gain either: device-owner state already lives in the
        # template's /data, and the kiosk re-applies its lock-task whitelist and
        # permission policy from onCreate on every boot.
        adb(acct, "shell", "sync", timeout=30)
        method = _shutdown(acct, label, timeout=120)
        if method in ("killed", "kill-failed"):
            return fail("engine_error",
                        f"the throwaway account would not power off ({method}); "
                        f"refusing to capture a possibly-inconsistent /data")
        # A clean power-off matters: this /data becomes the template every future
        # account overlays, so an unflushed write would corrupt all of them.
        src = account_dir(name) / "data.qcow2"
        bak = tmpl.with_suffix(".qcow2.bak")
        if not bak.exists():
            shutil.copy2(tmpl, bak)
            print(f"[{label}] backed up {tmpl.name} -> {bak.name}")
        # FLATTEN, don't copy: the account's data.qcow2 is now a THIN overlay
        # backed by this very template, so copying the file would leave the new
        # template pointing its backing at itself. qemu-img convert merges the
        # overlay + backing into a standalone image — the correct full template.
        staged = tmpl.with_suffix(".qcow2.new")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-f", "qcow2",
                        "-O", "qcow2", str(src), str(staged)],
                       check=True, capture_output=True, timeout=900)
        shutil.move(str(staged), str(tmpl))
        print(f"[{label}] captured /data -> {tmpl.name} "
              f"({tmpl.stat().st_size / 1048576:.0f} MiB, flattened)")
        return {"ok": True, "base": tag, "template": str(tmpl),
                "kiosk": str(kiosk_apk)}
    finally:
        try:
            if running_pid(name):
                _shutdown(acct, label, timeout=60)
            shutil.rmtree(account_dir(name), ignore_errors=True)
            print(f"[{label}] removed the throwaway account")
        except Exception as e:  # noqa: BLE001
            print(f"[{label}] could not clean up {name}: {e}")


def cmd_update_kiosk(args):
    ensure_qemu()
    cfg = load_config()
    tag = getattr(args, "base", None)
    if tag:
        r = update_kiosk_arm(cfg, args.apk, tag)
        if getattr(args, "json", False) and isinstance(r, dict):
            emit_json(r)
        return
    update_kiosk_base(cfg, args.apk)


# ---------- devkit build + base rooting (frida + Magisk) ----------
#
# Two orthogonal build steps, neither of which changes which base ships:
#
# `omnidroid build-devkit [--arch arm|x86]` — builds the ATTACHABLE devkit disk
#   `base_<arch>_devkit.qcow2`, an ext4 image BUILT ENTIRELY HOST-SIDE (no guest
#   boot, no root, cross-platform via `mke2fs -d`) carrying:
#     * frida-server for the guest arch (native — no translation),
#     * Magisk (the APK installer + the extracted magiskboot/magiskinit/…),
#     * the omni-* device scripts (hidden frida launch + root/frida hiding),
#     * a manifest.json (versions, hidden frida port, mount paths).
#   It belongs to no base; `omnidroid start --debug` attaches it as vdc.
#
# `omnidroid root-base [--base <tag>]` — bakes root INTO a shipped base by Magisk-
#   patching its boot into a THIN COW overlay of the production system
#   (`base_arm_system_rooted.qcow2` — see _patch_boot_into_overlay), plus a
#   matched rooted /data. The production system image is never modified and
#   current_base never changes; the base just gains `"rooted": true`.

def _curl_bin():
    """curl path if usable (present on macOS, Linux, and Windows 10+). Preferred
    over urllib because it uses the SYSTEM cert store — python.org builds ship
    without CA certs, so urllib SSL fails on a fresh machine."""
    return shutil.which("curl")


def _http_get(url, timeout, accept=None):
    """Fetch bytes from url, preferring curl (system certs) then urllib."""
    curl = _curl_bin()
    if curl:
        cmd = [curl, "-fsSL", "--max-time", str(timeout),
               "-A", "omnidroid-devbase"]
        if accept:
            cmd += ["-H", f"Accept: {accept}"]
        cmd += [url]
        r = subprocess.run(cmd, capture_output=True)
        if r.returncode == 0:
            return r.stdout
        # fall through to urllib on curl failure
    import urllib.request
    headers = {"User-Agent": "omnidroid-devbase"}
    if accept:
        headers["Accept"] = accept
    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.read()


def _http_json(url, timeout=60):
    return json.loads(_http_get(url, timeout,
                                accept="application/vnd.github+json").decode("utf-8"))


def _download(url, dest, label, timeout=300):
    print(f"[{label}] downloading {url}")
    curl = _curl_bin()
    if curl:
        r = subprocess.run([curl, "-fSL", "--max-time", str(timeout),
                            "-A", "omnidroid-devbase", "-o", str(dest), url],
                           capture_output=True)
        if r.returncode == 0 and Path(dest).exists():
            return dest
        # fall through to urllib on curl failure
    with open(dest, "wb") as f:
        f.write(_http_get(url, timeout))
    return dest


# Magisk native libs (arm64) extracted from the APK, mapped to bare names.
# The APK ships them as lib/arm64-v8a/lib<name>.so; on device they are the
# magisk multicall binaries. magiskboot patches boot images; magiskinit is the
# rooted first-stage init; magisk is the daemon/su (single binary in v26+, no
# 32/64 split); init-ld is LD_PRELOAD'd into /init. These are exactly what
# Magisk's boot_patch.sh injects into the ramdisk (+ stub.apk from assets).
_MAGISK_LIBS = ("magiskboot", "magiskinit", "magiskpolicy",
                "magisk", "init-ld", "busybox")


def _find_mke2fs():
    """Locate mke2fs (e2fsprogs) across platforms. It builds+populates an ext4
    image from a host directory (`-d`) with NO root and NO mount, so the devkit
    disk is produced identically on macOS/Linux/Windows(arm64)."""
    cand = shutil.which("mke2fs")
    if cand:
        return cand
    for p in ("/opt/homebrew/bin/mke2fs", "/opt/homebrew/opt/e2fsprogs/sbin/mke2fs",
              "/usr/local/opt/e2fsprogs/sbin/mke2fs", "/usr/local/sbin/mke2fs",
              "/usr/sbin/mke2fs", "/sbin/mke2fs"):
        if Path(p).exists():
            return p
    hint = ("install e2fsprogs: macOS `brew install e2fsprogs`, "
            "Debian/Ubuntu `sudo apt install e2fsprogs`, "
            "Windows use the e2fsprogs from MSYS2/Cygwin or the Android SDK")
    fail("mke2fs_missing", f"mke2fs not found — needed to build the devkit "
                           f"disk. {hint}")


def _build_ext4_qcow2(staging_dir, out_qcow2, label, min_mb=256):
    """Turn a host directory into a populated ext4 qcow2 (the devkit vdc disk),
    fully host-side and rootless: `mke2fs -d` writes a raw ext4 image seeded from
    staging_dir, then qemu-img converts it to qcow2. Cross-platform."""
    import tempfile
    mke2fs = _find_mke2fs()
    total = sum(f.stat().st_size for f in Path(staging_dir).rglob("*")
                if f.is_file())
    size_mb = max(min_mb, int(total / (1024 * 1024) * 1.6) + 96)
    raw = Path(tempfile.mktemp(prefix="omni_devkit_", suffix=".img"))
    try:
        subprocess.run([mke2fs, "-q", "-t", "ext4", "-L", "omnidevkit",
                        "-d", str(staging_dir), str(raw), f"{size_mb}M"],
                       check=True, capture_output=True)
        Path(out_qcow2).unlink(missing_ok=True)
        subprocess.run([qemu_bin("qemu-img"), "convert", "-O", "qcow2",
                        "-c", str(raw), str(out_qcow2)],
                       check=True, capture_output=True)
        print(f"[{label}] built devkit disk {Path(out_qcow2).name} "
              f"({size_mb} MiB ext4, {total} bytes of tools)")
    finally:
        raw.unlink(missing_ok=True)


# frida + Magisk per-arch download coordinates. The base runs its guest arch
# NATIVELY (arm64 under HVF/KVM, x86_64 under WHPX/KVM), so frida-server must
# match the guest arch, and Magisk's multicall libs come from the matching ABI
# dir inside the APK.
_DEVKIT_ARCH = {
    "arm": {"frida": "android-arm64", "abi": "arm64-v8a", "manifest_abi": "arm64-v8a"},
    "x86": {"frida": "android-x86_64", "abi": "x86_64", "manifest_abi": "x86_64"},
}


def _stage_devkit(arch, frida_version, include_magisk, frida_port, label):
    """Assemble (host-side) the directory that becomes the devkit disk root for
    `arch` ('arm' | 'x86'): the matching frida-server ELF, the Magisk APK + its
    extracted multicall binaries for that ABI, the LF-normalized omni-* scripts,
    and manifest.json. Returns {"dir": <root>, ...}. NOTHING is pushed to a
    guest here — the whole disk is built from this directory with mke2fs."""
    import tempfile
    import lzma
    import zipfile
    spec = _DEVKIT_ARCH.get(arch)
    if not spec:
        fail("bad_arch", f"devkit arch must be arm|x86, got {arch!r}")
    stg = Path(tempfile.mkdtemp(prefix="omnidevkit_"))
    (stg / "bin").mkdir()
    out = {"dir": stg, "arch": arch, "frida_version": frida_version,
           "frida_port": frida_port, "magisk": False, "magisk_version": None,
           "magiskboot": None, "tools": []}

    # frida-server for the guest arch (native, no translation), .xz -> ELF.
    frida_arch = spec["frida"]
    xz = stg / f"frida-server-{frida_version}-{frida_arch}.xz"
    url = (f"https://github.com/frida/frida/releases/download/{frida_version}/"
           f"frida-server-{frida_version}-{frida_arch}.xz")
    _download(url, xz, label)
    srv = stg / "frida-server"
    with lzma.open(xz) as zf, open(srv, "wb") as f:
        shutil.copyfileobj(zf, f)
    srv.chmod(0o755)
    xz.unlink()
    print(f"[{label}] frida-server {frida_version} ({frida_arch}) staged "
          f"({srv.stat().st_size} bytes)")

    # Magisk: keep the full APK (the on-device installer) AND extract the
    # matching-ABI multicall binaries + the patch scripts. Best-effort; build
    # continues if the download fails (root/hiding then needs a dropped Magisk).
    if include_magisk:
        abi = spec["abi"]
        try:
            meta = _http_json(
                "https://api.github.com/repos/topjohnwu/Magisk/releases/latest")
            apks = [a for a in meta.get("assets", [])
                    if a["name"].lower().endswith(".apk")]
            # Prefer the RELEASE apk ("Magisk-vN.N.apk") over "app-debug.apk".
            apk_asset = next((a for a in apks
                              if "debug" not in a["name"].lower()), None) or \
                (apks[0] if apks else None)
            if not apk_asset:
                raise RuntimeError("no .apk asset in Magisk latest release")
            apk = stg / "magisk.apk"
            _download(apk_asset["browser_download_url"], apk, label)
            with zipfile.ZipFile(apk) as z:
                names = set(z.namelist())
                for tool in _MAGISK_LIBS:
                    member = f"lib/{abi}/lib{tool}.so"
                    if member in names:
                        dst = stg / "bin" / tool
                        dst.write_bytes(z.read(member))
                        dst.chmod(0o755)
                # The Magisk patch scripts + stub app that boot_patch.sh injects.
                for asset in ("assets/boot_patch.sh", "assets/util_functions.sh",
                              "assets/stub.apk", "assets/addon.d.sh"):
                    if asset in names:
                        dst = stg / "bin" / Path(asset).name
                        dst.write_bytes(z.read(asset))
                        if asset.endswith(".sh"):
                            dst.chmod(0o755)
            mb = stg / "bin" / "magiskboot"
            # boot_patch.sh injects these into the ramdisk; verify they staged.
            need = ("magiskboot", "magiskinit", "magisk", "init-ld",
                    "stub.apk", "boot_patch.sh", "util_functions.sh")
            missing = [n for n in need if not (stg / "bin" / n).exists()]
            out["magisk"] = True
            out["magisk_version"] = meta.get("tag_name")
            out["magiskboot"] = mb if mb.exists() else None
            out["boot_patchable"] = not missing
            print(f"[{label}] Magisk {meta.get('tag_name')} staged from "
                  f"{apk_asset['name']} ({abi} "
                  f"{', '.join(t for t in _MAGISK_LIBS if (stg/'bin'/t).exists())})")
            if missing:
                print(f"[{label}] NOTE: boot-patch files missing {missing} — "
                      f"root-base may not work with this Magisk build.")
        except Exception as e:
            print(f"[{label}] WARNING: Magisk staging failed ({e}); the devkit "
                  f"disk will ship without Magisk — root + hiding are then "
                  f"unavailable until a Magisk APK is dropped into it.")

    # LF-normalized device scripts (a Windows checkout may hold CRLF; the guest
    # /system/bin/sh chokes on \r). Source of truth is devkit/. They live at the
    # disk root and are copied to an exec-capable dir on activation.
    devkit_src = REPO / "devkit"
    for name in ("omni-fridad", "omni-frida-stop", "omni-hide",
                 "omni-magisk-setup"):
        f = devkit_src / name
        if not f.exists():
            continue
        (stg / name).write_bytes(f.read_bytes().replace(b"\r\n", b"\n"))
        (stg / name).chmod(0o755)
        out["tools"].append(name)

    manifest = {
        "devkit": f"omnidroid-devkit-{arch}",
        "built": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "arch": spec["manifest_abi"],
        "frida_version": frida_version,
        "frida_port": frida_port,
        "frida_server": "frida-server",
        "frida_server_patched": "frida-server-patched (optional drop-in)",
        "magisk": bool(out["magisk"]),
        "magisk_version": out["magisk_version"],
        "magisk_apk": "magisk.apk" if out["magisk"] else None,
        "mount": DEVKIT_MOUNT,
        "work": DEVKIT_WORK,
        "root": "Magisk (baked into the shipped rooted boot)",
        "hide": "Magisk DenyList/Shamiko (hides root, Magisk, and frida)",
        "launch": "omni-fridad (start hidden) / omni-frida-stop",
        "note": "attached as the vdc disk only on a --debug boot.",
    }
    (stg / "manifest.json").write_text(json.dumps(manifest, indent=2))
    return out


def build_devkit(cfg, arch=None, frida_version=DEFAULT_FRIDA_VERSION,
                 frida_port=DEFAULT_FRIDA_PORT, include_magisk=True):
    """Build the ATTACHABLE devkit disk for `arch` (default: the host arch).

    This is the debug toolkit only (frida-server + the omni-* scripts + the
    Magisk multicall binaries), assembled fully host-side (no boot, no root) as
    a populated ext4 qcow2. It belongs to NO base entry: `omnidroid start --debug`
    attaches it as vdc. It never changes any base or current_base — rooting the
    shipped image is a separate step (`omnidroid root-base`).

    Returns the absolute path of the disk it wrote."""
    arch = arch or host_arch_token()
    label = f"build-devkit:{arch}"
    images = Path(cfg["images_dir"])
    print(f"[{label}] staging devkit (frida {frida_version} {arch}"
          f"{', + Magisk' if include_magisk else ', no Magisk'})...")
    staging = _stage_devkit(arch, frida_version, include_magisk, frida_port, label)
    try:
        devkit_disk = images / devkit_disk_name(arch)
        _build_ext4_qcow2(staging["dir"], devkit_disk, label)
        print(f"[{label}] DONE. Built {devkit_disk.name}. It attaches as vdc on "
              f"`omnidroid start --debug` / agent debug=true. No base was changed.")
        return devkit_disk
    finally:
        shutil.rmtree(staging["dir"], ignore_errors=True)


def _gpt_partition(raw_path, name):
    """Minimal GPT reader: return (start_byte, size_byte) of the partition whose
    UTF-16LE name matches `name` (e.g. 'boot'), or None. Enough to locate vda6
    in a raw disk export so its boot image can be extracted/rewritten offline —
    no nbd/libguestfs (unavailable on macOS)."""
    import struct
    with open(raw_path, "rb") as f:
        f.seek(512)                       # LBA1 = primary GPT header
        hdr = f.read(92)
        if hdr[:8] != b"EFI PART":
            return None
        part_lba = struct.unpack_from("<Q", hdr, 72)[0]
        num = struct.unpack_from("<I", hdr, 80)[0]
        esize = struct.unpack_from("<I", hdr, 84)[0]
        f.seek(part_lba * 512)
        for _ in range(num):
            e = f.read(esize)
            if len(e) < 128 or e[:16] == b"\x00" * 16:
                continue
            first = struct.unpack_from("<Q", e, 32)[0]
            last = struct.unpack_from("<Q", e, 40)[0]
            pname = e[56:128].decode("utf-16-le", "ignore").rstrip("\x00")
            if pname == name:
                return (first * 512, (last - first + 1) * 512)
    return None


# ---------- custom loading screen (arm base branding) ------------------------
# The arm base ships LineageOS's own boot animation, i.e. a vendor logo on every
# customer's screen. Replacing it is NOT a runtime setting — measured against
# LineageOS 23's frameworks/base/cmds/bootanimation/BootAnimation.cpp, the only
# paths it reads are, in order:
#
#   /apex/com.android.bootanimation/etc/bootanimation.zip
#   /product/media/bootanimation.zip        <- the one this base actually has
#   /oem/media/bootanimation.zip
#   /system/media/bootanimation.zip
#
# `/data/local/bootanimation.zip` (USER_BOOTANIMATION_FILE) is what every guide
# online still recommends; it was REMOVED from LineageOS years ago. Pushing there
# over adb is a silent no-op on this base — do not "fix" this by going back to it.
#
# So the animation has to be replaced inside `product`, an ext4 filesystem living
# in a linear extent of the `super` dynamic partition (vda2) of base_arm.qcow2.
# No mounting is involved (macOS cannot mount ext4, and mounting would need root):
# e2fsprogs' debugfs edits the filesystem in place, addressed straight into the
# raw disk export via its `?offset=` syntax. Build-machine tool, like
# build-dev-base — never something a customer runs.
BOOTANIM_FS = "product"                      # logical partition inside super
BOOTANIM_PATH = "/media/bootanimation.zip"   # path INSIDE that filesystem
BOOTANIM_SELINUX = "u:object_r:system_file:s0"

# Logical partition inside `super` that carries build.prop. `system`, not
# `product`: the boot animation lives in product, but the properties the
# framework reads at startup come from the system image.
LEAN_PROP_FS = "system"




def _debugfs_bin():
    """debugfs from e2fsprogs. Homebrew keeps it keg-only, so it is not on PATH;
    check the usual prefixes before giving up."""
    import shutil as _sh
    for cand in ("debugfs",
                 "/opt/homebrew/opt/e2fsprogs/sbin/debugfs",
                 "/usr/local/opt/e2fsprogs/sbin/debugfs",
                 "/sbin/debugfs", "/usr/sbin/debugfs"):
        p = _sh.which(cand) if "/" not in cand else (
            cand if Path(cand).exists() else None)
        if p:
            return p
    return None


def _lp_partition(raw_path, super_off, name):
    """(offset_bytes, size_bytes) of a logical partition inside a `super` image,
    or None. Minimal liblp reader (metadata_format.h): 4096 reserved, primary
    geometry, backup geometry, then the metadata slots.

    Only LINEAR extents are handled, and a partition split across several extents
    is refused rather than silently half-patched — on this base every partition is
    a single contiguous extent."""
    import struct
    RESERVED, GEO = 4096, 4096
    with open(raw_path, "rb") as f:
        f.seek(super_off + RESERVED)
        g = f.read(GEO)
        if struct.unpack_from("<I", g, 0)[0] != 0x616C4467:      # geometry magic
            return None
        meta_off = super_off + RESERVED + GEO * 2
        f.seek(meta_off)
        h = f.read(256)
        if struct.unpack_from("<I", h, 0)[0] != 0x414C5030:      # header magic
            return None
        header_size = struct.unpack_from("<I", h, 8)[0]
        p = 4 + 2 + 2 + 4 + 32                                   # -> tables_size
        tables_size = struct.unpack_from("<I", h, p)[0]
        p += 4 + 32
        parts_off, parts_num, parts_sz = struct.unpack_from("<III", h, p)
        p += 12
        exts_off, exts_num, exts_sz = struct.unpack_from("<III", h, p)
        f.seek(meta_off + header_size)
        t = f.read(tables_size)

    for i in range(parts_num):
        o = parts_off + i * parts_sz
        pname = t[o:o + 36].split(b"\x00")[0].decode("ascii", "replace")
        if pname != name:
            continue
        first, num = struct.unpack_from("<II", t, o + 40)
        if num != 1:
            return None       # multi-extent: not handled, see docstring
        e = exts_off + first * exts_sz
        num_sectors, target_type, target_data = struct.unpack_from("<QIQ", t, e)
        if target_type != 0:  # LP_TARGET_TYPE_LINEAR
            return None
        return (super_off + target_data * 512, num_sectors * 512)
    return None


def _replace_bootanimation(raw_path, fs_off, anim_zip, label):
    """Swap the boot animation inside the `product` ext4 at fs_off. Returns None
    on success, else an error string. Leaves the image untouched on failure."""
    import tempfile
    dbg = _debugfs_bin()
    if not dbg:
        return ("debugfs (e2fsprogs) not found — needed to edit the product "
                "filesystem. macOS: brew install e2fsprogs. "
                "Debian/Ubuntu: apt install e2fsprogs.")
    dev = f"{raw_path}?offset={fs_off}"
    tmp = Path(tempfile.mkdtemp(prefix="omni-bootanim-"))
    try:
        # The SELinux label must survive the swap: bootanim reads the file as a
        # confined domain, so an unlabelled replacement is unreadable -> the
        # screen just stays black. Written via a file so the trailing NUL that
        # the on-disk xattr carries is reproduced byte-exactly.
        val = tmp / "selinux.val"
        val.write_bytes(BOOTANIM_SELINUX.encode() + b"\x00")
        # debugfs scripts are whitespace-split with no quoting, so any path
        # handed to it must contain no spaces. The repo itself lives under
        # "Omni Apps", so stage the animation next to the script first.
        staged_anim = tmp / "anim.zip"
        shutil.copy2(anim_zip, staged_anim)
        script = tmp / "cmds.txt"
        script.write_text(
            f"rm {BOOTANIM_PATH}\n"
            f"cd {str(Path(BOOTANIM_PATH).parent)}\n"
            f"write {staged_anim} {Path(BOOTANIM_PATH).name}\n"
            f"sif {BOOTANIM_PATH} mode 0100644\n"
            f"sif {BOOTANIM_PATH} uid 0\n"
            f"sif {BOOTANIM_PATH} gid 0\n"
            f"ea_set -f {val} {BOOTANIM_PATH} security.selinux\n"
        )
        r = subprocess.run([dbg, "-w", "-f", str(script), dev],
                           capture_output=True, text=True, timeout=300)
        out = (r.stdout or "") + (r.stderr or "")
        if "Allocated inode" not in out:
            return f"debugfs write did not land: {out.strip()[-400:]}"

        # Verify by reading the file back OUT of the image rather than trusting
        # the write: a wrong size/label here is a black boot screen at best.
        back = tmp / "readback.zip"
        subprocess.run([dbg, "-R", f"dump {BOOTANIM_PATH} {back}", dev],
                       capture_output=True, text=True, timeout=300)
        if not back.exists() or back.read_bytes() != Path(anim_zip).read_bytes():
            return "readback of the written animation did not match the source"
        r = subprocess.run([dbg, "-R", f"ea_list {BOOTANIM_PATH}", dev],
                           capture_output=True, text=True, timeout=120)
        if BOOTANIM_SELINUX not in (r.stdout or ""):
            return (f"SELinux label missing after write (bootanim would be "
                    f"denied): {(r.stdout or '').strip()[-200:]}")
        print(f"[{label}] product{BOOTANIM_PATH}: replaced "
              f"({Path(anim_zip).stat().st_size} bytes, label {BOOTANIM_SELINUX})")
        return None
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _fsck_ok(raw_path, fs_off, label):
    """Read-only consistency check of the edited filesystem. e2fsck exits 0 when
    clean, 4 when errors are left uncorrected (-n fixes nothing)."""
    import shutil as _sh
    fsck = None
    for cand in ("e2fsck", "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
                 "/usr/local/opt/e2fsprogs/sbin/e2fsck", "/sbin/e2fsck"):
        p = _sh.which(cand) if "/" not in cand else (
            cand if Path(cand).exists() else None)
        if p:
            fsck = p
            break
    if not fsck:
        print(f"[{label}] e2fsck not found; skipping the consistency check")
        return True
    r = subprocess.run([fsck, "-fn", f"{raw_path}?offset={fs_off}"],
                       capture_output=True, text=True, timeout=600)
    if r.returncode in (0, 1):
        return True
    print(f"[{label}] e2fsck reported an UNCLEAN filesystem "
          f"(exit {r.returncode}):\n{(r.stdout or '')[-600:]}")
    return False


# The arm base renders the kernel/init log on the framebuffer for the first ~8 s
# of every boot — scrolling white-on-black text, which defeats "no vendor logo"
# harder than a logo does. x86 fixes this in the QEMU command line; arm cannot,
# because it boots UEFI -> GRUB -> kernel from INSIDE the image, so there is no
# -append to add. The kernel cmdline is assembled in grub.cfg on the ESP (vda1,
# FAT32), which is patched here.
#
# Only the NORMAL-boot `linux` line is touched; the recovery one is deliberately
# left verbose, since a silent recovery is a debugging own-goal.
GRUB_CFG_PATH = "boot/grub/grub.cfg"

# `androidboot.insecure_adb=1` (a LineageOS cmdline flag, see grub.cfg's
# set_kernel_cmdline_dynamic) sets ro.adb.secure=0, so adbd accepts any host key.
#
# This is not a convenience — without it the product does not work at all. adbd
# otherwise demands authorization, which Android asks for with an "Allow USB
# debugging?" DIALOG on the guest screen. Nothing can answer it: the instance is
# headless, the kiosk cannot dismiss a system dialog, and `omnidroid start` needs adb
# to reach the kiosk in the first place. Every fresh account would sit at that
# dialog forever.
#
# Baking this host's adb key into the /data template is NOT the fix: the key is
# ~/.android/adbkey, so a customer's machine has a different one and the dialog
# returns on their first boot.
#
# The exposure is unchanged by this: adb is bound to 127.0.0.1 only (a HARD RULE
# in qemu_command_arm), so the reachable set is local processes — which can
# already read the same account's token straight out of accounts/<n>/session.json.
SILENT_CMDLINE = ("quiet loglevel=0 vt.global_cursor_default=0 "
                  "androidboot.insecure_adb=1")

# ...and BEFORE the kernel even loads, GRUB itself draws a themed boot MENU with
# the LineageOS logo, four entries, and a 10-second countdown. That is both a
# vendor logo and a menu — the two things the product must never show. The menu
# is skipped by forcing GRUB's timeout to 0 and hiding the menu entirely; the
# theme then never renders. `set default=` is left alone, so a misc-triggered
# recovery boot still selects the recovery entry, it just does not wait.
GRUB_NO_MENU = ("\n# Omni: boot straight through - no menu, no vendor logo, no "
                "countdown.\nset timeout=0\nset timeout_style=hidden\n")
GRUB_TIMEOUT_ANCHOR = ("\t# Normal boot\n"
                       "\tset default=$grub_android_default\n"
                       "\tset timeout=$grub_timeout\n"
                       "fi\n")


def _mount_fat(img_path, label):
    """Mount a FAT image read-write and return (mount_point, detach_fn).

    macOS mounts FAT natively via hdiutil, no root and no mtools. This is a
    build-machine-only path (like the rest of brand-base); on Linux the same edit
    needs mtools/loop-mount, which is why the caller degrades gracefully instead
    of failing the whole command."""
    if not IS_MACOS:
        return None, None
    r = subprocess.run(["hdiutil", "attach", "-nobrowse", "-imagekey",
                        "diskimage-class=CRawDiskImage", str(img_path)],
                       capture_output=True, text=True, timeout=120)
    if r.returncode != 0:
        print(f"[{label}] could not mount the ESP: {(r.stderr or '').strip()[:200]}")
        return None, None
    # `hdiutil attach` prints tab-padded columns: "/dev/diskN <type> /Volumes/X".
    # The type column is empty for a bare FAT image, so a whitespace split can
    # yield as few as TWO fields — the mount point is simply the last one that
    # looks like a path under /Volumes.
    dev = mnt = None
    for line in (r.stdout or "").splitlines():
        parts = line.split()
        if parts and parts[0].startswith("/dev/"):
            dev = dev or parts[0]
            if len(parts) >= 2 and parts[-1].startswith("/Volumes/"):
                mnt = parts[-1]
    if not mnt:
        if dev:
            subprocess.run(["hdiutil", "detach", dev], capture_output=True)
        return None, None

    def _detach():
        subprocess.run(["sync"], capture_output=True)
        subprocess.run(["hdiutil", "detach", mnt, "-quiet"], capture_output=True)

    return Path(mnt), _detach


def _silence_boot(raw_path, label):
    """Add the quiet kernel cmdline to the ESP's grub.cfg. Returns True if the
    image was changed."""
    esp = _gpt_partition(str(raw_path), "EFI")
    if not esp:
        print(f"[{label}] no EFI partition found; skipping silent boot")
        return False
    off, size = esp
    import tempfile
    tmp = Path(tempfile.mkdtemp(prefix="omni-esp-"))
    img = tmp / "efi.img"
    try:
        with open(raw_path, "rb") as src, open(img, "wb") as dst:
            src.seek(off)
            remaining = size
            while remaining > 0:
                chunk = src.read(min(8 << 20, remaining))
                if not chunk:
                    break
                dst.write(chunk)
                remaining -= len(chunk)
        mnt, detach = _mount_fat(img, label)
        if not mnt:
            print(f"[{label}] silent boot needs a FAT mount (macOS hdiutil); "
                  f"skipped — the boot will still show kernel log text")
            return False
        try:
            cfg = mnt / GRUB_CFG_PATH
            if not cfg.exists():
                print(f"[{label}] {GRUB_CFG_PATH} not on the ESP; skipping")
                return False
            text = cfg.read_text()
            # No blanket "already applied" short-circuit: each step below tests
            # for its OWN edit. A single guard here silently skips steps added
            # later on an image that already has the earlier ones.
            changed = False

            # 1. Quiet the kernel/init log.
            # The normal-boot line; the recovery line boots ${recovery_partition}.
            if SILENT_CMDLINE not in text:
                m = re.search(r"^(\tlinux \$\{boot_partition\}/kernel .*)$",
                              text, re.M)
                if not m:
                    print(f"[{label}] could not find the normal-boot linux line "
                          f"in grub.cfg; skipping the quiet cmdline")
                else:
                    # Strip any cmdline WE added before, so re-branding an
                    # already-branded image (e.g. after this list grows)
                    # replaces it instead of appending a second copy.
                    cleaned = re.sub(
                        r"\s+(?:quiet|loglevel=\S+|vt\.global_cursor_default=\S+"
                        r"|androidboot\.insecure_adb=\S+)(?=\s|$)",
                        "", m.group(1))
                    text = text.replace(
                        m.group(1),
                        cleaned.rstrip() + " " + SILENT_CMDLINE + " ", 1)
                    changed = True
                    print(f"[{label}] grub.cfg: normal boot += "
                          f"'{SILENT_CMDLINE}'")

            # 2. Silence GRUB's own "Loading kernel..." chatter. Only inside
            #    boot_android — the recovery path keeps its progress echoes.
            m = re.search(r"(function boot_android \{\n)(.*?)(\n\})",
                          text, re.S)
            if m and "echo 'Loading" in m.group(2):
                body = re.sub(r"^\t*echo 'Loading [^\n]*\n", "", m.group(2),
                              flags=re.M)
                text = text.replace(m.group(0), m.group(1) + body + m.group(3), 1)
                changed = True
                print(f"[{label}] grub.cfg: dropped the 'Loading ...' echoes")

            # 3. Skip GRUB's own themed menu (LineageOS logo + 10s countdown).
            if GRUB_NO_MENU not in text:
                if GRUB_TIMEOUT_ANCHOR not in text:
                    print(f"[{label}] could not find GRUB's timeout block; the "
                          f"boot MENU will still appear")
                else:
                    text = text.replace(GRUB_TIMEOUT_ANCHOR,
                                        GRUB_TIMEOUT_ANCHOR + GRUB_NO_MENU, 1)
                    changed = True
                    print(f"[{label}] grub.cfg: boot menu hidden "
                          f"(timeout=0, no LineageOS logo, no countdown)")
            if not changed:
                return False
            cfg.write_text(text)
        finally:
            detach()
        with open(raw_path, "r+b") as dst, open(img, "rb") as src:
            dst.seek(off)
            while True:
                chunk = src.read(8 << 20)
                if not chunk:
                    break
                dst.write(chunk)
        return True
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# The arm base runs arm64 natively (no translation), so a baked system app takes
# its arm64-v8a libs. AOSP names the on-disk dir by the ABI's "instruction set",
# not the ABI: arm64-v8a -> arm64.
_BAKE_ABI = "arm64-v8a"
_BAKE_LIBDIR = "arm64"


def _bake_apk_into_product(raw_path, fs_off, apk, app_name, label):
    """Install an APK as a SYSTEM app inside the product filesystem.

    Production must ship with Roblox already installed — not adb-installed on
    first boot, which would need a boot, a network, and a writable /data. The
    x86 base does this already (base_x86 v4: "pre-installed game
    com.roblox.client as system app"); this is the arm equivalent.

    Layout copied from the apps the image already ships (e.g. /product/app/Jelly):

        /product/app/<Name>/<Name>.apk    0644 root:root  u:object_r:system_file:s0
        /product/app/<Name>/              0755 root:root  u:object_r:system_file:s0
        /product/app/<Name>/lib/arm64/*.so                u:object_r:system_file:s0

    The lib/ directory is NOT optional for an app with native code, and getting
    it wrong fails LATE and confusingly: PackageManager installs the app fine,
    it launches, and only then dies with

        UnsatisfiedLinkError: No implementation found for ...initNative

    A /data install unpacks lib/<abi>/ out of the APK itself; a SYSTEM app is
    expected to have them pre-extracted at build time, which is what AOSP's
    build does. Roblox declares extractNativeLibs="true" and ships ~107 MB of
    .so (libroblox.so alone is 100 MB), so this is the difference between a
    production image that runs and one that crashes on launch.

    Returns None on success, else an error string.
    """
    import tempfile
    dbg = _debugfs_bin()
    if not dbg:
        return ("debugfs (e2fsprogs) not found — needed to write into the "
                "product filesystem. macOS: brew install e2fsprogs.")
    dev = f"{raw_path}?offset={fs_off}"
    tmp = Path(tempfile.mkdtemp(prefix="omni-bake-"))
    try:
        val = tmp / "selinux.val"
        val.write_bytes(BOOTANIM_SELINUX.encode() + b"\x00")
        # debugfs scripts are whitespace-split with no quoting; the repo lives
        # under "Omni Apps", so stage the apk somewhere space-free first.
        staged = tmp / "game.apk"
        shutil.copy2(apk, staged)
        d = f"/app/{app_name}"
        f = f"{d}/{app_name}.apk"
        script = tmp / "cmds.txt"
        script.write_text(
            f"rm {f}\n"                       # ok to fail when absent
            f"rmdir {d}\n"
            f"mkdir {d}\n"
            f"sif {d} mode 040755\n"
            f"sif {d} uid 0\n"
            f"sif {d} gid 0\n"
            f"ea_set -f {val} {d} security.selinux\n"
            f"cd {d}\n"
            f"write {staged} {app_name}.apk\n"
            f"sif {f} mode 0100644\n"
            f"sif {f} uid 0\n"
            f"sif {f} gid 0\n"
            f"ea_set -f {val} {f} security.selinux\n"
        )
        # Pre-extract native libs into the AOSP system-app layout.
        import zipfile
        libs = []
        with zipfile.ZipFile(apk) as z:
            names = [n for n in z.namelist()
                     if n.startswith(f"lib/{_BAKE_ABI}/") and n.endswith(".so")]
            if names:
                libdir = tmp / "libs"
                libdir.mkdir()
                script_lines = [f"mkdir {d}/lib\n",
                                f"sif {d}/lib mode 040755\n",
                                f"ea_set -f {val} {d}/lib security.selinux\n",
                                f"mkdir {d}/lib/{_BAKE_LIBDIR}\n",
                                f"sif {d}/lib/{_BAKE_LIBDIR} mode 040755\n",
                                f"ea_set -f {val} {d}/lib/{_BAKE_LIBDIR} "
                                f"security.selinux\n",
                                f"cd {d}/lib/{_BAKE_LIBDIR}\n"]
                for n in names:
                    base_n = Path(n).name
                    outp = libdir / base_n
                    with z.open(n) as src, open(outp, "wb") as dst:
                        shutil.copyfileobj(src, dst)
                    lp = f"{d}/lib/{_BAKE_LIBDIR}/{base_n}"
                    script_lines += [f"write {outp} {base_n}\n",
                                     f"sif {lp} mode 0100644\n",
                                     f"sif {lp} uid 0\n",
                                     f"sif {lp} gid 0\n",
                                     f"ea_set -f {val} {lp} security.selinux\n"]
                    libs.append(base_n)
                script.write_text(script.read_text() + "".join(script_lines))

        r = subprocess.run([dbg, "-w", "-f", str(script), dev],
                           capture_output=True, text=True, timeout=1800)
        out = (r.stdout or "") + (r.stderr or "")
        if "Allocated inode" not in out:
            return f"debugfs write did not land: {out.strip()[-400:]}"
        if libs:
            r2 = subprocess.run([dbg, "-R", f"ls -l {d}/lib/{_BAKE_LIBDIR}", dev],
                                capture_output=True, text=True, timeout=300)
            missing = [n for n in libs if n not in (r2.stdout or "")]
            if missing:
                return (f"native libs did not land ({len(missing)}/{len(libs)} "
                        f"missing, e.g. {missing[:2]}) — the app would die with "
                        f"UnsatisfiedLinkError")
            print(f"[{label}] product{d}/lib/{_BAKE_LIBDIR}: {len(libs)} "
                  f"native libs extracted")
        # Verify by reading it back out rather than trusting the write.
        back = tmp / "back.apk"
        subprocess.run([dbg, "-R", f"dump {f} {back}", dev],
                       capture_output=True, text=True, timeout=600)
        if not back.exists() or back.stat().st_size != Path(apk).stat().st_size:
            return (f"readback of {f} did not match the source "
                    f"({back.stat().st_size if back.exists() else 0} vs "
                    f"{Path(apk).stat().st_size} bytes)")
        r = subprocess.run([dbg, "-R", f"ea_list {f}", dev],
                           capture_output=True, text=True, timeout=120)
        if BOOTANIM_SELINUX not in (r.stdout or ""):
            return f"SELinux label missing on {f}; PackageManager would skip it"
        print(f"[{label}] product{f}: {Path(apk).stat().st_size} bytes, "
              f"0644 root:root, {BOOTANIM_SELINUX}")
        return None
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _remove_apk_from_product(raw_path, fs_off, app_name, label):
    """Delete a baked SYSTEM app from the product filesystem — the inverse of
    _bake_apk_into_product, and what makes a base TRULY carry no game.

    Why it exists. Offsets already make the /data layer clean: the base's
    /data is pristine and every Roblox version is a sibling overlay. But the
    arm base's SYSTEM image (base v2) also has Roblox baked at
    /product/app/Roblox. That copy is shadowed at runtime — every offset
    installs with `pm install -r -d`, which lands an UPDATED SYSTEM APP in
    /data/app that wins over the /product one — so it changes no behaviour.
    It does mean `--offset none` boots a base that still HAS a Roblox, and
    that the system image carries ~130 MB it never uses.

    Run once on a build machine to be rid of it. Returns None on success,
    else an error string. Reads the directory back to prove the delete landed
    rather than trusting debugfs's exit status.
    """
    import tempfile
    dbg = _debugfs_bin()
    if not dbg:
        return ("debugfs (e2fsprogs) not found — needed to edit the product "
                "filesystem. macOS: brew install e2fsprogs.")
    dev = f"{raw_path}?offset={fs_off}"
    d = f"/app/{app_name}"
    probe = subprocess.run([dbg, "-R", f"ls -l {d}", dev],
                           capture_output=True, text=True, timeout=300)
    if "File not found" in ((probe.stdout or "") + (probe.stderr or "")):
        print(f"[{label}] product{d} is already absent — nothing to remove")
        return None
    tmp = Path(tempfile.mkdtemp(prefix="omni-unbake-"))
    try:
        # Depth-first: ext2 rmdir refuses a non-empty directory, and the lib
        # tree below is where ~107 MB of Roblox .so files live.
        lines = [f"rm {d}/{app_name}.apk\n"]
        libs = subprocess.run(
            [dbg, "-R", f"ls -l {d}/lib/{_BAKE_LIBDIR}", dev],
            capture_output=True, text=True, timeout=300).stdout or ""
        for tok in re.findall(r"\s(\S+\.so)\s*$", libs, re.M):
            lines.append(f"rm {d}/lib/{_BAKE_LIBDIR}/{tok}\n")
        lines += [f"rmdir {d}/lib/{_BAKE_LIBDIR}\n", f"rmdir {d}/lib\n",
                  f"rmdir {d}\n"]
        script = tmp / "cmds.txt"
        script.write_text("".join(lines))
        subprocess.run([dbg, "-w", "-f", str(script), dev],
                       capture_output=True, text=True, timeout=1800)
        back = subprocess.run([dbg, "-R", f"ls -l {d}", dev],
                              capture_output=True, text=True, timeout=300)
        out = (back.stdout or "") + (back.stderr or "")
        if "File not found" not in out:
            return (f"product{d} still exists after the delete — refusing to "
                    f"claim a clean base. debugfs said: {out.strip()[-300:]}")
        print(f"[{label}] product{d} removed — this system image now ships "
              f"NO game")
        return None
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def bake_offset(cfg, tag, out_name, apk, pkg, label):
    """Boot a builder, install ONE Roblox version into /DATA, bake
    `omni_game_package`, and keep the resulting THIN overlay as `out_name`.
    Returns the overlay path or None.

    This is the mechanism behind `omnidroid offset create`. Each call produces
    an INDEPENDENT sibling overlay of the base's pristine /data — never an
    overlay of another offset (see omnidroid/offsets.py for why chaining is
    refused) — so any number of Roblox versions coexist at roughly APK size
    each, and deleting one cannot disturb another.

    Why /data rather than the system image: `omnidroid bake-game` writes the APK
    into /product inside the 2.3 GB system image, so every Roblox version means
    a new 2.3 GB base and ~6 GiB of scratch. A `pm install -r -d` lands an
    UPDATED SYSTEM APP in /data/app, does the same job for a kiosk that
    launches by package name, and the package name never changes between
    Roblox versions — so a new version is one ~2-minute command.

    Uses no root: `pm install` and `settings put global` both work as uid
    shell, so this works on an unrooted deployment too."""
    images = Path(cfg["images_dir"])
    base = cfg["bases"][tag]
    src_name = data_bake_source(base)
    src = images / src_name
    if not src.exists():
        print(f"[{label}] pristine /data not found: {src}")
        return None
    # Build the overlay in images/ so its backing reference stays inside the
    # image directory, and under a .tmp name so a failed bake never clobbers
    # an offset that already exists and boots.
    out = images / out_name
    tmp = out.with_suffix(".tmp.qcow2")
    bname = "_gamedata"
    d = account_dir(bname)
    try:
        if d.exists():
            shutil.rmtree(d)
        d.mkdir(parents=True)
        adb_port, qmp_port, vnc_port = allocate_ports(cfg)
        acct = {"name": bname, "base": tag, "adb_port": adb_port,
                "qmp_port": qmp_port, "vnc_port": vnc_port,
                "game_package": pkg, "first_boot_done": True}
        save_account(acct)
        make_overlay(d / "system.qcow2", images / base["system"])
        make_overlay(d / "data.qcow2", src)
        shutil.copyfile(images / base.get("efivars", ARM_BASE_EFIVARS),
                        d / "efivars.fd")
        _spawn_builder_with_disks(acct, cfg, [], label)
        if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT, label):
            print(f"[{label}] builder did not boot; nothing captured.")
            return None
        guest_apk = None
        if apk:
            guest_apk = "/data/local/tmp/omni-game.apk"
            print(f"[{label}] pushing {apk.name} "
                  f"({apk.stat().st_size // (1024*1024)} MB)...")
            adb(acct, "push", str(apk), guest_apk, timeout=900)
        script = build_game_bake_script(pkg, guest_apk)
        r = adb(acct, "shell", "sh", "-c", shlex.quote(script), timeout=900)
        out_text = ((r.stdout or "") + (r.stderr or "")).strip()
        print(f"[{label}] guest: {out_text[-300:]}")
        if GAME_BAKE_INSTALL_FAILED in out_text:
            print(f"[{label}] the APK was REJECTED by the guest. The commonest "
                  f"cause is a signature mismatch: a replacement must be "
                  f"signed with the SAME key as the build baked into the "
                  f"system image (an officially-signed Roblox will not "
                  f"install over a re-signed one, or the reverse). Nothing "
                  f"was captured; the shipping /data is untouched.")
            _shutdown(acct, label)
            return None
        if GAME_BAKE_OK not in out_text:
            print(f"[{label}] the bake did not confirm — NOT capturing this "
                  f"/data. The shipping image is untouched.")
            _shutdown(acct, label)
            return None
        _shutdown(acct, label)
        # Keep the overlay itself: it is thin (only the APK + settings differ
        # from the pristine /data). Rewrite its backing reference to a bare
        # filename so the image directory stays relocatable, same convention
        # as base_arm_system_zram.qcow2.
        shutil.move(str(d / "data.qcow2"), str(tmp))
        subprocess.run([qemu_bin("qemu-img"), "rebase", "-u",
                        "-b", src_name, "-F", "qcow2", str(tmp)],
                       check=True, capture_output=True)
        tmp.replace(out)
        size_mb = out.stat().st_size // (1024 * 1024)
        print(f"[{label}] captured -> {out.name} ({size_mb} MB thin overlay "
              f"on {src_name})")
        return out
    except Exception as e:  # noqa: BLE001
        print(f"[{label}] offset bake failed ({type(e).__name__}: {e}).")
        tmp.unlink(missing_ok=True)
        return None
    finally:
        if d.exists():
            shutil.rmtree(d, ignore_errors=True)


def _offset_base(args, cfg=None):
    """(cfg, tag, base) for an `offset ...` subcommand, or exit with an
    actionable error. Offsets are an arm /data concept; an x86 base is
    refused by name rather than by a confusing missing-file error later."""
    cfg = cfg or load_config()
    tag = getattr(args, "base", None) or effective_base_tag(cfg)
    base = (cfg.get("bases") or {}).get(tag)
    if not base:
        fail("no_base", f"no base '{tag}'. Known: "
                        f"{list((cfg.get('bases') or {}))}")
    if base_type(base) != BASE_TYPE_ARM:
        fail("arch_boundary",
             f"base '{tag}' is {arch_of_base(base)}; offsets are an arm /data "
             f"concept and only exist on arm-uefi bases")
    return cfg, tag, base


def _write_base_entry(tag, mutate):
    """Re-read the RAW config, mutate one base entry, write it back.

    Re-reads deliberately: `load_config()` hands back a NORMALIZED copy
    (images_dir resolved to an absolute path, `_effective_base` injected), and
    writing that back would bake this host's absolute image path into a config
    the other platform also reads."""
    raw = json.loads(CONFIG_PATH.read_text())
    entry = raw.setdefault("bases", {}).setdefault(tag, {})
    mutate(entry)
    CONFIG_PATH.write_text(json.dumps(raw, indent=2))
    return entry


def cmd_offset_list(args):
    """List every baked Roblox version on a base, marking the default."""
    cfg, tag, base = _offset_base(args)
    rows = offsets_mod.offset_rows(base, cfg["images_dir"])
    if getattr(args, "json", False):
        emit_json({"ok": True, "base": tag,
                   "default": offsets_mod.default_offset_name(base),
                   "offsets": rows})
        return
    if not rows:
        print(f"base '{tag}' has NO Roblox baked — it is a clean base.\n"
              f"Bake one:  omnidroid offset create <name> --apk <roblox.apk>")
        return
    print(f"offsets on base '{tag}'  (* = default, used by a bare "
          f"`omnidroid start`)")
    for r in rows:
        ver = f" v{r['version_name']}" if r.get("version_name") else ""
        size = f" {r['size_mb']} MB" if r.get("size_mb") is not None else ""
        miss = "" if r.get("present", True) else "   [IMAGE MISSING]"
        print(f" {'*' if r['default'] else ' '} {r['name']}{ver}"
              f"   ({r['data']}{size}){miss}")
        if r.get("apk"):
            print(f"     from {r['apk']}"
                  + (f"  package {r['package']}" if r.get("package") else ""))
        if r.get("notes"):
            print(f"     {r['notes']}")
    if not offsets_mod.default_offset_name(base):
        print("\nNO DEFAULT SET — a bare `omnidroid start` will refuse. "
              "Set one: omnidroid offset default <name>")


def cmd_offset_create(args):
    """Bake a Roblox APK into a NEW named offset (or replace one by name).

        omnidroid offset create 2.740.101 --apk ~/Downloads/roblox.apk
        omnidroid offset create --apk build.apk           # name from the APK
        omnidroid offset create test --apk build.apk --default

    The base itself is NEVER modified: it keeps shipping its pristine, clean
    /data, and this only adds a sibling overlay next to the other offsets.
    """
    ensure_qemu()
    cfg, tag, base = _offset_base(args)
    apk = Path(args.apk)
    if not apk.exists():
        return fail("bad_apk", f"apk not found: {apk}")
    try:
        import zipfile
        with zipfile.ZipFile(apk) as z:
            if "AndroidManifest.xml" not in z.namelist():
                return fail("bad_apk", f"{apk} is not an APK")
    except zipfile.BadZipFile:
        return fail("bad_apk", f"{apk} is not a valid APK (bad zip)")

    info = offsets_mod.apk_version_info(apk)
    name = getattr(args, "name", None) or offsets_mod.suggest_offset_name(
        apk, info)
    if not name:
        return fail("bad_offset_name",
                    f"could not derive an offset name from {apk.name}; pass "
                    f"one explicitly: `omnidroid offset create <name> --apk "
                    f"{apk}`")
    if not offsets_mod.valid_offset_name(name):
        return fail("bad_offset_name",
                    f"offset name must match [A-Za-z0-9][A-Za-z0-9._-]{{0,47}} "
                    f"and cannot be '{offsets_mod.NO_OFFSET}' (got '{name}')")
    existing = offsets_mod.offsets_of(base)
    if name in existing and not getattr(args, "force", False):
        return fail("offset_exists",
                    f"offset '{name}' already exists on base '{tag}'. Re-bake "
                    f"it with --force, or pick another name. (Offsets are "
                    f"siblings — creating a new one never disturbs this one.)")
    # The package is NOT read from the APK for the registration: a Roblox
    # update never changes it, and making the bake depend on aapt2 fails hosts
    # that have no Android SDK (see bases.resolve_bake_package). The manifest
    # probe above is best-effort labelling only.
    pkg = (getattr(args, "package", None) or info.get("package")
           or resolve_bake_package(None, tag, cfg) or ROBLOX_PACKAGE)
    live = [a["name"] for a in all_accounts() if running_pid(a["name"])]
    if live:
        return fail("instance_running",
                    f"stop running instances first: {', '.join(live)}")

    # Re-baking an existing offset REUSES its recorded image name rather than
    # recomputing one. An offset baked under an older naming convention would
    # otherwise get a second file on disk while the registry moved to the new
    # name — leaving the old image orphaned and unreferenced.
    out_name = (offsets_mod.offset_data_image(base, name) if name in existing
                else offsets_mod.offset_image_name(name))
    label = f"offset create {name}"
    print(f"[{label}] baking {apk.name}"
          + (f" (v{info['version_name']})" if info.get("version_name") else "")
          + f" as offset '{name}' on base '{tag}' -> {out_name}")
    out = bake_offset(cfg, tag, out_name, apk, pkg, label)
    if not out:
        return fail("bake_failed", "bake failed; see the log above. Nothing "
                                   "was registered and no other offset was "
                                   "touched.")
    make_default = bool(getattr(args, "default", False))

    def _mutate(entry):
        # The pristine /data is recorded (once) so every FUTURE bake still
        # overlays it rather than this offset — the anti-chaining rule.
        entry.setdefault("root_manifest", {}).setdefault(
            "rooted_data", data_bake_source(base))
        offsets_mod.register_offset(entry, name, {
            "data": out_name, "package": pkg, "apk": apk.name,
            "apk_path": str(apk.resolve()),
            "version_name": info.get("version_name"),
            "version_code": info.get("version_code"),
            "created": int(time.time()),
            "notes": getattr(args, "notes", None),
        }, make_default=make_default)

    entry = _write_base_entry(tag, _mutate)
    is_default = entry.get("default_offset") == name
    print(f"[{label}] offset '{name}' ready"
          + ("  [DEFAULT — a bare `omnidroid start` now uses it]"
             if is_default else
             f"  (not default; switch with `omnidroid offset default {name}` "
             f"or launch with `--offset {name}`)"))
    if getattr(args, "json", False):
        emit_json({"ok": True, "base": tag, "offset": name,
                   "data": out_name, "package": pkg, "apk": apk.name,
                   "version_name": info.get("version_name"),
                   "version_code": info.get("version_code"),
                   "default": is_default})


def cmd_offset_default(args):
    """Mark one baked version as the default for bare launches."""
    cfg, tag, base = _offset_base(args)
    name = args.name
    if name not in offsets_mod.offsets_of(base):
        return fail("no_offset",
                    f"no offset '{name}' on base '{tag}'. Baked: "
                    f"{list(offsets_mod.offsets_of(base)) or 'none'}")
    _write_base_entry(tag, lambda e: e.update({"default_offset": name}))
    print(f"default offset for base '{tag}' is now '{name}' — a bare "
          f"`omnidroid start <username>` boots it.")
    if getattr(args, "json", False):
        emit_json({"ok": True, "base": tag, "default": name})


def cmd_offset_remove(args):
    """Delete a baked version: its registry entry AND its overlay image.

    Refuses while an instance booted from that offset is still running — the
    qcow2 is open by QEMU, and deleting it out from under a live guest is a
    corruption, not a cleanup."""
    cfg, tag, base = _offset_base(args)
    name = args.name
    if name not in offsets_mod.offsets_of(base):
        return fail("no_offset",
                    f"no offset '{name}' on base '{tag}'. Baked: "
                    f"{list(offsets_mod.offsets_of(base)) or 'none'}")
    img = offsets_mod.offset_data_image(base, name)
    live = [a["name"] for a in all_accounts()
            if running_pid(a["name"])
            and (a.get("offset") == name or a.get("data_image") == img)]
    if live:
        return fail("instance_running",
                    f"offset '{name}' is in use by running instance(s): "
                    f"{', '.join(live)}. Stop them first "
                    f"(`omnidroid stop <name>`).")
    removed = {}

    def _mutate(entry):
        removed["entry"] = offsets_mod.unregister_offset(entry, name)
        removed["default"] = entry.get("default_offset")

    _write_base_entry(tag, _mutate)
    images = Path(cfg["images_dir"]).resolve()
    path = (images / img).resolve()
    deleted = False
    # Structurally confined to images_dir: `img` comes out of a config file a
    # human can edit, and an unlink() driven by an unchecked config value is
    # how a cleanup command turns into an arbitrary delete.
    if images not in path.parents or "/" in img or "\\" in img:
        return fail("engine_error",
                    f"refusing to delete '{img}': an offset image must be a "
                    f"bare filename inside {images}")
    if getattr(args, "keep_image", False):
        print(f"kept the image: {path}")
    elif path.exists():
        path.unlink()
        deleted = True
        print(f"deleted {path}")
    now_default = removed.get("default") or (
        "UNSET (set one with `omnidroid offset default <name>`)")
    print(f"offset '{name}' removed from base '{tag}'. "
          f"Default is now {now_default}")
    if getattr(args, "json", False):
        emit_json({"ok": True, "base": tag, "removed": name,
                   "image_deleted": deleted,
                   "default": removed.get("default")})


def cmd_offset_show(args):
    """Everything recorded about one baked version."""
    cfg, tag, base = _offset_base(args)
    rows = {r["name"]: r for r in offsets_mod.offset_rows(base,
                                                          cfg["images_dir"])}
    name = args.name or offsets_mod.default_offset_name(base)
    if not name or name not in rows:
        return fail("no_offset",
                    f"no offset '{name}' on base '{tag}'. Baked: "
                    f"{list(rows) or 'none'}")
    row = rows[name]
    if getattr(args, "json", False):
        emit_json({"ok": True, "base": tag, **row})
        return
    print(json.dumps(row, indent=2))


def cmd_bake_data_game(args):
    """DEPRECATED alias for `omnidroid offset create` (kept so existing
    scripts and docs keep working).

    It baked into ONE fixed slot and pointed the base at it, which is exactly
    the "the base carries a Roblox version" model offsets replaced. Mapped
    onto an offset named after the APK (or `--name`), promoted to default so
    the old single-slot behaviour is preserved."""
    print("[deprecated] `bake-data-game` is now `omnidroid offset create`. "
          "Baking as an offset (the base stays clean).")
    if not getattr(args, "apk", None):
        return fail("bad_apk",
                    "`bake-data-game` with no APK baked only the kiosk's "
                    "game-package setting. That setting is now written on "
                    "EVERY boot by assert_kiosk_game(), so there is nothing "
                    "to bake — pass an APK to create an offset instead.")
    args.name = getattr(args, "name", None)
    args.default = True
    args.force = True
    return cmd_offset_create(args)


def cmd_bake_game(args):
    """Bake a game APK into an arm SYSTEM image, or (--remove) strip one out.

    LEGACY under the offsets model, and worth saying plainly: baking a game
    into the 2.3 GB system image is what offsets replaced. A version baked
    here needs a whole new base per Roblox update; a version baked as an
    offset is a ~130 MB sibling overlay produced in ~2 minutes. Use
    `omnidroid offset create` for versions.

    What is still worth running is the INVERSE. The shipped arm base (v2) has
    Roblox in /product/app/Roblox, so its system image is not truly clean:

        omnidroid bake-game --remove            # strip it; base ships no game

    Build-machine command either way (needs e2fsprogs + ~6 GiB scratch), same
    shape as brand-base:

        omnidroid bake-game roblox.apk --image ~/OmniImages/base_arm_branded.qcow2
    """
    cfg = load_config()
    remove = bool(getattr(args, "remove", False))
    apk = None
    if not remove:
        if not getattr(args, "apk", None):
            return fail("engine_error",
                        "bake-game needs an APK (or --remove to strip the "
                        "baked one out). For a new Roblox VERSION you almost "
                        "certainly want `omnidroid offset create` instead.")
        apk = Path(args.apk)
        if not apk.exists():
            return fail("engine_error", f"apk not found: {apk}")
    images = Path(cfg["images_dir"])
    tag = getattr(args, "base", None) or "arm"
    if getattr(args, "image", None):
        disk = Path(args.image)
    else:
        base = (cfg.get("bases") or {}).get(tag)
        if not base:
            return fail("no_base", f"no base '{tag}'")
        if base_type(base) != BASE_TYPE_ARM:
            return fail("arch_boundary",
                        f"base '{tag}' is not arm; this command only knows the "
                        f"arm super/product layout")
        disk, _why = _brand_target(cfg, base, images)
    if not disk.exists():
        return fail("no_base", f"image not found: {disk}")

    label = f"bake-game {disk.name}"
    live = [a["name"] for a in all_accounts()
            if running_pid(a["name"])
            and _brand_target(cfg, cfg["bases"].get(a.get("base"), {}),
                              images)[0] == disk]
    if live:
        return fail("instance_running",
                    f"cannot rewrite {disk.name}: {', '.join(live)} "
                    f"{'is' if len(live) == 1 else 'are'} running on it.")

    pkg = None
    if not remove:
        try:
            import zipfile
            with zipfile.ZipFile(apk) as z:
                if "AndroidManifest.xml" not in z.namelist():
                    return fail("engine_error", f"{apk} is not an APK")
        except zipfile.BadZipFile:
            return fail("engine_error", f"{apk} is not a valid zip")

    import tempfile
    work = Path(tempfile.mkdtemp(prefix="omni-bake-", dir=str(images)))
    raw = work / "base.raw"
    try:
        print(f"[{label}] exporting -> raw")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-f", "qcow2",
                        "-O", "raw", str(disk), str(raw)],
                       check=True, capture_output=True, timeout=1800)
        sup = _gpt_partition(str(raw), "super")
        if not sup:
            return fail("engine_error", "no 'super' partition in the image")
        found = _lp_partition(str(raw), sup[0], BOOTANIM_FS)
        if not found:
            return fail("engine_error",
                        f"could not locate the '{BOOTANIM_FS}' filesystem")
        fs_off, _fs_size = found
        name = getattr(args, "name", None) or ("Roblox" if remove
                                               else "OmniGame")
        err = (_remove_apk_from_product(str(raw), fs_off, name, label)
               if remove else
               _bake_apk_into_product(str(raw), fs_off, str(apk), name, label))
        if err:
            return fail("engine_error", err)
        if not _fsck_ok(str(raw), fs_off, label):
            return fail("engine_error",
                        "refusing to emit an image whose product filesystem is "
                        "not clean")
        staged = work / "out.qcow2"
        print(f"[{label}] importing raw -> qcow2")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-f", "raw",
                        "-O", "qcow2", str(raw), str(staged)],
                       check=True, capture_output=True, timeout=1800)
        bak = disk.with_suffix(".qcow2.bak")
        if not bak.exists():
            shutil.copy2(disk, bak)
            print(f"[{label}] backed up -> {bak.name}")
        shutil.move(str(staged), str(disk))
        print(f"[{label}] wrote {disk}")
        result = {"ok": True, "image": str(disk),
                  "apk": None if remove else str(apk), "removed": remove,
                  "app_dir": f"/product/app/{name}", "package": pkg,
                  "note": ("This system image now ships NO game — every "
                           "Roblox version comes from an offset "
                           "(`omnidroid offset list`)." if remove else
                           "Boot a FRESH account on this image: the game should "
                           "already be installed, with no adb install.")}
        if getattr(args, "json", False):
            emit_json(result)
        return
    except subprocess.CalledProcessError as e:
        return fail("engine_error", f"qemu-img failed: {(e.stderr or b'')[-300:]}")
    finally:
        shutil.rmtree(work, ignore_errors=True)


def _qcow2_backing(path):
    """The qcow2's backing file, or None if it is standalone."""
    try:
        r = subprocess.run([qemu_bin("qemu-img"), "info", "--output=json",
                            str(path)], capture_output=True, text=True,
                           timeout=60)
        return json.loads(r.stdout or "{}").get("backing-filename")
    except Exception:  # noqa: BLE001
        return None


def _brand_target(cfg, base, images):
    """Which image actually supplies /product and the ESP for this base.

    Normally that is the shared `base_disk`, and each account's system overlay is
    a thin COW on top of it — so branding the base reaches every account.

    The DEV base is different and it matters: `build-dev-base --patch-boot`
    produces base_arm_devsystem.qcow2 through a raw export -> re-import cycle,
    which FLATTENS it into a standalone 1.08 GiB image with no backing file
    (base_arm_system.qcow2, by contrast, is a 7 MiB thin overlay). A flattened
    image carries its own copy of every partition, so branding base_arm.qcow2
    does nothing for dev — the devsystem's own blocks shadow it. Brand that image
    directly instead.

    Returns (path, note).
    """
    system = base.get("system")
    if system:
        sys_path = images / system
        if sys_path.exists() and not _qcow2_backing(sys_path):
            return sys_path, (f"{system} is standalone (no backing file), so it "
                              f"shadows the shared base — branding it directly")
    return images / (base.get("base_disk") or ARM_BASE_DISK), None


def cmd_brand_base(args):
    """Bake the Omni loading screen into an arm base image, replacing the vendor
    (LineageOS) boot animation. Build-machine command — see the section comment
    above for why this cannot be done at runtime.

    Writes a NEW image by default: the base is immutable once accounts reference
    it, so overwriting the live one is opt-in (--in-place, which keeps a .bak)."""
    cfg = load_config()
    tag = getattr(args, "base", None) or "arm"
    bases = cfg.get("bases") or {}
    if tag not in bases:
        return fail("no_base", f"no base '{tag}'. Known: {list(bases)}")
    base = bases[tag]
    if base_type(base) != BASE_TYPE_ARM:
        return fail("arch_boundary",
                    f"base '{tag}' is {arch_of_base(base)}; the loading screen "
                    f"is baked into base_x86 at build time already, and this "
                    f"command only knows the arm (super/product) layout.")
    images = Path(cfg["images_dir"])
    disk, why = _brand_target(cfg, base, images)
    if not disk.exists():
        return fail("no_base", f"base disk not found: {disk}")
    anim = Path(getattr(args, "animation", None)
                or (REPO / "assets" / "loading" / "bootanimation.zip"))
    if not anim.exists():
        return fail("engine_error", f"boot animation not found: {anim}")
    # A bootanimation.zip MUST be stored (not deflated) and carry desc.txt, or
    # it silently does not play (see tools/make_bootanimation.py).
    try:
        import zipfile
        with zipfile.ZipFile(anim) as z:
            if "desc.txt" not in z.namelist():
                return fail("engine_error", f"{anim} has no desc.txt")
            bad = [i.filename for i in z.infolist()
                   if i.compress_type != zipfile.ZIP_STORED]
            if bad:
                return fail("engine_error",
                            f"{anim} is DEFLATED ({len(bad)} entries); Android "
                            f"needs a STORED zip. Rebuild it with "
                            f"tools/make_bootanimation.py")
    except zipfile.BadZipFile:
        return fail("engine_error", f"{anim} is not a valid zip")

    label = f"brand-base {tag}"
    if why:
        print(f"[{label}] {why}")
    in_place = bool(getattr(args, "in_place", False))
    if in_place:
        # Every account's system.qcow2 is COW-backed by this file, and a running
        # QEMU holds it open and reads through it live. Replacing it underneath
        # one is not "risky", it is corruption of a guest that is mid-write to
        # its overlay. Refuse rather than warn.
        live = [a["name"] for a in all_accounts()
                if running_pid(a["name"])
                and _brand_target(cfg, cfg["bases"].get(a.get("base"), {}),
                                  images)[0] == disk]
        if live:
            fail("instance_running",
                 f"cannot rewrite {disk.name} in place: "
                 f"{', '.join(live)} {'is' if len(live) == 1 else 'are'} "
                 f"running on it. Stop it first (omnidroid stop {live[0]}), or drop "
                 f"--in-place to write a new image alongside.")
    out = Path(getattr(args, "out", None) or
               (disk if in_place else disk.with_name(
                   disk.stem + "_branded.qcow2")))

    free = shutil.disk_usage(images).free
    need = 6 * 1024 ** 3
    if free < need:
        return fail("engine_error", _scratch_help(images, free, need))

    import tempfile
    work = Path(tempfile.mkdtemp(prefix="omni-brand-", dir=str(images)))
    raw = work / "base.raw"
    try:
        print(f"[{label}] exporting {disk.name} -> raw (5 GiB)")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-f", "qcow2",
                        "-O", "raw", str(disk), str(raw)],
                       check=True, capture_output=True, timeout=1800)
        sup = _gpt_partition(str(raw), "super")
        if not sup:
            return fail("engine_error", "no 'super' partition in the base disk")
        found = _lp_partition(str(raw), sup[0], BOOTANIM_FS)
        if not found:
            return fail("engine_error",
                        f"could not locate the '{BOOTANIM_FS}' filesystem in "
                        f"super (single-linear-extent liblp expected)")
        fs_off, fs_size = found
        print(f"[{label}] super at 0x{sup[0]:x}; {BOOTANIM_FS} fs at "
              f"0x{fs_off:x} ({fs_size / 1048576:.0f} MiB)")

        err = _replace_bootanimation(str(raw), fs_off, str(anim), label)
        if err:
            return fail("engine_error", err)
        if not _fsck_ok(str(raw), fs_off, label):
            return fail("engine_error",
                        "refusing to emit an image whose product filesystem is "
                        "not clean")
        silenced = False
        if not getattr(args, "no_silent_boot", False):
            silenced = _silence_boot(raw, label)

        staged = work / "branded.qcow2"
        print(f"[{label}] importing raw -> qcow2")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-f", "raw",
                        "-O", "qcow2", str(raw), str(staged)],
                       check=True, capture_output=True, timeout=1800)
        if in_place:
            bak = disk.with_suffix(".qcow2.bak")
            if not bak.exists():
                print(f"[{label}] backing up {disk.name} -> {bak.name}")
                shutil.copy2(disk, bak)
        shutil.move(str(staged), str(out))
        size_mb = out.stat().st_size / 1048576
        print(f"[{label}] wrote {out} ({size_mb:.0f} MiB)")
        result = {"ok": True, "base": tag, "image": str(out),
                  "animation": str(anim), "in_place": in_place,
                  "silent_boot": silenced,
                  "product_fs_offset": fs_off,
                  "note": ("Boot an account on this image to confirm the "
                           "animation plays; the vendor logo is only gone once "
                           "you have seen it.")}
        if getattr(args, "json", False):
            emit_json(result)
        return
    except subprocess.CalledProcessError as e:
        return fail("engine_error",
                    f"qemu-img failed: {(e.stderr or b'')[-400:]}")
    finally:
        shutil.rmtree(work, ignore_errors=True)


def _patch_boot_into_overlay(cfg, images, rooted_system, prod_system, staging,
                             label):
    """Magisk-patch the boot partition (vda6) of `rooted_system`, a THIN COW
    overlay of the production system `prod_system`. The production lineage is
    preserved — we never flatten and never touch prod_system.

    Cross-platform + bootstrap-free (no nbd/libguestfs): read the current boot
    image out of prod via a raw export + GPT, patch it with Magisk's
    boot_patch.sh INSIDE a throwaway arm guest (magiskboot patches a FILE, so no
    in-guest root), pull the patched boot back to the host, then write it INTO
    the rooted overlay at the vda6 offset with `qemu-io write`. qemu-io opens the
    qcow2 respecting its backing chain and lands the write in the TOP overlay
    only (COW), so the overlay stays thin AND the write needs no guest root (the
    earlier in-guest `dd` approach failed: the builder shell is not root, so it
    cannot write /dev/block/*). Returns True on success; any failure leaves the
    overlay unrooted (it can just be deleted).
    """
    if not staging.get("magisk") or not staging.get("magiskboot"):
        print(f"[{label}] cannot patch boot: Magisk (magiskboot) was not staged.")
        return False
    import tempfile
    work = Path(tempfile.mkdtemp(prefix="omni_bootpatch_"))
    bname = "_rootpatch"
    d = account_dir(bname)
    try:
        # Read the CURRENT boot image out of the production system (raw export
        # merged through its backing chain), located via GPT. The overlay shares
        # the identical partition layout (it is a COW child), so this offset is
        # valid for writing back into the overlay too.
        full = work / "prod.raw"
        print(f"[{label}] exporting prod system -> raw (merged) to read boot...")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-O", "raw",
                        str(prod_system), str(full)], check=True)
        part = _gpt_partition(full, "boot")
        if not part:
            print(f"[{label}] boot partition not found in GPT; aborting patch.")
            return False
        off, size = part
        boot_img = work / "boot.img"
        with open(full, "rb") as sf, open(boot_img, "wb") as bf:
            sf.seek(off)
            bf.write(sf.read(size))
        full.unlink(missing_ok=True)   # the raw export is only needed for read
        print(f"[{label}] boot partition @ {off} ({size} bytes) extracted")

        # Throwaway arm builder: boots production (its own COW system+data) just
        # to RUN boot_patch.sh on the boot.img FILE. No in-guest root is needed
        # (magiskboot patches a file); the overlay is NOT attached to the guest —
        # the write-back happens host-side below.
        if d.exists():
            shutil.rmtree(d)
        d.mkdir(parents=True)
        adb_port, qmp_port, vnc_port = allocate_ports(cfg)
        acct = {"name": bname, "base": ARM_BASE_TAG,
                "adb_port": adb_port, "qmp_port": qmp_port, "vnc_port": vnc_port,
                "first_boot_done": True}
        save_account(acct)
        arm = cfg["bases"][ARM_BASE_TAG]
        make_overlay(d / "system.qcow2", prod_system)
        make_overlay(d / "data.qcow2", images / arm["data"])
        shutil.copyfile(images / arm.get("efivars", ARM_BASE_EFIVARS),
                        d / "efivars.fd")

        print(f"[{label}] booting arm builder to run boot_patch.sh (headless)")
        spawn_qemu(acct, cfg, interactive=True)
        if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT, label):
            print(f"[{label}] builder boot failed; aborting patch.")
            return False
        W = "/data/local/tmp/omni-bootpatch"
        adb(acct, "shell", f"rm -rf {W}; mkdir -p {W}", timeout=15)
        adb(acct, "push", str(boot_img), f"{W}/boot.img", timeout=300)
        bindir = Path(staging["dir"]) / "bin"
        for f in sorted(bindir.iterdir()):
            if f.is_file():
                adb(acct, "push", str(f), f"{W}/{f.name}", timeout=120)
        r = adb(acct, "shell",
                f"cd {W} && chmod 755 magiskboot magiskinit magisk boot_patch.sh "
                f"2>/dev/null; KEEPVERITY=true KEEPFORCEENCRYPT=true "
                f"RECOVERYMODE=false sh boot_patch.sh boot.img 2>&1; "
                f"echo '---RC='$?; ls -l new-boot.img 2>&1",
                timeout=240)
        out = (r.stdout or "") + (r.stderr or "")
        print(f"[{label}] boot_patch.sh:\n{out.strip()[-2000:]}")
        # Pull the patched boot back to the host.
        patched = work / "new-boot.img"
        adb(acct, "pull", f"{W}/new-boot.img", str(patched), timeout=120)
        _shutdown(acct, label)
        if not patched.exists() or patched.stat().st_size == 0:
            print(f"[{label}] patched boot not produced; aborting (overlay "
                  f"unrooted).")
            return False
        if patched.stat().st_size > size:
            print(f"[{label}] patched boot ({patched.stat().st_size}) exceeds "
                  f"partition ({size}); aborting to avoid corruption.")
            return False
        # HOST-SIDE write-back into the qcow2 overlay via qemu-io. The write goes
        # into the TOP overlay only (COW over prod_system), so it stays thin and
        # needs no guest root. `write -s <file> <offset> <len>` copies the file's
        # bytes in at the byte offset.
        wlen = patched.stat().st_size
        cmd = (f"write -s {shlex.quote(str(patched))} {off} {wlen}")
        wb = subprocess.run([qemu_bin("qemu-io"), "-c", cmd, str(rooted_system)],
                            capture_output=True, text=True)
        print(f"[{label}] qemu-io write -> {rooted_system.name} @ {off}:\n"
              f"{(wb.stdout or '').strip()}{(wb.stderr or '').strip()}")
        if wb.returncode != 0:
            print(f"[{label}] qemu-io write-back failed (rc {wb.returncode}); "
                  f"delete {rooted_system.name} and retry.")
            return False
        print(f"[{label}] boot patched into {rooted_system.name} (thin overlay, "
              f"prod lineage preserved).")
        return True
    except Exception as e:
        print(f"[{label}] boot patch FAILED ({type(e).__name__}: {e}); the "
              f"rooted overlay is left unrooted (safe to delete).")
        return False
    finally:
        if d.exists():
            shutil.rmtree(d, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)


def _spawn_builder_with_disks(acct, cfg, extra_disks, label):
    """Spawn a throwaway arm builder guest (interactive profile) with EXTRA
    QEMU -drive/-device args appended — used to attach the rooted overlay as a
    write target. Thin wrapper: build the normal command, splice in the extras
    before -name, and Popen it like spawn_qemu does."""
    from omnidroid.qemu_proc import qemu_command
    from omnidroid.runtime import runtime_dir
    rd = runtime_dir(acct["name"])
    rd.mkdir(parents=True, exist_ok=True)
    cmd = qemu_command(acct, cfg, interactive=True)
    # Insert the extra disks just before the trailing -name flag.
    ni = cmd.index("-name") if "-name" in cmd else len(cmd)
    cmd = cmd[:ni] + list(extra_disks) + cmd[ni:]
    log = open(rd / "qemu.log", "w")
    kwargs = {"start_new_session": True}
    if IS_WINDOWS:
        kwargs = {"creationflags": 0x00000008 | 0x00000200}
    proc = subprocess.Popen(cmd, stdout=log, stderr=log, **kwargs)
    (rd / "run.json").write_text(json.dumps(
        {"pid": proc.pid, "started": time.time(),
         "identity": f"omni-{acct['name']}", "base": acct["base"],
         "adb_port": acct["adb_port"], "qmp_port": acct["qmp_port"],
         "vnc_port": acct["vnc_port"]}))
    return proc.pid


def root_base(cfg, tag=None, frida_version=DEFAULT_FRIDA_VERSION,
              frida_port=DEFAULT_FRIDA_PORT):
    """Make a shipped base DUAL-USE by ROOTING its image: Magisk-patch the boot
    into a thin rooted system overlay and register the base as rooted. The base
    stays the same production image (same backing chain, same /data provisioning)
    — root is simply baked in, hidden from the game by _enforce_hiding on every
    boot. current_base is unchanged; nothing about which base ships changes.

    arm today (the boot-patch flow is arm-uefi). Brick-risky (it edits a boot
    partition) and must be verified on a real boot — see DEV-BASE.md's successor
    notes. Returns the rooted system disk path on success.
    """
    tag = tag or effective_base_tag(cfg) or ARM_BASE_TAG
    label = f"root-base:{tag}"
    bases = cfg.get("bases", {})
    if tag not in bases:
        fail("no_base", f"no base '{tag}' to root. Known: {list(bases)}")
    base = bases[tag]
    if base_type(base) != BASE_TYPE_ARM:
        # x86 (Bliss) boots kernel + initrd, not a GPT boot partition, so the
        # arm boot-patch flow does not apply. Rooting Bliss means Magisk-patching
        # its initrd ramdisk into base_x86_rooted.initrd.img (auto-registered
        # when present) — a distinct, host-arch-specific build step. The devkit
        # (frida + omni-* tools) is arch-generic and already builds for x86 via
        # `omnidroid build-devkit --arch x86`; only the rooted initrd is x86-only.
        fail("arch_boundary",
             f"root-base's boot-patch flow is arm-uefi only; '{tag}' is "
             f"{arch_of_base(base)}. For x86, produce base_x86_rooted.initrd.img "
             f"by Magisk-patching the Bliss initrd on an x86 host; it is picked "
             f"up automatically. (The x86 devkit already builds with "
             f"`omnidroid build-devkit --arch x86`.)")
    images = Path(cfg["images_dir"])
    prod_system = images / base["system"]
    if not prod_system.exists():
        fail("no_base", f"base system image missing: {prod_system}")

    print(f"[{label}] staging devkit toolset (Magisk) for the boot patch...")
    staging = _stage_devkit("arm", frida_version, True, frida_port, label)
    try:
        # Thin COW overlay of the CURRENT production system — this is where the
        # rooted boot lands; the production system image is never modified.
        rooted_system = images / ARM_ROOTED_SYSTEM
        if rooted_system.exists():
            print(f"[{label}] {ARM_ROOTED_SYSTEM} exists; rebuilding it fresh.")
            rooted_system.unlink()
        make_overlay(rooted_system, prod_system)
        print(f"[{label}] created thin rooted overlay {ARM_ROOTED_SYSTEM} "
              f"(COW on {base['system']})")

        if not _patch_boot_into_overlay(cfg, images, rooted_system, prod_system,
                                        staging, label):
            rooted_system.unlink(missing_ok=True)
            fail("root_failed", "boot patch failed; base left unrooted "
                                "(see the log above).")

        # Rooted /data: the pre-granted matched /data. A rooted SYSTEM alone is
        # NOT shippable — an ungranted /data makes su PROMPT on every production
        # boot. So the base is only registered rooted when the granted /data
        # exists. Prefer an already-built one; else bake it now.
        rooted_data = images / ARM_ROOTED_DATA
        if not rooted_data.exists():
            _bake_rooted_data(cfg, images, base, rooted_system, staging, label)
        data_ok = rooted_data.exists()

        if not data_ok:
            # Keep the (verified) rooted system overlay on disk so a later
            # `root-base` re-run can reuse it, but DO NOT flip the base to rooted
            # — that would strand production on the su prompt.
            print(f"[{label}] boot patch OK, but the pre-granted rooted /data "
                  f"could not be produced (the MagiskSU Grant needs the Magisk "
                  f"manager app installed to show its dialog). Base left "
                  f"UNROOTED. Fix: `omnidroid build-devkit --arch arm` so "
                  f"omni-magisk-setup can install the app, then re-run "
                  f"`omnidroid root-base`. {ARM_ROOTED_SYSTEM} is kept for reuse.")
            return None

        # Re-register the base as rooted, pointing at the rooted matched pair.
        raw = read_config()
        entry = raw["bases"][tag]
        entry["system"] = ARM_ROOTED_SYSTEM
        entry["data"] = ARM_ROOTED_DATA
        entry["rooted"] = True
        notes = entry.get("notes", "")
        if ROOT_PENDING_MARKER in notes:
            notes = notes.replace(ROOT_PENDING_MARKER, ROOTED_MARKER)
        elif ROOTED_MARKER not in notes:
            notes += ROOTED_MARKER
        entry["notes"] = notes
        entry["root_manifest"] = {
            "magisk_version": staging.get("magisk_version"),
            "frida_version": frida_version, "frida_port": frida_port,
            "rooted_system": ARM_ROOTED_SYSTEM,
            "rooted_data": ARM_ROOTED_DATA,
        }
        CONFIG_PATH.write_text(json.dumps(raw, indent=2))
        print(f"[{label}] DONE. Base '{tag}' is now ROOTED (dual-use). "
              f"current_base UNCHANGED ('{raw.get('current_base')}'). VERIFY on "
              f"a real boot: `omnidroid start <name>` then `omnidroid adb <name> -- shell "
              f"/debug_ramdisk/su 0 id` should show uid=0.")
        return rooted_system
    finally:
        shutil.rmtree(staging["dir"], ignore_errors=True)


def _grant_su_via_dialog(acct, label, tries=8):
    """Headlessly approve MagiskSU's first-request dialog (SuRequestActivity).

    On a fresh /data the first `su` pops a GUI approval dialog and blocks. We
    trigger it in the BACKGROUND (so nothing hangs), then find the Grant button
    with `uiautomator dump` and tap its centre — the same thing a human does over
    VNC, done headlessly. Returns True if su works afterward. This is the one
    step the old dev /data needed a manual tap for."""
    import re as _re
    # Trigger the prompt without blocking: fire su in the background. It will sit
    # on the dialog; our tap releases it.
    for cand in SU_CANDIDATES:
        adb(acct, "shell",
            f"(setsid {cand} 0 id >/data/local/tmp/su_probe 2>&1 &) ; true",
            timeout=8)
    for _ in range(tries):
        time.sleep(3)
        try:
            adb(acct, "shell", "uiautomator dump /data/local/tmp/ui.xml",
                timeout=20)
            r = adb(acct, "shell", "cat /data/local/tmp/ui.xml", timeout=15)
        except Exception:  # noqa: BLE001
            continue
        xml = r.stdout or ""
        # Find the Grant node: text/content-desc/resource-id mentioning grant.
        node = None
        for m in _re.finditer(r'<node[^>]*bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"[^>]*/?>',
                              xml):
            seg = xml[max(0, m.start() - 400):m.end() + 1]
            if _re.search(r'(?i)(text|content-desc)="grant"|resource-id="[^"]*grant',
                          seg):
                x = (int(m.group(1)) + int(m.group(3))) // 2
                y = (int(m.group(2)) + int(m.group(4))) // 2
                node = (x, y)
                break
        if node:
            adb(acct, "shell", "input", "tap", str(node[0]), str(node[1]),
                timeout=15)
            print(f"[{label}] su dialog: tapped Grant @ {node}")
            time.sleep(2)
        if resolve_su(acct):
            return True
    return bool(resolve_su(acct))


def _bake_rooted_data(cfg, images, base, rooted_system, staging, label):
    """Produce base_arm_data_rooted.qcow2: the production /data with Magisk's
    policy pre-configured (shell su granted Forever, root_access=3, zygisk=1,
    denylist=1, the game on the DenyList) so root is headless from first boot and
    the game is hidden. Boots the ROOTED system with a COW of prod /data, grants
    su via the dialog, applies the policy over adb, then captures that /data.

    Returns True only if it produced a genuinely pre-granted rooted /data. A
    False return means the caller must NOT register the base rooted — a rooted
    system with an ungranted /data would prompt for su on every production boot.
    """
    print(f"[{label}] baking rooted /data (headless su + hiding policy)...")
    bname = "_rootdata"
    d = account_dir(bname)
    try:
        if d.exists():
            shutil.rmtree(d)
        d.mkdir(parents=True)
        adb_port, qmp_port, vnc_port = allocate_ports(cfg)
        acct = {"name": bname, "base": ARM_BASE_TAG,
                "adb_port": adb_port, "qmp_port": qmp_port, "vnc_port": vnc_port,
                "game_package": ROBLOX_PACKAGE, "first_boot_done": True}
        save_account(acct)
        # Boot the ROOTED system overlay + a private COW of prod /data.
        make_overlay(d / "system.qcow2", rooted_system)
        make_overlay(d / "data.qcow2", images / base["data"])
        shutil.copyfile(images / base.get("efivars", ARM_BASE_EFIVARS),
                        d / "efivars.fd")
        # Attach the devkit disk so omni-magisk-setup's files are reachable.
        devkit = images / devkit_disk_name("arm")
        extra = []
        if devkit.exists():
            extra = ["-device", "virtio-blk-pci,drive=vdKIT",
                     "-drive", f"file={devkit},if=none,id=vdKIT,format=qcow2,"
                               f"snapshot=on"]
        _spawn_builder_with_disks(acct, cfg, extra, label)
        if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT, label):
            print(f"[{label}] rooted boot did not come up; cannot bake /data.")
            return False
        su = resolve_su(acct)
        if not su:
            # Fresh /data: the first su prompts, and SuRequestActivity only
            # renders if the Magisk MANAGER APP is installed AND magiskd has
            # registered it as the manager (which happens on the boot AFTER
            # install). So: push+install the staged apk, REBOOT so magiskd picks
            # it up, then approve the dialog headlessly.
            apk = Path(staging["dir"]) / "magisk.apk"
            if apk.exists():
                try:
                    adb(acct, "push", str(apk), "/data/local/tmp/magisk.apk",
                        timeout=120)
                    ir = adb(acct, "shell",
                             "pm install -r /data/local/tmp/magisk.apk 2>&1",
                             timeout=120)
                    print(f"[{label}] Magisk app install: "
                          f"{((ir.stdout or '')+(ir.stderr or '')).strip()[-120:]}")
                    print(f"[{label}] rebooting so magiskd registers the manager...")
                    try:
                        adb(acct, "shell", "reboot", timeout=10)
                    except Exception:  # noqa: BLE001 — reboot drops the adb conn
                        pass
                    time.sleep(8)
                    adb_connect(acct)
                    if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT, label):
                        print(f"[{label}] guest did not come back after reboot.")
                        _shutdown(acct, label)
                        return False
                except Exception as e:  # noqa: BLE001
                    print(f"[{label}] Magisk app install/reboot failed: {e}")
            print(f"[{label}] su prompts on this fresh /data; approving the "
                  f"MagiskSU dialog headlessly...")
            if _grant_su_via_dialog(acct, label):
                su = resolve_su(acct)
        if not su:
            print(f"[{label}] could NOT obtain headless su (dialog not approved "
                  f"— Magisk app may not be installed to show it). Baking a "
                  f"granted /data needs a one-time Grant; skipping.")
            _shutdown(acct, label)
            return False
        # Pre-grant shell su Forever + turn on zygisk/denylist + the game on the
        # DenyList, straight into the Magisk policy DB.
        policy = (
            'M=""; for m in /debug_ramdisk/magisk /sbin/magisk magisk; do '
            '"$m" -v >/dev/null 2>&1 && { M="$m"; break; }; done; '
            '"$M" --sqlite "REPLACE INTO policies (uid,policy,until,logging,'
            'notification) VALUES(2000,2,0,1,1)" >/dev/null 2>&1; '
            '"$M" --sqlite "REPLACE INTO settings (key,value) VALUES(\'root_access\',3)" >/dev/null 2>&1; '
            '"$M" --sqlite "REPLACE INTO settings (key,value) VALUES(\'zygisk\',1)" >/dev/null 2>&1; '
            '"$M" --sqlite "REPLACE INTO settings (key,value) VALUES(\'denylist\',1)" >/dev/null 2>&1; '
            f'"$M" --denylist add {ROBLOX_PACKAGE} >/dev/null 2>&1; echo POLICY_OK')
        pr = adb(acct, "shell", f"{su} 0 sh -c {shlex.quote(policy)}", timeout=45)
        pout = ((pr.stdout or "") + (pr.stderr or "")).strip()
        print(f"[{label}] policy: {pout[-200:]}")
        if "POLICY_OK" not in pout:
            print(f"[{label}] policy write did not confirm; not capturing /data.")
            _shutdown(acct, label)
            return False
        _shutdown(acct, label)
        # Capture the resulting /data (flattened, standalone) as the rooted /data.
        rooted_data = images / ARM_ROOTED_DATA
        tmp = rooted_data.with_suffix(".tmp.qcow2")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-O", "qcow2", "-c",
                        str(d / "data.qcow2"), str(tmp)], check=True)
        tmp.replace(rooted_data)
        print(f"[{label}] captured rooted /data -> {ARM_ROOTED_DATA}")
        return True
    except Exception as e:
        print(f"[{label}] rooted /data bake failed ({type(e).__name__}: {e}).")
        return False
    finally:
        if d.exists():
            shutil.rmtree(d, ignore_errors=True)


def cmd_build_devkit(args):
    ensure_qemu()
    cfg = load_config()
    arch = getattr(args, "arch", None) or host_arch_token()
    disk = build_devkit(cfg, arch=arch,
                        frida_version=args.frida_version,
                        frida_port=args.frida_port,
                        include_magisk=not args.no_magisk)
    if getattr(args, "json", False):
        emit_json({"ok": True, "arch": arch, "devkit_disk": Path(disk).name})


def cmd_root_base(args):
    ensure_qemu()
    cfg = load_config()
    tag = getattr(args, "base", None)
    disk = root_base(cfg, tag=tag, frida_version=args.frida_version,
                     frida_port=args.frida_port)
    if getattr(args, "json", False):
        raw = read_config()
        rtag = tag or effective_base_tag(raw) or ARM_BASE_TAG
        # disk is None when the rooted SYSTEM built but the pre-granted /data
        # could not be produced — the base is deliberately left UNROOTED.
        emit_json({"ok": disk is not None, "base": rtag,
                   "rooted": disk is not None,
                   "rooted_system": (Path(disk).name if disk else None),
                   "current_base": raw.get("current_base"),
                   "root_manifest": raw["bases"].get(rtag, {}).get("root_manifest")})


def _host_rss_mb(pid):
    """Resident set of one QEMU process in MB, or None if it is gone."""
    try:
        r = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                           capture_output=True, text=True, timeout=10)
        v = (r.stdout or "").strip().split()
        return int(v[0]) / 1024 if v else None
    except Exception:
        return None


def _host_total_mb():
    """Total physical RAM of this host in MB, or None if unobtainable."""
    try:
        if IS_MACOS:
            r = subprocess.run(["sysctl", "-n", "hw.memsize"],
                               capture_output=True, text=True, timeout=10)
            return int(r.stdout.strip()) / 1048576
        if IS_LINUX:
            for line in Path("/proc/meminfo").read_text().splitlines():
                if line.startswith("MemTotal:"):
                    return int(line.split()[1]) / 1024
    except Exception:
        pass
    return None


def _runtime_handle(name):
    """A read-only account handle for a RUNNING instance, from its run.json.

    Enough to talk to the instance (adb + QMP ports) and nothing more. The
    point is that it allocates nothing and writes nothing, so read-only
    commands cannot disturb a live instance the way build_acct() would."""
    try:
        run = json.loads((runtime_dir(name) / "run.json").read_text())
    except Exception:
        return None
    if not all(k in run for k in ("adb_port", "qmp_port")):
        return None
    return {"name": name, "adb_port": run["adb_port"],
            "qmp_port": run["qmp_port"],
            "vnc_port": run.get("vnc_port"), "base": run.get("base")}


def cmd_measure(args):
    """Report what each RUNNING instance actually costs the host.

    Exists because every capacity number in this project used to be an
    assertion. The farming mode shipped at 512 MB on the theory that a
    squeezed instance needs no more, and nobody had booted one — it does not
    boot at all. This command is the cheapest way to keep that from
    recurring: it samples real processes and prints what it saw.

    Host RSS is sampled repeatedly and reduced to a median (see
    measure.summarize) because free-page-reporting makes any single sample
    close to meaningless."""
    from omnidroid import measure as _m
    from omnidroid.runtime import reconcile_runtime
    reconcile_runtime()
    names = ([args.name] if getattr(args, "name", None)
             else [a["name"] for a in all_accounts() if running_pid(a["name"])])
    if not names:
        return fail("no_instance",
                    "no running instances to measure. Start one first: "
                    "`omnidroid start <name> --mode farming`")
    n_samples = max(1, getattr(args, "samples", 8))
    interval = max(1, getattr(args, "interval", 5))

    rows, series = [], {n: [] for n in names}
    # Read-only handles built straight from run.json. Emphatically NOT
    # build_acct(): that ALLOCATES — it reserves a port triple and rewrites
    # run.json — so calling it here replaced a live instance's record with a
    # `reserving` placeholder and made the engine lose track of the QEMU it
    # was in the middle of measuring. A measurement command must not be able
    # to change what it measures.
    accts = {n: _runtime_handle(n) for n in names}
    # Interleave the sampling across instances so every instance is observed
    # over the SAME wall-clock window. Sampling them one after another would
    # compare an instance measured while its neighbour was booting against
    # one measured while the host was quiet.
    for i in range(n_samples):
        for name in names:
            pid = running_pid(name)
            series[name].append(_host_rss_mb(pid) if pid else None)
        if i < n_samples - 1:
            time.sleep(interval)

    for name in names:
        pid = running_pid(name)
        run = {}
        try:
            run = json.loads((runtime_dir(name) / "run.json").read_text())
        except Exception:
            pass
        acct = accts.get(name)
        guest_total = guest_avail = None
        balloon_mb = None
        if acct:
            try:
                out = adb(acct, "shell", "cat", "/proc/meminfo",
                          timeout=20).stdout
                guest_used = _m.parse_guest_used_kb(out)
                for line in (out or "").splitlines():
                    if line.startswith("MemTotal:"):
                        guest_total = int(line.split()[1]) / 1024
                    elif line.startswith("MemAvailable:"):
                        guest_avail = int(line.split()[1]) / 1024
            except Exception:
                guest_used = None
            r = qmp(acct, "query-balloon") or {}
            actual = (r.get("return") or {}).get("actual")
            balloon_mb = int(actual / 1048576) if actual else None
        else:
            guest_used = None
        host = _m.summarize(series[name])
        # KSM-merged pages for THIS qemu (Linux, kernel >= 6.1). This is the
        # number that decides whether 50 instances fit: 50 guests booted from
        # one base image hold overwhelmingly identical pages, and KSM
        # collapses them to one physical copy. Per-instance RSS counts a
        # merged page for every instance sharing it, so RSS alone
        # systematically over-states a large fleet.
        rows.append({"name": name, "mode": run.get("mode"),
                     "base": run.get("base"), "pid": pid,
                     "host_rss_median_mb": host["median_mb"],
                     "host_rss_max_mb": host["max_mb"],
                     "guest_total_mb": round(guest_total, 1) if guest_total else None,
                     "guest_avail_mb": round(guest_avail, 1) if guest_avail else None,
                     "guest_used_mb": round(guest_used / 1024, 1) if guest_used else None,
                     "balloon_mb": balloon_mb,
                     "ksm_merged_mb": (round(pid_ksm_merged_mb(pid), 1)
                                       if pid and IS_LINUX else None)})

    host_total = _host_total_mb()
    medians = [r["host_rss_median_mb"] for r in rows if r["host_rss_median_mb"]]
    per_inst = (sum(medians) / len(medians)) if medians else None
    # Capacity is planned against the CEILING, not against what the instances
    # happen to be using while you look at them. An observed median is a
    # snapshot of instances that may be idle, pre-game, or mid-reclaim; the
    # balloon cap is the most memory the guest is permitted to hold, so it is
    # the only number that a host is guaranteed to survive. Planning off the
    # median is how you fit "128 instances" onto a box that dies at 14.
    caps = [r["balloon_mb"] for r in rows if r["balloon_mb"]]
    ceiling = max(caps) if caps else None
    planning_mb = ceiling or per_inst
    result = {"ok": True, "instances": rows,
              "samples": n_samples, "interval_s": interval,
              "host_total_mb": round(host_total) if host_total else None,
              "per_instance_median_mb": round(per_inst, 1) if per_inst else None,
              "planning_mb": round(planning_mb) if planning_mb else None,
              "planning_basis": ("balloon cap" if ceiling
                                 else "observed median (NO balloon cap set - "
                                      "this is a floor, not a ceiling)"),
              "estimated_capacity": (
                  _m.capacity(host_total, planning_mb)
                  if (host_total and planning_mb) else None),
              "ksm": _ksm_summary(rows),
              "platform_note": (
                  "macOS/HVF: the balloon and free-page-reporting are advisory "
                  "here - QEMU's madvise does not reliably decommit, so host "
                  "RSS overstates what the same instance costs on Linux/KVM. "
                  "Treat the 50+-instance target as a Linux number."
                  if IS_MACOS else
                  "Linux/KVM: balloon + free-page-reporting decommit for real; "
                  "enable KSM (`omnidroid ksm --on`) to dedup identical guest pages "
                  "across instances on top of this.")}
    if getattr(args, "json", False):
        emit_json(result)
        return
    print(f"{'instance':<18}{'mode':<10}{'host RSS med':>13}"
          f"{'max':>8}{'guest used':>12}{'balloon':>9}")
    for r in rows:
        print(f"{r['name']:<18}{str(r['mode'] or '?'):<10}"
              f"{_fmt_mb(r['host_rss_median_mb']):>13}"
              f"{_fmt_mb(r['host_rss_max_mb']):>8}"
              f"{_fmt_mb(r['guest_used_mb']):>12}"
              f"{_fmt_mb(r['balloon_mb']):>9}")
    if per_inst:
        print(f"\n{len(rows)} instance(s) sampled {n_samples}x/{interval}s; "
              f"median {per_inst:.0f} MB each right now")
    if host_total and planning_mb:
        print(f"planning at {planning_mb:.0f} MB/instance "
              f"({result['planning_basis']})")
        print(f"host has {host_total / 1024:.1f} GiB -> ~"
              f"{result['estimated_capacity']} instances "
              f"(2 GiB reserved for the host)")
        if not ceiling:
            print("set a balloon cap (--balloon, or use --mode farming) to "
                  "make this a ceiling rather than a guess")
    k = result["ksm"]
    if not k.get("available") or not k.get("running"):
        print(f"KSM: {k.get('why')}")
    else:
        print(f"KSM: {k['saved_mb']:.0f} MB deduplicated across the fleet "
              f"({k['full_scans']} full scans)")
        print("  -> the per-instance rows above each count shared pages "
              "separately, so a large fleet costs LESS than their sum")
    print(f"\nNOTE: {result['platform_note']}")


def _fmt_mb(v):
    return f"{v:.0f}M" if v else "-"


def _ksm_summary(rows):
    """Fleet-wide KSM picture, or a reason there isn't one.

    Kept separate from the per-instance rows because the interesting quantity
    is fleet-level: KSM's whole value here is that N instances of the SAME
    base hold N copies of the same Android pages, and it collapses them to
    one. That saving does not belong to any single instance, and per-instance
    RSS double-counts it — which is exactly why a fleet's real cost is lower
    than summing the rows suggests."""
    if not IS_LINUX:
        return {"available": False,
                "why": "KSM is a Linux kernel feature; this host is not Linux"}
    if not ksm_available():
        return {"available": False,
                "why": "no /sys/kernel/mm/ksm - kernel built without KSM"}
    stats = ksm_stats() or {}
    if not stats.get("run"):
        return {"available": True, "running": False,
                "why": "KSM is present but OFF - turn it on with "
                       "`omnidroid ksm --on`. Until then every instance keeps its "
                       "own copy of identical guest pages."}
    merged = [r["ksm_merged_mb"] for r in rows if r.get("ksm_merged_mb")]
    return {"available": True, "running": True,
            "saved_mb": round(ksm_saved_mb(stats), 1),
            "merged_mb_per_instance": round(sum(merged) / len(merged), 1)
            if merged else None,
            "pages_sharing": stats.get("pages_sharing"),
            "full_scans": stats.get("full_scans")}


def _bake_lean_props(raw_path, fs_off, label, props=None):
    """Merge the lean profile into build.prop inside the ext4 at fs_off.

    Returns (None, path, added) on success or (error_string, None, 0). The
    image is left untouched on any failure — this runs against a base that
    every account overlay is COW-backed by, so a half-written build.prop is
    not a bug, it is a fleet-wide brick."""
    import tempfile
    dbg = _debugfs_bin()
    if not dbg:
        return ("debugfs (e2fsprogs) not found — needed to edit the system "
                "filesystem. macOS: brew install e2fsprogs. "
                "Debian/Ubuntu: apt install e2fsprogs.", None, 0)
    dev = f"{raw_path}?offset={fs_off}"
    tmp = Path(tempfile.mkdtemp(prefix="omni-lean-"))
    try:
        # Probe for build.prop: the layout differs between system-as-root and
        # nested-system images, and guessing wrong would either fail loudly
        # (fine) or write a build.prop nothing reads (not fine).
        target, current = None, ""
        for cand in lean.BUILD_PROP_CANDIDATES:
            out = tmp / "cur.prop"
            subprocess.run([dbg, "-R", f"dump {cand} {out}", dev],
                           capture_output=True, text=True, timeout=120)
            if out.exists() and out.stat().st_size:
                target = cand
                current = out.read_text(errors="replace")
                break
        if not target:
            return (f"no build.prop found in this filesystem (tried "
                    f"{', '.join(lean.BUILD_PROP_CANDIDATES)})", None, 0)

        # Preserve the SELinux label. build.prop is read by init and by every
        # process that reads a system property file; an unlabelled
        # replacement is denied and the guest boots with none of these
        # properties (or does not boot). Read the REAL label off the image
        # rather than hardcoding one — it differs across images.
        ea = tmp / "label.val"
        subprocess.run([dbg, "-R", f"ea_get -f {ea} {target} security.selinux",
                        dev], capture_output=True, text=True, timeout=120)
        label_val = ea.read_bytes() if ea.exists() else b""
        if not label_val:
            return (f"could not read the SELinux label of {target}; refusing "
                    f"to write an unlabelled build.prop", None, 0)

        props = lean.baked_props() if props is None else props
        merged = tmp / "build.prop"
        merged.write_text(lean.merge_build_prop(current, props))
        script = tmp / "cmds.txt"
        script.write_text(
            f"rm {target}\n"
            f"cd {str(Path(target).parent) or '/'}\n"
            f"write {merged} {Path(target).name}\n"
            f"sif {target} mode 0100644\n"
            f"sif {target} uid 0\n"
            f"sif {target} gid 0\n"
            f"ea_set -f {ea} {target} security.selinux\n"
        )
        r = subprocess.run([dbg, "-w", "-f", str(script), dev],
                           capture_output=True, text=True, timeout=300)
        out = (r.stdout or "") + (r.stderr or "")
        if "Allocated inode" not in out:
            return (f"debugfs write did not land: {out.strip()[-400:]}",
                    None, 0)

        # Read back and verify EVERY property, rather than trusting the
        # write. A build.prop that is present but missing ro.config.low_ram
        # is the failure mode that costs a whole rebuild to notice.
        back = tmp / "readback.prop"
        subprocess.run([dbg, "-R", f"dump {target} {back}", dev],
                       capture_output=True, text=True, timeout=120)
        if not back.exists():
            return ("readback of the written build.prop failed", None, 0)
        text = back.read_text(errors="replace")
        missing = [f"{k}={v}" for k, v in props.items()
                   if f"{k}={v}" not in text]
        if missing:
            return (f"{len(missing)} propert"
                    f"{'y' if len(missing) == 1 else 'ies'} missing after "
                    f"write (e.g. {missing[0]})", None, 0)
        rl = subprocess.run([dbg, "-R", f"ea_list {target}", dev],
                            capture_output=True, text=True, timeout=120)
        if "security.selinux" not in (rl.stdout or ""):
            return (f"SELinux label missing after write on {target}",
                    None, 0)
        print(f"[{label}] {target}: {len(props)} lean properties baked "
              f"(label preserved)")
        return None, target, len(props)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _backing_args(backing):
    """qemu-img convert args that make the OUTPUT a thin overlay on `backing`.

    The flag was renamed: `-B` up to QEMU 10.0, `-b` (plus an explicit
    `-F` backing format) after. Probe the help text rather than the version
    string — the help is what this binary will actually accept."""
    try:
        h = subprocess.run([qemu_bin("qemu-img"), "convert", "--help"],
                           capture_output=True, text=True, timeout=30)
        text = (h.stdout or "") + (h.stderr or "")
    except Exception:
        text = ""
    if "--backing" in text or "-b, " in text:
        return ["-b", str(backing), "-F", "qcow2"]
    return ["-B", str(backing)]


def scratch_needed(disk):
    """Bytes of free space a qcow2 round trip on `disk` really needs.

    Peak usage is the RAW export plus the thin overlay we emit, not two full
    copies — so this is sized from the image's ACTUAL allocated size (what
    the raw will occupy on any filesystem with sparse-file support: APFS,
    ext4, btrfs, xfs) plus a 1 GiB margin.

    The previous flat 6 GiB was a constant copied from an older command that
    wrote a full second copy. Over-stating the requirement is not harmless
    here: it is the difference between telling someone to free one stale
    backup and telling them to free three."""
    try:
        r = subprocess.run([qemu_bin("qemu-img"), "info", "--output=json",
                            str(disk)], capture_output=True, text=True,
                           timeout=60)
        info = json.loads(r.stdout)
        actual = int(info.get("actual-size") or info["virtual-size"])
    except Exception:
        # Unknown -> fall back to the old conservative figure rather than
        # guess low and fail halfway through a multi-GiB conversion.
        return 6 * 1024 ** 3
    return actual + 1024 ** 3


def reclaimable_backups(images):
    """Backup images in `images` whose ORIGINAL still exists, newest last.

    Returns [(path, bytes)]. A base-image round trip needs several GiB of
    scratch, and on a full disk the useful answer is not "free some space" —
    it is which specific files are redundant. Only files matching a backup
    suffix are listed, and only when the thing they back up is still present,
    so nothing here is a last copy. This ADVISES; it never deletes."""
    out = []
    for p in sorted(Path(images).glob("*")):
        if not p.is_file():
            continue
        name = p.name
        orig = None
        if name.endswith(".bak"):
            orig = p.with_name(name[:-4])
        elif ".safebak" in name:
            orig = p.with_name(name.split(".safebak")[0])
        if orig and orig.exists() and orig != p:
            out.append((p, p.stat().st_size))
    return sorted(out, key=lambda t: t[1])


def _scratch_help(scratch, free, need, images=None):
    """The 'not enough disk' message, naming what is actually reclaimable.

    Offers BOTH ways out: move the scratch elsewhere, or reclaim a redundant
    backup. Deleting someone's backups should never be the only option a
    tool leaves them."""
    msg = (f"need ~{need // 1024**3} GiB free in {scratch} for the qcow2 "
           f"round trip, have {free / 1024**3:.1f} GiB.\n\n"
           f"Either point the scratch at another volume with "
           f"--scratch-dir /path/on/another/disk (it holds only temporary "
           f"state; the image written at the end is a thin overlay of a few "
           f"hundred KB), or reclaim space below.")
    cands = reclaimable_backups(images or scratch)
    if not cands:
        return msg + " No redundant backup images found to reclaim."
    total = sum(sz for _, sz in cands)
    lines = [f"  {p.name}  ({sz / 1024**3:.1f} GiB)" for p, sz in cands[::-1]]
    return (msg + f"\n\nThese {len(cands)} file(s) in {images} are BACKUPS "
            f"whose original is still present, totalling "
            f"{total / 1024**3:.1f} GiB:\n" + "\n".join(lines) +
            f"\n\nThey are yours to keep or delete - this command will not "
            f"touch them. Removing ONE of the larger ones is usually enough "
            f"to reach {need // 1024**3} GiB.")


def cmd_enable_zram_base(args):
    """Bake `persist.sys.zram_enabled=1` into a base image's build.prop.

    This is how a NON-ROOTED production instance gets zram, and it is one
    property because the base already ships everything else: the zram device,
    the lz4 compressor, an fstab entry sized `zramsize=50%`, and an init
    trigger that runs swapon_all when this property is 1 (all read off a live
    instance; see lean.py). Verified there: flipping it took SwapTotal from 0
    to 470980 kB with no other change.

    Worth it because zram is a third of the per-instance footprint — lz4
    compressed 496 MB of guest pages into 167 MB (2.97x measured), which
    takes the safe balloon cap from 1536 MB to 1024 MB and a 64 GB server
    from ~40 to ~62 instances.

    Unlike `strip-base` this is NOT gated. That gate exists because the
    low-RAM PROFILE was measured to break boot; this is a single
    LineageOS-supported toggle for a subsystem the image already carries, and
    it is a persist.* property rather than one of the ro.* framework switches
    that caused the RescueParty loop. Build-machine command (e2fsprogs +
    scratch for the qcow2 round trip); writes a NEW image unless --in-place.
    """
    cfg = load_config()
    tag = getattr(args, "base", None) or effective_base_tag(cfg) or "arm"
    bases = cfg.get("bases") or {}
    if tag not in bases:
        return fail("no_base", f"no base '{tag}'. Known: {list(bases)}")
    base = bases[tag]
    if base_type(base) != BASE_TYPE_ARM:
        return fail("arch_boundary",
                    f"base '{tag}' is {arch_of_base(base)}; this command only "
                    f"knows the arm (super/logical-partition) layout.")
    images = Path(cfg["images_dir"])
    disk, why = _brand_target(cfg, base, images)
    if not disk.exists():
        return fail("no_base", f"base disk not found: {disk}")

    label = f"enable-zram-base {tag}"
    if why:
        print(f"[{label}] {why}")
    in_place = bool(getattr(args, "in_place", False))
    if in_place:
        live = [a["name"] for a in all_accounts()
                if running_pid(a["name"])
                and _brand_target(cfg, bases.get(a.get("base"), {}),
                                  images)[0] == disk]
        if live:
            return fail("instance_running",
                        f"cannot rewrite {disk.name} in place: "
                        f"{', '.join(live)} running on it. Stop it first, or "
                        f"drop --in-place.")
    out = Path(getattr(args, "out", None) or
               (disk if in_place else
                disk.with_name(disk.stem + "_zram.qcow2")))

    # Scratch may live on ANOTHER volume. The round trip's peak cost is the
    # raw export, which is pure temporary state -- there is no reason it must
    # sit next to the images, and on a full internal disk an external drive
    # is a far better answer than deleting someone's backups.
    scratch = Path(getattr(args, "scratch_dir", None) or images).expanduser()
    if not scratch.is_dir():
        return fail("engine_error", f"--scratch-dir not a directory: {scratch}")
    free = shutil.disk_usage(scratch).free
    need = scratch_needed(disk)
    if free < need:
        return fail("engine_error", _scratch_help(scratch, free, need,
                                                  images=images))

    import tempfile
    work = Path(tempfile.mkdtemp(prefix="omni-zram-", dir=str(scratch)))
    raw = work / "base.raw"
    try:
        print(f"[{label}] exporting {disk.name} -> raw")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-f", "qcow2",
                        "-O", "raw", str(disk), str(raw)],
                       check=True, capture_output=True, timeout=1800)
        sup = _gpt_partition(str(raw), "super")
        if not sup:
            return fail("engine_error", "no 'super' partition in the base disk")
        found = _lp_partition(str(raw), sup[0], LEAN_PROP_FS)
        if not found:
            return fail("engine_error",
                        f"could not locate the '{LEAN_PROP_FS}' filesystem "
                        f"in super")
        fs_off, fs_size = found
        print(f"[{label}] super at 0x{sup[0]:x}; {LEAN_PROP_FS} fs at "
              f"0x{fs_off:x} ({fs_size / 1048576:.0f} MiB)")

        # Same verified surgery strip-base uses (probe, preserve the SELinux
        # label, merge rather than append, read back every key).
        err, prop_path, n = _bake_lean_props(str(raw), fs_off, label,
                                             props=lean.ZRAM_ENABLE_PROP)
        if err:
            return fail("engine_error", err)
        if not _fsck_ok(str(raw), fs_off, label):
            return fail("engine_error",
                        f"refusing to emit an image whose {LEAN_PROP_FS} "
                        f"filesystem is not clean")

        staged = work / "zram.qcow2"
        # Write the result as a THIN OVERLAY backed by the original rather
        # than a full second copy. Only clusters that actually differ get
        # stored, and a one-property build.prop edit differs in a handful —
        # so the output is hundreds of KB instead of ~2.3 GiB, and the peak
        # scratch is just the raw export. That is the difference between
        # needing ~3 GiB free and needing ~6, which on a full disk is the
        # difference between deleting one stale backup and three.
        #
        # This is the same shape the project already relies on:
        # base_arm_system.qcow2 is itself a 7.5 MiB overlay on the shared
        # base. In-place is the exception — it must be self-contained, since
        # an image cannot be its own backing file.
        conv = [qemu_bin("qemu-img"), "convert", "-f", "raw", "-O", "qcow2"]
        if not in_place:
            conv += _backing_args(disk)
        print(f"[{label}] importing raw -> qcow2"
              + ("" if in_place else f" (thin, backed by {disk.name})"))
        subprocess.run(conv + [str(raw), str(staged)],
                       check=True, capture_output=True, timeout=1800)
        if in_place:
            bak = disk.with_suffix(".qcow2.bak")
            if not bak.exists():
                print(f"[{label}] backing up {disk.name} -> {bak.name}")
                shutil.copy2(disk, bak)
        shutil.move(str(staged), str(out))
        print(f"[{label}] wrote {out} ({out.stat().st_size / 1048576:.0f} MiB)")
        result = {"ok": True, "base": tag, "image": str(out),
                  "build_prop": prop_path, "properties": n,
                  "in_place": in_place,
                  "note": ("Boot a farming instance on this image and check "
                           "`grep SwapTotal /proc/meminfo` is non-zero; the "
                           "balloon then picks the lower 1024 MB cap itself.")}
        if getattr(args, "json", False):
            emit_json(result)
        return
    except subprocess.CalledProcessError as e:
        return fail("engine_error",
                    f"qemu-img failed: {(e.stderr or b'')[-400:]}")
    finally:
        shutil.rmtree(work, ignore_errors=True)


def cmd_strip_base(args):
    """Bake the low-RAM property profile (lean.py) into a base image.

    This is the half of the footprint work that CANNOT be done at runtime:
    `ro.*` properties are frozen by init once set, so `ro.config.low_ram` —
    the single biggest Android-level memory lever there is — has to be in the
    image. Everything that can be done over adb lives in the farming squeeze
    instead.

    Build-machine command. Writes a NEW image by default, because a base is
    immutable once any account references it (--in-place keeps a .bak and
    refuses while an instance is running on the image)."""
    # HARD GATE, first thing, before any work or any disk is touched.
    # strip-base writes to an image every account's COW overlay is backed by,
    # and the profile it would write is known to break boot (see lean.py:
    # recovery/RescueParty with the full set, never-boots with the memory-only
    # subset). Running it blind would take out the whole fleet at once, so it
    # refuses unless a human has explicitly accepted that.
    force = bool(getattr(args, "force_unverified", False))
    if not lean.PROFILE_VERIFIED and not force:
        return fail("profile_unverified",
                    f"refusing to bake: {lean.PROFILE_EVIDENCE} "
                    f"Re-run with --force-unverified if you are bisecting and "
                    f"understand that the resulting image may not boot. "
                    f"Prefer bisecting on the DEV base with a Magisk "
                    f"system.prop module first - it needs no disk space and "
                    f"no image is modified.")
    cfg = load_config()
    tag = getattr(args, "base", None) or effective_base_tag(cfg) or "arm"
    bases = cfg.get("bases") or {}
    if tag not in bases:
        return fail("no_base", f"no base '{tag}'. Known: {list(bases)}")
    base = bases[tag]
    if base_type(base) != BASE_TYPE_ARM:
        return fail("arch_boundary",
                    f"base '{tag}' is {arch_of_base(base)}; strip-base only "
                    f"knows the arm (super/logical-partition) layout. The x86 "
                    f"base carries its properties in its own build tree — "
                    f"bake them there and rebuild with `omnidroid rebuild-base`.")
    images = Path(cfg["images_dir"])
    disk, why = _brand_target(cfg, base, images)
    if not disk.exists():
        return fail("no_base", f"base disk not found: {disk}")

    label = f"strip-base {tag}"
    if why:
        print(f"[{label}] {why}")
    in_place = bool(getattr(args, "in_place", False))
    if in_place:
        live = [a["name"] for a in all_accounts()
                if running_pid(a["name"])
                and _brand_target(cfg, bases.get(a.get("base"), {}),
                                  images)[0] == disk]
        if live:
            return fail("instance_running",
                        f"cannot rewrite {disk.name} in place: "
                        f"{', '.join(live)} "
                        f"{'is' if len(live) == 1 else 'are'} running on it. "
                        f"Stop it first (omnidroid stop {live[0]}), or drop "
                        f"--in-place to write a new image alongside.")
    out = Path(getattr(args, "out", None) or
               (disk if in_place else
                disk.with_name(disk.stem + "_lean.qcow2")))

    free = shutil.disk_usage(images).free
    need = 6 * 1024 ** 3
    if free < need:
        return fail("engine_error", _scratch_help(images, free, need))

    import tempfile
    work = Path(tempfile.mkdtemp(prefix="omni-lean-", dir=str(images)))
    raw = work / "base.raw"
    try:
        print(f"[{label}] exporting {disk.name} -> raw")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-f", "qcow2",
                        "-O", "raw", str(disk), str(raw)],
                       check=True, capture_output=True, timeout=1800)
        sup = _gpt_partition(str(raw), "super")
        if not sup:
            return fail("engine_error", "no 'super' partition in the base disk")
        found = _lp_partition(str(raw), sup[0], LEAN_PROP_FS)
        if not found:
            return fail("engine_error",
                        f"could not locate the '{LEAN_PROP_FS}' filesystem in "
                        f"super (single-linear-extent liblp expected)")
        fs_off, fs_size = found
        print(f"[{label}] super at 0x{sup[0]:x}; {LEAN_PROP_FS} fs at "
              f"0x{fs_off:x} ({fs_size / 1048576:.0f} MiB)")

        err, prop_path, n = _bake_lean_props(
            str(raw), fs_off, label,
            props=lean.baked_props(include_unverified=True))
        if err:
            return fail("engine_error", err)
        if not _fsck_ok(str(raw), fs_off, label):
            return fail("engine_error",
                        f"refusing to emit an image whose {LEAN_PROP_FS} "
                        f"filesystem is not clean")

        staged = work / "lean.qcow2"
        print(f"[{label}] importing raw -> qcow2")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-f", "raw",
                        "-O", "qcow2", str(raw), str(staged)],
                       check=True, capture_output=True, timeout=1800)
        if in_place:
            bak = disk.with_suffix(".qcow2.bak")
            if not bak.exists():
                print(f"[{label}] backing up {disk.name} -> {bak.name}")
                shutil.copy2(disk, bak)
        shutil.move(str(staged), str(out))
        size_mb = out.stat().st_size / 1048576
        print(f"[{label}] wrote {out} ({size_mb:.0f} MiB)")
        result = {"ok": True, "base": tag, "image": str(out),
                  "build_prop": prop_path, "properties": n,
                  "in_place": in_place,
                  "note": ("Boot a farming instance on this image and check "
                           "`getprop ro.config.low_ram` is 'true'; the "
                           "properties only count once init has read them.")}
        if getattr(args, "json", False):
            emit_json(result)
        return
    except subprocess.CalledProcessError as e:
        return fail("engine_error",
                    f"qemu-img failed: {(e.stderr or b'')[-400:]}")
    finally:
        shutil.rmtree(work, ignore_errors=True)


def cmd_use_base(args):
    """Set the default base for NEW accounts (e.g. switch between the arm and
    x86 bases). Does not touch existing accounts (use update-all for that)."""
    raw = read_config()
    bases = raw.get("bases") or {}
    if args.tag not in bases:
        sys.exit(f"error: no base '{args.tag}'. Known: {list(bases)}")
    raw["current_base"] = args.tag
    CONFIG_PATH.write_text(json.dumps(raw, indent=2))
    print(f"current base = {args.tag} "
          f"({raw['bases'][args.tag].get('notes','')})")


CONTRACT_VERSION = "1.0"


def registered_commands():
    """Every subcommand this engine actually accepts, sorted.

    Built by asking the parser, so it cannot drift from the code the way the
    hand-written list it replaced did. Kept out of `_HIDDEN_COMMANDS` is
    deliberate: a client is better served by knowing a build-machine command
    exists and choosing not to call it than by a list that quietly lies."""
    parser = build_parser()
    for action in parser._actions:
        if getattr(action, "dest", None) == "cmd" and action.choices:
            return sorted(action.choices)
    return []


def cmd_version(args):
    """Contract handshake (omnidroid-api.md v1 §4). First call every client
    makes so it can refuse/degrade against an engine that predates this
    contract. 'bases' maps arch token -> registered base tag (current_base
    preferred for its arch)."""
    raw = read_config()
    bases = raw.get("bases") or {}
    by_arch = {}
    for tag in [raw.get("current_base")] + list(bases):
        if tag in bases:
            by_arch.setdefault(arch_of_base(bases[tag]), tag)
    # Roblox versions, per base. A client (omni-executor, omni-agent) needs
    # this to offer a version picker and to know whether a bare launch will
    # even resolve — an engine with no default offset refuses `start`.
    offs = {tag: {"default": offsets_mod.default_offset_name(b),
                  "available": list(offsets_mod.offsets_of(b))}
            for tag, b in bases.items()
            if base_type(b) == BASE_TYPE_ARM}
    rep = {"engine": "omnidroid", "contract": CONTRACT_VERSION,
           "arch_aware": True, "host_arch": host_arch_token(),
           "bases": by_arch, "current_base": raw.get("current_base"),
           "offsets": offs,
           # Advertise millisecond-precise VNC capture so a client can prefer
           # it over adb-screencap polling and fall back cleanly on old engines
           # (omni-agent reads capabilities.capture; see _emulator_capture_contract).
           "capabilities": {
               "capture": {
                   "supported": True,
                   "metadata_version": 2,
                   "coverage": "vnc_framebuffer",
                   "auto": True,          # continuous dev-base auto-screenshots
                   "always_on": True,     # auto-starts on every dev boot (engine-owned)
                   "options": ["package", "duration", "sample-scale-w",
                               "change-percent", "black-threshold",
                               "auto", "max-seconds", "max-keyframes"],
               },
               # Many baked Roblox versions on one clean base; the base ships
               # no game. `start --offset <name>` picks one per launch.
               "offsets": {
                   "supported": True,
                   "clean_base": True,
                   "per_account": False,
                   "commands": ["offset list", "offset create",
                                "offset default", "offset remove",
                                "offset show"],
               },
               # Everything an AI/dev needs for high-level debugging without
               # shelling around the engine.
               "debug": {
                   "su": True,            # `omnidroid su <name> -- <cmd>`
                   "frida": True,         # `omnidroid frida <name> --start`
                   "screenshot": True,
                   "logcat": True,
                   "apk_install": True,   # `start --apk` / `install`
                   "devkit_boot": True,   # `start --debug`
                   "info": True,          # `omnidroid debug-info <name>`
               },
           },
           # DERIVED from the parser, never hand-listed. The literal that used
           # to live here had drifted from the engine it describes: it
           # advertised `create` (since removed) and omitted `setup`, `login`
           # and `view` -- the three calls omni-executor actually makes. A
           # client that trusts this field would call a command that does not
           # exist and refuse three that do.
           "commands": registered_commands(),
           "modes": list(MODES),
           "ok": True}
    if getattr(args, "json", False):
        emit_json(rep)
    else:
        print(json.dumps(rep, indent=2))


def cmd_bases(args):
    raw = read_config()
    cur = raw["current_base"]
    # Every registered base is shipped/dual-use now; there is nothing to hide.
    listed = raw.get("bases") or {}
    if getattr(args, "json", False):
        bases = [{"tag": tag, "arch": arch_of_base(b), "type": base_type(b),
                  "game_package": raw.get("base_game", {}).get(tag),
                  "rooted": base_is_rooted(b),
                  # A base ships NO game now — the versions live in offsets.
                  "offsets": list(offsets_mod.offsets_of(b)),
                  "default_offset": offsets_mod.default_offset_name(b),
                  "notes": b.get("notes", "")}
                 for tag, b in listed.items()]
        emit_json({"current_base": cur, "bases": bases, "ok": True})
        return
    for tag, b in listed.items():
        game = raw.get("base_game", {}).get(tag)
        mark = " *" if tag == cur else "  "
        print(f"{mark}{tag}: {b.get('notes','')}  [{arch_of_base(b)}]"
              + (f"  [game: {game}]" if game else ""))
        if base_type(b) == BASE_TYPE_ARM:
            print(f"     roblox: {offsets_mod.offsets_summary(b)}")
    print(f"\ncurrent (default for new accounts): {cur}")


def cmd_qemu_info(args):
    import shutil
    if args.install:
        ensure_qemu()
    resolved = qemu_bin("qemu-system-x86_64")
    where = resolved if Path(resolved).exists() else shutil.which(resolved)
    print(json.dumps({
        "qemu_system": resolved,
        "resolved_to": where,
        "present": _qemu_present(),
        "local_qemu_dir": str(QEMU_DIR),
        "qemu_img": qemu_bin("qemu-img"),
    }, indent=2))


def install_readiness():
    """Everything doctor/setup need to say whether THIS deployment can
    create/boot instances: per-file base-asset presence (after auto-
    registering any new base triples found in images_dir), QEMU and adb
    resolution, and an overall 'ready' verdict with the exact missing
    file paths."""
    import shutil as _sh
    raw, new = autoregister_bases()
    images = Path(images_dir(raw))
    # Base selected by host architecture (arm64 -> arm-uefi; x86 -> current).
    tag = effective_base_tag(raw)
    bases = raw.get("bases") or {}
    missing = []
    base_ready = bool(tag and tag in bases)
    arm = base_ready and base_type(bases[tag]) == BASE_TYPE_ARM
    if base_ready:
        missing += base_missing_files(images, bases[tag])
        base_ready = not missing
    # arm-uefi bakes /data into the base's own data template (no separate
    # ext4 data-template needed); x86 requires the shared data_template.
    if arm:
        template_ready = True
    else:
        template = raw.get("data_template", "data-template-8g.qcow2")
        template_ready = (images / template).exists()
        if not template_ready:
            missing.append(str(images / template))
    qemu_ok = _qemu_present()
    adb_ok = _sh.which("adb") is not None
    rep = {"config": str(CONFIG_PATH),
           "images_dir": str(images),
           "images_dir_exists": images.exists(),
           "auto_registered": new,
           "bases_registered": sorted(bases),
           "host_arch": HOST_ARCH,
           "effective_base": tag,
           "base_type": base_type(bases[tag]) if base_ready else None,
           "current_base": raw.get("current_base"),
           "base_ready": base_ready,
           "data_template_ready": template_ready,
           "missing_files": missing,
           # A base that boots is not the same as a base that can LAUNCH: the
           # base ships no Roblox, so a bare `start` also needs a default
           # offset. Reported separately so doctor names the right fix.
           "offsets": (offsets_mod.offset_rows(bases[tag], images)
                       if arm else []),
           "default_offset": (offsets_mod.default_offset_name(bases[tag])
                              if arm else None),
           "qemu_present": qemu_ok,
           "qemu": qemu_bin(qemu_system_name()) if qemu_ok else None,
           "adb_present": adb_ok,
           "accounts": len(all_accounts()),
           "ready": base_ready and template_ready and qemu_ok and adb_ok}
    if not qemu_ok:
        rep["qemu_hint"] = ("run: omnidroid setup (Windows: portable "
                            "download into ./qemu; Linux: sudo apt "
                            "install qemu-system-x86 qemu-utils)")
    if not adb_ok:
        rep["adb_hint"] = ("adb not on PATH - install Android "
                           "platform-tools (Linux: sudo apt install "
                           "android-tools-adb)")
    # NOT folded into `ready`: a deployment with no offset is correctly
    # installed and can boot, run adb, take screenshots and test an --apk
    # build. It just cannot do a bare `start` yet, so it gets a hint rather
    # than a failed doctor.
    if arm and base_ready and not rep["default_offset"]:
        rep["offset_hint"] = (
            "no default Roblox version baked - a bare `omnidroid start "
            "<username>` will refuse. Bake one: `omnidroid offset create "
            "<name> --apk <roblox.apk>`"
            + ("  (offsets exist but none is default: `omnidroid offset "
               "default <name>`)" if rep["offsets"] else ""))
    return rep


def cmd_doctor(args):
    """Readiness check for this deployment. Exit 0 = ready to create/boot
    instances; exit 1 = something is missing (report says exactly what)."""
    rep = install_readiness()
    if getattr(args, "json", False):
        emit_json({**rep, "ok": rep["ready"]})
    else:
        print(json.dumps(rep, indent=2))
        if not (rep["base_ready"] and rep["data_template_ready"]):
            print(base_setup_help(rep["images_dir"], read_config()))
    if not rep["ready"]:
        sys.exit(1)


def cmd_setup(args):
    """First-run setup. Idempotent; also runs implicitly on first use.

    Windows: fully self-contained/portable — creates the tool's folders
    and downloads a PORTABLE QEMU into ./qemu ONLY. Never installs
    anything to the host system (no global install, no registry, no PATH).
    Linux: creates folders/config; uses SYSTEM QEMU (never a portable
    download) — preflights qemu/adb//dev/kvm/KSM and prints the exact
    install command for anything missing.
    """
    ensure_config()          # blank deployment: bootstrap default config
    cfg = read_config()
    images = Path(images_dir(cfg))
    report = {"platform": "windows" if IS_WINDOWS else "linux",
              "images_dir": str(images), "ok": True}
    for d in (images, ACCOUNTS_DIR):
        d.mkdir(parents=True, exist_ok=True)
    if IS_WINDOWS:
        ensure_qemu()                       # portable download into ./qemu
        report["qemu"] = qemu_bin("qemu-system-x86_64")
        report["qemu_portable_dir"] = str(QEMU_DIR)
    else:
        import shutil as _sh
        missing = [t for t in ("qemu-system-x86_64", "qemu-img", "adb")
                   if not _sh.which(t)]
        if missing:
            report["ok"] = False
            report["missing"] = missing
            report["install"] = ("sudo apt install qemu-system-x86 "
                                 "qemu-utils android-tools-adb")
        else:
            report["qemu"] = _sh.which("qemu-system-x86_64")
        import os
        kvm = Path("/dev/kvm")
        report["kvm"] = (kvm.exists()
                         and os.access(kvm, os.R_OK | os.W_OK))
        if not report["kvm"]:
            report["ok"] = False
            report["kvm_fix"] = ("enable VT-x/AMD-V in BIOS; sudo usermod "
                                 "-aG kvm $USER; re-login; check kvm-ok")
        report["ksm"] = ksm_available()
        if report["ksm"] and ksm_stats().get("run") != 1:
            report["ksm_hint"] = "enable page dedup: omnidroid ksm on"
    # Base assets present? Auto-register anything the user (later: the
    # downloader) dropped into images_dir, then report readiness with the
    # exact missing paths (see HANDOFF 'server base updates').
    ready = install_readiness()
    report["base_assets"] = (ready["base_ready"]
                             and ready["data_template_ready"])
    report["current_base"] = ready["current_base"]
    if ready["auto_registered"]:
        report["auto_registered"] = ready["auto_registered"]
    if not report["base_assets"]:
        report["ok"] = False
        report["missing_files"] = ready["missing_files"]
        report["base_hint"] = "see the file list below (or: omnidroid doctor)"
    print(json.dumps(report, indent=2))
    if not report["base_assets"]:
        print(base_setup_help(images, cfg))
    if not report["ok"]:
        sys.exit(1)


def cmd_ksm(args):
    """Inspect/control Linux KSM. On Windows: documented no-op (KSM is a
    Linux kernel feature; WHPX shares nothing between VMs)."""
    if IS_WINDOWS:
        print("ksm: no-op on Windows. KSM (kernel samepage merging) is a "
              "Linux kernel feature - on the Linux/KVM host it dedups "
              "identical guest pages across instances. Nothing to do here.")
        return
    if not ksm_available():
        sys.exit("error: /sys/kernel/mm/ksm not present - kernel built "
                 "without KSM")
    if args.action == "on":
        if args.aggressive:
            ksm_write("pages_to_scan", 1000)
            ksm_write("sleep_millisecs", 20)
        ksm_write("run", 1)
        print("[ksm] scanning enabled"
              + (" (aggressive)" if args.aggressive else ""))
    elif args.action == "off":
        # run=0 stops scanning but keeps already-merged pages shared;
        # run=2 would force-unmerge everything (not offered: it spikes RAM).
        ksm_write("run", 0)
        print("[ksm] scanning stopped (existing merges kept)")
    st = ksm_stats()
    st["_deduped_mb"] = round(ksm_saved_mb(st), 1)
    print(json.dumps(st, indent=2))


def _wait_game_running(acct, pkg, timeout=180):
    """Poll until the game process exists in the guest (kiosk launches it
    on boot). Process presence, not foreground - same rule as the watchdog."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if adb(acct, "shell", "pidof", pkg, timeout=8).stdout.strip():
                return True
        except Exception:
            pass
        time.sleep(3)
    return False


def cmd_bench_ksm(args):
    """Measure REAL instances-per-GB with KSM on a Linux/KVM host.

    SCAFFOLD - written on Windows 2026-07-06, first executed when the
    Ubuntu laptop exists; treat the first Linux run as its test.

    Method (honest marginal cost, not RSS - RSS double-counts pages KSM
    shares): start identical EPHEMERAL instances one at a time (default
    brutal/headless), each to boot_completed + game process up; after each,
    wait for pages_sharing to plateau, then record the marginal drop in
    host MemAvailable. Stops when MemAvailable < --floor-mb: the RAM floor
    ends the bench, never a count cap. One JSON line per step + summary
    table.

    Diskless model: each step is a fresh build_acct() instance (arm-only;
    shared base templates, snapshot=on). No account.json and no
    accounts/<name>/ folder are ever created -- an ephemeral arm instance is
    already fully provisioned (kiosk + Roblox baked into the base template),
    so unlike the old persistent-account bench there is no first-boot/dexopt
    step here; every step boots at the same ~2-4 min speed. Roblox
    (build_acct's default game_package) is the real workload measured;
    --apk installs an override game fresh into that step's instance instead
    (nothing persists between steps, so a reinstall happens every time it's
    given -- there is no "first run is slow, reruns are fast" anymore)."""
    if IS_WINDOWS:
        sys.exit("bench-ksm needs a Linux host with KVM+KSM (Phase 8 "
                 "measurement tool; Windows/WHPX shares nothing).")
    ensure_qemu()
    check_accel()
    if not ksm_available():
        sys.exit("error: /sys/kernel/mm/ksm not present on this kernel")
    cfg = load_config()
    if ksm_stats().get("run") != 1:
        print("[bench] enabling KSM (aggressive scan for the bench)")
        ksm_write("pages_to_scan", 1000)
        ksm_write("sleep_millisecs", 20)
        ksm_write("run", 1)

    base_avail = host_mem_available_mb()
    print(f"[bench] baseline: MemAvailable {base_avail:.0f} MB, "
          f"pages_sharing {ksm_stats().get('pages_sharing', 0)}, "
          f"mode {args.mode}, floor {args.floor_mb} MB")
    rows = []
    prev_avail = base_avail
    for i in range(1, args.max + 1):
        name = f"{args.prefix}{i}"
        # Never re-spawn over a still-live instance of this name (e.g. a prior
        # `bench-ksm --keep` run): that would orphan the old QEMU process and
        # clobber its run.json/efivars. Skip to the next free name instead.
        if running_pid(name):
            print(f"[bench] {name} already running (kept from a prior run) "
                  f"- skipping this slot")
            continue
        # `--apk` supplies the build, so a clean base is fine there; otherwise
        # the bench measures whatever the default offset has baked.
        acct = build_acct(name, cfg, debug=False,
                          offset=getattr(args, "offset", None),
                          allow_no_offset=bool(getattr(args, "apk", None)))
        pkg = acct.get("game_package")
        mode = resolve_mode(cfg, args.mode)
        spawn_qemu(acct, cfg, interactive=False, mode=mode)
        if not wait_for_boot(acct, NORMAL_BOOT_TIMEOUT, f"bench {name}"):
            print(f"[bench] {name} boot timeout - stopping bench")
            _shutdown(acct, f"bench {name}")
            _wipe_runtime(name)
            break
        post_boot(acct, f"bench {name}")
        if args.apk:
            r = adb(acct, "install", "-r", "-g", "--no-incremental",
                    args.apk, timeout=600)
            if "Success" in (r.stdout + r.stderr):
                pkg = apk_package_name(args.apk) or pkg
                if pkg:
                    acct["game_package"] = pkg
                    adb(acct, "shell", "settings", "put", "global",
                        "omni_game_package", pkg, timeout=10)
        if pkg and not _wait_game_running(acct, pkg):
            print(f"[bench] WARNING: {pkg} never came up in {name}; "
                  f"numbers for this step measure an idle instance")
        sharing = _ksm_wait_settle(args.settle_secs)
        avail = host_mem_available_mb()
        st = ksm_stats()
        pids = [running_pid(f"{args.prefix}{k}") for k in range(1, i + 1)]
        rss = sum(r for r in (host_rss_mb(p) for p in pids if p) if r)
        row = {"n": i, "name": name,
               "mem_available_mb": round(avail),
               "marginal_mb": round(prev_avail - avail),
               "sum_qemu_rss_mb": round(rss),
               "ksm_pages_sharing": sharing,
               "ksm_deduped_mb": round(ksm_saved_mb(st), 1)}
        if isinstance(st.get("general_profit"), int):
            row["ksm_general_profit_mb"] = round(
                st["general_profit"] / (1024 * 1024), 1)
        rows.append(row)
        print(json.dumps(row))
        prev_avail = avail
        if avail < args.floor_mb:
            print(f"[bench] MemAvailable {avail:.0f} < floor "
                  f"{args.floor_mb} MB - stopping (RAM floor, not a cap)")
            break
    print("\n[bench]  n  marginal_MB  avail_MB  ksm_deduped_MB")
    for r in rows:
        print(f"[bench] {r['n']:>2}  {r['marginal_mb']:>11}  "
              f"{r['mem_available_mb']:>8}  {r['ksm_deduped_mb']:>14}")
    if rows and not args.keep:
        print("[bench] stopping bench instances (--keep leaves them up)")
        for r in rows:
            _shutdown(load_account(r["name"]), f"bench {r['name']}")
            _wipe_runtime(r["name"])


def account_status(a, stats=False):
    """One account's live state as a plain dict — shared by the human
    list output and --json (the GUI relies on these exact keys)."""
    pid = running_pid(a["name"])
    adb_port = a.get("adb_port")
    rec = {"name": a["name"], "base": a["base"], "arch": acct_arch(a),
           "running": bool(pid),
           "pid": pid, "mode": None,
           "adb_port": adb_port, "qmp_port": a.get("qmp_port"),
           "vnc_port": a.get("vnc_port"), "vnc_host": "127.0.0.1",
           "adb_serial": f"127.0.0.1:{adb_port}" if adb_port else None,
           "offset": a.get("offset"), "debug": bool(a.get("debug")),
           "game_package": a.get("game_package")}
    if pid:
        try:
            run = json.loads((runtime_dir(a["name"]) /
                              "run.json").read_text())
            rec["mode"] = run.get("mode")
            rec["started"] = run.get("started")
            rec["offset"] = run.get("offset")
        except Exception:
            pass
    if pid and stats:
        rss = host_rss_mb(pid)
        rec["host_rss_mb"] = round(rss) if rss else None
        rec["guest_used_mb"] = None
        try:
            mem = adb(a, "shell", "head", "-3", "/proc/meminfo",
                      timeout=8).stdout
            tot = int(re.search(r"MemTotal:\s+(\d+)", mem).group(1))
            avail = int(re.search(r"MemAvailable:\s+(\d+)", mem).group(1))
            rec["guest_used_mb"] = round((tot - avail) / 1024)
        except Exception:
            pass
        if IS_LINUX:
            merged = pid_ksm_merged_mb(pid)
            if merged is not None:
                rec["ksm_merged_mb"] = round(merged)
    return rec


def cmd_list(args):
    from omnidroid.runtime import reconcile_runtime
    reconcile_runtime()
    accts = all_accounts()
    if getattr(args, "json", False):
        emit_json([account_status(a, stats=args.stats) for a in accts])
        return
    if not accts:
        print("no accounts. log one in: omnidroid login <username>")
        return
    for a in accts:
        rec = account_status(a, stats=args.stats)
        state = f"RUNNING pid {rec['pid']}" if rec["running"] else "stopped"
        line = (f"{rec['name']:<16} base {rec['base']}  "
                f"adb {rec['adb_port'] or '?'}  qmp {rec['qmp_port'] or '?'}  "
                f"vnc {rec['vnc_port'] or '?'}  {state}")
        if rec["running"]:
            line += f"  roblox {rec.get('offset') or 'none'}"
            if rec.get("mode"):
                line += f"  mode {rec['mode']}"
            if rec.get("debug"):
                line += "  [debug]"
        if rec["running"] and args.stats:
            if rec.get("host_rss_mb"):
                line += f"  host-rss {rec['host_rss_mb']} MB"
            if rec.get("guest_used_mb"):
                line += f"  guest-used {rec['guest_used_mb']} MB"
            if rec.get("ksm_merged_mb") is not None:
                line += f"  ksm-merged {rec['ksm_merged_mb']} MB"
        print(line)


# ---------- ABI-safe install (contract omnidroid-api.md v1 §5) ----------
# A fat APK on the x86 base would let Android pick the x86_64 lib and run
# NATIVE, bypassing libndk translation — the wrong path. So x86 accounts
# default to pinning arm64-v8a; the arm base runs arm64 native (no pin).

def _resolve_install_abi(acct, abi, no_abi_pin):
    """ABI to pin on install: explicit --abi wins; else default arm64-v8a on
    x86 accounts (exercise libndk translation); arm accounts and --no-abi-pin
    get no pin (native selection)."""
    if no_abi_pin:
        return None
    if abi:
        return abi
    return "arm64-v8a" if acct_arch(acct) == "x86" else None


def _abi_install(acct, apk, abi, timeout=600):
    """adb install with an optional forced --abi (pins native-lib extraction)."""
    argv = ["install", "-r", "-g", "--no-incremental"]
    if abi:
        argv += ["--abi", abi]
    argv.append(apk)
    return adb(acct, *argv, timeout=timeout)


def installed_primary_abi(acct, pkg):
    """The ABI Android actually bound for <pkg> (primaryCpuAbi) — i.e. which
    native libs the app will load. None if unknown / no native libs."""
    try:
        out = adb(acct, "shell", "dumpsys", "package", pkg, timeout=20).stdout
    except Exception:
        return None
    m = re.search(r"primaryCpuAbi=(\S+)", out or "")
    if m and m.group(1) not in ("null", "none", ""):
        return m.group(1)
    return None


def _is_arm_abi(abi):
    return bool(abi and abi.startswith(("arm", "armeabi")))


def _install_needs_clean_replace(out):
    """adb-install failures that a force-stop + uninstall + reinstall recovers:
    a signature change or a version downgrade over an ALREADY-installed build.
    Both are the normal case on a reused instance (stock Roblox vs a re-signed
    test build, or two successive agent builds) — not a bad APK."""
    o = out or ""
    return ("INSTALL_FAILED_UPDATE_INCOMPATIBLE" in o
            or "signatures do not match" in o
            or "INSTALL_FAILED_VERSION_DOWNGRADE" in o)


def _install_block_reason(out):
    return "version downgrade" if "VERSION_DOWNGRADE" in (out or "") else "signature mismatch"


def _install_apk(acct, apk_path, label, abi=None, no_abi_pin=False):
    """Shared install core: ABI-safe `adb install` + the reused-instance
    pin/sig auto-recovery. Used by both `omnidroid install` (cmd_install) and
    `omnidroid start --apk` (cmd_start's dev-apk path). Returns a result dict; it
    never calls fail() / sys.exit -- callers decide how to surface an error
    (cmd_install turns a failure into fail("install_failed", ...); cmd_start
    turns it into an `apk_install_failed` JSON result + sys.exit(1))."""
    # adb is a per-invocation client: nothing has necessarily `adb connect`ed to
    # this instance yet, and every `adb -s <serial> …` then fails with "device
    # not found". That is exactly the dev loop (`start` -> `install` -> `play`
    # are three separate processes), so connect first.
    adb_connect(acct)
    abi = _resolve_install_abi(acct, abi, no_abi_pin)
    print(f"[{label}] installing {apk_path}"
          + (f" (--abi {abi})" if abi else " (no ABI pin)") + " ...")
    pkg = apk_package_name(apk_path)
    r = _abi_install(acct, apk_path, abi)
    out = (r.stdout + r.stderr).strip()
    print(f"[{label}] {out}")
    # A REUSED instance usually already carries a differently-signed / version-
    # incompatible build of the same package (stock Roblox vs a re-signed test
    # build, or two successive agent builds). adb refuses to replace across a
    # signature change (INSTALL_FAILED_UPDATE_INCOMPATIBLE) or a downgrade, and
    # the stale build can't simply be uninstalled while it is the kiosk's PINNED
    # Lock-Task app (DELETE_FAILED_APP_PINNED). Recover automatically instead of
    # dead-ending the caller: force-stop the app (this releases the pin —
    # verified: uninstall while pinned => DELETE_FAILED_APP_PINNED, uninstall
    # after force-stop => Success), uninstall it, then reinstall fresh. The kiosk
    # relaunches the new build on its own via its ACTION_PACKAGE_ADDED receiver.
    if "Success" not in out and _install_needs_clean_replace(out) and pkg:
        print(f"[{label}] existing build of {pkg} blocks the update "
              f"({_install_block_reason(out)}); clearing the kiosk pin + reinstalling ...")
        # The kiosk RE-PINS the configured game the instant it is force-stopped
        # (Lock Task + auto-relaunch), so a bare force-stop -> uninstall races the
        # kiosk and loses with DELETE_FAILED_APP_PINNED. Do it deterministically:
        # point the kiosk AWAY from the game first (it then stops relaunching /
        # re-pinning it), force-stop the stale build, bring the kiosk to the
        # foreground so that build is no longer the pinned task, THEN uninstall.
        adb(acct, "shell", "settings", "put", "global", "omni_game_package", "none", timeout=15)
        adb(acct, "shell", "am", "force-stop", pkg, timeout=25)
        adb(acct, "shell", "am", "start", "-n", "com.omni.kiosk/.MainActivity", timeout=15)
        time.sleep(3)   # let the kiosk take the foreground before we uninstall
        u = adb(acct, "uninstall", pkg, timeout=120)
        print(f"[{label}] uninstall {pkg}: {(u.stdout + u.stderr).strip()[:200]}")
        # Point the kiosk back at the game so it relaunches the fresh build (the
        # success path below re-asserts this; setting it here also leaves the
        # kiosk correctly targeted if the reinstall itself then fails).
        adb(acct, "shell", "settings", "put", "global", "omni_game_package", pkg, timeout=15)
        r = _abi_install(acct, apk_path, abi)
        out = (r.stdout + r.stderr).strip()
        print(f"[{label}] reinstall: {out}")
    if "Success" not in out:
        return {"ok": False, "error": "apk_install_failed",
                "detail": out[:400], "package": pkg}
    abi_installed = installed_primary_abi(acct, pkg) if pkg else None
    native_bridge_used = _is_arm_abi(abi_installed) and acct_arch(acct) == "x86"
    return {"ok": True, "package": pkg, "abi_installed": abi_installed,
            "native_bridge_used": native_bridge_used, "out": out}


def cmd_install(args):
    acct = load_account(args.name)
    json_mode = getattr(args, "json", False)
    arch = acct_arch(acct)
    label = f"install {args.name}"
    ir = _install_apk(acct, args.apk, label,
                      abi=getattr(args, "abi", None),
                      no_abi_pin=getattr(args, "no_abi_pin", False))
    if ir["ok"] is False:
        fail("install_failed", f"adb install failed: {ir['detail']}")
    pkg = ir["package"]
    abi_installed = ir["abi_installed"]
    native_bridge_used = ir["native_bridge_used"]
    if pkg:
        acct["game_package"] = pkg
        save_account(acct)
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", pkg, timeout=10)
        print(f"[{label}] game package = {pkg} "
              f"(saved + pushed to guest); primaryCpuAbi={abi_installed} "
              f"native_bridge_used={native_bridge_used}")
    if getattr(args, "require_translation", False) and not native_bridge_used:
        fail("abi_not_translated",
             f"required ARM translation not exercised: primaryCpuAbi="
             f"{abi_installed} on {arch} account (native_bridge_used=false)")
    if json_mode:
        emit_json({"name": args.name, "package": pkg, "installed": True,
                   "abi_installed": abi_installed,
                   "native_bridge_used": native_bridge_used,
                   "arch": arch, "ok": True})


def apk_package_name(apk):
    """Read the package name from an APK via build-tools aapt2/aapt.

    Cross-platform SDK resolution (same order as omni-agent's
    find_android_sdk_tools): $ANDROID_SDK_ROOT / $ANDROID_HOME, then the OS
    default Android Studio location. This used to hardcode the Windows
    AppData path, so `pkg` came back None on every non-Windows host — the
    caller's `if pkg:` guard then silently skipped saving game_package and
    the --json output reported `"package": null` even on a successful
    install."""
    import glob
    is_nt = os.name == "nt"
    aapt2_name = "aapt2.exe" if is_nt else "aapt2"
    aapt_name = "aapt.exe" if is_nt else "aapt"

    sdk_roots = []
    for var in ("ANDROID_SDK_ROOT", "ANDROID_HOME"):
        v = os.environ.get(var)
        if v:
            sdk_roots.append(Path(v))
    home = Path.home()
    if is_nt:
        localappdata = os.environ.get("LOCALAPPDATA")
        sdk_roots.append(Path(localappdata) / "Android/Sdk" if localappdata
                          else home / "AppData/Local/Android/Sdk")
    elif platform.system() == "Darwin":
        sdk_roots.append(home / "Library/Android/sdk")
    else:
        sdk_roots.append(home / "Android/Sdk")

    for sdk_root in sdk_roots:
        for bt in sorted(glob.glob(str(sdk_root / "build-tools" / "*")), reverse=True):
            for tool, argv in ((aapt2_name, ["dump", "packagename", apk]),
                               (aapt_name, ["dump", "badging", apk])):
                exe = Path(bt) / tool
                if not exe.exists():
                    continue
                try:
                    r = subprocess.run([str(exe)] + argv, capture_output=True,
                                       text=True, timeout=30)
                    if tool == aapt2_name and r.returncode == 0:
                        return r.stdout.strip().splitlines()[0]
                    m = re.search(r"package: name='([^']+)'", r.stdout)
                    if m:
                        return m.group(1)
                except Exception:
                    pass
    return None


# Bliss packages that claim HOME; disabled per-account so the kiosk is
# the only launcher. Settings' FallbackHome must NOT be disabled.
BLISS_HOME_PACKAGES = ("com.android.launcher3",
                       "com.farmerbb.taskbar.support",
                       "cu.axel.smartdock")


def cmd_kioskify(args):
    """Install the kiosk APK on a running instance, make it the HOME app,
    and disable Bliss launchers/taskbar (all per-/data, reversible)."""
    acct = load_account(args.name)
    label = f"kioskify {args.name}"
    adb(acct, "root")
    time.sleep(2)
    adb_connect(acct)
    r = adb(acct, "install", "-r", "-g", "--no-incremental", args.apk,
            timeout=120)
    out = (r.stdout + r.stderr).strip()
    print(f"[{label}] install kiosk: {out}")
    if "Success" not in out:
        sys.exit(1)
    adb(acct, "shell", "cmd", "package", "set-home-activity",
        "--user", "0", "com.omni.kiosk/.MainActivity", timeout=15)
    for pkg in BLISS_HOME_PACKAGES:
        adb(acct, "shell", "pm", "disable-user", "--user", "0", pkg,
            timeout=15)
    if acct.get("game_package"):
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", acct["game_package"], timeout=10)
    r = adb(acct, "shell", "cmd", "shortcut", "get-default-launcher",
            timeout=10)
    print(f"[{label}] default launcher now: {r.stdout.strip()}")


# ---------- dev / testing harness (scriptable, JSON output) ----------

# ---------- live VNC viewer (real-time screen + mouse/keyboard control) ----------

def _port_open(host, port, timeout=0.5):
    """True if a TCP connection to host:port succeeds right now."""
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def _macos_screen_sharing_app():
    """Absolute path to the built-in Screen Sharing.app, or None. Its
    location moved across macOS versions (Utilities on 13+, CoreServices on
    older); mdfind is the last resort."""
    for p in ("/System/Applications/Utilities/Screen Sharing.app",
              "/System/Library/CoreServices/Applications/Screen Sharing.app",
              "/Applications/Screen Sharing.app"):
        if Path(p).exists():
            return p
    try:
        r = subprocess.run(["mdfind", "-name", "Screen Sharing.app"],
                           capture_output=True, text=True, timeout=5)
        for line in r.stdout.splitlines():
            if line.strip().endswith("Screen Sharing.app") \
                    and Path(line.strip()).exists():
                return line.strip()
    except Exception:
        pass
    return None


def _vnc_viewer_command(host, port, viewer=None):
    """The command that opens a real VNC client window at host:port, with
    live screen + mouse/keyboard. Resolution order:
      1. explicit template (--viewer or config qemu.vnc_viewer): a shell
         string with {host}/{port}/{url}/{display} placeholders, e.g.
         "vncviewer {host}::{port}" or "/Applications/…/VNC Viewer {url}".
      2. OS-native client (no install): macOS Screen Sharing via `open
         vnc://`; Windows hands the vnc:// URL to the shell.
      3. A known VNC client found on PATH (TigerVNC/remmina/gvncviewer).
    Returns (argv_list, shell_bool) or None if nothing suitable was found.
    QEMU maps -vnc display D to TCP port 5900+D, so port = 5900 + display."""
    url = f"vnc://{host}:{port}"
    display = port - 5900
    tmpl = viewer or (read_config().get("qemu", {}).get("vnc_viewer"))
    if tmpl:
        argv = [a.format(host=host, port=port, url=url, display=display)
                for a in shlex.split(tmpl)]
        return argv, False
    if IS_MACOS:
        # Built-in Screen Sharing.app — real-time, full mouse/keyboard.
        # Launch it BY PATH, not via `open vnc://`: the vnc:// URL scheme is
        # often hijacked by a third-party handler (e.g. RealVNC), so the
        # scheme route can silently open the wrong app or nothing. `open -a
        # <app> vnc://…` forces the built-in client regardless.
        app = _macos_screen_sharing_app()
        if app:
            return ["open", "-a", app, url], False
        return ["open", url], False   # fall back to the scheme handler
    if IS_WINDOWS:
        for exe in ("vncviewer.exe", "tvnviewer.exe"):
            p = shutil.which(exe)
            if p:
                return [p, f"{host}::{port}"], False
        # Fall back to whatever is registered for the vnc:// scheme.
        return ["cmd", "/c", "start", "", url], False
    # Linux / other: try common clients on PATH.
    if shutil.which("vncviewer"):                       # TigerVNC / TightVNC
        return ["vncviewer", f"{host}::{port}"], False   # :: = raw TCP port
    if shutil.which("remmina"):
        return ["remmina", "-c", url], False
    if shutil.which("gvncviewer"):
        return ["gvncviewer", f"{host}:{display}"], False
    if shutil.which("xdg-open"):
        return ["xdg-open", url], False
    return None


def _spawn_builtin_viewer(name, host, port, title):
    """Launch the self-contained Tk+RFB viewer (manager/vncview.py) as a
    detached process so the terminal returns and multiple viewers can run.
    Works frozen (exe supports the hidden `_vncview` subcommand) and as a
    plain script (re-invoke this file with `_vncview`)."""
    a = ["_vncview", "--host", host, "--port", str(port)]
    if title:
        a += ["--title", title]
    if getattr(sys, "frozen", False):
        cmd = [sys.executable] + a
    else:
        cmd = [sys.executable, str(Path(__file__).resolve())] + a
    # Detach stdio too: if the child inherited the terminal's stdout/stderr,
    # the shell would block waiting for EOF while the (long-lived) viewer
    # holds the pipe open. Send viewer output to the per-instance runtime dir
    # (diskless model: product-path per-instance state lives under
    # runtime/<name>/, not accounts/<name>/ -- wiped on stop like the rest).
    d = runtime_dir(name)
    d.mkdir(parents=True, exist_ok=True)
    log = open(d / "viewer.log", "a")
    kwargs = {"stdin": subprocess.DEVNULL, "stdout": log, "stderr": log}
    if IS_WINDOWS:
        kwargs["creationflags"] = 0x00000008 | 0x00000200   # DETACHED|NEW_GRP
    else:
        kwargs["start_new_session"] = True
    return subprocess.Popen(cmd, **kwargs)


def cmd_view(args):
    """Open a LIVE window onto an instance — real-time screen with mouse and
    keyboard control — launched straight from the terminal.

    Default: the SELF-CONTAINED Python viewer (manager/vncview.py: Tk + a
    minimal RFB client) — identical on Windows/macOS/Linux, no OS
    screen-sharing app. `--native` instead launches the OS/native VNC client
    (macOS Screen Sharing, or --viewer/config qemu.vnc_viewer, or a client on
    PATH). Instance must be running; --start boots it first and waits for the
    VNC port. Localhost-only: the viewer connects to 127.0.0.1 — the server
    has no auth, safe ONLY on the loopback bind (port-scheme HARD RULE)."""
    cfg = load_config()
    host = "127.0.0.1"
    started = False
    if not running_pid(args.name):
        if not args.start:
            sys.exit(f"error: '{args.name}' is not running. Start it first "
                     f"(omnidroid start {args.name}) or: omnidroid view {args.name} "
                     f"--start")
        debug = bool(getattr(args, "debug", False))
        acct = build_acct(args.name, cfg, debug=debug,
                          offset=getattr(args, "offset", None),
                          label=f"view {args.name}")
        spawn_qemu(acct, cfg, interactive=False, debug=debug,
                   mode=resolve_mode(cfg, args.mode))
        started = True
        port = acct["vnc_port"]
        print(f"[view {args.name}] started instance (detached); waiting for "
              f"VNC on {host}:{port} ...")
    else:
        acct = load_account(args.name)
        port = acct["vnc_port"]

    # Wait for the QEMU VNC server to accept connections (it binds at process
    # start, so this is quick; generous bound covers a cold spawn).
    deadline = time.time() + (args.timeout if started else 5)
    while not _port_open(host, port):
        if not running_pid(args.name):
            sys.exit(f"error: '{args.name}' is not running (QEMU exited "
                     f"before its VNC port opened)")
        if time.time() > deadline:
            sys.exit(f"error: VNC port {host}:{port} did not open in time")
        time.sleep(0.5)

    title = f"omni: {args.name}  ({host}:{port})"
    use_native = getattr(args, "native", False) or args.viewer \
        or cfg.get("qemu", {}).get("vnc_viewer")
    if use_native:
        resolved = _vnc_viewer_command(host, port, viewer=args.viewer)
        if not resolved:
            sys.exit("error: no native VNC client found. Drop --native to use "
                     "the built-in viewer, or set --viewer 'client "
                     "{host}::{port}'. Screen is at " f"{host}:{port}.")
        argv, use_shell = resolved
        try:
            subprocess.Popen(argv, shell=use_shell)
        except Exception as e:
            sys.exit(f"error: failed to launch native viewer {argv}: {e}")
        viewer_desc, vpid = argv[0], None
    else:
        try:
            proc = _spawn_builtin_viewer(args.name, host, port, title)
        except Exception as e:
            sys.exit(f"error: failed to launch built-in viewer: {e}")
        viewer_desc, vpid = "built-in (Tk+RFB)", proc.pid

    print(f"[view {args.name}] live viewer opened [{viewer_desc}] on "
          f"{host}:{port} - real-time screen, mouse + keyboard control"
          + (f"; viewer pid {vpid}" if vpid else ""))
    if getattr(args, "json", False):
        emit_json({"name": args.name, "vnc_host": host, "vnc_port": port,
                   "viewer": viewer_desc, "viewer_pid": vpid,
                   "started": started, "ok": True})


def _run_vncview(a):
    """Internal: run the built-in viewer in THIS process (invoked as the
    hidden `_vncview` subcommand by _spawn_builtin_viewer)."""
    from omnidroid import vncview
    return vncview.run_viewer(a.host, a.port, a.title)


def cmd_screenshot(args):
    """Pull a screenshot from the guest framebuffer (true colors, works
    headless). Prints JSON: {ok, path}."""
    acct = load_account(args.name)
    out = args.out or str(runtime_dir(args.name)
                          / f"shot-{int(time.time())}.png")
    try:
        adb(acct, "shell", "screencap", "-p", "/data/local/tmp/_s.png",
            timeout=30)
        adb(acct, "pull", "/data/local/tmp/_s.png", out, timeout=60)
    except Exception as e:
        print(json.dumps({"ok": False, "error": str(e)}))
        sys.exit(1)
    ok = Path(out).exists() and Path(out).stat().st_size > 0
    print(json.dumps({"ok": ok, "path": out if ok else None}))
    if not ok:
        sys.exit(1)


def cmd_logcat(args):
    """Read guest logcat (raw, machine-parseable). --clear wipes it;
    --tag filters to a tag; otherwise dumps and returns."""
    acct = load_account(args.name)
    if args.clear:
        adb(acct, "logcat", "-c", timeout=15)
        print(json.dumps({"ok": True, "cleared": True}))
        return
    argv = ["logcat", "-d"]
    if args.tag:
        argv += ["-s", args.tag]
    r = adb(acct, *argv, timeout=args.timeout)
    sys.stdout.write(r.stdout)


# ---------- millisecond-precise capture (contract omnidroid-api.md v1 §6.8) ----
# Quick logcat crash markers for the engine's own summary. The AGENT does the
# rich analysis (omni-agent/tools/_emulator_diagnostics.py) on the logcat.txt we
# write; this is only enough to set crash_detected in the one-line JSON so a
# standalone client (omni-executor) still learns "it crashed".
_CAP_CRASH_RE = re.compile(
    r"FATAL EXCEPTION|Fatal signal|signal\s+\d+\s+\(SIG|beginning of crash|"
    r"ANR in |Abort message:|FORTIFY|CheckJNI",
    re.IGNORECASE)


class _PidPoller(threading.Thread):
    """Polls `pidof <pkg>` on a fixed cadence to time when the app process
    starts and (crucially) when it DISAPPEARS — the signal that distinguishes a
    real crash/close from a merely black screen. Timestamps are relative to the
    same monotonic origin the frame capture uses so events line up with frames.
    """

    def __init__(self, acct, pkg, start_ns, interval=0.5):
        super().__init__(name="pid-poller", daemon=True)
        self.acct, self.pkg, self.start_ns = acct, pkg, start_ns
        self.interval = interval
        self._stop = threading.Event()
        self.events = []          # merge_diagnostics-shaped: type/t_ms/pid
        self.last_pid = None
        self.ever_started = False

    def _t_ms(self):
        return max(0, int(round((time.perf_counter_ns() - self.start_ns) / 1e6)))

    def run(self):
        while not self._stop.is_set():
            pid = _pidof(self.acct, self.pkg)
            t = self._t_ms()
            if pid and self.last_pid is None:
                self.events.append({"type": ("app_restarted" if self.ever_started
                                             else "app_started"),
                                    "t_ms": t, "pid": pid})
                self.ever_started = True
            elif pid and self.last_pid and pid != self.last_pid:
                self.events.append({"type": "app_restarted", "t_ms": t, "pid": pid})
            elif not pid and self.last_pid is not None:
                self.events.append({"type": "app_exited", "t_ms": t,
                                    "pid": self.last_pid})
            self.last_pid = pid
            self._stop.wait(self.interval)

    def stop(self):
        self._stop.set()


def _annotate_frames(frames, events):
    """Light per-frame app_state/pid/crash from the pid timeline, so the engine's
    metadata is useful to a client that does NO post-processing. The agent
    re-derives this (also folding logcat) via merge_diagnostics."""
    timeline = sorted(events, key=lambda e: e.get("t_ms", 0))
    state, pid, started = "not_started", None, False
    cur = 0
    for fr in sorted(frames, key=lambda f: f.get("t_ms", 0)):
        while cur < len(timeline) and (timeline[cur].get("t_ms") or 0) <= fr.get("t_ms", 0):
            ev = timeline[cur]
            if ev["type"] in ("app_started", "app_restarted"):
                state, pid, started = "running", ev.get("pid"), True
            elif ev["type"] == "app_crashed":
                state, pid = "crashed", None
            elif ev["type"] in ("app_exited", "app_killed"):
                state, pid = ("exited" if started else state), None
            cur += 1
        fr["app_state"], fr["pid"] = state, pid
        fr["crash"] = bool(fr.get("crash") or state == "crashed")
    return state


def _capture_finalize(acct, out_dir, meta, events, pkg, running=False):
    """Shared tail for both bounded and auto capture: dump the epoch logcat,
    decide crash/exit, annotate frames, and write the FINAL metadata.json. In
    auto mode capture.py has been flushing metadata live throughout; this is the
    authoritative last write (running=False) with logcat folded in."""
    logcat_text = ""
    try:
        logcat_text = adb(acct, "logcat", "-b", "all", "-v", "epoch", "-d",
                          timeout=60).stdout or ""
    except Exception as e:  # noqa: BLE001
        meta.setdefault("warnings", []).append(f"logcat dump failed: {e}")
    try:
        (out_dir / "logcat.txt").write_text(logcat_text, encoding="utf-8",
                                            errors="replace")
    except Exception:
        pass

    crash_detected = bool(_CAP_CRASH_RE.search(logcat_text))
    exit_detected = any(e["type"] in ("app_exited", "app_killed") for e in events)
    if crash_detected:
        events.append({"type": "app_crashed",
                       "t_ms": events[-1]["t_ms"] if events else None,
                       "pid": None})
    app_state = _annotate_frames(meta.get("keyframes", []), events)

    meta.update({
        "running": running,
        "package": pkg,
        "process_events": events,
        "logcat_file": "logcat.txt",
        "crash_detected": crash_detected,
        "exit_detected": exit_detected,
        "app_state": app_state,
    })
    (out_dir / "metadata.json").write_text(
        json.dumps(meta, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    return crash_detected, exit_detected, app_state


def _wait_for_vnc(host, port, timeout=30.0):
    """Block until QEMU's VNC server accepts a connection, or timeout.

    The auto recorder is started the instant QEMU is spawned (so it catches the
    boot screen), which races QEMU binding its VNC socket by a few hundred ms.
    Without this wait the recorder would die on connection-refused and the whole
    session would silently have no screenshots."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            socket.create_connection((host, port), timeout=1.0).close()
            return True
        except OSError:
            time.sleep(0.2)
    return False


def cmd_capture(args):
    """Record millisecond-precise keyframes from the instance's VNC framebuffer
    while tracking the app process, then emit one JSON line describing where the
    keyframes + metadata.json + logcat.txt were written.

    Unlike `screenshot` (one adb round-trip) this observes EVERY display update
    (see manager/capture.py), so a loading screen shown for a few ms before a
    black screen is captured as two frames with the true gap between them. A
    black frame is only a VISUAL fact; crash/exit is decided from the process
    timeline + logcat, so the caller can tell "app crashed/closed" from "screen
    is black but the app is alive".

    With ``--auto`` this becomes the always-on auto-screenshot feature: there is
    no fixed window — it observes continuously and drops a keyframe on EVERY big
    change until stopped (a STOP sentinel file in the output dir, an optional
    --max-seconds cap, SIGINT/SIGTERM, or the instance powering off), flushing
    metadata.json live so a reader sees frames as they land. Auto mode is a
    DEV-BASE-ONLY feature (the arm devkit disk); it refuses to run on the
    production bases."""
    import os
    from omnidroid import capture as _capture
    acct = load_account(args.name)
    json_mode = getattr(args, "json", False)
    auto = bool(getattr(args, "auto", False))
    if not running_pid(args.name):
        return fail("not_running",
                    f"account '{args.name}' is not running; start it first",
                    exit_code=1)
    if auto and not acct.get("debug"):
        return fail("debug_boot_required",
                    f"auto screenshots are a debug feature; instance "
                    f"'{args.name}' was not booted with --debug. Restart it "
                    f"with `omnidroid start {args.name} --debug` to use --auto.")
    vnc_port = acct.get("vnc_port")
    if not vnc_port:
        return fail("engine_error", f"account '{args.name}' has no vnc_port")

    pkg = getattr(args, "package", None) or acct.get("game_package")
    prefix = "autocap" if auto else "capture"
    out_dir = Path(args.out) if getattr(args, "out", None) else \
        (runtime_dir(args.name) / f"{prefix}-{int(time.time())}")
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    # Clear the full log buffer so the dump at the end brackets THIS window only.
    # NOT in auto mode: that recorder starts while the guest is still booting, so
    # (a) adbd isn't up yet and the round-trip would only delay the VNC attach
    # past the first frames, and (b) the boot log is exactly what we want kept.
    if not auto:
        try:
            adb(acct, "logcat", "-b", "all", "-c", timeout=15)
        except Exception:
            pass

    start_ns = time.perf_counter_ns()
    poller = _PidPoller(acct, pkg, start_ns) if pkg else None
    if poller:
        poller.start()

    stop_event = threading.Event()
    watcher = None
    if auto:
        # Record our pid so a supervisor (omni-agent) can hard-stop if needed,
        # and watch for the STOP sentinel + termination signals for a clean stop.
        try:
            (out_dir / "capture.pid").write_text(str(os.getpid()), encoding="utf-8")
        except Exception:
            pass
        stop_file = out_dir / "STOP"
        max_seconds = float(getattr(args, "max_seconds", 0) or 0)

        def _watch_stop():
            deadline = start_ns + int(max_seconds * 1e9) if max_seconds > 0 else None
            while not stop_event.is_set():
                if stop_file.exists():
                    stop_event.set(); return
                if deadline and time.perf_counter_ns() >= deadline:
                    stop_event.set(); return
                stop_event.wait(0.25)

        watcher = threading.Thread(target=_watch_stop, name="cap-stop", daemon=True)
        watcher.start()
        try:
            import signal
            for _sig in (getattr(signal, "SIGINT", None),
                         getattr(signal, "SIGTERM", None)):
                if _sig is not None:
                    signal.signal(_sig, lambda *_a: stop_event.set())
        except Exception:
            pass

    def _enrich(meta):
        # Live process annotation during auto mode: fold the pid timeline into
        # each flushed metadata so a reader sees app_state/pid without waiting.
        evs = list(poller.events) if poller else []
        meta["app_state"] = _annotate_frames(meta.get("keyframes", []), evs)
        meta["package"] = pkg
        meta["process_events"] = evs

    # Saved-keyframe cap: an always-on recorder must not stop saving after the
    # bounded default (240) on a long session, so --auto gets a much higher cap.
    max_kf = getattr(args, "max_keyframes", 0) or 0
    if max_kf <= 0:
        max_kf = AUTOCAP_MAX_KEYFRAMES if auto else _capture.DEFAULT_MAX_KEYFRAMES

    if auto:
        cap_kwargs = dict(stop_event=stop_event,
                          metadata_path=str(out_dir / "metadata.json"),
                          enrich=_enrich, max_keyframes=max_kf)
        duration_arg = None  # unbounded
        # Started at spawn time to catch the boot screen: give QEMU its
        # sub-second head start to bind the VNC socket before connecting.
        if not _wait_for_vnc("127.0.0.1", vnc_port):
            return fail("engine_error",
                        f"VNC 127.0.0.1:{vnc_port} never came up; "
                        f"no auto-screenshots for '{args.name}'")
        print(f"[autocap {args.name}] observing VNC 127.0.0.1:{vnc_port} "
              f"continuously from boot (STOP file / --max-seconds / signal "
              f"to end)" + (f", tracking {pkg}" if pkg else ""))
    else:
        cap_kwargs = dict(max_keyframes=max_kf)
        duration_arg = args.duration
        print(f"[capture {args.name}] observing VNC 127.0.0.1:{vnc_port} for "
              f"{args.duration}s" + (f", tracking {pkg}" if pkg else ""))

    # Only flags the caller actually passed are forwarded; anything left None
    # inherits capture.DEFAULT_* (the single source of truth for the tuning).
    for _flag in ("sample_scale_w", "change_percent", "change_threshold",
                  "black_threshold"):
        _v = getattr(args, _flag, None)
        if _v is not None:
            cap_kwargs[_flag] = _v

    try:
        meta = _capture.run_capture(
            "127.0.0.1", vnc_port, str(out_dir), duration_arg, **cap_kwargs)
    except Exception as e:  # noqa: BLE001
        if poller:
            poller.stop()
        stop_event.set()
        return fail("engine_error", f"capture failed: {e}")
    if poller:
        poller.stop()
        poller.join(timeout=2)
    stop_event.set()
    events = list(poller.events) if poller else []

    crash_detected, exit_detected, app_state = _capture_finalize(
        acct, out_dir, meta, events, pkg, running=False)

    result = {
        "name": args.name,
        "auto": auto,
        "output_dir": str(out_dir),
        "metadata_path": str(out_dir / "metadata.json"),
        "logcat_path": str(out_dir / "logcat.txt"),
        "keyframe_count": meta["keyframe_count"],
        "samples_seen": meta["samples_taken"],
        "duration_ms": meta["duration_ms"],
        "package": pkg,
        "crash_detected": crash_detected,
        "exit_detected": exit_detected,
        "app_state": app_state,
        "coverage": "vnc_framebuffer",
        "ok": True,
    }
    print(f"[{prefix} {args.name}] kept {meta['keyframe_count']} keyframe(s) from "
          f"{meta['samples_taken']} update(s); app_state={app_state} "
          f"crash={crash_detected} exit={exit_detected}")
    if json_mode:
        emit_json(result)
    else:
        print(json.dumps(result, indent=2))


# ---------- always-on dev auto-screenshots (engine-owned lifecycle) ----------
# The auto-screenshot recorder is NOT an opt-in the caller toggles: for a DEV
# account it is started automatically the moment the instance finishes booting
# (cmd_start --wait), runs continuously for the instance's lifetime,
# and is stopped on power-off (cmd_stop / cmd_remove). It writes to
# $OMNI_AUTOCAP_DIR when set (omni-agent points that at its /workspace), else
# runtime/<name>/autocap. `ensure_autocap` is idempotent — booting, resuming,
# or an explicit `omnidroid autocap --ensure` never stacks a second recorder — so the
# same feed is guaranteed on whenever a dev instance is up.
AUTOCAP_MAX_KEYFRAMES = 5000            # long always-on session, not a 20s window
_AUTOCAP_STATE_FILE = "autocap.json"    # in runtime_dir: {pid, out_dir, package}


def _autocap_out_dir(name, override=None):
    import os
    d = override or os.environ.get("OMNI_AUTOCAP_DIR")
    return str(Path(d)) if d else str(runtime_dir(name) / "autocap")


def _autocap_state(name):
    """(pid, out_dir) of a LIVE recorder for this account, or (None, None)."""
    p = runtime_dir(name) / _AUTOCAP_STATE_FILE
    if not p.exists():
        return None, None
    try:
        st = json.loads(p.read_text())
    except Exception:  # noqa: BLE001
        return None, None
    pid = st.get("pid")
    if pid and pid_alive(pid):
        return pid, st.get("out_dir")
    return None, None


def _spawn_autocap(name, out_dir, package=None, max_keyframes=AUTOCAP_MAX_KEYFRAMES):
    """Detached self-invocation of `capture <name> --auto` (mirrors _spawn_view):
    the recorder outlives the start/resume process that launched it."""
    a = ["capture", name, "--auto", "--out", out_dir,
         "--max-keyframes", str(max_keyframes)]
    if package:
        a += ["--package", package]
    if getattr(sys, "frozen", False):
        cmd = [sys.executable] + a
    else:
        cmd = [sys.executable, str(Path(__file__).resolve())] + a
    Path(out_dir).mkdir(parents=True, exist_ok=True)
    logf = open(Path(out_dir) / "autocap_engine.log", "ab")
    kwargs = {"stdin": subprocess.DEVNULL, "stdout": logf,
              "stderr": subprocess.STDOUT}
    if IS_WINDOWS:
        kwargs["creationflags"] = 0x00000008 | 0x00000200   # DETACHED|NEW_GRP
    else:
        kwargs["start_new_session"] = True
    try:
        proc = subprocess.Popen(cmd, **kwargs)
    finally:
        logf.close()
    return proc.pid


def ensure_autocap(acct, out_dir=None, force=False):
    """Idempotently ensure the continuous recorder is running for a DEBUG boot.
    No-op (and returns running=False) on a plain production boot — the feature
    rides on the debug boot. Returns a small status dict."""
    name = acct["name"]
    if not acct.get("debug"):
        return {"running": False, "reason": "not_debug_boot",
                "out_dir": None, "pid": None}
    want = _autocap_out_dir(name, out_dir)
    pid, cur_dir = _autocap_state(name)
    if pid and not force:
        same = cur_dir and Path(cur_dir).resolve() == Path(want).resolve()
        if same or not out_dir:
            # Already running (and either the caller didn't pin a dir, or it
            # matches) — idempotent no-op, the whole point of "ensure".
            return {"running": True, "pid": pid, "out_dir": cur_dir,
                    "already": True}
        # A DIFFERENT output dir was explicitly requested -> repoint.
        _stop_autocap_proc(name, pid, cur_dir)
    elif pid and force:
        _stop_autocap_proc(name, pid, cur_dir)
    out = want
    outp = Path(out)
    outp.mkdir(parents=True, exist_ok=True)
    # Fresh feed for this instance session: drop stale frames/metadata/sentinel
    # so a reader never mixes a previous boot's screenshots with this one.
    for f in outp.glob("frame_*.png"):
        try:
            f.unlink()
        except OSError:
            pass
    for fn in ("metadata.json", "STOP", "capture.pid"):
        try:
            (outp / fn).unlink()
        except OSError:
            pass
    pkg = acct.get("game_package")
    pid = _spawn_autocap(name, out, package=pkg)
    rd = runtime_dir(name)
    rd.mkdir(parents=True, exist_ok=True)
    (rd / _AUTOCAP_STATE_FILE).write_text(
        json.dumps({"pid": pid, "out_dir": out, "package": pkg,
                    "started": time.time()}), encoding="utf-8")
    return {"running": True, "pid": pid, "out_dir": out, "already": False}


def _stop_autocap_proc(name, pid, out_dir):
    """STOP the recorder gracefully (it watches for the sentinel + finalizes),
    then hard-kill if it lingers."""
    if out_dir:
        try:
            (Path(out_dir) / "STOP").write_text("stop\n", encoding="utf-8")
        except OSError:
            pass
    if pid:
        for _ in range(25):
            if not pid_alive(pid):
                break
            time.sleep(0.2)
        if pid_alive(pid):
            try:
                if IS_WINDOWS:
                    subprocess.run(["taskkill", "/F", "/PID", str(pid)],
                                   capture_output=True)
                else:
                    import os as _os
                    _os.kill(pid, 15)
            except Exception:  # noqa: BLE001
                pass


def stop_autocap(name):
    """Stop any recorder for this account and clear its state file. Called on
    power-off so a stopped instance never leaves a recorder attached to a dead
    VNC (the recorder also self-exits when VNC closes; this is the clean path)."""
    pid, out_dir = _autocap_state(name)
    if pid:
        _stop_autocap_proc(name, pid, out_dir)
    try:
        (runtime_dir(name) / _AUTOCAP_STATE_FILE).unlink()
    except OSError:
        pass
    return {"stopped": bool(pid), "out_dir": out_dir}


def maybe_start_autocap(acct, label):
    """Best-effort auto-start hook for the boot paths. Never raises into the
    boot flow — a recorder failure must not fail `start`."""
    try:
        r = ensure_autocap(acct)
        if r.get("running") and not r.get("already"):
            print(f"[{label}] auto-screenshots ON (debug boot) -> {r['out_dir']}")
    except Exception as e:  # noqa: BLE001
        print(f"[{label}] auto-screenshots could not start: {e}")


def cmd_autocap(args):
    """Inspect/control the always-on dev auto-screenshot recorder. The default
    action, --ensure, is IDEMPOTENT: it starts a recorder only if one is not
    already running, so omni-agent can call it on every ensure-emulator without
    ever stacking two. --status reports; --stop halts it; --restart forces a
    fresh one (e.g. to repoint --out)."""
    acct = load_account(args.name)
    json_mode = getattr(args, "json", False)
    if getattr(args, "stop", False):
        r = stop_autocap(args.name)
    elif getattr(args, "status", False):
        pid, out_dir = _autocap_state(args.name)
        r = {"running": bool(pid), "pid": pid, "out_dir": out_dir,
             "base": acct.get("base")}
    else:  # --ensure (default)
        if not running_pid(args.name):
            return fail("not_running",
                        f"account '{args.name}' is not running; start it first")
        if not acct.get("debug"):
            return fail("debug_boot_required",
                        f"auto screenshots are a debug feature; instance "
                        f"'{args.name}' was not booted with --debug.")
        r = ensure_autocap(acct, out_dir=getattr(args, "out", None),
                           force=getattr(args, "restart", False))
    out = {"name": args.name, "ok": True, **r}
    if json_mode:
        emit_json(out)
    else:
        print(json.dumps(out, indent=2))


def cmd_test_apk(args):
    """One-shot APK-swap harness: ensure a FRESH session, install the given
    APK, let the kiosk launch it, and report machine-readable JSON. Works on any
    base. Headless by default. After this, drive with: omnidroid screenshot / logcat
    / adb.

    Emits a single JSON line: {account, adb_port, qmp_port, package,
    installed, launched, foreground, pid, mode}."""
    ensure_qemu()
    cfg = load_config()
    name = args.name
    result = {"account": name}
    fresh = not (account_dir(name) / "account.json").exists()
    if fresh and not args.reuse:
        # Create a clean account on the current base. This is a persistent
        # folder-backed build account, NOT a product/store account, so use the
        # handle it returns directly (load_account is store-based and would not
        # find it).
        acct = _make_persistent_arm_account(name, cfg)
    else:
        acct = _load_persistent_arm_account(name)   # --reuse an existing one
    result["base"] = acct["base"]
    result["arch"] = acct_arch(acct)
    if not running_pid(name):
        mode = resolve_mode(cfg, args.mode)
        spawn_qemu(acct, cfg, interactive=False, mode=mode)
        if not wait_for_boot(acct, NORMAL_BOOT_TIMEOUT, f"test {name}"):
            print(json.dumps({**result, "ok": False,
                              "error": "boot timeout"}))
            sys.exit(1)
        post_boot(acct, f"test {name}")
        result["mode"] = mode["name"]
    pkg = apk_package_name(args.apk)
    result["package"] = pkg
    abi = _resolve_install_abi(acct, getattr(args, "abi", None),
                               getattr(args, "no_abi_pin", False))
    adb(acct, "logcat", "-c", timeout=15)
    r = _abi_install(acct, args.apk, abi)
    result["installed"] = "Success" in (r.stdout + r.stderr)
    abi_installed = (installed_primary_abi(acct, pkg)
                     if pkg and result["installed"] else None)
    result["abi_installed"] = abi_installed
    result["native_bridge_used"] = _is_arm_abi(abi_installed) \
        and result["arch"] == "x86"
    if getattr(args, "require_translation", False) \
            and not result["native_bridge_used"]:
        print(json.dumps({**result, "ok": False,
                          "error": "abi_not_translated"}))
        sys.exit(1)
    if pkg:
        acct["game_package"] = pkg
        save_account(acct)
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", pkg, timeout=10)
        # kiosk auto-launches on PACKAGE_ADDED; also nudge explicitly.
        adb(acct, "shell", "monkey", "-p", pkg, "-c",
            "android.intent.category.LAUNCHER", "1", timeout=30)
    deadline = time.time() + 30
    while time.time() < deadline:
        fg = _foreground(acct)
        if fg and pkg and fg.startswith(pkg):
            break
        time.sleep(3)
    result["foreground"] = _foreground(acct)
    try:
        result["pid"] = (adb(acct, "shell", "pidof", pkg, timeout=8)
                         .stdout.strip() or None) if pkg else None
    except Exception:
        result["pid"] = None
    result["launched"] = bool(result["pid"])
    result["adb_port"] = acct["adb_port"]
    result["qmp_port"] = acct["qmp_port"]
    result["adb_serial"] = f"127.0.0.1:{acct['adb_port']}"
    result["ok"] = result["installed"] and result["launched"]
    print(json.dumps(result))


WATCH_POLL_SECS = 3


def cmd_watch(args):
    """Host-side shutdown watchdog. THE decider for 'game closed'.

    States:
      WAITING  - game process has never been seen yet (kiosk may still
                 be launching it). No timeout here by default.
      RUNNING  - game process exists. Blips (ads, dialogs, webviews,
                 focus loss, loading screens) keep the process alive,
                 so they never leave this state.
      GRACE    - process is GONE. Confirm it stays gone for --grace
                 seconds of consecutive polls; any reappearance (quick
                 relaunch, in-place restart) returns to RUNNING.
      -> after grace expires: power the instance off (in-guest adb
         shutdown first, QMP quit fallback -> _shutdown chain).

    'Not foreground' is deliberately NOT a shutdown signal - only
    process death is. adb hiccups count as 'unknown' and never advance
    the grace timer; if the QEMU process itself dies we just exit.
    """
    try:
        sys.stdout.reconfigure(line_buffering=True)   # visible in logs
    except Exception:
        pass
    acct = load_account(args.name)
    pkg = args.package or acct.get("game_package")
    if not pkg:
        sys.exit("error: no game package known; pass --package or "
                 "run 'omnidroid install' first")
    grace = args.grace
    label = f"watch {args.name}"
    print(f"[{label}] pkg={pkg} grace={grace}s poll={WATCH_POLL_SECS}s")

    state = "WAITING"
    gone_since = None
    while True:
        if not running_pid(args.name):
            print(f"[{label}] QEMU process is gone - exiting watchdog")
            return
        pid_out = None
        try:
            r = adb(acct, "shell", "pidof", pkg, timeout=8)
            pid_out = r.stdout.strip()
        except Exception:
            pid_out = None          # adb hiccup -> unknown

        if pid_out is None:
            # Unknown: never advance grace on missing information.
            print(f"[{label}] adb unreachable (state={state}) - holding")
        elif pid_out:
            if state != "RUNNING":
                print(f"[{label}] game process up (pid {pid_out}) "
                      f"[{state} -> RUNNING]")
            state = "RUNNING"
            gone_since = None
        else:
            if state == "RUNNING":
                state = "GRACE"
                gone_since = time.time()
                print(f"[{label}] game process GONE - grace "
                      f"{grace}s starts [RUNNING -> GRACE]")
            elif state == "GRACE":
                waited = time.time() - gone_since
                if waited >= grace:
                    print(f"[{label}] gone for {waited:.0f}s >= "
                          f"{grace}s - shutting instance down")
                    _shutdown(acct, label)
                    return
                print(f"[{label}] still gone ({waited:.0f}/{grace}s)")
            # WAITING: game never started yet; keep waiting.
        time.sleep(WATCH_POLL_SECS)


def cmd_run_app(args):
    acct = load_account(args.name)
    r = adb(acct, "shell", "monkey", "-p", args.package,
            "-c", "android.intent.category.LAUNCHER", "1", timeout=30)
    print((r.stdout + r.stderr).strip())


def cmd_dev_ui(args):
    """Switch a DEBUG instance's visible UI. `--show kiosk` (default) foregrounds
    the kiosk and stops the Magisk app; `--show magisk` opens the Magisk manager
    (root UI), then switch back with `--show kiosk`. The Magisk manager app is
    installed by the devkit, so this needs a --debug boot."""
    acct = load_account(args.name)
    if not acct.get("debug"):
        out = {"ok": False, "error": "not_debug_boot",
               "message": f"'{args.name}' was not booted with --debug "
                          f"(no Magisk manager app to toggle)."}
        if getattr(args, "json", False):
            emit_json(out)
        else:
            print(out["message"])
        return
    if not running_pid(args.name):
        out = {"ok": False, "error": "not_running",
               "message": f"'{args.name}' is not running — start it first."}
        if getattr(args, "json", False):
            emit_json(out)
        else:
            print(out["message"])
        return
    if args.show == "magisk":
        su = resolve_su(acct)
        mpkg = _magisk_pkg(acct, su)
        diag = {"su": su, "magisk_pkg": mpkg}
        if not mpkg:
            out = {"ok": False, "error": "magisk_not_found", "diag": diag,
                   "message": ("Could not find the Magisk manager app. List packages to see its "
                               f"name: omnidroid adb {args.name} -- shell pm list packages | grep -i magisk "
                               "(if hidden/repackaged it has a random name).")}
            print(out["message"])
        else:
            # The kiosk pins itself with Lock Task Mode (device-owner), which blocks
            # launching other apps — that is why a plain `am start`/monkey did
            # nothing. Release the lock task via ROOT first, then launch Magisk as
            # root so it comes to the front. Best-effort; we report each step so a
            # failure is diagnosable from the --json output.
            steps = {}
            if su:
                r1 = adb(acct, "shell", f"{su} 0 sh -c {shlex.quote('am task lock stop')}",
                         timeout=15)
                steps["lock_stop"] = ((r1.stdout or "") + (r1.stderr or "")).strip()[-200:]
                r2 = adb(acct, "shell",
                         f"{su} 0 sh -c {shlex.quote(f'monkey -p {mpkg} -c android.intent.category.LAUNCHER 1')}",
                         timeout=20)
                steps["launch"] = ((r2.stdout or "") + (r2.stderr or "")).strip()[-200:]
            else:
                r2 = adb(acct, "shell", "monkey", "-p", mpkg,
                         "-c", "android.intent.category.LAUNCHER", "1", timeout=20)
                steps["launch_no_root"] = ((r2.stdout or "") + (r2.stderr or "")).strip()[-200:]
            diag["steps"] = steps
            out = {"ok": True, "showing": "magisk", "diag": diag,
                   "message": (f"Tried to open Magisk ({mpkg}) via {'root' if su else 'shell'}. "
                               f"If it still didn't appear, the kiosk Lock Task is holding the "
                               f"foreground — paste this --json diag. "
                               f"Back to kiosk: omnidroid dev-ui {args.name} --show kiosk")}
            print(out["message"])
    else:
        res = _assert_kiosk_foreground(acct, f"dev-ui {args.name}")
        out = {"ok": bool(res.get("kiosk_foreground")), "showing": "kiosk", **res}
        if not res.get("kiosk_foreground"):
            out["message"] = res.get("reason", "could not foreground the kiosk")
            print(out["message"])
    if getattr(args, "json", False):
        emit_json(out)


# ---------- Roblox session: token login + zero-click join --------------------
# Contract: ../contracts/omni-session.md. The two halves are INDEPENDENT because
# the Roblox Android client makes them so (verified against com.roblox.client
# 2.726.1142, arm64):
#
#   JOIN  — `com.roblox.client.ActivityProtocolLaunch` is exported and handles
#           the roblox:// scheme, so a place is joined with one intent and zero
#           taps. The only params it parses are placeId, gameInstanceId,
#           accessCode, linkCode, launchData, joinAttemptId, userId.
#   LOGIN — the client has NO authTicket/gameinfo/launchmode deep-link support
#           (that is the DESKTOP web-launch flow). Its session is the
#           .ROBLOSECURITY cookie in the WebView cookie jar (JNICookieManager /
#           WebViewCookieHandler over android.webkit.CookieManager), and
#           allowBackup="false" rules out the adb-backup injection trick.
#
# So the token can only be planted from INSIDE Roblox's own uid. The engine does
# not attempt that itself: it hands the session to the kiosk, which owns both
# halves in-guest (see launcher/src/com/omni/kiosk/SessionReceiver.java). This is
# identical on the dev and production bases — same kiosk, same intent, no clicks.
ROBLOX_PACKAGE = "com.roblox.client"
KIOSK_PACKAGE = "com.omni.kiosk"
KIOSK_RECEIVER = KIOSK_PACKAGE + "/.SessionReceiver"
KIOSK_ACTION_SET_SESSION = "com.omni.kiosk.SET_SESSION"
KIOSK_ACTION_CLEAR_SESSION = "com.omni.kiosk.CLEAR_SESSION"

# Runtime permissions granted to the game before launch so Android never parks a
# consent dialog on top of it. POST_NOTIFICATIONS is the one that actually bites
# (Android 13+, asked on first run); the rest are pre-answered so a later Roblox
# feature (voice chat, camera) cannot introduce a new prompt into a flow that is
# supposed to have none. Granting a permission the build does not declare is a
# harmless no-op error.
ROBLOX_RUNTIME_PERMS = (
    "android.permission.POST_NOTIFICATIONS",
    "android.permission.RECORD_AUDIO",
    "android.permission.CAMERA",
)


def public_session(sess):
    """The session as it may be logged / returned as JSON: token redacted."""
    out = {k: v for k, v in (sess or {}).items() if k != "token"}
    out["token"] = redact_token((sess or {}).get("token"))
    out["has_token"] = bool((sess or {}).get("token"))
    return out


def account_cookie(username):
    """The saved .ROBLOSECURITY for a Roblox username, or None."""
    from omnidroid import accounts as _acc
    rec = _acc.get_account(_store_root(), username)
    return rec.get("cookie") if rec else None


def resolve_token(args):
    """The cookie to log in with, in priority order:

    1. the instance name IS a saved account username (`omnidroid start <username>`) —
       the normal path; no flag needed;
    2. an explicit --token-file / --token-stdin / --token (manual override).

    A token passed as an argv string is readable by every process on the HOST for
    the lifetime of the call, so --token-file is preferred over --token."""
    # An explicit token flag always wins (lets you override / test a raw cookie).
    if getattr(args, "token_stdin", False):
        return sys.stdin.read().strip() or None
    tf = getattr(args, "token_file", None)
    if tf:
        try:
            return Path(tf).read_text(encoding="utf-8").strip() or None
        except OSError as e:
            fail("bad_token", f"could not read --token-file {tf}: {e}")
    tok = getattr(args, "token", None)
    if tok:
        return tok.strip() or None
    # Otherwise the instance name is the account username.
    name = getattr(args, "name", None)
    if name:
        return account_cookie(name)
    return None


def store_session(name, args=None, place_override=None):
    """The launch session for <name>, sourced from the central store:
    token = the stored cookie, place_id = an explicit override else the stored
    place_id. Optional transient join params come from CLI args (not
    persisted)."""
    from omnidroid import accounts as _acc
    rec = _acc.get_account(_store_root(), name) or {}
    sess = {"token": rec.get("cookie"), "place_id": None}
    place = place_override if place_override is not None else rec.get("place_id")
    if place is not None:
        sess["place_id"] = place
    if args is not None:
        for attr, key in (("job", "game_instance_id"), ("user_id", "user_id"),
                          ("access_code", "access_code"), ("link_code", "link_code"),
                          ("launch_data", "launch_data")):
            v = getattr(args, attr, None)
            if v is not None:
                sess[key] = v
    return sess


def _validate_place_id(v):
    try:
        pid = int(str(v).strip())
    except (TypeError, ValueError):
        return fail("bad_place", f"--place must be a numeric Roblox placeId, "
                                 f"got {v!r}")
    if pid <= 0:
        return fail("bad_place", f"--place must be positive, got {pid}")
    return pid


def roblox_deeplink(sess):
    """The join URL, in the form the app's own ActivityProtocolLaunch parses.

    `roblox://experiences/start?placeId=...` is the current documented route and
    matches the EXPERIENCES/START route strings in the client's dex. Optional
    params are only appended when set, because the client treats an empty value
    as present-but-blank rather than absent."""
    from urllib.parse import urlencode
    place = sess.get("place_id")
    if not place:
        return None
    q = {"placeId": str(place)}
    for key, param in (("game_instance_id", "gameInstanceId"),
                       ("access_code", "accessCode"),
                       ("link_code", "linkCode"),
                       ("launch_data", "launchData")):
        if sess.get(key):
            q[param] = str(sess[key])
    return "roblox://experiences/start?" + urlencode(q)


def _parse_broadcast_result(out):
    """`am broadcast` prints: Broadcast completed: result=N, data="...".
    The kiosk answers with a JSON blob in data= so the host learns what actually
    happened in-guest instead of guessing from an exit code."""
    m = re.search(r'data="(.*?)"\s*$', out or "", re.S | re.M)
    if not m:
        return None
    raw = m.group(1).replace('\\"', '"')
    try:
        return json.loads(raw)
    except Exception:  # noqa: BLE001
        return {"raw": raw}


def kiosk_broadcast(acct, action, extras=None, timeout=45):
    """Send an ordered broadcast to the kiosk and return its parsed reply.

    Values are quoted for the GUEST shell: `adb shell` concatenates argv into one
    string that the device's sh re-parses, so an unquoted token (or launch_data
    JSON) would be word-split there."""
    parts = ["am", "broadcast", "-a", shlex.quote(action),
             "-n", shlex.quote(KIOSK_RECEIVER)]
    for key, (flag, val) in (extras or {}).items():
        parts += [flag, shlex.quote(key), shlex.quote(str(val))]
    r = adb(acct, "shell", " ".join(parts), timeout=timeout)
    out = (r.stdout or "") + (r.stderr or "")
    return _parse_broadcast_result(out), out


def kiosk_installed(acct):
    r = adb(acct, "shell", "pm", "path", KIOSK_PACKAGE, timeout=15)
    return "package:" in (r.stdout or "")


def deliver_session(acct, label, sess, play=True, restart=True):
    """Hand `sess` (see store_session()) to the in-guest kiosk and (by default)
    tell it to join. `play=False` (no place_id) is HOME mode: the cookie is
    still delivered — the account is logged in — but no place is joined.
    Returns a status dict; never raises into a boot path."""
    name = acct["name"]
    if not sess.get("token") and not sess.get("place_id"):
        return {"delivered": False, "reason": "no_session"}
    if not kiosk_installed(acct):
        return {"delivered": False, "reason": "kiosk_missing",
                "detail": f"{KIOSK_PACKAGE} is not installed on '{name}'; "
                          f"the session/auto-join feature needs it "
                          f"(omnidroid kioskify {name})"}
    # Answer Android's runtime-permission prompts before they can appear. On
    # Android 13+ Roblox asks for POST_NOTIFICATIONS on first run and parks an
    # "Allow Roblox to send you notifications?" dialog ON TOP of the game — a
    # menu with a button, i.e. exactly what this product promises never to show.
    # Granting as shell (we hold GRANT_RUNTIME_PERMISSIONS) is what actually
    # works; the kiosk's device-owner setPermissionPolicy(AUTO_GRANT) is kept as
    # a belt-and-braces default but does not retroactively answer a prompt the
    # app has already queued. Failures are ignored: a permission the build does
    # not declare simply errors out, which is not a reason to fail the launch.
    for perm in ROBLOX_RUNTIME_PERMS:
        try:
            adb(acct, "shell", "pm", "grant", ROBLOX_PACKAGE, perm, timeout=15)
        except Exception:  # noqa: BLE001
            pass
    # Tell the kiosk which package IS the game, so that a later REBOOT re-joins
    # on its own (MainActivity -> resolveGamePackage -> launchGame -> deep link)
    # instead of relying on its "first launchable non-system app" guess. The arm
    # bases never run provision_settings (their /data template is pre-provisioned
    # and first_boot_done is set at create time), so nothing else sets this.
    try:
        adb(acct, "shell", "settings", "put", "global", "omni_game_package",
            ROBLOX_PACKAGE, timeout=15)
    except Exception as e:  # noqa: BLE001 — the broadcast below still works
        print(f"[{label}] could not set omni_game_package: {e}")
    if restart:
        # Cold-start Roblox so the new cookie is read at startup. Without this,
        # switching accounts silently joins as the PREVIOUS user: the client
        # caches the authenticated user in-process. This has to happen host-side
        # — as shell we hold FORCE_STOP_PACKAGES, whereas the kiosk (an ordinary
        # app, device owner or not) cannot stop a foreground app at all. Applies
        # in HOME mode too (play=False): the cookie still needs a cold start to
        # take effect.
        try:
            adb(acct, "shell", "am", "force-stop", ROBLOX_PACKAGE, timeout=25)
        except Exception as e:  # noqa: BLE001 — a live instance still plays
            print(f"[{label}] could not force-stop {ROBLOX_PACKAGE}: {e}")
    extras = {"play": ("--ez", "true" if play else "false")}
    if sess.get("place_id"):
        extras["place_id"] = ("--el", int(sess["place_id"]))
    if sess.get("token"):
        extras["token"] = ("--es", sess["token"])
    for key, flag in (("game_instance_id", "--es"), ("access_code", "--es"),
                      ("link_code", "--es"), ("launch_data", "--es")):
        if sess.get(key):
            extras[key] = (flag, sess[key])
    if sess.get("user_id"):
        extras["user_id"] = ("--el", int(sess["user_id"]))
    reply, raw = kiosk_broadcast(acct, KIOSK_ACTION_SET_SESSION, extras)
    if not reply:
        return {"delivered": False, "reason": "no_kiosk_reply",
                "detail": raw.strip()[-400:]}
    ok = bool(reply.get("ok"))
    status = {"delivered": ok, "kiosk": reply,
              "place_id": sess.get("place_id"), "played": bool(reply.get("launched"))}
    if not ok:
        status["reason"] = reply.get("error") or "kiosk_rejected"
    print(f"[{label}] session -> kiosk: place {sess.get('place_id') or 'home'}, "
          f"token {redact_token(sess.get('token')) or 'NONE'}, "
          f"{'joined' if reply.get('launched') else 'not joined'}"
          + (f" ({reply.get('error')})" if reply.get("error") else ""))
    return status


def apply_farming_squeeze(acct, mode=None):
    """Run the farming runtime squeeze over adb. Called only on a farming boot.

    Every step is fire-and-forget by construction (each shell one-liner ends
    in `true`), because a squeeze is an optimization, never a precondition:
    an instance that could not disable one package must still end up joined
    and running, just fatter."""
    for cmd in farming.build_squeeze_sequence(mode):
        adb(acct, *cmd, timeout=20)


def apply_gaming_tuning(acct, mode=None, label=None):
    """Run the gaming runtime tune-up over adb. Called only on a gaming boot.

    Fire-and-forget for the same reason as the farming squeeze: this is what
    makes an instance pleasant to play, never what makes it work. It also
    UNDOES the farming levers that persist in /data, so an account that was
    farmed and is then started in gaming mode does not keep a 480x270 display
    and a background-cpuset game — see gaming.py.

    Two steps need root (swappiness, top-app cpuset). Without it they are
    skipped and SAID OUT LOUD rather than emitted to fail silently: as uid
    shell both writes are denied, and every script here ends in `; true`, so
    an unreported skip would look exactly like success."""
    su = resolve_su(acct)
    for cmd in gaming.build_tuning_sequence(mode, su=su):
        adb(acct, *cmd, timeout=20)
    if label:
        if su:
            print(f"[{label}] gaming tune-up: native resolution, animations "
                  f"off, doze off, swappiness {gaming.GAMING_SWAPPINESS}")
        else:
            print(f"[{label}] gaming tune-up: applied WITHOUT root - skipped "
                  + ", ".join(gaming.root_only_steps()))


def pin_game_to_top_app(acct, label=None):
    """Move the running game onto the top-app cpuset. Call AFTER the session
    has been delivered — that broadcast is what launches the game, so there is
    no pid to move before it. Returns True when the move landed."""
    step = gaming.build_pin_game_step(resolve_su(acct))
    if not step:
        if label:
            print(f"[{label}] cpuset: SKIPPED - no root on this instance")
        return False
    adb(acct, *step, timeout=gaming.PIN_WAIT_SECS + 15)
    ok = "top-app" in (adb(acct, "shell",
                           f"cat /proc/$(pidof {gaming.GAME_PKG})/cgroup",
                           timeout=20).stdout or "")
    if label:
        print(f"[{label}] cpuset: "
              + ("game on top-app (latency-critical scheduler set)" if ok
                 else "game NOT pinned - it runs in its default cpuset"))
    return ok


# Balloon inflation is asynchronous; these bound how long we wait for the
# guest to actually hand the pages back before calling it a miss. ~30 s total,
# against a measured ~20 s to settle a 2048 -> 1024 MB inflation.
ZRAM_SETTLE_TRIES = 6
ZRAM_SETTLE_SECS = 2

BALLOON_SETTLE_TRIES = 10
BALLOON_SETTLE_SECS = 3
BALLOON_TOLERANCE = 1.05     # within 5% of target counts as reached


def zram_active(acct):
    """True when the guest actually has swap on (i.e. zram came up).

    Deliberately a guest-state probe, not a record of whether we ran the
    zram step: enabling zram needs root, so on the non-rooted production base
    the step runs, fails silently, and would otherwise leave us confidently
    applying a cap that kills the game."""
    try:
        out = adb(acct, "shell", "sh", "-c",
                  shlex.quote("grep ^SwapTotal /proc/meminfo"),
                  timeout=20).stdout or ""
        return int(re.sub(r"\D", "", out) or 0) > 0
    except Exception:
        return False


def enable_zram(acct, mode=None, label=None):
    """Turn on the zram swap the base already ships, and REPORT the outcome.

    The image is not missing zram — it ships the device, the lz4 compressor,
    an fstab entry (`zramsize=50%`) and an init trigger that calls
    swapon_all. All of it sits behind one property. So this flips that
    property rather than poking /sys/block/zram0 by hand; the old manual
    dance duplicated what /vendor/etc/init/zram.rc already does, and picked a
    fixed size where the fstab's 50% scales with the guest.

    Verified on the real base: setting it made init run swapon_all and
    SwapTotal went 0 -> 470980 kB immediately.

    Needs root: it is a persist.* property (settable at runtime, unlike ro.*)
    but SELinux denies uid shell — measured "Failed to set property". So on
    the production base this reports a skip and names the fix, rather than
    failing quietly the way the original zram step did for months."""
    su = resolve_su(acct)
    prop = next(iter(lean.ZRAM_ENABLE_PROP))
    val = lean.ZRAM_ENABLE_PROP[prop]
    if su:
        adb(acct, "shell",
            f"{su} 0 sh -c {shlex.quote(f'setprop {prop} {val}')}", timeout=30)
        # init reacts to the property asynchronously; give swapon_all a beat.
        for _ in range(ZRAM_SETTLE_TRIES):
            if zram_active(acct):
                break
            time.sleep(ZRAM_SETTLE_SECS)
    ok = zram_active(acct)
    if label:
        if ok:
            print(f"[{label}] zram: swap on via {prop} "
                  f"(lz4, 50% of guest RAM, ~3x compression measured) - "
                  f"instance can hold a third less RAM")
        else:
            print(f"[{label}] zram: NOT enabled"
                  + ("" if su else f" (no root; {prop} is denied to uid "
                                   f"shell by SELinux)")
                  + f". Instance keeps the higher memory cap. For production, "
                    f"bake {prop}={val} into the base: "
                    f"`omnidroid enable-zram-base`.")
    return ok


def apply_roblox_settings(acct, label=None, settings=None):
    """Install a Roblox ClientAppSettings.json profile.

    `settings` selects the profile; None keeps the farming default (fps cap +
    lowest quality), which is what every pre-existing caller means. A gaming
    boot passes lean.GAMING_APP_SETTINGS instead — the same install path and
    the same root requirement, with the tick cap opened up rather than clamped.

    Measured 2026-08-05: no memory change (680 -> 677 MB) but host CPU per
    instance roughly HALVED (36% -> 18.8%). For the 50+-instance target CPU
    binds as hard as RAM, so this is the biggest CPU lever available.

    Returns True if applied, False if it could not be. It reports the skip
    LOUDLY rather than returning quietly, because a step that silently does
    nothing while looking successful is exactly the failure mode this
    codebase already had (see farming.sh)."""
    su = resolve_su(acct)
    script = farming.build_client_settings_script(su, settings)
    if not script:
        if label:
            print(f"[{label}] roblox settings: SKIPPED - no root on this "
                  f"instance, and the file lives in the game's private data "
                  f"dir. The ~2x CPU saving is NOT applied here. Bake it into "
                  f"the base /data image or have OmniBootstrap write it.")
        return False
    adb(acct, "shell", f"{su} 0 sh -c {shlex.quote(script)}", timeout=30)
    # Verify by reading it back as root; a chown/label mistake leaves a file
    # the app cannot read, which is indistinguishable from success.
    r = adb(acct, "shell",
            f"{su} 0 sh -c {shlex.quote(f'cat {lean.CLIENT_SETTINGS_FILE}')}",
            timeout=20)
    ok = "DFIntTaskSchedulerTargetFps" in (r.stdout or "")
    if label:
        # Describe the profile that was actually installed, DERIVED from the
        # dict rather than from which constant it happens to be. The original
        # line hardcoded the farming description, so a gaming boot -- which
        # opens the tick cap up rather than clamping it -- reported "fps cap +
        # lowest quality; ~2x less host CPU" while doing the opposite; keying
        # off object identity then made a third profile silently misreport too.
        eff = settings or lean.CLIENT_APP_SETTINGS
        fps = eff.get("DFIntTaskSchedulerTargetFps")
        qlvl = eff.get("DFIntDebugFRMQualityLevelOverride")
        fx = "post-FX on" if eff.get("FFlagDisablePostFx") is False \
            else "post-FX off"
        detail = (f"tick target {fps} fps, quality level {qlvl}, {fx}"
                  if fps and fps > 30
                  else f"fps cap {fps} + quality level {qlvl}; "
                       f"~2x less host CPU")
        print(f"[{label}] roblox settings: "
              + (f"applied ({detail})" if ok
                 else "write did NOT land - instance runs uncapped"))
    return ok


def apply_balloon_target(acct, mode, label=None):
    """Inflate the virtio-balloon to the mode's post-boot target.

    This is the HARD cap on what one instance costs the host, as opposed to
    free-page-reporting, which is best-effort and only returns pages the
    guest happens to have freed. It runs AFTER the squeeze so the guest has
    already released what it can and the balloon only has to claim what is
    genuinely spare.

    Returns the balloon's actual size in MB, or None when the mode wants no
    balloon or the guest has no balloon driver to answer with."""
    target_mb = (mode or {}).get("balloon")
    # zram changes which floor is safe, so ASK the guest rather than assume.
    # With lz4 compressing ~3x, the guest survives a third less RAM; without
    # it, the same target kills the game outright. Reading SwapTotal is the
    # only honest way to know which regime this instance is actually in --
    # the squeeze's zram step needs root and is a no-op on the production
    # base, so "we ran the step" proves nothing.
    if target_mb and (mode or {}).get("balloon_zram"):
        if zram_active(acct):
            target_mb = mode["balloon_zram"]
            if label:
                print(f"[{label}] zram is active - using the lower "
                      f"{target_mb} MB cap")
        elif label:
            print(f"[{label}] no zram in this guest - holding the "
                  f"{target_mb} MB cap (the lower one would OOM the game)")
    if not target_mb:
        return None
    if qmp(acct, "balloon", {"value": int(target_mb) * 1024 * 1024}) is None:
        if label:
            print(f"[{label}] balloon: QMP unreachable, instance keeps its "
                  f"full {mode.get('mem')} MB")
        return None
    # Read back rather than trusting the request: a guest without the balloon
    # driver accepts the command and simply never inflates, and reporting a
    # target we did not actually reach is how a capacity plan turns into an
    # out-of-memory host.
    #
    # POLL, do not sample once. Inflation is asynchronous: QEMU returns the
    # moment the request is queued, and the guest then walks its free lists
    # handing pages back over some seconds. An immediate query-balloon
    # reports the pre-inflation size every time, which reads exactly like a
    # missing balloon driver — measured 2026-08-05, a guest that reached its
    # 1024 MB target in ~20 s was reported as "driver missing (actual 2046)"
    # by a single eager read.
    actual_mb = None
    for _ in range(BALLOON_SETTLE_TRIES):
        r = qmp(acct, "query-balloon") or {}
        actual = (r.get("return") or {}).get("actual")
        actual_mb = int(actual / (1024 * 1024)) if actual else None
        if actual_mb and actual_mb <= target_mb * BALLOON_TOLERANCE:
            break
        time.sleep(BALLOON_SETTLE_SECS)
    if label:
        if actual_mb and actual_mb <= target_mb * BALLOON_TOLERANCE:
            print(f"[{label}] balloon: guest capped at {actual_mb} MB "
                  f"(target {target_mb} MB)")
        else:
            print(f"[{label}] balloon: target {target_mb} MB NOT reached "
                  f"(actual {actual_mb} MB after "
                  f"{BALLOON_SETTLE_TRIES * BALLOON_SETTLE_SECS}s) - guest "
                  f"balloon driver missing? Instance still runs, just fatter.")
    return actual_mb


RESTORE_TIMEOUT = 30       # a healthy warm restore is seconds, not minutes
_QEMU_VERSION_CACHE = {}


def _halt_qemu(acct):
    """Kill this instance's QEMU immediately. NOT _shutdown().

    _shutdown() tries a graceful in-guest power-off first, which cannot work
    on a guest that is PAUSED (the bake stops the VM before migrating) and
    would burn its 90 s timeout on every bake. Both callers here -- the
    post-bake handoff and the poisoned-entry fallback -- want the process
    gone now, and neither has any in-guest state worth preserving.
    """
    from omnidroid.qemu_proc import qmp
    name = acct["name"]
    pid = running_pid(name)
    qmp(acct, "quit")
    time.sleep(1)
    if pid and pid_alive(pid):
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    # run.json is deliberately left alone: the next spawn_qemu overwrites it,
    # and wiping it here would strip the port reservation this instance still
    # owns for the restore that follows.


def _qemu_version(tool):
    """First line of `<tool> --version`, cached. Part of the cache key: the
    migration stream format is tied to the QEMU build that wrote it."""
    if tool not in _QEMU_VERSION_CACHE:
        try:
            out = subprocess.run([qemu_bin(tool), "--version"],
                                 capture_output=True, text=True,
                                 timeout=20).stdout.splitlines()
            _QEMU_VERSION_CACHE[tool] = out[0].strip() if out else ""
        except Exception:      # noqa: BLE001 - unknown version = no cache
            _QEMU_VERSION_CACHE[tool] = ""
    return _QEMU_VERSION_CACHE[tool]


def _warm_cache_allowed(debug, in_use, key, no_warm=False):
    """May THIS launch use the warm cache?

    False for a debug boot (the devkit vdc disk changes device topology, so a
    restore would not match), for an unknown key, for an entry already
    backing a running instance -- the second concurrent restore comes up alive
    but `offline` on adb (design spec 8b) -- and for `no_warm` (the
    --no-warm / OMNI_NO_WARM=1 kill switch). This is the ONE decision point
    both restore and bake are gated through, so the kill switch and every
    other refusal reason only need to be encoded once. Refusing costs one
    cold boot.
    """
    if debug or not key or no_warm:
        return False
    return key not in in_use


def _stage_bake_overlays(acct, cfg, rd):
    """Writable COW overlays for a BAKE boot, plus a fresh efivars.

    A bake must persist its freeze point, so it cannot use the shared
    templates opened snapshot=on the way a normal ephemeral boot does.
    """
    base = cfg["bases"][acct["base"]]
    images = Path(images_dir(cfg))
    rd.mkdir(parents=True, exist_ok=True)
    pairs = ((images / base["system"], rd / "bake_system.qcow2"),
             (images / (acct.get("data_image") or base["data"]),
              rd / "bake_data.qcow2"))
    for backing, overlay in pairs:
        overlay.unlink(missing_ok=True)
        subprocess.run([qemu_bin("qemu-img"), "create", "-f", "qcow2", "-F", "qcow2",
                        "-b", str(backing), str(overlay)],
                       check=True, capture_output=True)
    # efivars/pflash is a UEFI concept: only the arm base boots through EDK2
    # firmware. x86 boots by direct kernel/initrd and has no efivars file at
    # all. Staging one unconditionally (as this used to, defaulting to
    # ARM_BASE_EFIVARS when the base has no "efivars" key) either raised on
    # an x86-only host -- caught by the caller, want_bake silently went False
    # forever -- or, on a host that ALSO has an arm base, quietly copied that
    # UNRELATED arm base's UEFI blob into this x86 runtime dir and then into
    # the entry warmboot.bake_entry() produces. See _ensure_booted, which
    # already refuses to key/restore/bake a non-arm base for the same reason.
    if base_type(base) == BASE_TYPE_ARM:
        shutil.copyfile(images / base.get("efivars", ARM_BASE_EFIVARS),
                        rd / "efivars.fd")


def _ensure_booted(acct, cfg, label, timeout=None, accel=None, mode_name=None,
                   mem=None, balloon=None, debug=None, smp=None, quality=None,
                   no_warm=None, _no_rebake=False):
    """Boot the instance if it isn't up, and block until Android is ready.
    Returns (ok, first_boot). `debug` (per-boot) attaches the devkit disk;
    default None means fall back to the account handle's own `debug` flag.
    `no_warm` (per-boot) disables the warm-restore cache; default None means
    fall back to the OMNI_NO_WARM=1 kill switch (cmd_start resolves --no-warm
    itself and passes the result down). `quality` overrides the mode's own
    ClientAppSettings profile."""
    if debug is None:
        debug = bool(acct.get("debug"))
    if no_warm is None:
        no_warm = _no_warm_requested()
    first = not acct.get("first_boot_done")
    # Resolved ONCE and reused for the spawn, the squeeze, and the balloon.
    # Previously the spawn called resolve_mode() inline and dropped `mem`
    # entirely, so `omnidroid start --mem 2048` silently booted at the mode's own
    # size — a 512 MB farming boot that never reached adbd, reported as a
    # boot timeout with no hint that the flag had been ignored.
    mode = resolve_mode(cfg, mode_name, mem=mem, balloon=balloon, smp=smp)
    if mode.get("profile") == "performance":
        print(f"[{label}] mode {mode['name']}: {mode['mem']} MB / "
              f"{mode['smp']} vCPU, quality {quality or mode.get('quality')}, "
              f"no balloon"
              + (" (sized to this host)" if mode.get("autoscale") else ""))
    # Set in the cold-boot branch below when this launch staged a bake;
    # left False here so the shared wait/tail code after the if/else (taken
    # also when the instance was already up) has a safe default to test.
    want_bake = False
    # True only once a warm restore has both completed AND Android has
    # finished booting -- lets the shared tail below skip the cold-boot
    # spawn/wait it would otherwise (redundantly, or wrongly) run, while
    # still reaching the SAME post-boot pipeline a cold boot reaches
    # (post_boot, kiosk/hiding enforcement, and -- the actual bug this
    # fixes -- the mode tuning block). A restored instance must be
    # equivalent to a cold-booted one, not a stripped-down one that
    # returned early before any of that ran.
    restored = False
    if running_pid(acct["name"]):
        adb_connect(acct)
        if adb_getprop(acct, "sys.boot_completed") == "1":
            return True, first
        print(f"[{label}] instance is up but Android is still booting; waiting")
    else:
        # `interactive` is the boot PROFILE (full host smp/mem + serial log),
        # historically used for a first/provisioning boot. It is independent of
        # `debug`, which attaches the devkit disk.
        interactive = first
        from omnidroid import warmboot, warmcache
        from omnidroid.runtime import warm_keys_in_use
        # Resolving the cache key needs a fully-populated cfg (images_dir,
        # bases) that not every caller has (e.g. a bare read_config() with no
        # base registered yet). That must be a MISS, never a crash: nothing
        # about the warm cache may raise into a boot path (see warmcache.py's
        # own module docstring) -- key stays None and this launch just cold-
        # boots exactly as it did before this feature existed.
        images = None
        qver = None
        base = None
        key = None
        in_use = set()
        try:
            images = Path(images_dir(cfg))
            tool = ("qemu-system-aarch64" if acct_base_is_arm(acct)
                    else "qemu-system-x86_64")
            qver = _qemu_version(tool)
            base = cfg["bases"][acct["base"]]
            if not acct_base_is_arm(acct):
                # The cache only knows how to stage/move an efivars.fd (UEFI
                # pflash vars): x86 boots by direct kernel/initrd and has no
                # such concept. Leaving `key` unset makes _warm_cache_allowed()
                # refuse both a restore lookup and a bake for this launch --
                # explicit and logged, rather than a bake that raises deep
                # inside _stage_bake_overlays/warmboot.bake_entry, or (on a
                # host that also has an arm base) one that succeeds by
                # accident and files that unrelated arm base's efivars.fd
                # inside an x86 entry.
                print(f"[{label}] warm-restore cache is arm-only on this "
                      f"host; '{acct['base']}' has no efivars/pflash "
                      f"concept -- cold-booting")
            elif qver:
                # Folds the offset's BACKING IMAGE identity (size, mtime)
                # into the key, not just its name: `offset delete <name>`
                # followed by `offset create <name> <different apk>` reuses
                # the name for a different build, and the name alone would
                # key-match the OLD entry and silently restore the stale
                # Roblox. None (no offset, e.g. `--apk`/`--offset none`) is
                # itself a stable, distinct value -- no extra branching
                # needed for that case.
                offset_image_stat = None
                data_image = acct.get("data_image")
                if data_image:
                    st = (images / data_image).stat()
                    offset_image_stat = (st.st_size, int(st.st_mtime))
                key = warmcache.cache_key(
                    arch=acct_arch(acct), base_tag=acct["base"],
                    base_version=base.get("version", 0),
                    offset=acct.get("offset") or "none",
                    offset_image_stat=offset_image_stat,
                    mode_name=mode["name"], mem_mb=mode["mem"], smp=mode["smp"],
                    machine="virt" if acct_base_is_arm(acct) else "q35",
                    accel=accel or default_accel(), qemu_version=qver)
            # Inside the same guarded region as the rest of cache-key
            # resolution: a sibling instance's malformed run.json (or any
            # other unexpected failure reading the runtime root) must yield
            # "no keys in use" here, not an exception straight into the boot
            # path -- that would break EVERY launch on the host, including
            # the cold-boot fallback this except clause exists to guarantee.
            in_use = warm_keys_in_use()
        except Exception:      # noqa: BLE001 - unresolved cfg = no cache, not a crash
            images = None
            key = None
            in_use = set()
        entry = None
        if _warm_cache_allowed(debug, in_use, key, no_warm=no_warm):
            entry = warmcache.lookup(images, key, qver)

        if entry is not None:
            # FAST PATH: restore a pre-booted machine instead of booting one.
            spawn_qemu(acct, cfg, interactive=False, mode=mode, accel=accel,
                       debug=debug, warm=entry, warm_key=key)
            maybe_start_autocap(acct, label)
            migrated = warmboot.restore_into(acct, entry, label)
            if migrated:
                warmcache.touch(entry)
            if migrated and wait_for_boot(acct, RESTORE_TIMEOUT, label):
                warmboot.resync_guest_clock(acct, label)
                restored = True
            else:
                # The QEMU this launch just spawned is either paused (a
                # rejected/failed migration) or running but never reached
                # adbd -- either way it must die before the cold-boot
                # fallback below spawns a fresh one on the same ports.
                _halt_qemu(acct)
                if not migrated:
                    # POISONED ENTRY: restore_into() ITSELF failed, so the
                    # state file is the suspect -- discard it. A mere
                    # wait_for_boot timeout (migrated True) does NOT imply
                    # that: adb not answering inside RESTORE_TIMEOUT can be a
                    # port conflict, a transient adb hiccup, or any number of
                    # causes unrelated to the state file, so that case keeps
                    # the entry rather than destroying a ~2.4 GiB, one-cold-
                    # boot-plus-bake artifact over an unrelated failure.
                    #
                    # `in_use` above was sampled before THIS launch's own
                    # spawn_qemu made its run.json visible, so a second,
                    # concurrent launch against the same entry could have
                    # started restoring from it in the meantime. Re-check
                    # warm_keys_in_use() FRESH, immediately before the
                    # rmtree, so this launch never deletes an entry a
                    # sibling is now actually running on.
                    if key not in warm_keys_in_use():
                        print(f"[{label}] warm restore rejected; discarding "
                              f"the entry and cold-booting")
                        shutil.rmtree(entry, ignore_errors=True)
                    else:
                        print(f"[{label}] warm restore rejected, but "
                              f"another instance is now running off this "
                              f"entry; keeping it and cold-booting")
                else:
                    print(f"[{label}] warm restore came up but Android "
                          f"never finished booting; cold-booting instead "
                          f"(entry kept -- the state file is not implicated)")
                entry = None

        if not restored:
            # COLD PATH, optionally baking a new entry on the way.
            want_bake = (not _no_rebake
                        and _warm_cache_allowed(debug, in_use, key,
                                                no_warm=no_warm)
                        and not interactive
                        and warmcache.has_room(
                            images, warmboot.projected_entry_bytes(mode["mem"])))
            if want_bake:
                rd = runtime_dir(acct["name"])
                try:
                    _stage_bake_overlays(acct, cfg, rd)
                except Exception as e:      # noqa: BLE001 - a bake is an optimisation,
                    # never a precondition of a successful launch. want_bake is
                    # true on essentially every non-first, non-debug boot, so a
                    # missing/unreadable backing image, a full disk or a
                    # permission error here is the ordinary path, not an edge
                    # case -- it must degrade to a normal cold boot, not fail
                    # the launch. Falling through with want_bake left True would
                    # have spawn_qemu told to bake off overlays that were never
                    # created.
                    print(f"[{label}] could not stage warm-bake overlays ({e}); "
                          f"booting normally without baking")
                    want_bake = False
            # bake/warm_key are only ever non-default on a bake attempt: passing
            # them unconditionally would change this call's kwargs on EVERY
            # ordinary boot, not just a baking one.
            bake_kwargs = {"bake": True, "warm_key": key} if want_bake else {}
            spawn_qemu(acct, cfg, interactive=interactive,
                       mode=None if interactive else mode, accel=accel,
                       debug=debug, **bake_kwargs)
            # Same rule as `start`: the recorder attaches at spawn so a debug
            # session has screenshots of the boot screen itself.
            maybe_start_autocap(acct, label)
    if not restored:
        t = timeout or (FIRST_BOOT_TIMEOUT if first else NORMAL_BOOT_TIMEOUT)
        if not wait_for_boot(acct, t, label, first_boot=first):
            return False, first
        if want_bake:
            meta = {"qemu_version": qver, "mem_mb": mode["mem"],
                    "smp": mode["smp"], "mode": mode["name"],
                    "base": acct["base"], "base_version": base.get("version", 0),
                    "offset": acct.get("offset") or "none"}
            if warmboot.bake_entry(acct, images, key, meta,
                                   runtime_dir(acct["name"]), label):
                warmcache.evict_lru(images, warm_keys_in_use())
                # The bake stopped the VM and moved its disks into the entry;
                # restore from what we just made so the FIRST launch takes the
                # same code path as every later one.
                #
                # _no_rebake guards the recursion: if THAT restore also fails,
                # the entry is discarded and the retry cold-boots WITHOUT
                # baking again -- otherwise a reproducibly-bad bake would loop
                # bake -> restore -> discard -> bake forever.
                _halt_qemu(acct)
                return _ensure_booted(acct, cfg, label, timeout=timeout,
                                      accel=accel, mode_name=mode_name,
                                      mem=mem, smp=smp, balloon=balloon,
                                      quality=quality, debug=debug,
                                      no_warm=no_warm, _no_rebake=True)
            # BAKE FAILED: bake_entry() issues a QMP `stop` before attempting the
            # migrate and does not resume the guest on failure, so the boot this
            # launch already waited for is paused, not usable, until resumed here.
            # Baking was an add-on, not a precondition of a successful launch --
            # resume and fall through to the ordinary post-boot pipeline below
            # exactly as an unbaked cold boot would.
            qmp(acct, "cont")
    # SHARED TAIL: reached by every successful path -- an instance that was
    # already up, a fresh cold boot, and (the fix here) a successful warm
    # restore alike -- so a restored instance gets the exact same post-boot
    # pipeline, including the mode tuning below, as a cold-booted one.
    post_boot(acct, label)
    if first:
        provision_settings(acct, label)
        acct["first_boot_done"] = True
        # NOTE: no save_account() here. The only caller (cmd_start) always
        # passes a build_acct() handle, which already carries
        # first_boot_done=True -- so `first` is always False in practice and
        # this branch is dead in the product path. A stray save_account()
        # would write accounts/<name>/account.json, a file nothing reads
        # under the diskless model; dropped rather than left as a landmine.
    # EVERY boot, production included: re-enforce Magisk hiding so the game
    # sees an unrooted device. Idempotent, needs only su, no devkit disk. On an
    # unrooted base it is a logged no-op. This is what lets the shipped base be
    # rooted-yet-safe in production.
    _enforce_hiding(acct, label)
    # EVERY boot, production included: tell the kiosk which package is the
    # game and re-front it. Without this the kiosk's dev-mode fallback guesses,
    # and on a ROOTED base it guesses the Magisk manager — see
    # assert_kiosk_game for the full trace. This used to happen only inside
    # _devkit_activate, i.e. only on a --debug boot, which is exactly the
    # dev-base-era gating the dual-use change was supposed to remove.
    assert_kiosk_game(acct, cfg, label)
    # A debug boot ALSO stages the devkit toolkit (frida + omni-* tools) off
    # the vdc disk attached at spawn. `debug` is the per-boot flag resolved at
    # the top of this function.
    if debug:
        _devkit_activate(acct, label)
    # POST-BOOT TUNING, branched on the mode's PROFILE rather than on the raw
    # --mode string. That distinction is the bug fix: this used to compare
    # `mode_name`, the argument as typed, so a bare `omnidroid start` — which
    # resolves to `playable`, the DEFAULT mode — matched neither "gaming" nor
    # "farming" and received no tuning whatsoever. The most-used mode was the
    # only untuned one.
    profile = mode.get("profile", "performance")
    quality = quality or mode.get("quality")
    settings = lean.app_settings_for(quality)
    if quality and settings is None:
        print(f"[{label}] unknown quality profile '{quality}' — leaving "
              f"Roblox's own settings alone. Known: "
              f"{list(lean.QUALITY_PROFILES)}")
    if profile == "density":
        # FARMING. Every lever here trades quality and responsiveness for
        # instance COUNT, in a fixed order: settings, then zram (which decides
        # WHICH balloon cap is survivable), then the squeeze, then the balloon
        # strictly last so it only claims memory the guest has already given up.
        if settings is not None:
            apply_roblox_settings(acct, label, settings=settings)
        enable_zram(acct, mode, label)
        # The squeeze is a PRODUCTION memory optimization; a debug boot carries
        # the extra devkit/frida footprint and its numbers aren't the
        # production baseline, so skip it there (as the old dev path did).
        if not debug:
            apply_farming_squeeze(acct, mode)
        apply_balloon_target(acct, mode, label)
    else:
        # PERFORMANCE (playable / gaming / hard / brutal). No zram, no squeeze
        # and no balloon — each of those trades responsiveness for density,
        # which is the wrong direction here. The tune-up also REVERSES the
        # farming levers that persist in /data, so an offset that was last
        # touched by a farming boot does not carry a 480x270 display into a
        # playable one.
        if settings is not None:
            apply_roblox_settings(acct, label, settings=settings)
        if not debug:
            apply_gaming_tuning(acct, mode, label)
    return True, first


def _token_flag_given(args):
    """True iff a --token/--token-file/--token-stdin flag was explicitly
    passed, even if it resolves to an empty cookie (a blank file/stdin/arg).

    `is not None`, not truthiness: `--token ""` must still count as GIVEN.
    Used by `omnidroid login` to fail fast on an unusable token rather than
    silently falling back to the interactive browser flow (which would turn a
    supposedly-headless, few-second call into an up-to-5-minute wait for a
    visible browser sign-in that nobody is watching for)."""
    return (getattr(args, "token", None) is not None
           or getattr(args, "token_file", None) is not None
           or getattr(args, "token_stdin", False))


def _capture_and_save_account(args):
    """Shared account-registration core for `login` and the bare `create`: adopt
    a cookie (--token*) verified headlessly, or capture one via a real browser
    sign-in; save it under the auto-detected USERNAME. Returns (record, None) on
    success or (None, (error, message)) on failure — no printing/exit here."""
    from omnidroid import accounts as _acc
    token_requested = _token_flag_given(args)
    tok = resolve_token(args)
    if token_requested and not tok:
        # A --token* flag was GIVEN but resolved empty (blank file/stdin/arg).
        # Failing loudly beats silently swapping a headless call for a 5-minute
        # wait on a visible browser sign-in.
        return None, ("bad_token", "--token/--token-file/--token-stdin was "
                                   "given but resolved to an empty cookie")
    if tok:
        r = _acc.capture_login_from_cookie(_store_root(), tok,
                                          browser=args.browser)
    else:
        r = _acc.capture_login(_store_root(), browser=args.browser,
                              timeout=args.timeout,
                              profile_dir=getattr(args, "profile_dir", None))
    if not r.get("ok"):
        return None, (r.get("error", "login_failed"), r.get("message"))
    # Optional display-only alias (never the identity/instance name).
    alias = getattr(args, "alias", None)
    if alias:
        _acc.set_custom_name(_store_root(), r["username"], alias)
    return r, None


def cmd_login(args):
    """Save a Roblox account's session cookie, saved under its USERNAME.

    Two paths:
    - No --token*: capture a session cookie through a real browser login. You
      sign in (password/2FA/captcha stay between you and Roblox), and once the
      session is confirmed authenticated the account is saved.
    - --token/--token-file/--token-stdin: adopt a cookie you already have (e.g.
      exported from another browser/device) instead of signing in again. Loaded
      into a HEADLESS browser (no window) purely to prove it behaves like a
      real authenticated session before it is trusted and saved — the same bar
      an interactive login has to clear.

    Either way: ready to use as `omnidroid start <username> --place`."""
    r, err = _capture_and_save_account(args)
    if err:
        return fail(err[0], err[1])
    # NOTE: the cookie itself is deliberately absent from this output.
    out = {"ok": True, "username": r["username"], "user_id": r["user_id"],
           "custom_name": getattr(args, "alias", None) or None, "path": r["path"]}
    if getattr(args, "json", False):
        emit_json(out)
    else:
        print(json.dumps(out, indent=2))
        print(f"\nplay as this account:  omnidroid start {r['username']} "
              f"--place <placeId>")


def cmd_accounts(args):
    """List / verify / remove saved Roblox accounts. Never prints a cookie."""
    from omnidroid import accounts as _acc
    if getattr(args, "set_custom_name", None):
        username, custom = args.set_custom_name
        # Display-only label, separate from the username (the account's real
        # identity and the instance name — never changed by this).
        existed = _acc.set_custom_name(_store_root(), username, custom)
        if not existed:
            return fail("no_account", f"no saved account '{username}'")
        out = {"ok": True, "username": username, "custom_name": custom or None}
    elif getattr(args, "remove", None):
        existed = _acc.remove_account(_store_root(), args.remove)
        out = {"ok": True, "removed": args.remove, "existed": existed}
    else:
        accts = _acc.list_accounts(_store_root())
        if getattr(args, "verify", False):
            # A cookie dies when the account signs out or changes password, and
            # otherwise only surfaces as a login screen inside the VM minutes
            # later. One cheap call tells you now.
            for a in accts:
                rec = _acc.get_account(_store_root(), a["username"])
                uid, _uname = _acc.whoami((rec or {}).get("cookie") or "")
                a["valid"] = bool(uid)
        out = {"ok": True, "accounts": accts, "store": str(_acc.accounts_path(_store_root()))}
    if getattr(args, "json", False):
        emit_json(out)
    else:
        print(json.dumps(out, indent=2))


def cmd_session(args):
    """Inspect / set / clear an account's Roblox session (token + place),
    sourced from the central store, without launching.

    The token always comes from `omnidroid login`; this command only ever touches
    place_id. A --token* flag, if passed, is accepted but not persisted here
    — the store owns the cookie."""
    from omnidroid import accounts as _acc
    acct = load_account(args.name)
    name = args.name
    if getattr(args, "clear", False):
        _acc.set_fields(_store_root(), name, place_id=None)
        if running_pid(name) and kiosk_installed(acct):
            kiosk_broadcast(acct, KIOSK_ACTION_CLEAR_SESSION)
        out = {"name": name, "ok": True, "cleared": True}
    elif getattr(args, "place", None) is not None:
        try:
            _acc.set_fields(_store_root(), name, place_id=args.place)
        except ValueError as e:
            return fail("bad_place", str(e))
        sess = store_session(name)
        # A live instance picks the change up now; --play re-joins with it.
        applied = None
        if running_pid(name) and kiosk_installed(acct):
            applied = deliver_session(acct, f"session {name}", sess,
                                      play=bool(getattr(args, "play", False)))
        out = {"name": name, "ok": True, "session": public_session(sess),
               "applied": applied}
    else:
        out = {"name": name, "ok": True,
               "session": public_session(store_session(name))}
    if getattr(args, "json", False):
        emit_json(out)
    else:
        print(json.dumps(out, indent=2))


def cmd_adb(args):
    acct = load_account(args.name)
    adb_connect(acct)          # same reason as cmd_install: connect first
    r = adb(acct, *args.rest, timeout=120)
    sys.stdout.write(r.stdout)
    sys.stderr.write(r.stderr)
    sys.exit(r.returncode)


# --------------------------------------------------------- debugging surface
#
# Everything below exists so that a person or an AI can do high-level
# debugging through ONE documented command surface, instead of each caller
# re-deriving the guest's quirks and getting them subtly wrong. The three
# quirks that keep biting, all of them already paid for once:
#
#   1. `su` is not on $PATH. Magisk's su lives in its own tmpfs
#      (/debug_ramdisk/su); a bare `su` fails with "inaccessible or not found".
#   2. MagiskSU PERMUTES argv, so `su 0 id -u` is read as an su OPTION and
#      exits 2. Everything must go through `su 0 sh -c '<script>'`.
#   3. `adb shell` does not forward argv — it JOINS the arguments and re-parses
#      them in the guest shell, so an unquoted `a; b` silently runs a fragment
#      of itself and reports success (see farming.sh for the measured case).
#
# cmd_su gets all three right in one place. Nothing else should hand-roll it.

def cmd_su(args):
    """Run a command as ROOT in the guest, correctly quoted.

        omnidroid su erin7231 -- id -u
        omnidroid su erin7231 -- 'pm list packages | grep roblox'
        omnidroid su erin7231 --json -- getprop ro.build.fingerprint

    Exits with the guest command's own status, so it composes in scripts. A
    base without working root fails with `no_root` and the exact fix rather
    than running the command as uid shell and quietly producing wrong output —
    silent privilege downgrade is how a debugging session reaches a false
    conclusion."""
    rest = list(args.rest or [])
    # argparse's REMAINDER swallows EVERYTHING after the account name, flags
    # included — so `omnidroid su bob --json -- id` would run the literal
    # command "--json -- id" as root and report whatever that produced. Say so
    # loudly instead: a silently-mangled root command is exactly the class of
    # failure this command exists to eliminate.
    if rest and rest[0].startswith("-"):
        return fail("bad_args",
                    f"omnidroid's own flags must come BEFORE the account name "
                    f"(argparse stops parsing them at the first positional). "
                    f"Write: omnidroid su {rest[0]} ... {args.name} -- "
                    f"<command>")
    script = " ".join(rest) if len(rest) > 1 else (rest[0] if rest else "")
    if not script.strip():
        return fail("bad_args",
                    "nothing to run. Usage: omnidroid su [--json] [--timeout N]"
                    " <name> -- <command>")
    # Argument validation FIRST, then the instance: a malformed command line is
    # the caller's mistake and should be reported as such, not masked by
    # whatever the account lookup happens to say.
    acct = load_account(args.name)
    adb_connect(acct)
    su = resolve_su(acct)
    if not su:
        return fail("no_root",
                    f"Magisk root is not available on '{args.name}'. The base "
                    f"must be rooted (`omnidroid root-base`) and the instance "
                    f"restarted. Note this is NOT `adb root` — the arm base is "
                    f"a LineageOS 'user' build, so root comes only from the "
                    f"Magisk-patched boot.")
    r = adb(acct, "shell", f"{su} 0 sh -c {shlex.quote(script)}",
            timeout=getattr(args, "timeout", 120))
    if getattr(args, "json", False):
        emit_json({"ok": r.returncode == 0, "su": su, "command": script,
                   "exit_code": r.returncode,
                   "stdout": r.stdout, "stderr": r.stderr})
    else:
        sys.stdout.write(r.stdout or "")
        sys.stderr.write(r.stderr or "")
    sys.exit(r.returncode)


def _frida_status(acct, su, guest_port):
    """(running, detail) for the hidden frida-server inside the guest.

    Probes the PORT, not the process name: the devkit deliberately runs
    frida-server under a randomized name on a non-standard loopback port so a
    naive scan misses it — which means a name-based check here would miss it
    too and report "not running" at every call."""
    try:
        r = adb(acct, "shell",
                f"{su} 0 sh -c "
                f"{shlex.quote(f'netstat -lnt 2>/dev/null | grep :{guest_port}')}",
                timeout=20)
        out = (r.stdout or "").strip()
        return bool(out), out
    except Exception as e:  # noqa: BLE001 — a probe must never fail the command
        return False, f"probe failed: {type(e).__name__}"


def cmd_frida(args):
    """Start / inspect / stop the guest's hidden frida-server and forward it
    to a host port you can attach to.

        omnidroid start erin7231 --debug        # frida lives on the devkit disk
        omnidroid frida erin7231                # start + forward, prints -H target
        omnidroid frida erin7231 --status
        omnidroid frida erin7231 --stop

    Requires a DEBUG boot (the devkit disk carries frida-server) on a rooted
    base. Both preconditions are checked and named separately, because "not a
    debug boot" and "debug boot but not rooted" need different fixes and the
    generic message that conflated them sent people to the wrong one."""
    acct = load_account(args.name)
    adb_connect(acct)
    guest_port = getattr(args, "port", None) or DEFAULT_FRIDA_PORT
    su = resolve_su(acct)
    if not su:
        return fail("no_root",
                    f"'{args.name}' has no Magisk root, so frida-server "
                    f"cannot be started. Root the base once with `omnidroid "
                    f"root-base`, then restart the instance.")
    # The devkit disk is what CARRIES frida-server; it is attached only on a
    # --debug boot. Distinguish "no disk" from "disk present, not staged".
    has_disk = "vdc" in (adb(acct, "shell", "ls", "/dev/block/vdc",
                             timeout=10).stdout or "")
    if not has_disk:
        return fail("no_devkit",
                    f"'{args.name}' was not booted with --debug, so the devkit "
                    f"disk (frida-server + omni-* tools) is not attached. "
                    f"Restart it: `omnidroid stop {args.name} && omnidroid "
                    f"start {args.name} --debug`. If the disk itself has never "
                    f"been built: `omnidroid build-devkit`.")
    if getattr(args, "stop", False):
        adb(acct, "shell",
            f"{su} 0 sh -c {shlex.quote('pkill -f frida || true')}", timeout=30)
        subprocess.run(["adb", "-s", f"127.0.0.1:{acct['adb_port']}",
                        "forward", "--remove-all"],
                       capture_output=True, text=True, timeout=15)
        print(f"[frida {args.name}] stopped and forwards removed")
        if getattr(args, "json", False):
            emit_json({"ok": True, "running": False})
        return
    if not getattr(args, "status", False):
        # Idempotent: omni-fridad no-ops when a server is already up.
        _devkit_activate(acct, f"frida {args.name}")
        adb(acct, "shell", f"{su} 0 sh -c {shlex.quote(DEVKIT_WORK + '/omni-fridad')}",
            timeout=60)
    running, detail = _frida_status(acct, su, guest_port)
    host_port = None
    if running:
        # tcp:0 asks adb to allocate a free host port and print it. The guest
        # port is loopback-INSIDE the VM, so the forward is not a convenience —
        # without it there is nothing on the host to attach to.
        fwd = subprocess.run(["adb", "-s", f"127.0.0.1:{acct['adb_port']}",
                              "forward", "tcp:0", f"tcp:{guest_port}"],
                             capture_output=True, text=True, timeout=20)
        cand = (fwd.stdout or "").strip()
        host_port = int(cand) if cand.isdigit() else guest_port
        if not cand.isdigit():
            subprocess.run(["adb", "-s", f"127.0.0.1:{acct['adb_port']}",
                            "forward", f"tcp:{host_port}",
                            f"tcp:{guest_port}"],
                           capture_output=True, text=True, timeout=20)
    rep = {"ok": bool(running), "running": bool(running),
           "guest_port": guest_port, "host_port": host_port,
           "attach": f"frida -H 127.0.0.1:{host_port}" if host_port else None,
           "detail": detail}
    if getattr(args, "json", False):
        emit_json(rep)
    elif running:
        print(f"[frida {args.name}] up on guest 127.0.0.1:{guest_port} "
              f"(hidden name/port)\n"
              f"  attach from the host:  frida -H 127.0.0.1:{host_port}\n"
              f"  hide root from an app: omnidroid su {args.name} -- "
              f"{DEVKIT_WORK}/omni-hide <package>")
    else:
        print(f"[frida {args.name}] NOT running (guest port {guest_port} has "
              f"no listener). Try `omnidroid frida {args.name}` without "
              f"--status to start it.")
    if not running and getattr(args, "status", False):
        sys.exit(1)


def cmd_debug_info(args):
    """One JSON blob answering 'what can I actually do to this instance?'.

    The command an AI should call FIRST when a debugging step fails, because
    every capability below has a different precondition and the failures look
    alike from the outside: no root and no devkit both present as "the tool
    did nothing". Reports what IS true rather than what was requested."""
    acct = load_account(args.name)
    if not running_pid(args.name):
        return fail("not_running",
                    f"'{args.name}' is not running. Start it: `omnidroid "
                    f"start {args.name}` (add --debug for frida).")
    adb_connect(acct)
    cfg = read_config()
    base = (cfg.get("bases") or {}).get(acct["base"]) or {}
    su = resolve_su(acct)
    has_disk = "vdc" in (adb(acct, "shell", "ls", "/dev/block/vdc",
                             timeout=10).stdout or "")
    frida_running = False
    if su and has_disk:
        frida_running, _ = _frida_status(acct, su, DEFAULT_FRIDA_PORT)
    try:
        run = json.loads((runtime_dir(args.name) / "run.json").read_text())
    except Exception:  # noqa: BLE001
        run = {}
    fg = _foreground(acct)
    rep = {
        "ok": True,
        "name": args.name,
        "base": acct["base"],
        "arch": acct_arch(acct),
        "mode": run.get("mode"),
        "debug_boot": bool(run.get("debug")),
        "offset": run.get("offset"),
        "offset_default": offsets_mod.default_offset_name(base),
        "offsets_available": list(offsets_mod.offsets_of(base)),
        "adb_serial": f"127.0.0.1:{acct['adb_port']}",
        "vnc": f"127.0.0.1:{acct['vnc_port']}",
        "qmp_port": acct.get("qmp_port"),
        "game_package": resolve_game_package(acct, cfg),
        "foreground": fg,
        "root": {"available": bool(su), "su": su,
                 "fix": None if su else "omnidroid root-base, then restart"},
        "devkit": {"attached": has_disk, "mount": DEVKIT_MOUNT,
                   "tools": DEVKIT_WORK,
                   "fix": None if has_disk
                   else f"restart with: omnidroid start {args.name} --debug"},
        "frida": {"running": frida_running, "guest_port": DEFAULT_FRIDA_PORT,
                  "start": f"omnidroid frida {args.name}"},
        "can": {
            "screenshot": True,
            "logcat": True,
            "install_apk": True,
            "run_su": bool(su),
            "frida": bool(su and has_disk),
            "hide_root": bool(su and has_disk),
        },
    }
    if getattr(args, "json", False):
        emit_json(rep)
    else:
        print(json.dumps(rep, indent=2))


def build_parser():
    """The full argparse parser, with every subcommand registered.

    Extracted from main() so the command list can be DERIVED rather
    than hand-maintained. cmd_version advertises this engine's
    subcommands over the client contract, and the literal it used to
    carry had drifted: it advertised `create` (removed from the
    engine) while omitting `setup`, `login` and `view` -- the three
    calls omni-executor actually makes. A client honouring that
    contract would invoke a dead command and refuse three live ones.
    Deriving it from the parser makes that class of drift impossible.
    """
    p = argparse.ArgumentParser(prog="omnidroid")
    sub = p.add_subparsers(dest="cmd", required=True)

    def _token_args(parser):
        # No --account flag: the positional IS the account username (from
        # `omnidroid login`), so its cookie is looked up automatically. These flags
        # are a manual OVERRIDE for a raw cookie (testing / an account you have
        # not saved).
        g = parser.add_mutually_exclusive_group()
        g.add_argument("--token", default=None,
                       help="override: a raw Roblox .ROBLOSECURITY cookie. "
                            "Prefer --token-file — an argv token is visible to "
                            "every process on this host while the command runs")
        g.add_argument("--token-file", dest="token_file", default=None,
                       help="override: file containing the .ROBLOSECURITY cookie")
        g.add_argument("--token-stdin", dest="token_stdin", action="store_true",
                       help="override: read the .ROBLOSECURITY cookie from stdin")

    vr = sub.add_parser("version",
                        help="print the engine + contract handshake "
                             "(omnidroid-api.md v1 §4)")
    vr.add_argument("--json", action="store_true")
    vr.set_defaults(func=cmd_version)

    s = sub.add_parser("start",
                       help="launch an instance for a saved account: boot, "
                            "deliver its Roblox session and land INSIDE a "
                            "place if one is set, or on the account's home "
                            "screen if not — logged in, no menu, no taps. "
                            "`omnidroid start <username> [--place <id>]`. The "
                            "instance is auto-created (ephemeral); run two "
                            "for two accounts at once. Dev and production "
                            "alike")
    s.add_argument("name", metavar="username",
                   help="a saved Roblox account username (from `omnidroid login`). "
                        "It names the instance too — its cookie is used "
                        "automatically, no --account needed")
    s.add_argument("--place", default=None,
                   help="Roblox placeId to join (persisted; reused next "
                        "time). If omitted, boots to the account's home "
                        "screen, logged in but not joined to any place")
    _token_args(s)
    s.add_argument("--job", default=None,
                   help="gameInstanceId (JobId) to join a SPECIFIC server")
    s.add_argument("--access-code", dest="access_code", default=None,
                   help="private server accessCode")
    s.add_argument("--link-code", dest="link_code", default=None,
                   help="private server linkCode")
    s.add_argument("--launch-data", dest="launch_data", default=None,
                   help="launchData string (<=200 bytes decoded), readable "
                        "in-game via Player:GetJoinData()")
    s.add_argument("--user-id", dest="user_id", type=int, default=None,
                   help="informational: which Roblox user the token belongs to")
    s.add_argument("--no-token", dest="no_token", action="store_true",
                   help="proceed without a login (lands on Roblox's login "
                        "screen, which needs taps — almost never what you want)")
    s.add_argument("--no-cookie-check", dest="no_cookie_check",
                   action="store_true",
                   help="skip the pre-boot check that asks Roblox whether the "
                        "saved cookie is still valid (the check fails open on "
                        "network problems, so you rarely need this)")
    s.add_argument("--debug", action="store_true",
                   help="attach the devkit disk (frida + omni-* tools) for "
                        "reverse-engineering. The base is the same dual-use "
                        "production image either way; this just adds the "
                        "toolkit (vdc). Also settable via OMNI_DEBUG_BOOT=1.")
    s.add_argument("--no-warm", dest="no_warm", action="store_true",
                   help="disable the warm-restore boot cache for THIS launch "
                        "(no restore, no bake) -- always cold-boot. Kill "
                        "switch; also settable for every launch on the host "
                        "via OMNI_NO_WARM=1.")
    s.add_argument("--apk", default=None,
                   help="install this Roblox APK before delivering the session "
                        "(for testing a custom build). Works on any base; does "
                        "NOT require --debug. Implies a clean boot: the APK "
                        "IS the version, so no offset is required.")
    s_off = s.add_mutually_exclusive_group()
    s_off.add_argument("--offset", default=None,
                       help="which BAKED Roblox version to boot (see "
                            "`omnidroid offset list`). Omit to use the base's "
                            "DEFAULT offset — that is what a bare "
                            "`omnidroid start <username>` means. Offsets are "
                            "per-LAUNCH, never per-account.")
    s_off.add_argument("--no-offset", dest="no_offset", action="store_true",
                       help="boot the clean base with NO Roblox baked in "
                            "(same as `--offset none`). For base work, or "
                            "alongside --apk.")
    s_win = s.add_mutually_exclusive_group()
    s_win.add_argument("--window", action="store_true",
                       help="open a live window even in --json mode (two "
                            "starts = two accounts side by side)")
    s_win.add_argument("--no-window", dest="no_window", action="store_true",
                       help="do not open a window (headless; watch via "
                            "`omnidroid view` or capture)")
    s.add_argument("--mode", choices=list(MODES), default=None,
                   help="what this instance is FOR. playable (DEFAULT) - "
                        "MAXIMUM resources: sized to this host (up to 8G / 6 "
                        "vCPU), no balloon, no squeeze, native resolution, "
                        "high-quality render, game on the top-app cpuset. The "
                        "mode to test and to play in. gaming - the same, plus "
                        "a native window on the host. farming - MINIMUM "
                        "resources: 2G/1c headless, joined-idle, squeezed "
                        "post-boot then ballooned to ~896 MB, for many "
                        "instances at once. hard 3G/4c | brutal 2G/2c are "
                        "fixed smaller tiers for a tight host")
    s.add_argument("--mem", type=int, default=None,
                   help="override guest RAM in MB. This is the guest's "
                        "ADDRESS SPACE, not its host footprint - see "
                        "--balloon for the number the host pays. Wins over "
                        "the host-derived size in playable/gaming")
    s.add_argument("--smp", type=int, default=None,
                   help="override guest vCPU count. Wins over the "
                        "host-derived count in playable/gaming")
    s.add_argument("--quality", choices=list(lean.QUALITY_PROFILES),
                   default=None,
                   help="Roblox render profile. high (playable/gaming "
                        "default): real textures/lighting/post-FX - what a "
                        "player sees, and what a screenshot must show to be "
                        "worth reasoning about. balanced: effects off, "
                        "maximum frame rate. low (farming default): 5 fps "
                        "cap, lowest everything")
    s.add_argument("--balloon", type=int, default=None,
                   help="post-boot balloon target in MB: the hard cap on "
                        "this instance's host memory. 0 disables it. "
                        "Default: the mode's own (farming 1024, others off)")
    s.add_argument("--accel", default=None,
                   help="override hypervisor (auto: Windows=whpx, "
                        "Linux=kvm). E.g. 'tcg' for a no-hypervisor test")
    s.add_argument("--timeout", type=int, default=None)
    s.add_argument("--json", action="store_true")
    s.set_defaults(func=cmd_start)

    st = sub.add_parser("stop",
                        help="power the instance OFF (adb shutdown -> QMP "
                             "quit -> kill). A viewer merely disconnecting "
                             "must NOT call this: instances keep running "
                             "headless by default")
    st.add_argument("name")
    st.add_argument("--timeout", type=int, default=90,
                    help="seconds to wait for graceful power-off before "
                         "escalating (default 90)")
    st.add_argument("--json", action="store_true")
    st.set_defaults(func=cmd_stop)

    rm = sub.add_parser("remove",
                        help="DESTRUCTIVE: stop if running, then delete the "
                             "account's store entry and wipe runtime/<name>/; "
                             "ports are freed. Also deletes a legacy "
                             "accounts/<name>/ folder if one exists (can only "
                             "ever delete inside accounts/)")
    rm.add_argument("name", help="exact account name (no globs)")
    rm.add_argument("--timeout", type=int, default=90,
                    help="seconds to wait for graceful power-off first")
    rm.add_argument("--json", action="store_true")
    rm.set_defaults(func=cmd_remove)

    l = sub.add_parser("list")
    l.add_argument("--stats", action="store_true")
    l.add_argument("--json", action="store_true",
                   help="JSON array of accounts on stdout")
    l.set_defaults(func=cmd_list)

    i = sub.add_parser("install",
                       help="install a game APK (ABI-safe: x86 accounts "
                            "default to --abi arm64-v8a so a fat APK "
                            "exercises libndk translation)")
    i.add_argument("name")
    i.add_argument("apk")
    i.add_argument("--abi", default=None,
                   help="force this ABI on install (default arm64-v8a on x86 "
                        "accounts; none on arm accounts)")
    i.add_argument("--no-abi-pin", dest="no_abi_pin", action="store_true",
                   help="do not pin an ABI (let Android select natively)")
    i.add_argument("--require-translation", dest="require_translation",
                   action="store_true",
                   help="fail (abi_not_translated) if the ARM translation "
                        "path was not actually exercised")
    i.add_argument("--json", action="store_true")
    i.set_defaults(func=cmd_install)

    w = sub.add_parser("watch")
    w.add_argument("name")
    w.add_argument("--package", default=None)
    w.add_argument("--grace", type=int, default=20,
                   help="seconds the game process must stay gone "
                        "before shutdown (default 20)")
    w.set_defaults(func=cmd_watch)

    bb = sub.add_parser("brand-base",
                        help="bake the Omni loading screen into an arm base "
                             "(replaces the LineageOS boot animation inside "
                             "super/product). BUILD-machine command: needs "
                             "e2fsprogs + ~6 GiB scratch")
    bb.add_argument("--base", default="arm", help="base tag (default: arm)")
    bb.add_argument("--animation", default=None,
                    help="bootanimation.zip to bake in (default: "
                         "assets/loading/bootanimation.zip)")
    bb.add_argument("--out", default=None,
                    help="output image (default: <base>_branded.qcow2)")
    bb.add_argument("--in-place", dest="in_place", action="store_true",
                    help="overwrite the base image itself, keeping a .bak. "
                         "Only safe when no account is running")
    bb.add_argument("--no-silent-boot", dest="no_silent_boot",
                    action="store_true",
                    help="leave the kernel console log visible on the boot "
                         "screen (grub.cfg is patched by default)")
    bb.add_argument("--json", action="store_true")
    bb.set_defaults(func=cmd_brand_base)

    ms = sub.add_parser("measure",
                        help="report what running instances actually cost the "
                             "host (median RSS over repeated samples, guest "
                             "used, balloon size) and how many fit")
    ms.add_argument("name", nargs="?", default=None,
                    help="one instance (default: every running instance)")
    ms.add_argument("--samples", type=int, default=8,
                    help="host-RSS samples per instance (default 8). More is "
                         "better: free-page-reporting makes RSS very spiky")
    ms.add_argument("--interval", type=int, default=5,
                    help="seconds between samples (default 5)")
    ms.add_argument("--json", action="store_true")
    ms.set_defaults(func=cmd_measure)

    ez = sub.add_parser("enable-zram-base",
                        help="bake a zram swap entry into a base image's "
                             "fstab so init brings zram up at boot. This is "
                             "how a NON-ROOTED production instance gets zram "
                             "- worth a third of the per-instance footprint "
                             "(2.97x compression measured). BUILD-machine "
                             "command: e2fsprogs + ~6 GiB scratch")
    ez.add_argument("--base", default=None,
                    help="base tag (default: this host's effective base)")
    ez.add_argument("--out", default=None,
                    help="output image (default: <base>_zram.qcow2)")
    ez.add_argument("--scratch-dir", dest="scratch_dir", default=None,
                    help="directory for the temporary raw export (default: "
                         "the images dir). Point this at an external drive "
                         "when the internal disk is full - it holds only "
                         "temporary state")
    ez.add_argument("--in-place", dest="in_place", action="store_true",
                    help="overwrite the base image itself, keeping a .bak")
    ez.add_argument("--json", action="store_true")
    ez.set_defaults(func=cmd_enable_zram_base)

    sb = sub.add_parser("strip-base",
                        help="bake the low-RAM property profile into an arm "
                             "base (ro.config.low_ram + lmkd/dalvik/hwui "
                             "tuning). These CANNOT be set at runtime - init "
                             "freezes ro.* - so this is the base-image half "
                             "of the footprint work. BUILD-machine command: "
                             "needs e2fsprogs + ~6 GiB scratch")
    sb.add_argument("--base", default=None,
                    help="base tag (default: this host's effective base)")
    sb.add_argument("--out", default=None,
                    help="output image (default: <base>_lean.qcow2)")
    sb.add_argument("--in-place", dest="in_place", action="store_true",
                    help="overwrite the base image itself, keeping a .bak. "
                         "Only safe when no account is running")
    sb.add_argument("--force-unverified", dest="force_unverified",
                    action="store_true",
                    help="bake the profile even though it is UNVERIFIED and "
                         "known to break boot on the arm base (recovery / "
                         "RescueParty). Only for bisecting. Without this the "
                         "command refuses.")
    sb.add_argument("--json", action="store_true")
    sb.set_defaults(func=cmd_strip_base)

    bg = sub.add_parser("bake-game",
                        help="LEGACY: bake a game APK into an arm SYSTEM image "
                             "(use `offset create` for versions). Still the "
                             "way to STRIP a baked game out: `bake-game "
                             "--remove` makes the system image ship no game "
                             "at all. BUILD-machine command (e2fsprogs + "
                             "~6 GiB scratch)")
    bg.add_argument("apk", nargs="?", default=None)
    bg.add_argument("--remove", action="store_true",
                    help="DELETE the baked system app instead of installing "
                         "one, so the base truly carries no Roblox. Every "
                         "version then comes from an offset")
    bg.add_argument("--base", default="arm", help="base tag (default: arm)")
    bg.add_argument("--image", default=None,
                    help="operate on this image instead of the base's "
                         "(e.g. the branded production candidate)")
    bg.add_argument("--name", default=None,
                    help="directory name under /product/app (default: "
                         "OmniGame when baking, Roblox when --remove)")
    bg.add_argument("--json", action="store_true")
    bg.set_defaults(func=cmd_bake_game)

    # ---- offsets: many baked Roblox versions on ONE clean base -------------
    off = sub.add_parser("offset",
                         help="manage BAKED ROBLOX VERSIONS ('offsets'). Each "
                              "offset is a named, thin /data overlay carrying "
                              "one Roblox build; the base itself stays clean "
                              "and one offset is the DEFAULT that a bare "
                              "`omnidroid start` boots. Add versions freely — "
                              "they are siblings, so a new one never replaces "
                              "or disturbs an existing one")
    offsub = off.add_subparsers(dest="offset_cmd", required=True)

    ol = offsub.add_parser("list", help="every baked version (* = default)")
    ol.add_argument("--base", default=None, help="base tag (default: current)")
    ol.add_argument("--json", action="store_true")
    ol.set_defaults(func=cmd_offset_list)

    oc = offsub.add_parser("create",
                           help="bake a Roblox APK into a NEW named version "
                                "(~2 min; no base rebuild, no scratch space)")
    oc.add_argument("name", nargs="?", default=None,
                    help="name for this version (default: the APK's own "
                         "versionName, e.g. 2.731.944)")
    oc.add_argument("--apk", required=True, help="the Roblox APK to bake")
    oc.add_argument("--base", default=None, help="base tag (default: current)")
    oc.add_argument("--package", default=None,
                    help="package to register as the game (default: the APK's "
                         "own, else base_game.<tag>)")
    oc.add_argument("--default", action="store_true",
                    help="also make this the default version for bare launches")
    oc.add_argument("--force", action="store_true",
                    help="re-bake over an offset of the same name")
    oc.add_argument("--notes", default=None, help="free-text note to record")
    oc.add_argument("--json", action="store_true")
    oc.set_defaults(func=cmd_offset_create)

    od = offsub.add_parser("default",
                           help="make one baked version the default for a "
                                "bare `omnidroid start`")
    od.add_argument("name")
    od.add_argument("--base", default=None, help="base tag (default: current)")
    od.add_argument("--json", action="store_true")
    od.set_defaults(func=cmd_offset_default)

    orm = offsub.add_parser("remove",
                            help="DESTRUCTIVE: delete a baked version and its "
                                 "overlay image (refuses while in use)")
    orm.add_argument("name")
    orm.add_argument("--base", default=None, help="base tag (default: current)")
    orm.add_argument("--keep-image", dest="keep_image", action="store_true",
                     help="unregister it but leave the qcow2 on disk")
    orm.add_argument("--json", action="store_true")
    orm.set_defaults(func=cmd_offset_remove)

    osh = offsub.add_parser("show", help="everything recorded about a version")
    osh.add_argument("name", nargs="?", default=None,
                     help="default: the default offset")
    osh.add_argument("--base", default=None, help="base tag (default: current)")
    osh.add_argument("--json", action="store_true")
    osh.set_defaults(func=cmd_offset_show)

    bdg = sub.add_parser("bake-data-game",
                         help="DEPRECATED alias for `omnidroid offset create "
                              "--default`. It baked into one fixed slot and "
                              "pointed the base at it; offsets keep the base "
                              "clean and let versions coexist")
    bdg.add_argument("apk", nargs="?", default=None,
                     help="game APK to bake as an offset")
    bdg.add_argument("--name", default=None,
                     help="offset name (default: the APK's versionName)")
    bdg.add_argument("--base", default=None, help="base tag (default: current)")
    bdg.add_argument("--package", default=None,
                     help="package name to register as the game (default: "
                          "base_game.<tag> from configs/paths.json). The APK is "
                          "NOT parsed for it — Roblox never changes its package "
                          "name and aapt2 is not installed everywhere.")
    bdg.add_argument("--notes", default=None)
    bdg.add_argument("--json", action="store_true")
    bdg.set_defaults(func=cmd_bake_data_game)

    k = sub.add_parser("kioskify")
    k.add_argument("name")
    k.add_argument("--apk", default=str(REPO / "launcher" / "build"
                                        / "omni-kiosk.apk"))
    k.set_defaults(func=cmd_kioskify)

    r = sub.add_parser("run-app")
    r.add_argument("name")
    r.add_argument("package")
    r.set_defaults(func=cmd_run_app)

    du = sub.add_parser("dev-ui",
                        help="switch a dev instance's visible UI between the kiosk "
                             "and the Magisk manager app")
    du.add_argument("name")
    du.add_argument("--show", choices=("kiosk", "magisk"), default="kiosk",
                    help="'kiosk' (default): foreground the kiosk + stop the Magisk "
                         "app. 'magisk': open the Magisk manager app (root UI).")
    du.add_argument("--json", action="store_true")
    du.set_defaults(func=cmd_dev_ui)

    lg = sub.add_parser("login",
                        help="sign in to a Roblox account. Default: opens a "
                             "VISIBLE browser for you to sign in. With "
                             "--token/--token-file/--token-stdin: adopt a "
                             "cookie you already have instead — verified in a "
                             "HEADLESS browser, no window, nothing to click. "
                             "Either way it saves the cookie under the "
                             "account's USERNAME (auto-detected). Then: "
                             "omnidroid start <username> --place <id>")
    lg.add_argument("--browser", choices=["chrome", "firefox"], default="chrome")
    lg.add_argument("--timeout", type=int, default=300,
                    help="interactive sign-in only: seconds to wait for you "
                         "to finish signing in (ignored with --token*, which "
                         "verifies headlessly in a few seconds)")
    lg.add_argument("--profile-dir", dest="profile_dir", default=None,
                    help="reuse a browser profile dir (keeps you signed in "
                         "between runs); interactive sign-in only")
    _token_args(lg)
    lg.add_argument("--json", action="store_true")
    lg.set_defaults(func=cmd_login)

    ac = sub.add_parser("accounts",
                        help="list/verify/remove saved Roblox accounts "
                             "(cookies are never printed)")
    ac.add_argument("--verify", action="store_true",
                    help="check each saved cookie still authenticates")
    ac.add_argument("--remove", default=None, help="delete a saved account")
    ac.add_argument("--set-custom-name", dest="set_custom_name", nargs=2,
                    metavar=("USERNAME", "NAME"),
                    help="attach a friendly custom_name to a saved account, "
                         "e.g. `omnidroid accounts --set-custom-name erin7231 "
                         "\"Farm 3\"`. Display-only: the USERNAME stays the "
                         "account's real identity and the instance name — "
                         "this never renames anything. Pass an empty string "
                         "to clear it")
    ac.add_argument("--json", action="store_true")
    ac.set_defaults(func=cmd_accounts)

    se = sub.add_parser("session",
                        help="inspect/set/clear an account's Roblox session "
                             "(token + place) without launching")
    se.add_argument("name")
    se.add_argument("--place", default=None)
    _token_args(se)
    se.add_argument("--job", default=None)
    se.add_argument("--launch-data", dest="launch_data", default=None)
    se.add_argument("--user-id", dest="user_id", type=int, default=None)
    se.add_argument("--play", action="store_true",
                    help="if the instance is running, re-join with the new "
                         "session immediately")
    se.add_argument("--clear", action="store_true",
                    help="forget the token+place, and log the live instance out")
    se.add_argument("--json", action="store_true")
    se.set_defaults(func=cmd_session)

    a = sub.add_parser("adb",
                       help="raw adb against one instance: "
                            "`omnidroid adb <name> -- shell ls /sdcard`")
    a.add_argument("name")
    a.add_argument("rest", nargs=argparse.REMAINDER)
    a.set_defaults(func=cmd_adb)

    suc = sub.add_parser("su",
                         help="run a command as ROOT in the guest, quoted "
                              "correctly: `omnidroid su <name> -- <command>`. "
                              "Handles Magisk's off-PATH su, its argv "
                              "permutation, and adb's argv re-parse - the "
                              "three traps that make hand-rolled root calls "
                              "silently no-op. NOTE: omnidroid's own flags go "
                              "BEFORE the name (everything after it is the "
                              "guest command)")
    # Flags declared before the positional so `--json`/`--timeout` are usable;
    # argparse's REMAINDER stops option parsing at the first positional, so
    # they must be TYPED before the name too. cmd_su detects and reports the
    # wrong order rather than running a mangled command as root.
    suc.add_argument("--timeout", type=int, default=120)
    suc.add_argument("--json", action="store_true")
    suc.add_argument("name")
    suc.add_argument("rest", nargs=argparse.REMAINDER)
    suc.set_defaults(func=cmd_su)

    fr = sub.add_parser("frida",
                        help="start/inspect/stop the guest's hidden "
                             "frida-server and forward it to a host port. "
                             "Needs a --debug boot (the devkit disk carries "
                             "frida) on a rooted base")
    fr.add_argument("name")
    fr_g = fr.add_mutually_exclusive_group()
    fr_g.add_argument("--status", action="store_true",
                      help="report only; exit 1 if it is not running")
    fr_g.add_argument("--stop", action="store_true",
                      help="stop the server and drop the host forwards")
    fr.add_argument("--port", type=int, default=None,
                    help=f"guest frida port (default {DEFAULT_FRIDA_PORT}, "
                         f"deliberately not the well-known 27042)")
    fr.add_argument("--json", action="store_true")
    fr.set_defaults(func=cmd_frida)

    di = sub.add_parser("debug-info",
                        help="what can actually be done to this instance: "
                             "root, devkit, frida, offset, mode, foreground "
                             "app, ports. The first call to make when a "
                             "debugging step failed for an unclear reason")
    di.add_argument("name")
    di.add_argument("--json", action="store_true")
    di.set_defaults(func=cmd_debug_info)

    ub = sub.add_parser("update-base",
                        help="migrate one account to a base (default: "
                             "current), keeping its data")
    ub.add_argument("name")
    ub.add_argument("--to", default=None, help="target base tag (e.g. v3)")
    ub.add_argument("--no-reprovision", action="store_true")
    ub.add_argument("--json", action="store_true")
    ub.set_defaults(func=cmd_update_base)

    rb = sub.add_parser("rebuild-base",
                        help="bake/replace the pre-installed game in a new "
                             "base version (production); ephemeral instances "
                             "pick it up on next boot")
    rb.add_argument("--game", required=True, help="path to the game APK")
    rb.set_defaults(func=cmd_rebuild_base)

    uk = sub.add_parser("update-kiosk",
                        help="ship a new kiosk launcher. x86: a new base "
                             "version (then update-all). arm (--base arm|dev): "
                             "refresh that base's /data template in place")
    uk.add_argument("--apk", default=str(REPO / "launcher" / "build"
                                         / "omni-kiosk.apk"))
    uk.add_argument("--base", default=None,
                    help="arm base tag whose /data template to refresh "
                         "(e.g. arm, dev). Omit for the legacy x86 flow")
    uk.add_argument("--json", action="store_true")
    uk.set_defaults(func=cmd_update_kiosk)

    bdk = sub.add_parser("build-devkit",
                         help="build the attachable devkit disk "
                              "(base_<arch>_devkit.qcow2 = frida-server + omni-* "
                              "tools + Magisk binaries). Attached as vdc only on "
                              "an `omnidroid start --debug` boot; changes no base")
    bdk.add_argument("--arch", choices=("arm", "x86"), default=None,
                     help="which arch's devkit to build (default: this host's)")
    bdk.add_argument("--frida-version", default=DEFAULT_FRIDA_VERSION,
                     dest="frida_version",
                     help=f"frida-server version to stage "
                          f"(default {DEFAULT_FRIDA_VERSION})")
    bdk.add_argument("--frida-port", type=int, default=DEFAULT_FRIDA_PORT,
                     dest="frida_port",
                     help=f"hidden frida-server loopback port "
                          f"(default {DEFAULT_FRIDA_PORT}, deliberately not 27042)")
    bdk.add_argument("--no-magisk", action="store_true", dest="no_magisk",
                     help="skip staging the Magisk binaries")
    bdk.add_argument("--json", action="store_true")
    bdk.set_defaults(func=cmd_build_devkit)

    rbb = sub.add_parser("root-base",
                         help="make a shipped base DUAL-USE by baking a "
                              "Magisk-patched (rooted) boot into a thin overlay "
                              "of it, and a matched rooted /data. Same production "
                              "image, root simply baked in + hidden every boot. "
                              "Brick-risky (edits a boot partition)")
    rbb.add_argument("--base", default=None,
                     help="base tag to root (default: the host's effective base)")
    rbb.add_argument("--frida-version", default=DEFAULT_FRIDA_VERSION,
                     dest="frida_version",
                     help=f"frida-server version to stage into the toolset "
                          f"(default {DEFAULT_FRIDA_VERSION})")
    rbb.add_argument("--frida-port", type=int, default=DEFAULT_FRIDA_PORT,
                     dest="frida_port", help="hidden frida-server loopback port")
    rbb.add_argument("--json", action="store_true")
    rbb.set_defaults(func=cmd_root_base)

    su = sub.add_parser("setup",
                        help="first-run setup: folders + QEMU (Windows: "
                             "portable download, self-contained; Linux: "
                             "system QEMU preflight). Idempotent")
    su.set_defaults(func=cmd_setup)

    qi = sub.add_parser("qemu-info",
                        help="show resolved QEMU path / install if missing")
    qi.add_argument("--install", action="store_true")
    qi.set_defaults(func=cmd_qemu_info)

    dr = sub.add_parser("doctor",
                        help="readiness check: exact base/template files "
                             "present in images_dir, QEMU/adb resolvable. "
                             "Exit 0 = ready, 1 = not (says what to copy "
                             "where)")
    dr.add_argument("--json", action="store_true")
    dr.set_defaults(func=cmd_doctor)

    km = sub.add_parser("ksm",
                        help="Linux KSM status/on/off (clean no-op on "
                             "Windows)")
    km.add_argument("action", nargs="?", default="status",
                    choices=["status", "on", "off"])
    km.add_argument("--aggressive", action="store_true",
                    help="with 'on': pages_to_scan=1000, sleep=20ms "
                         "(faster dedup, more ksmd CPU)")
    km.set_defaults(func=cmd_ksm)

    bk = sub.add_parser("bench-ksm",
                        help="Linux-only: measure real instances-per-GB "
                             "with KSM (Phase 8; scaffold until the Linux "
                             "host exists)")
    bk.add_argument("--mode", choices=list(MODES), default="brutal")
    bk.add_argument("--max", type=int, default=99,
                    help="safety bound on bench steps (the RAM floor is "
                         "what actually stops the bench)")
    bk.add_argument("--floor-mb", type=int, default=2048, dest="floor_mb",
                    help="stop when host MemAvailable drops below this")
    bk.add_argument("--settle-secs", type=int, default=60,
                    dest="settle_secs",
                    help="KSM pages_sharing must be stable this long "
                         "before measuring")
    bk.add_argument("--apk", default=None,
                    help="override game APK to install fresh into each "
                         "instance instead of measuring the baked-in "
                         "Roblox workload")
    bk.add_argument("--offset", default=None,
                    help="which baked Roblox version to bench (default: the "
                         "base's default offset)")
    bk.add_argument("--prefix", default="bench",
                    help="bench account name prefix")
    bk.add_argument("--keep", action="store_true",
                    help="leave bench instances running afterwards")
    bk.set_defaults(func=cmd_bench_ksm)

    bs = sub.add_parser("bases", help="list registered bases + current")
    bs.add_argument("--json", action="store_true")
    bs.set_defaults(func=cmd_bases)

    ubz = sub.add_parser("use-base",
                         help="set default base for new accounts (dev vs "
                              "production mode switch)")
    ubz.add_argument("tag")
    ubz.set_defaults(func=cmd_use_base)

    vw = sub.add_parser("view",
                        help="open a LIVE VNC window (real-time screen + "
                             "mouse/keyboard) onto an instance, from the "
                             "terminal. macOS uses built-in Screen Sharing")
    vw.add_argument("name")
    vw.add_argument("--start", action="store_true",
                    help="boot the instance first if it isn't running")
    vw.add_argument("--mode", choices=list(MODES), default=None,
                    help="mode to use when --start boots the instance")
    vw.add_argument("--debug", action="store_true",
                    help="with --start: attach the devkit disk (frida + tools)")
    vw.add_argument("--offset", default=None,
                    help="with --start: which baked Roblox version to boot "
                         "(default: the base's default offset)")
    vw.add_argument("--native", action="store_true",
                    help="use the OS/native VNC client instead of the "
                         "built-in cross-platform viewer")
    vw.add_argument("--viewer", default=None,
                    help="native VNC client launch template (implies "
                         "--native), e.g. 'vncviewer {host}::{port}' or a "
                         "viewer path with {url}. Overrides config "
                         "qemu.vnc_viewer")
    vw.add_argument("--timeout", type=int, default=NORMAL_BOOT_TIMEOUT,
                    help="seconds to wait for the VNC port when --start")
    vw.add_argument("--json", action="store_true")
    vw.set_defaults(func=cmd_view)

    # Hidden internal: run the built-in Tk+RFB viewer in-process (spawned by
    # `omnidroid view` as a detached child). Not for direct use.
    vv = sub.add_parser("_vncview")
    vv.add_argument("--host", default="127.0.0.1")
    vv.add_argument("--port", type=int, required=True)
    vv.add_argument("--title", default=None)
    vv.set_defaults(func=lambda a: sys.exit(_run_vncview(a)))

    sc = sub.add_parser("screenshot", help="pull a screenshot (JSON out)")
    sc.add_argument("name")
    sc.add_argument("--out", default=None)
    sc.set_defaults(func=cmd_screenshot)

    lc = sub.add_parser("logcat", help="read/clear guest logcat")
    lc.add_argument("name")
    lc.add_argument("--tag", default=None, help="filter to a log tag")
    lc.add_argument("--clear", action="store_true")
    lc.add_argument("--timeout", type=int, default=30)
    lc.set_defaults(func=cmd_logcat)

    cap = sub.add_parser("capture",
                         help="millisecond-precise keyframe capture from the "
                              "VNC framebuffer + process/logcat diagnostics")
    cap.add_argument("name")
    cap.add_argument("--out", default=None,
                     help="output dir for keyframes + metadata.json + "
                          "logcat.txt (default runtime/<name>/capture-<ts>)")
    cap.add_argument("--duration", type=float, default=20.0,
                     help="seconds to observe the screen (bounded mode)")
    cap.add_argument("--auto", action="store_true",
                     help="continuous auto-screenshot mode (DEV BASE ONLY): "
                          "observe indefinitely, drop a keyframe on every big "
                          "change, flush metadata.json live; stop via a STOP "
                          "file in --out, --max-seconds, a signal, or shutdown")
    cap.add_argument("--max-seconds", dest="max_seconds", type=float, default=0.0,
                     help="auto mode safety cap in seconds (0 = no cap; run "
                          "until STOP/signal/instance shutdown)")
    cap.add_argument("--package", default=None,
                     help="app package to track (start/exit/crash); default "
                          "the account's kiosk game_package")
    # Detection defaults live in ONE place (capture.DEFAULT_*, which documents
    # the measured spinner-vs-popup separation they are derived from). These
    # stay None so an unpassed flag inherits the module default instead of
    # pinning a second copy of the number here that silently overrides it.
    cap.add_argument("--sample-scale-w", dest="sample_scale_w", type=int,
                     default=None, help="downscale width for change detection")
    cap.add_argument("--change-percent", dest="change_percent", type=float,
                     default=None, help="%% of changed pixels => a scene change")
    cap.add_argument("--change-threshold", dest="change_threshold", type=float,
                     default=None, help="mean abs delta (0-255) => a scene change")
    cap.add_argument("--black-threshold", dest="black_threshold", type=float,
                     default=None, help="mean brightness below => black frame")
    cap.add_argument("--max-keyframes", dest="max_keyframes", type=int, default=0,
                     help="cap on saved keyframes (0 = default: 240 bounded / "
                          "5000 for --auto)")
    cap.add_argument("--json", action="store_true")
    cap.set_defaults(func=cmd_capture)

    aco = sub.add_parser("autocap",
                         help="control the always-on dev auto-screenshot "
                              "recorder (auto-starts on a dev boot; this is for "
                              "explicit ensure/status/stop)")
    aco.add_argument("name")
    aco_g = aco.add_mutually_exclusive_group()
    aco_g.add_argument("--ensure", action="store_true",
                       help="start the recorder if not already running "
                            "(idempotent; the default action)")
    aco_g.add_argument("--stop", action="store_true",
                       help="stop the recorder for this account")
    aco_g.add_argument("--status", action="store_true",
                       help="report whether a recorder is running + its out dir")
    aco.add_argument("--restart", action="store_true",
                     help="force a fresh recorder (e.g. to repoint --out)")
    aco.add_argument("--out", default=None,
                     help="output dir (default $OMNI_AUTOCAP_DIR or "
                          "runtime/<name>/autocap)")
    aco.add_argument("--json", action="store_true")
    aco.set_defaults(func=cmd_autocap)

    ta = sub.add_parser("test-apk",
                        help="dev harness: fresh session, install+launch an "
                             "APK, report JSON (headless, scriptable)")
    ta.add_argument("name")
    ta.add_argument("--apk", required=True)
    ta.add_argument("--mode", choices=list(MODES), default="hard")
    ta.add_argument("--abi", default=None,
                    help="force this ABI on install (default arm64-v8a on "
                         "x86 accounts; none on arm accounts)")
    ta.add_argument("--no-abi-pin", dest="no_abi_pin", action="store_true",
                    help="do not pin an ABI (native selection)")
    ta.add_argument("--require-translation", dest="require_translation",
                    action="store_true",
                    help="fail (abi_not_translated) if the ARM translation "
                         "path was not exercised")
    ta.add_argument("--reuse", action="store_true",
                    help="reuse the account if it already exists")
    ta.set_defaults(func=cmd_test_apk)

    return p


def main():
    p = build_parser()
    args = p.parse_args()
    if getattr(args, "json", False):
        enable_json_mode()
    try:
        args.func(args)
    except SystemExit as e:
        # In --json mode even fatal errors are machine-readable: emit
        # {ok:false, error} on stdout, keep the message on stderr, exit 1.
        if getattr(args, "json", False) and isinstance(e.code, str):
            print(e.code)                       # -> stderr in json mode
            emit_json({"ok": False, "error": e.code})
            sys.exit(1)
        raise


if __name__ == "__main__":
    main()
