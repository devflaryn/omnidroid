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
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

from omnidroid import config
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


# Linux KSM (kernel samepage merging) sysfs interface. Dedups identical
# guest RAM pages across instances (same immutable base => big overlap).
KSM_DIR = Path("/sys/kernel/mm/ksm")
PAGE_SIZE = 4096

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
BASE_TYPE_X86 = "x86-bliss"
BASE_TYPE_ARM = "arm-uefi"


def base_type(base):
    return base.get("type", BASE_TYPE_X86)


def acct_base_is_arm(acct):
    """True if this account's base is arm-uefi (reads config; safe/cheap)."""
    try:
        b = (read_config().get("bases") or {}).get(acct.get("base"), {})
        return base_type(b) == BASE_TYPE_ARM
    except Exception:
        return False


def base_is_dev(base):
    """True if a base entry carries the dev devkit disk (frida + Magisk tools).
    The dev base is arm-uefi + a 'devkit' field naming the extra vdc disk."""
    return bool(base.get("devkit"))


# ---------- dev-mode gate ----------
# Dev mode (frida + Magisk root + always-on screenshots) is a DEVELOPER
# capability, not a product feature: customers must not be able to reach it. The
# strongest guard is that the dev images are never shipped, but that is not
# enough on its own — `bases` and `use-base` are generic over whatever is
# registered, so on a workstation where the dev base EXISTS the product GUI
# (omni-executor calls exactly those two commands) would happily list "dev" and
# let a click switch to it.
#
# So dev bases are invisible and unselectable unless the caller opts in via
# OMNI_DEV_MODE=1. omni-agent — the only thing that should ever boot dev — sets
# it explicitly (see android_emulator._omni_env). A customer running the shipped
# product never has it set, so for them the dev base does not exist even if its
# images somehow do.
DEV_MODE_ENV = "OMNI_DEV_MODE"


def dev_mode_enabled():
    return str(os.environ.get(DEV_MODE_ENV, "")).strip().lower() in (
        "1", "true", "yes", "on")


def _truthy_env(name):
    return str(os.environ.get(name, "")).strip().lower() in (
        "1", "true", "yes", "on")


def _dev_mode_for_play(args):
    """Whether `omni start` should target the DEV base for a NEW instance.

    This is SELECTION (use dev), which is distinct from ACCESS (may use dev,
    i.e. OMNI_DEV_MODE / dev_mode_enabled). The agent sets OMNI_DEV_MODE=1 just to
    UNLOCK the dev base, but still runs production by default — so dev selection
    must NOT be implied by OMNI_DEV_MODE, only by an explicit --dev or the
    dedicated OMNI_USE_DEV_BASE 'default to dev' env. assert_dev_allowed still
    refuses dev to a caller that has not unlocked it."""
    if getattr(args, "dev", False):
        return True
    return _truthy_env("OMNI_USE_DEV_BASE")


def visible_bases(cfg_or_raw):
    """The bases this caller is allowed to see: everything, minus dev bases when
    dev mode is off."""
    bases = cfg_or_raw.get("bases") or {}
    if dev_mode_enabled():
        return dict(bases)
    return {t: b for t, b in bases.items() if not base_is_dev(b)}


def assert_dev_allowed(tag, base):
    """Refuse a dev base to a caller that has not opted in."""
    if base_is_dev(base) and not dev_mode_enabled():
        fail("dev_base_locked",
             f"base '{tag}' is a development base (frida/Magisk root) and is "
             f"not available in this build. It is unlocked only for the "
             f"omni-agent devtool ({DEV_MODE_ENV}=1).")


def acct_is_dev(acct):
    """True if this account is a dev account: either it was created from a dev
    base (its base entry has a 'devkit') or it carries an explicit dev flag.
    Reads config; safe/cheap. Dev accounts get the devkit disk attached as vdc
    and the frida/Magisk activation on start."""
    if acct.get("dev"):
        return True
    try:
        b = (read_config().get("bases") or {}).get(acct.get("base"), {})
        return base_is_dev(b)
    except Exception:
        return False


# ---------- canonical arch tokens (contract omnidroid-api.md v1 §2) ----------
# The frozen arch enum both clients code against is "x86" | "arm". It maps
# base type x86-bliss->"x86", arm-uefi->"arm"; host amd64/x86_64->"x86",
# arm64/aarch64->"arm".
def arch_of_base(base):
    """Canonical arch token for a base entry: 'x86' | 'arm'."""
    return "arm" if base_type(base) == BASE_TYPE_ARM else "x86"


def acct_arch(acct):
    """Canonical arch token for an account: 'x86' | 'arm'."""
    return "arm" if acct_base_is_arm(acct) else "x86"


def host_arch_token():
    """Canonical arch token for THIS host: 'arm' on arm64, else 'x86'."""
    return "arm" if IS_ARM64_HOST else "x86"


# arm-uefi base default filenames in images_dir (a future downloaded base
# can override any of these in its config entry).
ARM_BASE_DISK = "base_arm.qcow2"            # pristine system, shared backing
ARM_BASE_SYSTEM = "base_arm_system.qcow2"   # provisioned overlay (FBE keys)
ARM_BASE_DATA = "base_arm_data.qcow2"       # provisioned /data (kiosk + DO)
ARM_BASE_EFIVARS = "base_arm_efivars.fd"    # provisioned UEFI vars
ARM_BASE_TAG = "arm"

# x86-bliss base canonical filenames in images_dir — mirrors the base_arm
# scheme exactly: versionless filename, version tracked INSIDE the config
# entry ("version" + "changelog"). Legacy base-vN.* triples are still
# auto-registered so old deployments keep working.
X86_BASE_DISK = "base_x86.qcow2"
X86_BASE_KERNEL = "base_x86.kernel"
X86_BASE_INITRD = "base_x86.initrd.img"
X86_BASE_TAG = "x86"

# dev/debug base — the arm "devkit disk" model (replaces the old x86 base-dev).
#
# Instead of baking frida/Magisk into a whole new /system image (the retired
# x86 `base-dev.qcow2`), the dev environment is the SHARED `base_arm` PLUS one
# extra virtio disk attached as vdc: `base_arm_devkit.qcow2`, an ext4 image
# built host-side (rootless, cross-platform via `mke2fs -d`) carrying the
# arm64 frida-server, Magisk (apk + magiskboot), the omni-* device scripts, and
# a manifest. `base_arm.qcow2` is NEVER modified — a dev account is a normal arm
# account (system-overlay + data + efivars trio) with the devkit disk added.
# DEV-ONLY: only omni-agent selects it (`create --base dev`); the shipped bases
# (base_x86 / base_arm) never carry any of it, and building it NEVER changes
# current_base.
DEV_BASE_TAG = "dev"
# The extra devkit disk (attached to dev accounts as vdc). Shared + immutable;
# each dev account gets a cheap COW overlay of it (like the system overlay).
ARM_DEVKIT_DISK = "base_arm_devkit.qcow2"
# The rooted dev SYSTEM overlay (COW on base_arm.qcow2) — holds the Magisk-
# patched boot. base_arm.qcow2 stays immutable.
ARM_DEVSYSTEM_DISK = "base_arm_devsystem.qcow2"
# The dev /data template: a copy of the provisioned arm /data that ALSO has
# Magisk fully configured (shell su granted Forever, root_access=3, Zygisk +
# DenyList on), captured once from a rooted dev boot. When present, dev accounts
# use it so root works HEADLESSLY from first boot (no GUI su prompt). It is a
# matched pair with base_arm_devsystem (same /metadata FBE keys).
ARM_DEVDATA_DISK = "base_arm_devdata.qcow2"
# Where the guest mounts the devkit disk (read-only) and where the activated,
# exec-capable copy of the toolkit lives. /mnt is a noexec tmpfs on this base,
# so the toolkit is copied to /data/local/tmp for execution (see _devkit_*).
DEVKIT_MOUNT = "/mnt/omni-devkit"          # ro mount of vdc (source of truth)
DEVKIT_WORK = "/data/local/tmp/omni-devkit"  # exec-capable activated copy
DEVKIT_MANIFEST_GUEST = DEVKIT_WORK + "/manifest.json"
# frida-server pinned for the dev base (android-ARM64 — the base runs arm64
# natively under HVF/KVM, no translation). Override with
# `build-dev-base --frida-version`. The hidden frida port is intentionally NOT
# the well-known 27042.
DEFAULT_FRIDA_VERSION = "17.15.4"
DEFAULT_FRIDA_PORT = 27142

# EDK2 aarch64 firmware CODE (read-only); resolved from the QEMU install.
# On macOS/brew it ships inside the qemu Cellar; overridable via config
# qemu.arm_edk2_code.
ARM_EDK2_CANDIDATES = (
    "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",
    "/usr/local/share/qemu/edk2-aarch64-code.fd",
    "/usr/share/qemu/edk2-aarch64-code.fd",
)


def arm_edk2_code():
    """Absolute path to edk2-aarch64-code.fd (UEFI firmware CODE volume).
    Config qemu.arm_edk2_code wins; else the brew Cellar (globbed, newest);
    else the well-known share dirs."""
    import glob
    try:
        cfgd = read_config().get("qemu", {}).get("arm_edk2_code")
    except Exception:
        cfgd = None
    if cfgd and Path(cfgd).exists():
        return cfgd
    cellar = sorted(glob.glob(
        "/opt/homebrew/Cellar/qemu/*/share/qemu/edk2-aarch64-code.fd"))
    for cand in ([cellar[-1]] if cellar else []) + list(ARM_EDK2_CANDIDATES):
        if Path(cand).exists():
            return cand
    return None


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

def emit_json(obj):
    """The one JSON payload a --json command prints on stdout."""
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


# Set True by enable_json_mode(); read by fail() to shape typed errors.
_JSON_MODE = False


def enable_json_mode():
    """--json: stdout must carry EXACTLY the JSON payload. Redirect every
    informational print() (progress, warnings) to stderr so a GUI can
    parse stdout blindly. emit_json writes to sys.stdout directly and is
    unaffected."""
    global _JSON_MODE
    _JSON_MODE = True
    import builtins
    orig = builtins.print

    def _to_stderr(*a, **k):
        k.setdefault("file", sys.stderr)
        orig(*a, **k)
    builtins.print = _to_stderr


def fail(code, message=None, exit_code=1):
    """Contract-shaped fatal error (omnidroid-api.md v1 §8). In --json mode
    emit {"ok":false,"error":<code>,"message":<msg>} on stdout; always write a
    human line to stderr; exit nonzero. Use for the TYPED errors the contract
    names (arch_boundary, abi_not_translated, install_failed, no_base, ...);
    legacy sys.exit(str) sites are left untouched to keep [CURRENT] behavior."""
    msg = message or code
    if _JSON_MODE:
        emit_json({"ok": False, "error": code, "message": msg})
    sys.stderr.write(f"error: {msg}\n")
    sys.exit(exit_code)


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


def base_setup_help(images_dir, cfg=None):
    """The exact, actionable 'make this install ready' message — shown by
    setup, doctor, and every base-needing command when no base is usable."""
    template = (cfg or {}).get("data_template", "data-template-8g.qcow2")
    return (
        f"\nThis install has no usable base image yet. Copy the base "
        f"assets into:\n"
        f"  {images_dir}\n"
        f"required files (exact names):\n"
        f"  base_x86.qcow2        the immutable Bliss OS system image\n"
        f"  base_x86.kernel       its extracted kernel\n"
        f"  base_x86.initrd.img   its extracted initrd\n"
        f"  {template}    formatted-empty ext4 /data template\n"
        f"(legacy versioned triples base-vN.qcow2/.kernel/.initrd.img are "
        f"also accepted.)\nComplete bases are registered automatically on "
        f"the next command\n(or run: omnidroid setup). Check readiness any "
        f"time with: omnidroid doctor\n"
        f"(These files will arrive via download in a future version.)")


def autoregister_bases():
    """Scan images_dir for complete, not-yet-registered base file sets and
    register them (src from config 'default_src'): the canonical versionless
    base_x86 triple (mirrors base_arm; version lives in the entry, not the
    filename) plus legacy base-vN triples. If no current_base is set, point
    it at the canonical x86 base (else the highest legacy version). Persists
    the RAW config (keeps the per-platform images_dir dict intact). Returns
    (raw_config, newly_registered_tags). Registration only ADDS entries —
    existing bases/accounts are never touched, honoring base immutability."""
    raw = read_config()
    images = Path(images_dir(raw))
    bases = raw.setdefault("bases", {})
    known_disks = {b.get("disk") for b in bases.values()}
    new = []
    # Canonical x86 base: versionless base_x86 triple (mirrors base_arm).
    if (X86_BASE_TAG not in bases and X86_BASE_DISK not in known_disks
            and images.exists()
            and (images / X86_BASE_DISK).exists()
            and (images / X86_BASE_KERNEL).exists()
            and (images / X86_BASE_INITRD).exists()):
        bases[X86_BASE_TAG] = {"type": BASE_TYPE_X86,
                               "disk": X86_BASE_DISK,
                               "kernel": X86_BASE_KERNEL,
                               "initrd": X86_BASE_INITRD,
                               "src": raw.get("default_src", DEFAULT_SRC),
                               "notes": "auto-registered canonical x86 base "
                                        "from images_dir"}
        new.append(X86_BASE_TAG)
    if images.exists():
        for disk in sorted(images.glob("base-*.qcow2")):
            m = re.fullmatch(r"base-(v\d+)\.qcow2", disk.name)
            if not m or disk.name in known_disks or m.group(1) in bases:
                continue
            tag = m.group(1)
            kernel = images / f"base-{tag}.kernel"
            initrd = images / f"base-{tag}.initrd.img"
            if kernel.exists() and initrd.exists():
                bases[tag] = {"disk": disk.name, "kernel": kernel.name,
                              "initrd": initrd.name,
                              "src": raw.get("default_src", DEFAULT_SRC),
                              "notes": "auto-registered from images_dir"}
                new.append(tag)
    # arm-uefi base: register the provisioned matched-pair trio if present
    # (base_arm.qcow2 backing + base_arm_system.qcow2 overlay + _data + _efivars).
    # Independent of the x86 vN scheme; only ADDS an "arm" entry.
    if (ARM_BASE_TAG not in bases and images.exists()
            and (images / ARM_BASE_DISK).exists()
            and (images / ARM_BASE_SYSTEM).exists()
            and (images / ARM_BASE_DATA).exists()):
        bases[ARM_BASE_TAG] = {
            "type": BASE_TYPE_ARM,
            "base_disk": ARM_BASE_DISK,
            "system": ARM_BASE_SYSTEM,
            "data": ARM_BASE_DATA,
            "efivars": ARM_BASE_EFIVARS,
            "src": "https://github.com/jqssun/android-lineage-qemu "
                   "(LineageOS 23.2 arm64, virtio_arm64only)",
            "notes": "auto-registered arm64/UEFI base (LineageOS 23.2, "
                     "kiosk+device-owner provisioned matched pair)"}
        new.append(ARM_BASE_TAG)
    # dev/debug base: the arm base PLUS the extra devkit disk (attached as vdc).
    # ADD-ONLY; never made current_base (the shipped product stays on the arm/x86
    # production base). Registered only when the arm base files AND the devkit
    # disk are present. It reuses the arm provisioned trio (a rooted dev system
    # overlay is preferred if `base_arm_devsystem.qcow2` exists). See
    # build_dev_base() / DEV_BASE_TAG.
    if (DEV_BASE_TAG not in bases and images.exists()
            and (images / ARM_DEVKIT_DISK).exists()
            and (images / ARM_BASE_DISK).exists()
            and (images / ARM_BASE_SYSTEM).exists()
            and (images / ARM_BASE_DATA).exists()):
        dev_system = (ARM_DEVSYSTEM_DISK
                      if (images / ARM_DEVSYSTEM_DISK).exists()
                      else ARM_BASE_SYSTEM)
        dev_data = (ARM_DEVDATA_DISK if (images / ARM_DEVDATA_DISK).exists()
                    else ARM_BASE_DATA)
        bases[DEV_BASE_TAG] = {
            "type": BASE_TYPE_ARM,
            "base_disk": ARM_BASE_DISK,
            "system": dev_system,
            "data": dev_data,
            "efivars": ARM_BASE_EFIVARS,
            "devkit": ARM_DEVKIT_DISK,
            "src": "base_arm + devkit disk (frida + Magisk + omni tools)",
            "notes": "auto-registered arm dev base: base_arm + the "
                     "base_arm_devkit.qcow2 extra disk (vdc); omni-agent only"}
        new.append(DEV_BASE_TAG)
    changed = bool(new)
    if not raw.get("current_base") and bases:
        # Prefer an arm base on an arm64 host, else the canonical x86 base,
        # else the highest legacy x86 vN.
        x86 = [t for t in bases if base_type(bases[t]) == BASE_TYPE_X86]
        if IS_ARM64_HOST and ARM_BASE_TAG in bases:
            raw["current_base"] = ARM_BASE_TAG
        elif X86_BASE_TAG in bases:
            raw["current_base"] = X86_BASE_TAG
        elif x86:
            raw["current_base"] = max(
                x86, key=lambda t: int(re.sub(r"\D", "", t) or 0))
        else:
            raw["current_base"] = next(iter(bases))
        changed = True
    if changed:
        CONFIG_PATH.write_text(json.dumps(raw, indent=2))
        if new:
            print(f"[config] auto-registered base(s) from {images}: "
                  f"{', '.join(new)} (current: {raw['current_base']})")
    return raw, new


def effective_base_tag(cfg):
    """The base tag to use, selected by HOST ARCHITECTURE. On an arm64 host
    prefer an arm-uefi base (config 'current_base_arm', else the first
    arm-uefi base, else 'arm'); on x86 hosts use current_base. This keeps
    x86 behavior byte-identical while letting the same checkout pick the
    arm base automatically on Apple Silicon."""
    bases = cfg.get("bases") or {}
    if IS_ARM64_HOST:
        cand = cfg.get("current_base_arm")
        if cand and cand in bases and base_type(bases[cand]) == BASE_TYPE_ARM:
            return cand
        for t, b in bases.items():
            if base_type(b) == BASE_TYPE_ARM:
                return t
    return cfg.get("current_base")


def base_missing_files(images, base):
    """Per-type list of a base's missing files (absolute paths)."""
    if base_type(base) == BASE_TYPE_ARM:
        keys = ("base_disk", "system", "data")   # efivars optional
        missing = [str(images / base[k]) for k in keys
                   if base.get(k) and not (images / base[k]).exists()]
        # A dev base additionally needs its extra devkit disk (vdc).
        if base.get("devkit") and not (images / base["devkit"]).exists():
            missing.append(str(images / base["devkit"]))
        return missing
    return [str(images / base[k]) for k in ("disk", "kernel", "initrd")
            if not (images / base[k]).exists()]


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
    """Resolve a store record's base MODE ("prod"/"dev"/None -- see
    omnidroid/accounts.py) to a cfg base TAG (a key into cfg["bases"]).
    The store and the engine speak different vocabularies for "base": the
    store tracks a coarse mode, the engine needs the exact registered base
    entry to boot/introspect. `rec` need only carry a "base" key (a full
    record or a list_accounts entry both work); pass `cfg` to avoid a
    re-read when resolving many accounts at once (e.g. all_accounts)."""
    cfg = cfg if cfg is not None else read_config()
    bases = cfg.get("bases") or {}
    mode = (rec or {}).get("base")
    if mode == "dev":
        for t, b in bases.items():
            if base_is_dev(b):
                return t
        return DEV_BASE_TAG
    # prod or unset: the arm production tag.
    tag = effective_base_tag(cfg)
    if tag and tag in bases and not base_is_dev(bases[tag]):
        return tag
    for t, b in bases.items():
        if base_type(b) == BASE_TYPE_ARM and not base_is_dev(b):
            return t
    return ARM_BASE_TAG


def load_account(name):
    """Build a runtime HANDLE for `name` -- identity from the central store
    (omnidroid/accounts.py), live ports (and, if running, the exact base tag)
    from runtime/<name>/run.json. Reads NO per-account folder: the ~14
    running-instance commands only ever need name/base/ports/dev/game_package,
    all of which live in one of those two places now."""
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
        "dev": base_is_dev((cfg.get("bases") or {}).get(base_tag, {})),
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
                "dev": base_is_dev((cfg.get("bases") or {}).get(base_tag, {})),
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
                "dev": base_is_dev((cfg.get("bases") or {}).get(base_tag, {})),
                "game_package": ROBLOX_PACKAGE, "first_boot_done": True}
        for k in ("adb_port", "qmp_port", "vnc_port"):
            if r.get(k) is not None:
                acct[k] = r[k]
        out.append(acct)
    return sorted(out, key=lambda a: a["name"])


# Per-instance PORT SCHEME (documented invariant):
#   instance index i (0-based)  ->  adb = adb_port_start + i   (16001+)
#                                   qmp = qmp_port_start + i   (17001+)
#                                   vnc = vnc_port_start + i   (18001+)
# One shared index per account keeps the triple aligned; the three ranges
# are 1000 apart, so adb/qmp/vnc can NEVER collide below 1000 instances
# (and instance counts are host-RAM-bound long before that). vnc_port is
# WIRED: QEMU's built-in VNC server listens on it, 127.0.0.1 ONLY. No
# auth — that is safe ONLY because of the localhost bind (HARD RULE:
# never bind VNC to a network interface without adding auth).
VNC_PORT_START_DEFAULT = 18001


def vnc_start(cfg):
    return cfg["qemu"].get("vnc_port_start", VNC_PORT_START_DEFAULT)


def allocate_ports(cfg):
    """Lowest free port-index across RUNNING instances (a stopped instance
    frees its slot immediately). The three ranges are 1000 apart, so the shared
    index keeps adb/qmp/vnc aligned and collision-free below 1000 concurrent."""
    q = cfg["qemu"]
    used = set()
    for inst in running_instances():
        if inst.get("adb_port") is not None:
            used.add(inst["adb_port"] - q["adb_port_start"])
    i = 0
    while i in used:
        i += 1
    return (q["adb_port_start"] + i, q["qmp_port_start"] + i,
            vnc_start(cfg) + i)


@contextlib.contextmanager
def _launch_lock():
    """Serialize the allocate-ports + reserve-slot critical section across
    concurrent `start` launches on one host. Without it, two parallel launches
    race to the same free port index. POSIX flock; a no-op on Windows (the
    120-concurrent farm is Linux, dev is macOS -- both POSIX; the shipped
    Windows product launches one instance at a time)."""
    lock_path = config.runtime_root() / ".launch.lock"
    f = open(lock_path, "w")
    try:
        try:
            import fcntl
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        except (ImportError, OSError):
            pass   # Windows / no-flock: degrade to no lock (single-launch host)
        yield
    finally:
        f.close()


# ---------- process helpers ----------

def pid_alive(pid):
    if pid is None:
        return False
    if IS_WINDOWS:
        # NEVER use os.kill(pid, 0) on Windows: it TERMINATES the process.
        import ctypes
        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        STILL_ACTIVE = 259
        h = ctypes.windll.kernel32.OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not h:
            return False
        code = ctypes.c_ulong()
        ok = ctypes.windll.kernel32.GetExitCodeProcess(h, ctypes.byref(code))
        ctypes.windll.kernel32.CloseHandle(h)
        return bool(ok) and code.value == STILL_ACTIVE
    else:
        import os
        # Reap first, and treat a zombie as DEAD. os.kill(pid, 0) succeeds on a
        # zombie, so a QEMU we spawned IN-PROCESS (update-kiosk, play's
        # _ensure_booted) reports as "still running" after it has exited — which
        # made _shutdown escalate powerdown -> QMP quit -> SIGKILL against an
        # already-dead process and then return 'kill-failed'. Callers ask "is the
        # instance running?"; a zombie is not.
        #
        # The usual detached case (`omni start` exits, QEMU reparents to init) is
        # unaffected: waitpid raises ChildProcessError and we fall through.
        try:
            wpid, _status = os.waitpid(pid, os.WNOHANG)
            if wpid == pid:
                return False          # exited; just reaped it
        except (ChildProcessError, OSError):
            pass                      # not our child — the normal case
        try:
            os.kill(pid, 0)
            return True
        except OSError:
            return False


def runtime_dir(username):
    """Per-instance throwaway dir: efivars, run.json (ports+pid), qemu.log,
    autocap frames. Wiped on `stop` and `remove` (see _wipe_runtime).
    Replaces the old accounts/<name>/ for the product path."""
    return config.runtime_root() / username


def _reserve_ports(name, adb_port, qmp_port, vnc_port):
    """Claim a port slot for `name` by writing a run.json reservation with THIS
    launcher process's pid, so a concurrent allocate_ports() (which counts
    runtime/*/run.json with a live pid) sees the slot as taken until
    spawn_qemu() overwrites it with the real QEMU pid. Self-healing: if the
    launch aborts before spawn, the launcher exits, its pid dies, and
    running_instances() stops counting the stale reservation -> slot freed."""
    d = runtime_dir(name)
    d.mkdir(parents=True, exist_ok=True)
    (d / "run.json").write_text(json.dumps(
        {"pid": os.getpid(), "started": time.time(), "reserving": True,
         "adb_port": adb_port, "qmp_port": qmp_port, "vnc_port": vnc_port}))


def _wipe_runtime(name):
    """Delete the per-instance runtime dir (efivars.fd, run.json, qemu.log,
    autocap frames) once an ephemeral instance (build_acct) has stopped.
    Called from cmd_stop (after a successful power-off) and cmd_remove.
    Ephemeral instances write nothing under accounts/<name>/, so this IS
    the entire teardown -- no folder to remove there."""
    import shutil
    shutil.rmtree(runtime_dir(name), ignore_errors=True)


def running_instances():
    """Every instance with a LIVE qemu pid, read from runtime/*/run.json.
    Dead/stale run.json files are ignored. Returns dicts with name + ports."""
    out = []
    root = config.data_dir() / "runtime"
    if not root.exists():
        return out
    for d in sorted(root.iterdir()):
        rj = d / "run.json"
        if not rj.exists():
            continue
        try:
            data = json.loads(rj.read_text())
        except Exception:  # noqa: BLE001
            continue
        if pid_alive(data.get("pid")):
            out.append({"name": d.name, "pid": data["pid"],
                        "base": data.get("base"),
                        "adb_port": data.get("adb_port"),
                        "qmp_port": data.get("qmp_port"),
                        "vnc_port": data.get("vnc_port")})
    return out


def host_rss_mb(pid):
    """Resident memory of a host process, in MB."""
    try:
        if IS_WINDOWS:
            import ctypes
            import ctypes.wintypes as wt

            class PMC(ctypes.Structure):
                _fields_ = [("cb", wt.DWORD), ("PageFaultCount", wt.DWORD),
                            ("PeakWorkingSetSize", ctypes.c_size_t),
                            ("WorkingSetSize", ctypes.c_size_t),
                            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                            ("QuotaPagedPoolUsage", ctypes.c_size_t),
                            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                            ("PagefileUsage", ctypes.c_size_t),
                            ("PeakPagefileUsage", ctypes.c_size_t)]
            PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
            h = ctypes.windll.kernel32.OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
            if not h:
                return None
            pmc = PMC()
            pmc.cb = ctypes.sizeof(PMC)
            ok = ctypes.windll.psapi.GetProcessMemoryInfo(
                h, ctypes.byref(pmc), pmc.cb)
            ctypes.windll.kernel32.CloseHandle(h)
            return pmc.WorkingSetSize / (1024 * 1024) if ok else None
        else:
            txt = Path(f"/proc/{pid}/status").read_text()
            m = re.search(r"VmRSS:\s+(\d+) kB", txt)
            return int(m.group(1)) / 1024 if m else None
    except Exception:
        return None


def host_mem_available_mb():
    """Host free-for-use memory in MB (the number that decides how many
    instances fit). Linux: MemAvailable. Windows: ullAvailPhys."""
    try:
        if IS_WINDOWS:
            import ctypes

            class MEMSTAT(ctypes.Structure):
                _fields_ = [("dwLength", ctypes.c_uint32),
                            ("dwMemoryLoad", ctypes.c_uint32),
                            ("ullTotalPhys", ctypes.c_uint64),
                            ("ullAvailPhys", ctypes.c_uint64),
                            ("ullTotalPageFile", ctypes.c_uint64),
                            ("ullAvailPageFile", ctypes.c_uint64),
                            ("ullTotalVirtual", ctypes.c_uint64),
                            ("ullAvailVirtual", ctypes.c_uint64),
                            ("ullAvailExtendedVirtual", ctypes.c_uint64)]
            st = MEMSTAT()
            st.dwLength = ctypes.sizeof(MEMSTAT)
            if not ctypes.windll.kernel32.GlobalMemoryStatusEx(
                    ctypes.byref(st)):
                return None
            return st.ullAvailPhys / (1024 * 1024)
        txt = Path("/proc/meminfo").read_text()
        m = re.search(r"MemAvailable:\s+(\d+) kB", txt)
        return int(m.group(1)) / 1024 if m else None
    except Exception:
        return None


# ---------- KSM (Linux kernel samepage merging) ----------

def ksm_available():
    return IS_LINUX and KSM_DIR.exists()


def ksm_stats():
    """Read all /sys/kernel/mm/ksm/* values (ints where possible).
    None when KSM is not available (non-Linux or kernel without KSM)."""
    if not ksm_available():
        return None
    out = {}
    for f in sorted(KSM_DIR.iterdir()):
        try:
            v = f.read_text().strip()
            out[f.name] = int(v) if v.lstrip("-").isdigit() else v
        except OSError:
            pass
    return out


def ksm_write(name, value):
    """Write one KSM sysfs knob; exits with sudo advice on EPERM."""
    try:
        (KSM_DIR / name).write_text(str(value))
    except PermissionError:
        sys.exit(f"error: no permission to write {KSM_DIR / name} - "
                 f"run with sudo (or install/enable ksmtuned)")


def ksm_saved_mb(stats):
    """Approx MB deduplicated: each page in pages_sharing points at a
    shared page instead of owning its own copy."""
    return stats.get("pages_sharing", 0) * PAGE_SIZE / (1024 * 1024)


def pid_ksm_merged_mb(pid):
    """Per-process KSM-merged pages (kernel >= 6.1 exposes
    ksm_merging_pages). None if unsupported."""
    try:
        n = int(Path(f"/proc/{pid}/ksm_merging_pages").read_text())
        return n * PAGE_SIZE / (1024 * 1024)
    except Exception:
        return None


# ---------- adb / qmp ----------

def _require_adb_port(acct):
    """A diskless handle carries ports only while the instance is RUNNING
    (they live in runtime/<name>/run.json). A command run against a stopped
    account gets a portless handle — fail cleanly here instead of a raw
    KeyError traceback. Internal pollers (wait_for_boot etc.) always run
    against a live instance, so this never fires for them."""
    port = acct.get("adb_port")
    if port is None:
        fail("not_running",
             f"'{acct.get('name')}' is not running — start it first "
             f"(omnidroid start {acct.get('name')})")
    return port


def adb(acct, *args, timeout=20, check=False):
    serial = f"127.0.0.1:{_require_adb_port(acct)}"
    cmd = ["adb", "-s", serial] + list(args)
    return subprocess.run(cmd, capture_output=True, text=True,
                          timeout=timeout, check=check)


def adb_connect(acct):
    port = _require_adb_port(acct)
    try:
        subprocess.run(["adb", "connect", f"127.0.0.1:{port}"],
                       capture_output=True, text=True, timeout=15)
    except subprocess.TimeoutExpired:
        pass


def adb_getprop(acct, prop):
    try:
        r = adb(acct, "shell", "getprop", prop, timeout=8)
        return r.stdout.strip()
    except Exception:
        return ""


def qmp(acct, execute, arguments=None, timeout=6):
    """Send one QMP command; returns parsed response line or None."""
    try:
        with socket.create_connection(("127.0.0.1", acct["qmp_port"]),
                                      timeout=timeout) as s:
            s.settimeout(timeout)
            f = s.makefile("rw", encoding="utf-8", newline="\n")
            f.readline()                                   # greeting
            f.write('{"execute":"qmp_capabilities"}\n'); f.flush()
            f.readline()
            msg = {"execute": execute}
            if arguments:
                msg["arguments"] = arguments
            f.write(json.dumps(msg) + "\n"); f.flush()
            return json.loads(f.readline())
    except Exception:
        return None


# ---------- qemu ----------

def default_accel():
    """Hypervisor auto-detect: WHPX on Windows, HVF on macOS (Apple Silicon),
    KVM on Linux. Overridable per-start with --accel (e.g. 'tcg' for a
    no-hypervisor smoke test)."""
    if IS_WINDOWS:
        return "whpx,kernel-irqchip=off"
    if IS_MACOS:
        return "hvf"
    return "kvm"


def machine_arg(accel):
    """-machine string. On Linux/KVM add mem-merge=on explicitly: it marks
    guest RAM MADV_MERGEABLE so KSM can dedup identical pages across
    instances (it is the QEMU default, but distro builds vary — be
    explicit; it is what the whole Linux scaling story depends on)."""
    m = f"q35,accel={accel}"
    if IS_LINUX and accel.split(",")[0] == "kvm":
        m += ",mem-merge=on"
    return m


def check_accel():
    """Linux preflight: warn loudly if /dev/kvm is unusable (QEMU would
    fail or crawl under TCG). Windows/WHPX has no equivalent check."""
    if not IS_LINUX:
        return
    import os
    kvm = Path("/dev/kvm")
    if not kvm.exists():
        print("[accel] WARNING: /dev/kvm missing - KVM unavailable. "
              "Enable VT-x/AMD-V in BIOS and install qemu-system-x86; "
              "check with 'kvm-ok' (apt install cpu-checker).")
    elif not os.access(kvm, os.R_OK | os.W_OK):
        print("[accel] WARNING: no permission on /dev/kvm - add your user "
              "to the kvm group: sudo usermod -aG kvm $USER (re-login).")


# Per-instance performance modes. Counts are NEVER capped — these tune the
# per-instance footprint; the host's free RAM decides how many run.
# ALL instances are HEADLESS (-display none), always: no host window
# exists anywhere. View/control happens via adb (screenshot/logcat) or an
# optional VNC viewer on the instance's vnc_port (127.0.0.1 only — see
# the port scheme note at allocate_ports). With no window the
# old VirGL path (needed a host GL window) and the R/B software-blit swap
# are both moot — guest-side rendering is unchanged and screencap is
# always true-color.
MODES = {
    "playable": {"mem": 4096, "smp": 4},
    "hard":     {"mem": 3072, "smp": 4},
    "brutal":   {"mem": 2048, "smp": 2},
}
DEFAULT_MODE = "playable"


def resolve_mode(cfg, name=None, mem=None):
    m = dict(MODES[name or DEFAULT_MODE])
    m["name"] = name or DEFAULT_MODE
    if mem:
        m["mem"] = mem
    return m


def _assert_port_triple(acct):
    """The per-account port triple must be distinct (the shared-index scheme
    guarantees it below 1000 instances; assert anyway before handing the
    ports to QEMU). Returns the QEMU -vnc display number."""
    if len({acct["adb_port"], acct["qmp_port"], acct["vnc_port"]}) != 3:
        sys.exit(f"error: port collision for '{acct['name']}': "
                 f"adb {acct['adb_port']} qmp {acct['qmp_port']} "
                 f"vnc {acct['vnc_port']}")
    vnc_display = acct["vnc_port"] - 5900     # QEMU -vnc takes a display #
    if vnc_display < 0:
        sys.exit(f"error: vnc_port {acct['vnc_port']} is below QEMU's "
                 f"5900 display offset")
    return vnc_display


def qemu_command_arm(acct, cfg, dev, mode=None, accel=None):
    """arm-uefi (LineageOS arm64) QEMU command — native under HVF on Apple
    Silicon, NO translation layer. UEFI/GRUB disk boot: EDK2 pflash CODE +
    per-account writable efivars, GPT system disk (vda, the provisioned
    overlay carrying /metadata FBE keys) + /data (vdb). Same headless +
    localhost-VNC model as x86; base flags proven in tools/arm64/boot_arm64.sh.
    Silent boot is handled inside the guest image (GRUB/kernel), not via a
    Bliss-style -append, so there is no dev/prod append split here — the dev
    flag only adds a serial log."""
    base = cfg["bases"][acct["base"]]
    q = cfg["qemu"]
    d = account_dir(acct["name"])          # overlay disks only (Task 4 removes these)
    rd = runtime_dir(acct["name"])         # per-boot files: efivars.fd, serial.log
    images = Path(cfg["images_dir"])
    accel = accel or default_accel()
    mode = mode or resolve_mode(cfg)
    vnc_display = _assert_port_triple(acct)
    smp = q["smp"] if dev else mode["smp"]
    mem = q["mem_mb"] if dev else mode["mem"]

    # EPHEMERAL (fully-shared, no-persistence) instances boot the SHARED provisioned
    # base templates DIRECTLY with snapshot=on: every write goes to a throwaway
    # per-process overlay that QEMU discards on exit, so nothing persists and many
    # instances of the same base run CONCURRENTLY (each opens the backing read-only).
    # The instance is then pure config (accounts.json cookie/alias) with NO
    # per-account system/data/devkit qcow2 files — only a fresh per-boot efivars.
    # Non-ephemeral accounts keep their per-account COW overlays (unchanged).
    ephemeral = bool(acct.get("ephemeral"))
    if ephemeral:
        sys_src = images / base["system"]
        data_src = images / base["data"]
        disk_opts = ",discard=unmap,detect-zeroes=unmap,snapshot=on"
        # Ephemeral efivars is refreshed fresh EVERY boot (see
        # _refresh_ephemeral_efivars, called from spawn_qemu before this
        # command is built) into runtime_dir — genuinely per-boot, throwaway.
        efivars_src = rd / "efivars.fd"
    else:
        sys_src = d / "system.qcow2"
        data_src = d / "data.qcow2"
        disk_opts = ",discard=unmap,detect-zeroes=unmap"
        # Non-ephemeral efivars is written ONCE at account creation (see
        # _make_persistent_arm_account, used only by base-build/maintenance
        # flows now) and persists across boots like the overlay disks —
        # stays under account_dir; this whole non-ephemeral path is
        # live-path-straggler territory (Task 5).
        efivars_src = d / "efivars.fd"

    code = arm_edk2_code()
    if not code:
        sys.exit("error: edk2-aarch64-code.fd (UEFI firmware) not found - "
                 "install qemu (brew install qemu) or set qemu.arm_edk2_code "
                 "in configs/paths.json")
    cmd = [
        qemu_bin("qemu-system-aarch64"),
        "-machine", "virt",
        "-accel", accel,          # hvf on Apple Silicon (no translation)
        "-cpu", "host",
        "-smp", str(smp),
        "-m", str(mem),
        # UEFI firmware: read-only CODE + per-account writable vars.
        "-drive", (f"if=pflash,unit=0,file={code},file.locking=off,"
                   "format=raw,readonly=on"),
        "-drive", f"if=pflash,unit=1,file={efivars_src}",
        # System overlay (vda, has /metadata FBE keys) + /data (vdb).
        "-device", "virtio-blk-pci,drive=vda,bootindex=0",
        "-device", "virtio-blk-pci,drive=vdb,bootindex=1",
        "-drive", f"file={sys_src},if=none,id=vda{disk_opts}",
        "-drive", f"file={data_src},if=none,id=vdb{disk_opts}",
        "-device", "virtio-gpu-pci",
        "-display", "none",       # headless ALWAYS (same rule as x86)
        # Built-in VNC server, LOCALHOST ONLY (no auth is safe ONLY because
        # of the 127.0.0.1 bind — HARD RULE, same as x86; never bind a
        # network interface without adding auth in the same change).
        "-vnc", f"127.0.0.1:{vnc_display}",
        "-device", "nec-usb-xhci,id=usb-bus",
        "-device", "qemu-xhci,id=usb-controller-0",
        "-device", "usb-tablet,bus=usb-bus.0",
        "-device", "usb-kbd,bus=usb-bus.0",
        "-netdev", ("user,id=net0,"
                    f"hostfwd=tcp:127.0.0.1:{acct['adb_port']}-:5555"),
        "-device", "virtio-net-pci,netdev=net0",
        "-device", "virtio-serial",
        "-device", "virtio-rng-pci",
        "-qmp", f"tcp:127.0.0.1:{acct['qmp_port']},server=on,wait=off",
        "-name", f"omni-{acct['name']}",
    ]
    # Dev accounts: attach the devkit disk as a THIRD virtio-blk (vdc). It is a
    # cheap per-account COW overlay of the shared base_arm_devkit.qcow2 (frida +
    # Magisk + omni tools). The guest sees it as /dev/block/vdc and mounts it
    # read-only during activation (see _devkit_activate). Not bootable.
    # Dev vdc: the shared devkit template (snapshot=on) for ephemeral instances,
    # else the per-account COW overlay.
    devkit_src = (images / base["devkit"]) if (ephemeral and base.get("devkit")) \
        else (d / "devkit.qcow2")
    if acct_is_dev(acct) and Path(devkit_src).exists():
        cmd += [
            "-device", "virtio-blk-pci,drive=vdc",
            "-drive", f"file={devkit_src},if=none,id=vdc{disk_opts}",
        ]
    if dev:
        cmd += ["-serial", f"file:{rd / 'serial.log'}"]
    return cmd


def qemu_command(acct, cfg, dev, mode=None, accel=None):
    base = cfg["bases"][acct["base"]]
    if base_type(base) == BASE_TYPE_ARM:
        return qemu_command_arm(acct, cfg, dev, mode=mode, accel=accel)
    images = Path(cfg["images_dir"])
    q = cfg["qemu"]
    d = account_dir(acct["name"])
    accel = accel or default_accel()
    mode = mode or resolve_mode(cfg)

    vnc_display = _assert_port_triple(acct)

    append = ("stack_depot_disable=on cgroup_disable=pressure "
              "root=/dev/ram0 noexec=off "
              f"SRC={base['src']} DATA=vdb")
    smp = q["smp"]
    mem = q["mem_mb"]

    if dev:
        # Dev/builder boot: serial console log for debugging (headless like
        # everything else; virtio-vga kept so the guest has its usual DRM
        # device during provisioning/builder sessions).
        append += " console=tty0 console=ttyS0,115200"
        gpu = ["-device", "virtio-vga"]
        nic = "virtio-net-pci,netdev=net0"
    else:
        # Production silent boot (no firmware/console text).
        append += (" quiet loglevel=0 console=null "
                   "vt.global_cursor_default=0 SETUPWIZARD=0")
        nic = "virtio-net-pci,netdev=net0,romfile="   # no iPXE option ROM
        smp = mode["smp"]
        mem = mode["mem"]
        gpu = ["-vga", "none", "-device", "virtio-gpu-pci"]

    cmd = [
        qemu_bin("qemu-system-x86_64"),
        "-machine", machine_arg(accel),
        "-cpu", "qemu64",
        "-smp", str(smp),
        "-m", str(mem),
        "-drive", f"file={d / 'system.qcow2'},format=qcow2,if=virtio",
        "-drive", f"file={d / 'data.qcow2'},format=qcow2,if=virtio",
        *gpu,
        "-display", "none",       # headless ALWAYS; VNC below is an
                                  # attach point, never a window
        # Built-in VNC server on the account's reserved port. LOCALHOST
        # ONLY: no auth is safe ONLY because of the 127.0.0.1 bind — never
        # bind a network interface without adding auth in the same change.
        # Idle (no viewer) it does no framebuffer encoding, so leaving it
        # on costs ~nothing across hours-long headless runs; a viewer
        # disconnecting never affects the instance.
        "-vnc", f"127.0.0.1:{vnc_display}",
        "-device", "qemu-xhci",
        "-device", "usb-kbd",
        "-device", "usb-tablet",
        "-netdev", ("user,id=net0,"
                    f"hostfwd=tcp:127.0.0.1:{acct['adb_port']}-:5555"),
        "-device", nic,
        "-qmp", f"tcp:127.0.0.1:{acct['qmp_port']},server=on,wait=off",
        "-kernel", str(images / base["kernel"]),
        "-initrd", str(images / base["initrd"]),
        "-append", append,
        "-name", f"omni-{acct['name']}",
    ]
    if dev:
        cmd += ["-serial", f"file:{d / 'serial.log'}"]
    return cmd


def _refresh_ephemeral_efivars(acct, cfg):
    """Give an ephemeral instance a FRESH copy of the base UEFI vars for this boot,
    so nothing persists across boots (the system/data/devkit disks are the shared
    templates opened snapshot=on; efivars is the only writable file, and pflash
    needs a real file). arm-only; no-op otherwise."""
    import shutil
    base = cfg["bases"][acct["base"]]
    if base_type(base) != BASE_TYPE_ARM:
        return
    images = Path(cfg["images_dir"])
    d = runtime_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    efi_tmpl = images / base.get("efivars", ARM_BASE_EFIVARS)
    if efi_tmpl.exists():
        shutil.copyfile(efi_tmpl, d / "efivars.fd")


def spawn_qemu(acct, cfg, dev, mode=None, accel=None):
    check_accel()
    d = runtime_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    if acct.get("ephemeral"):
        _refresh_ephemeral_efivars(acct, cfg)
    log = open(d / "qemu.log", "w")
    kwargs = {}
    if IS_WINDOWS:
        DETACHED = 0x00000008          # DETACHED_PROCESS
        NEW_GROUP = 0x00000200         # CREATE_NEW_PROCESS_GROUP
        kwargs["creationflags"] = DETACHED | NEW_GROUP
    else:
        kwargs["start_new_session"] = True
    proc = subprocess.Popen(
        qemu_command(acct, cfg, dev, mode, accel=accel),
        stdout=log, stderr=log, **kwargs)
    (d / "run.json").write_text(json.dumps(
        {"pid": proc.pid, "started": time.time(),
         "mode": (mode or {}).get("name", "dev" if dev else DEFAULT_MODE),
         "base": acct["base"],
         "adb_port": acct["adb_port"], "qmp_port": acct["qmp_port"],
         "vnc_port": acct["vnc_port"]}))
    return proc.pid


def running_pid(name):
    p = runtime_dir(name) / "run.json"
    if not p.exists():
        return None
    pid = json.loads(p.read_text()).get("pid")
    return pid if pid_alive(pid) else None


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


def build_acct(name, cfg, dev=False):
    """Build the EPHEMERAL launch handle for `name`: resolves the base tag,
    allocates a fresh port triple, and stages a per-boot efivars copy into
    runtime_dir(name) -- but writes NO account.json and creates NO overlays.
    Ephemeral instances boot the shared base templates directly (snapshot=on,
    see qemu_command_arm), so there is nothing per-account to persist; the
    handle is pure launch state (name/base/ports), same key shape as
    load_account()'s but always carrying ports since this is what actually
    reserves them.

    This is the LAUNCH counterpart to load_account(): load_account reads an
    identity that may or may not be running; build_acct allocates a fresh
    instance to run. arm-only by design (the product is arm; dev is
    arm+devkit). dev=True selects the dev base (gated by OMNI_DEV_MODE
    upstream via assert_dev_allowed)."""
    if not re.fullmatch(r"[A-Za-z0-9_-]+", name):
        fail("bad_name",
             f"instance/username must be [A-Za-z0-9_-]+ (got '{name}')")
    tag = "dev" if dev else _select_base_tag(cfg, arch="arm")
    base = cfg["bases"][tag]
    if base_type(base) != BASE_TYPE_ARM:
        fail("arch_boundary",
             f"instances are arm-only; base '{tag}' is {arch_of_base(base)}")
    assert_dev_allowed(tag, base)
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
            "ephemeral": True, "dev": base_is_dev(base),
            "game_package": ROBLOX_PACKAGE, "first_boot_done": True}


def _select_base_tag(cfg, arch=None, base_tag=None):
    """Base tag for a NEW account, honoring --base/--arch (contract §6.1).
    Default (neither given): the host-arch effective base — byte-identical to
    the previous behavior. --base pins an explicit tag; --arch picks that
    arch's base (preferring the effective/current base if it matches).
    An --arch/--base mismatch is refused with arch_boundary."""
    bases = cfg.get("bases") or {}
    if base_tag is not None:
        if base_tag not in bases:
            fail("no_base", f"no base '{base_tag}'. "
                            f"Known: {list(visible_bases(cfg))}")
        assert_dev_allowed(base_tag, bases[base_tag])
        if arch and arch_of_base(bases[base_tag]) != arch:
            fail("arch_boundary",
                 f"--base {base_tag} is {arch_of_base(bases[base_tag])} but "
                 f"--arch {arch} was requested")
        return base_tag
    # Auto-selection must never LAND on a dev base by accident (e.g. it happens
    # to be the only arm base registered) — dev is only ever explicit.
    bases = visible_bases(cfg)
    if arch is not None:
        cands = [t for t in bases if arch_of_base(bases[t]) == arch]
        for pref in (cfg.get("_effective_base"), cfg.get("current_base")):
            if pref in cands:
                return pref
        if cands:
            return cands[0]
        fail("no_base", f"no {arch} base registered (known: "
                        f"{ {t: arch_of_base(bases[t]) for t in bases} })")
    default = cfg.get("_effective_base") or cfg["current_base"]
    if default not in bases:
        # Reachable when current_base points at a dev base and this caller has
        # no dev opt-in. Refusing beats silently booting a rooted frida image as
        # if it were the product.
        fail("no_base",
             f"default base '{default}' is not available in this build "
             f"(known: {list(bases)})")
    return default


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
    # Dev-base safety gate: an explicit tag (e.g. `update-kiosk --base dev`)
    # bypasses _select_base_tag's auto-avoidance, so gate here unconditionally
    # — same convention build_acct() follows. Refuses a dev base without
    # OMNI_DEV_MODE=1 (customer-safety boundary; see dev-mode gate comment).
    assert_dev_allowed(tag, base)
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
    devkit = base["devkit"] if base_is_dev(base) else None
    if devkit:
        make_overlay(d / "devkit.qcow2", images / devkit)
        acct["dev"] = True
    save_account(acct)
    print(f"[create {name}] arm64 disks ready (provisioned pair copied from "
          f"{base['system']}+{base['data']}); adb {adb_port}, qmp {qmp_port}, "
          f"vnc {vnc_port}")
    return load_account(name)


# Magisk's su on this all-read-only LineageOS lives in Magisk's own tmpfs, NOT
# in $PATH, so a bare `su` fails ("inaccessible or not found"). Probe the known
# spots. The dev /data template pre-grants shell (Forever), so a granted su
# returns uid 0 with no prompt.
SU_CANDIDATES = ("/debug_ramdisk/su", "/sbin/su", "su")


def resolve_su(acct):
    """Return the working Magisk su path in the guest ('/debug_ramdisk/su' etc.)
    or None if root is unavailable (dev boot not patched / not granted)."""
    for cand in SU_CANDIDATES:
        # Must go through `sh -c` (matches _devkit_activate's invocation):
        # MagiskSU's getopt permutes argv, so a bare trailing `-u` (as in
        # `su 0 id -u`) is misread as an unrecognized su OPTION (usage/exit 2)
        # instead of being passed to `id`.
        r = adb(acct, "shell", f"{cand} 0 sh -c {shlex.quote('id -u')}",
                timeout=15)
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
    """Activate the dev devkit disk after boot: mount vdc read-only and stage
    the omni-* tools into an exec-capable dir (/data/local/tmp/omni-devkit).
    Needs Magisk root (su) — the tools all run as root. Best-effort: on a
    not-yet-rooted dev boot it explains what to do and returns without failing
    the start. Returns a small status dict."""
    if not acct_is_dev(acct):
        return {"activated": False, "reason": "not_dev"}
    # Root via Magisk su (the arm base is a 'user' build — `adb root` is NOT
    # available; root comes only from the patched-boot Magisk daemon). The dev
    # /data template pre-grants shell, so this is headless (no su prompt).
    su = resolve_su(acct)
    if not su:
        print(f"[{label}] devkit: Magisk root not available (su denied/missing). "
              f"The dev boot is not patched/granted — frida can't attach and "
              f"hiding is off. Build it with: omni build-dev-base --patch-boot")
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
        # `omni dev-ui <name> --show magisk`.
        kiosk_ui = _assert_kiosk_foreground(acct, label)
        return {"activated": True, "su": su, "mount": DEVKIT_MOUNT,
                "work": DEVKIT_WORK,
                "magisk_env_fixed": "MAGISK_ENV_FIXED" in out,
                "kiosk_ui": kiosk_ui}
    print(f"[{label}] devkit: activation incomplete:\n{out.strip()[-800:]}")
    return {"activated": False, "reason": "mount_failed", "detail": out.strip()[-400:]}


def cmd_start(args):
    """Boot an instance, deliver its saved Roblox session, and land either
    INSIDE a place (if one is set) or on the account's home screen, logged in,
    with no menu and no simulated taps — the product's whole point. Rejects if
    the instance is already running (one live instance per username).

    Identical on the dev and production bases: same kiosk, same session
    broadcast, same roblox:// join. The dev base only differs in what is
    additionally available (frida/Magisk + always-on screenshots)."""
    ensure_qemu()
    cfg = load_config()
    dev = _dev_mode_for_play(args)
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
                    f"`omni login` (saves the account under its username), then "
                    f"`omni start {args.name}`. (Or override with "
                    f"--token-file <file>, or --no-token to land on Roblox's "
                    f"own login screen.) No instance was created for "
                    f"'{args.name}'.")
    # A place is OPTIONAL: with one set, this is a JOIN; without one, it's a
    # HOME boot — logged in via the delivered cookie, no deep link, no join.
    is_join = bool(sess.get("place_id"))

    # Only NOW do we know a session is deliverable (a real token, or the
    # explicit --no-token escape hatch) — build the launch handle. Nothing is
    # persisted here: the store owns the account's cookie (via `omni login`)
    # and its default place (via `omni session --place`); --place above is a
    # one-off override for THIS launch only.
    acct = build_acct(args.name, cfg, dev=dev)

    booted, first = _ensure_booted(acct, cfg, label,
                                   timeout=getattr(args, "timeout", None),
                                   accel=getattr(args, "accel", None))
    result = {"name": args.name, "place_id": sess.get("place_id"),
              "deeplink": roblox_deeplink(sess), "first_boot": first,
              "arch": acct_arch(acct), "dev": acct_is_dev(acct),
              "adb_port": acct["adb_port"], "vnc_port": acct["vnc_port"],
              "session": public_session(sess)}
    if not booted:
        result.update({"ok": False, "booted": False, "error": "boot_timeout"})
        if json_mode:
            emit_json(result)
        sys.exit(1)

    status = deliver_session(acct, label, sess, play=is_join)
    result.update({"booted": True, "ok": bool(status.get("delivered")),
                   **{k: v for k, v in status.items() if k != "kiosk"}})
    result["kiosk"] = status.get("kiosk")

    # Open a live WINDOW onto this instance so you can watch/play it, and so two
    # `omni start` runs give two accounts side by side. Each viewer is its own
    # detached process bound to this instance's own VNC port, so N windows for N
    # accounts just work. Default ON for interactive use; suppressed by
    # --no-window and by --json (a machine/automation caller drives via capture).
    want_window = (not getattr(args, "no_window", False)
                   and (getattr(args, "window", False) or not json_mode))
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
    # Boot once (dev) and re-apply settings so the new base's kiosk/system
    # game/HOME take effect on the existing data disk.
    spawn_qemu(acct, cfg, dev=True)
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

def _next_base_tag(cfg):
    """Next build tag. Counts legacy vN tags AND the internal 'version'
    field of versionless entries (base_x86), so a rebuild on the canonical
    base continues its lineage (x86 at version 5 -> next build is v6)."""
    nums = [int(k[1:]) for k in cfg["bases"] if re.fullmatch(r"v\d+", k)]
    nums += [b["version"] for b in cfg["bases"].values()
             if isinstance(b.get("version"), int)]
    return f"v{max(nums, default=0) + 1}"


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
        spawn_qemu(acct, cfg, dev=True)
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
              f"Roll out: omni update-all")
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
    template was last captured — so `omni start` gets no_kiosk_reply until someone
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
        spawn_qemu(acct, cfg, dev=False, mode=resolve_mode(cfg))
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


# ---------- dev/debug base build (arm devkit disk: frida + Magisk) ----------
#
# The dev environment is NOT a separate flattened /system image anymore. It is
# the SHARED, immutable `base_arm` PLUS one extra virtio disk:
# `base_arm_devkit.qcow2` (attached to dev accounts as vdc). That disk is an
# ext4 filesystem BUILT ENTIRELY HOST-SIDE (no guest boot, no root, cross-
# platform via `mke2fs -d`) carrying:
#   * frida-server (android-arm64 — the base runs arm64 natively, no libndk),
#   * Magisk (the APK installer + the extracted arm64 magiskboot/magiskinit/…),
#   * the omni-* device scripts (hidden frida launch + root/frida hiding),
#   * a manifest.json (versions, hidden frida port, mount paths).
# `base_arm.qcow2` is NEVER modified. Root comes from a Magisk-patched boot
# living in the cheap dev SYSTEM OVERLAY (`base_arm_devsystem.qcow2`, COW on
# base_arm.qcow2) — see _patch_dev_boot(); the production base stays untouched.
# Only omni-agent (a dev-only dependency) ever selects it (`create --base dev`),
# and building it NEVER changes current_base.

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
_MAGISK_ARM64_LIBS = ("magiskboot", "magiskinit", "magiskpolicy",
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


def _stage_devkit_arm(frida_version, include_magisk, frida_port, label):
    """Assemble (host-side) the directory that becomes the devkit disk root:
    the android-arm64 frida-server ELF, the Magisk APK + its extracted arm64
    magiskboot/… binaries, the LF-normalized omni-* scripts, and manifest.json.
    Returns {"dir": <root>, ...}. NOTHING is pushed to a guest here — the whole
    disk is built from this directory with mke2fs."""
    import tempfile
    import lzma
    import zipfile
    stg = Path(tempfile.mkdtemp(prefix="omnidevkit_"))
    (stg / "bin").mkdir()
    out = {"dir": stg, "frida_version": frida_version, "frida_port": frida_port,
           "magisk": False, "magisk_version": None, "magiskboot": None,
           "tools": []}

    # frida-server (android-arm64 — native, no translation), .xz -> bare ELF.
    xz = stg / f"frida-server-{frida_version}-android-arm64.xz"
    url = (f"https://github.com/frida/frida/releases/download/{frida_version}/"
           f"frida-server-{frida_version}-android-arm64.xz")
    _download(url, xz, label)
    srv = stg / "frida-server"
    with lzma.open(xz) as zf, open(srv, "wb") as f:
        shutil.copyfileobj(zf, f)
    srv.chmod(0o755)
    xz.unlink()
    print(f"[{label}] frida-server {frida_version} (arm64) staged "
          f"({srv.stat().st_size} bytes)")

    # Magisk: keep the full APK (the on-device installer) AND extract the arm64
    # multicall binaries + the patch scripts. Best-effort; build continues if
    # the download fails (root/hiding then needs a manually-dropped Magisk).
    if include_magisk:
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
                for tool in _MAGISK_ARM64_LIBS:
                    member = f"lib/arm64-v8a/lib{tool}.so"
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
                  f"{apk_asset['name']} (arm64 "
                  f"{', '.join(t for t in _MAGISK_ARM64_LIBS if (stg/'bin'/t).exists())})")
            if missing:
                print(f"[{label}] NOTE: boot-patch files missing {missing} — "
                      f"--patch-boot may not work with this Magisk build.")
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
        "devkit": "omnidroid-dev-base-arm",
        "built": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "arch": "arm64-v8a",
        "frida_version": frida_version,
        "frida_port": frida_port,
        "frida_server": "frida-server",
        "frida_server_patched": "frida-server-patched (optional drop-in)",
        "magisk": bool(out["magisk"]),
        "magisk_version": out["magisk_version"],
        "magisk_apk": "magisk.apk" if out["magisk"] else None,
        "mount": DEVKIT_MOUNT,
        "work": DEVKIT_WORK,
        "root": "Magisk (patched boot in the dev system overlay)",
        "hide": "Magisk DenyList/Shamiko (hides root, Magisk, and frida)",
        "launch": "omni-fridad (start hidden) / omni-frida-stop",
        "note": "dev/debug base only; never shipped. Delivered as the vdc disk.",
    }
    (stg / "manifest.json").write_text(json.dumps(manifest, indent=2))
    return out


def build_dev_base(cfg, frida_version=DEFAULT_FRIDA_VERSION,
                   frida_port=DEFAULT_FRIDA_PORT, include_magisk=True,
                   keep_builder=False, patch_boot=False):
    """Build the arm dev environment WITHOUT touching base_arm: assemble the
    devkit disk (frida + Magisk + omni tools) fully host-side, create the cheap
    dev system overlay that will carry the Magisk-patched (rooted) boot, and
    register the 'dev' base. current_base is never changed.

    patch_boot=True additionally runs the Magisk boot patch on the dev overlay
    (roots it). That step edits the boot partition and is brick-risky, so it is
    OFF by default — see _patch_dev_boot()."""
    label = "build-dev-base"
    bases = cfg.get("bases", {})
    if ARM_BASE_TAG not in bases or base_type(bases[ARM_BASE_TAG]) != BASE_TYPE_ARM:
        fail("no_base", f"the arm base '{ARM_BASE_TAG}' is not registered; the "
                        f"dev base is built on top of it. Known: {list(bases)}")
    arm = bases[ARM_BASE_TAG]
    images = Path(cfg["images_dir"])
    for k in ("base_disk", "system", "data"):
        if not (images / arm[k]).exists():
            fail("no_base", f"arm base file missing: {images / arm[k]}")

    print(f"[{label}] staging devkit (frida {frida_version} arm64"
          f"{', + Magisk' if include_magisk else ', no Magisk'})...")
    staging = _stage_devkit_arm(frida_version, include_magisk, frida_port, label)

    try:
        # 1) The extra disk (vdc): a populated ext4 qcow2, built entirely on the
        #    host (no boot, no root). This is the whole "dev toolkit".
        devkit_disk = images / ARM_DEVKIT_DISK
        _build_ext4_qcow2(staging["dir"], devkit_disk, label)

        # 2) The dev system overlay: a copy of the provisioned arm system overlay
        #    (thin, still COW-backed by the immutable base_arm.qcow2). It will
        #    hold the Magisk-patched boot. base_arm.qcow2 is NEVER modified.
        devsystem = images / ARM_DEVSYSTEM_DISK
        if not devsystem.exists():
            shutil.copyfile(images / arm["system"], devsystem)
            print(f"[{label}] created dev system overlay {ARM_DEVSYSTEM_DISK} "
                  f"(COW on {arm['base_disk']}; base stays immutable)")

        rooted = False
        if patch_boot:
            rooted = _patch_dev_boot(cfg, images, devsystem, staging, label)
        else:
            print(f"[{label}] SKIPPED boot patch (root). The dev base is built "
                  f"and usable for tooling; to ROOT it (needed for frida to "
                  f"attach and for on-device hiding) run: "
                  f"omni build-dev-base --patch-boot  (brick-risky; see DEV-BASE.md)")

        # 3) Register the dev base: arm base + the devkit disk (+ rooted overlay).
        raw = read_config()
        dev_data = (ARM_DEVDATA_DISK if (images / ARM_DEVDATA_DISK).exists()
                    else arm["data"])
        raw.setdefault("bases", {})[DEV_BASE_TAG] = {
            "type": BASE_TYPE_ARM,
            "base_disk": arm["base_disk"],
            "system": ARM_DEVSYSTEM_DISK,
            "data": dev_data,
            "efivars": arm.get("efivars", ARM_BASE_EFIVARS),
            "devkit": ARM_DEVKIT_DISK,
            "src": "base_arm + devkit disk (frida + Magisk + omni tools)",
            "notes": (f"arm dev base: base_arm + {ARM_DEVKIT_DISK} (vdc) with "
                      f"frida {frida_version} (arm64) + Magisk"
                      f"{' [rooted]' if rooted else ' [root pending: --patch-boot]'}"
                      f". omni-agent only; NOT shipped. hidden frida port "
                      f"{frida_port}."),
            "devkit_manifest": {
                "frida_version": frida_version,
                "frida_port": frida_port,
                "magisk": bool(staging.get("magisk")),
                "magisk_version": staging.get("magisk_version"),
                "rooted": rooted,
                "tools": ["frida-server", "omni-fridad", "omni-frida-stop",
                          "omni-hide", "omni-magisk-setup"],
            },
        }
        # HARD RULE: do NOT change current_base — the shipped product stays on
        # the production base. The dev base is opt-in via `--base dev` only.
        CONFIG_PATH.write_text(json.dumps(raw, indent=2))
        print(f"[{label}] DONE. Registered base '{DEV_BASE_TAG}' = base_arm + "
              f"{ARM_DEVKIT_DISK} (vdc){' [ROOTED]' if rooted else ''}")
        print(f"[{label}] current_base UNCHANGED (still '{raw.get('current_base')}').")
        print(f"[{label}] use it: omni create <name> --base {DEV_BASE_TAG}")
        return DEV_BASE_TAG
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
# headless, the kiosk cannot dismiss a system dialog, and `omni start` needs adb
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


def cmd_bake_game(args):
    """Bake the game APK into an arm image as a pre-installed system app, so a
    production instance ships with it and boots straight into it.

    Build-machine command (needs e2fsprogs + ~6 GiB scratch), same shape as
    brand-base. Point it at the branded production candidate:

        omni bake-game roblox.apk --image ~/OmniImages/base_arm_branded.qcow2
    """
    cfg = load_config()
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
        name = getattr(args, "name", None) or "OmniGame"
        err = _bake_apk_into_product(str(raw), fs_off, str(apk), name, label)
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
        result = {"ok": True, "image": str(disk), "apk": str(apk),
                  "app_dir": f"/product/app/{name}", "package": pkg,
                  "note": ("Boot a FRESH account on this image: the game should "
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
                 f"running on it. Stop it first (omni stop {live[0]}), or drop "
                 f"--in-place to write a new image alongside.")
    out = Path(getattr(args, "out", None) or
               (disk if in_place else disk.with_name(
                   disk.stem + "_branded.qcow2")))

    free = shutil.disk_usage(images).free
    need = 6 * 1024 ** 3
    if free < need:
        return fail("engine_error",
                    f"need ~{need // 1024**3} GiB free in {images} for the raw "
                    f"round-trip, have {free // 1024**3} GiB")

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


def _patch_dev_boot(cfg, images, devsystem, staging, label):
    """Root the dev system overlay by Magisk-patching its boot partition (vda6),
    keeping base_arm.qcow2 immutable. Bootstrap-free + cross-platform: export the
    overlay to raw (merged through its base backing), pull the boot image out via
    GPT, patch it with magiskboot INSIDE a throwaway arm guest (magiskboot only
    needs to run on a FILE — no in-guest root), write the patched image back, and
    re-import to qcow2. Returns True on success. Brick-risky + must be verified on
    a real boot; any failure leaves the UNROOTED overlay intact and returns False.
    """
    if not staging.get("magisk") or not staging.get("magiskboot"):
        print(f"[{label}] cannot patch boot: Magisk (magiskboot) was not staged.")
        return False
    import tempfile
    # The offline method exports the full disk to raw (~5 GiB) then reimports a
    # patched qcow2 (~2 GiB) — needs real headroom. Fail cleanly (don't fill the
    # disk) if it isn't there.
    vsize = 0
    try:
        info = json.loads(subprocess.run(
            [qemu_bin("qemu-img"), "info", "--output=json", str(devsystem)],
            capture_output=True, text=True, check=True).stdout)
        vsize = int(info.get("virtual-size", 0))
    except Exception:
        pass
    need = int(vsize * 1.6) or (7 * 1024**3)
    free = shutil.disk_usage(images).free
    if free < need:
        print(f"[{label}] NOT enough free disk for the offline boot patch: need "
              f"~{need // 1024**3} GiB, have {free // 1024**3} GiB free in "
              f"{images}. Free space (or root on a machine with headroom) and "
              f"re-run `omni build-dev-base --patch-boot`. Overlay left UNROOTED.")
        return False
    work = Path(tempfile.mkdtemp(prefix="omni_bootpatch_"))
    bname = "_devbootpatch"
    d = account_dir(bname)
    try:
        full = work / "vda.raw"
        print(f"[{label}] exporting dev overlay -> raw (merged) to read boot...")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-O", "raw",
                        str(devsystem), str(full)], check=True)
        part = _gpt_partition(full, "boot")
        if not part:
            print(f"[{label}] boot partition not found in GPT; aborting patch.")
            return False
        off, size = part
        boot_img = work / "boot.img"
        with open(full, "rb") as sf, open(boot_img, "wb") as bf:
            sf.seek(off)
            bf.write(sf.read(size))
        print(f"[{label}] boot partition @ {off} ({size} bytes) extracted")

        # Boot a throwaway arm builder (plain base_arm) to run Magisk's
        # boot_patch.sh. It patches a boot.img FILE using the pushed toolset, so
        # NO in-guest root is needed (magiskboot/magiskinit run as the shell user
        # from /data/local/tmp — the same domain frida-server runs in).
        if d.exists():
            shutil.rmtree(d)
        d.mkdir(parents=True)
        adb_port, qmp_port, vnc_port = allocate_ports(cfg)
        acct = {"name": bname, "base": ARM_BASE_TAG,
                "adb_port": adb_port, "qmp_port": qmp_port, "vnc_port": vnc_port,
                "first_boot_done": True}
        save_account(acct)
        arm = cfg["bases"][ARM_BASE_TAG]
        shutil.copyfile(images / arm["system"], d / "system.qcow2")
        shutil.copyfile(images / arm["data"], d / "data.qcow2")
        shutil.copyfile(images / arm.get("efivars", ARM_BASE_EFIVARS),
                        d / "efivars.fd")

        print(f"[{label}] booting arm builder to run boot_patch.sh (headless)")
        spawn_qemu(acct, cfg, dev=True)
        if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT, label):
            print(f"[{label}] builder boot failed; aborting patch.")
            return False
        W = "/data/local/tmp/omni-bootpatch"
        adb(acct, "shell", f"rm -rf {W}; mkdir -p {W}", timeout=15)
        adb(acct, "push", str(boot_img), f"{W}/boot.img", timeout=300)
        # Push the full Magisk patch toolset (magiskboot, magiskinit, magisk,
        # init-ld, stub.apk, boot_patch.sh, util_functions.sh) — boot_patch.sh
        # injects magisk/init-ld/stub into the ramdisk and swaps init.
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
        patched = work / "new-boot.img"
        adb(acct, "pull", f"{W}/new-boot.img", str(patched), timeout=120)
        _shutdown(acct, label)
        if not patched.exists() or patched.stat().st_size == 0:
            print(f"[{label}] patched boot not produced; aborting (overlay left "
                  f"UNROOTED).")
            return False
        if patched.stat().st_size > size:
            print(f"[{label}] patched boot ({patched.stat().st_size}) exceeds "
                  f"partition ({size}); aborting to avoid corruption.")
            return False
        # Write the patched boot back into the raw disk (same offset, in place)
        # then re-import to qcow2 (standalone, dev-only).
        with open(full, "r+b") as sf, open(patched, "rb") as pf:
            sf.seek(off)
            sf.write(pf.read())
        print(f"[{label}] re-importing rooted disk -> {devsystem.name}")
        tmp_qcow = devsystem.with_suffix(".rooted.tmp.qcow2")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-O", "qcow2", "-c",
                        str(full), str(tmp_qcow)], check=True)
        tmp_qcow.replace(devsystem)
        print(f"[{label}] boot patched (Magisk). VERIFY on a real boot: create a "
              f"dev account, `omni adb <n> -- shell su -c id` should show uid=0.")
        return True
    except Exception as e:
        print(f"[{label}] boot patch FAILED ({type(e).__name__}: {e}); the dev "
              f"overlay is left UNROOTED (safe).")
        return False
    finally:
        if d.exists():
            shutil.rmtree(d, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)


def cmd_build_dev_base(args):
    ensure_qemu()
    cfg = load_config()
    tag = build_dev_base(cfg, frida_version=args.frida_version,
                         frida_port=args.frida_port,
                         include_magisk=not args.no_magisk,
                         keep_builder=args.keep_builder,
                         patch_boot=getattr(args, "patch_boot", False))
    if getattr(args, "json", False):
        raw = read_config()
        emit_json({"ok": True, "base": tag, "devkit_disk": ARM_DEVKIT_DISK,
                   "current_base": raw.get("current_base"),
                   "devkit": raw["bases"][tag].get("devkit_manifest")})


def cmd_use_base(args):
    """Set the default base for NEW accounts (mode switch: e.g. a dev base
    without the game vs a production base with the game pre-installed).
    Does not touch existing accounts (use update-all for that)."""
    raw = read_config()
    visible = visible_bases(raw)
    if args.tag not in visible:
        # A locked dev base reports the SAME "no base" as a nonexistent one:
        # for a customer build the dev base genuinely does not exist, and a
        # distinct error would just advertise it.
        sys.exit(f"error: no base '{args.tag}'. Known: {list(visible)}")
    raw["current_base"] = args.tag
    CONFIG_PATH.write_text(json.dumps(raw, indent=2))
    print(f"current base = {args.tag} "
          f"({raw['bases'][args.tag].get('notes','')})")


CONTRACT_VERSION = "1.0"


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
    rep = {"engine": "omnidroid", "contract": CONTRACT_VERSION,
           "arch_aware": True, "host_arch": host_arch_token(),
           "bases": by_arch, "current_base": raw.get("current_base"),
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
           },
           "commands": ["version", "create", "start", "stop", "remove", "list",
                        "install", "run-app", "adb", "screenshot", "logcat",
                        "capture", "autocap", "test-apk", "doctor", "bases",
                        "use-base"],
           "ok": True}
    if getattr(args, "json", False):
        emit_json(rep)
    else:
        print(json.dumps(rep, indent=2))


def cmd_bases(args):
    raw = read_config()
    cur = raw["current_base"]
    # Dev bases are omitted unless OMNI_DEV_MODE=1: this is the list the product
    # GUI renders its base picker from, so anything listed here is reachable by
    # a customer's click.
    listed = visible_bases(raw)
    if getattr(args, "json", False):
        bases = [{"tag": tag, "arch": arch_of_base(b), "type": base_type(b),
                  "game_package": raw.get("base_game", {}).get(tag),
                  "dev": base_is_dev(b),
                  "notes": b.get("notes", "")}
                 for tag, b in listed.items()]
        emit_json({"current_base": cur, "bases": bases,
                   "dev_mode": dev_mode_enabled(), "ok": True})
        return
    for tag, b in listed.items():
        game = raw.get("base_game", {}).get(tag)
        mark = " *" if tag == cur else "  "
        print(f"{mark}{tag}: {b.get('notes','')}  [{arch_of_base(b)}]"
              + (f"  [game: {game}]" if game else ""))
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


def _ksm_wait_settle(settle_secs, timeout=600):
    """Block until KSM pages_sharing stops moving (<1% drift held for
    settle_secs). Returns the settled pages_sharing value."""
    last = None
    stable_since = None
    start = time.time()
    while time.time() - start < timeout:
        cur = ksm_stats().get("pages_sharing", 0)
        if last is not None and abs(cur - last) <= max(last, 100) * 0.01:
            stable_since = stable_since or time.time()
            if time.time() - stable_since >= settle_secs:
                return cur
        else:
            stable_since = None
        last = cur
        time.sleep(10)
    return last or 0


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
        acct = build_acct(name, cfg, dev=False)
        pkg = acct.get("game_package")
        mode = resolve_mode(cfg, args.mode)
        spawn_qemu(acct, cfg, dev=False, mode=mode)
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
           "game_package": a.get("game_package")}
    if pid:
        try:
            run = json.loads((runtime_dir(a["name"]) /
                              "run.json").read_text())
            rec["mode"] = run.get("mode")
            rec["started"] = run.get("started")
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


def cmd_install(args):
    acct = load_account(args.name)
    json_mode = getattr(args, "json", False)
    # adb is a per-invocation client: nothing has necessarily `adb connect`ed to
    # this instance yet, and every `adb -s <serial> …` then fails with "device
    # not found". That is exactly the dev loop (`start` -> `install` -> `play`
    # are three separate processes), so connect first.
    adb_connect(acct)
    arch = acct_arch(acct)
    abi = _resolve_install_abi(acct, getattr(args, "abi", None),
                               getattr(args, "no_abi_pin", False))
    print(f"[install {args.name}] installing {args.apk}"
          + (f" (--abi {abi})" if abi else " (no ABI pin)") + " ...")
    pkg = apk_package_name(args.apk)
    r = _abi_install(acct, args.apk, abi)
    out = (r.stdout + r.stderr).strip()
    print(f"[install {args.name}] {out}")
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
        print(f"[install {args.name}] existing build of {pkg} blocks the update "
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
        print(f"[install {args.name}] uninstall {pkg}: {(u.stdout + u.stderr).strip()[:200]}")
        # Point the kiosk back at the game so it relaunches the fresh build (the
        # success path below re-asserts this; setting it here also leaves the
        # kiosk correctly targeted if the reinstall itself then fails).
        adb(acct, "shell", "settings", "put", "global", "omni_game_package", pkg, timeout=15)
        r = _abi_install(acct, args.apk, abi)
        out = (r.stdout + r.stderr).strip()
        print(f"[install {args.name}] reinstall: {out}")
    if "Success" not in out:
        fail("install_failed", f"adb install failed: {out[:400]}")
    abi_installed = installed_primary_abi(acct, pkg) if pkg else None
    native_bridge_used = _is_arm_abi(abi_installed) and arch == "x86"
    if pkg:
        acct["game_package"] = pkg
        save_account(acct)
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", pkg, timeout=10)
        print(f"[install {args.name}] game package = {pkg} "
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

def _foreground(acct):
    try:
        r = adb(acct, "shell", "dumpsys", "activity", "activities",
                timeout=10)
        m = re.search(r"topResumedActivity=ActivityRecord\{\S+ \S+ (\S+)",
                      r.stdout)
        return m.group(1) if m else None
    except Exception:
        return None


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
                     f"(omni start {args.name}) or: omni view {args.name} "
                     f"--start")
        acct = build_acct(args.name, cfg, dev=args.dev)
        spawn_qemu(acct, cfg, dev=args.dev, mode=resolve_mode(cfg, args.mode))
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


def _pidof(acct, pkg):
    """First numeric pid of pkg in the guest, or None. Cheap; polled on a
    background thread during capture to build a process lifecycle timeline."""
    if not pkg:
        return None
    try:
        out = adb(acct, "shell", "pidof", pkg, timeout=8).stdout
    except Exception:
        return None
    for tok in out.split():
        if tok.isdigit():
            return int(tok)
    return None


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
    if auto and not acct_is_dev(acct):
        return fail("dev_base_required",
                    f"auto screenshots are a dev-base feature; account "
                    f"'{args.name}' is on base '{acct.get('base')}'. Recreate it "
                    f"with --base dev to use --auto.")
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
# or an explicit `omni autocap --ensure` never stacks a second recorder — so the
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
    """Idempotently ensure the continuous recorder is running for a DEV account.
    No-op (and returns running=False) on non-dev bases — the feature is dev-only.
    Returns a small status dict."""
    name = acct["name"]
    if not acct_is_dev(acct):
        return {"running": False, "reason": "not_dev_base",
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
            print(f"[{label}] auto-screenshots ON (dev base) -> {r['out_dir']}")
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
        if not acct_is_dev(acct):
            return fail("dev_base_required",
                        f"auto screenshots are a dev-base feature; account "
                        f"'{args.name}' is on base '{acct.get('base')}'.")
        r = ensure_autocap(acct, out_dir=getattr(args, "out", None),
                           force=getattr(args, "restart", False))
    out = {"name": args.name, "ok": True, **r}
    if json_mode:
        emit_json(out)
    else:
        print(json.dumps(out, indent=2))


def cmd_test_apk(args):
    """One-shot dev harness: ensure a FRESH session with NO app pre-baked
    (v3 dev base, kiosk), install the given APK, let the kiosk launch it,
    and report machine-readable JSON. Headless by default. After this,
    drive with: omni screenshot / logcat / adb.

    Emits a single JSON line: {account, adb_port, qmp_port, package,
    installed, launched, foreground, pid, mode}."""
    ensure_qemu()
    cfg = load_config()
    name = args.name
    result = {"account": name}
    fresh = not (account_dir(name) / "account.json").exists()
    if fresh and not args.reuse:
        # Create a clean dev account on the current (dev) base.
        _make_persistent_arm_account(name, cfg)
    acct = load_account(name)
    result["base"] = acct["base"]
    result["arch"] = acct_arch(acct)
    if not running_pid(name):
        mode = resolve_mode(cfg, args.mode)
        spawn_qemu(acct, cfg, dev=False, mode=mode)
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
                 "run 'omni install' first")
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
    """Switch a DEV instance's visible UI. `--show kiosk` (default) foregrounds the
    kiosk and stops the Magisk app; `--show magisk` opens the Magisk manager (root
    UI) so the agent/user can manage root, then switch back with `--show kiosk`."""
    acct = load_account(args.name)
    if not acct_is_dev(acct):
        out = {"ok": False, "error": "not_dev",
               "message": f"'{args.name}' is not a dev instance (no Magisk UI to toggle)."}
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
                               f"name: omni adb {args.name} -- shell pm list packages | grep -i magisk "
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
                               f"Back to kiosk: omni dev-ui {args.name} --show kiosk")}
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


def redact_token(tok):
    """Never print a token. Enough tail to tell two tokens apart in a log, never
    enough to use one."""
    if not tok:
        return None
    return f"<{len(tok)} chars, ...{tok[-6:]}>"


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

    1. the instance name IS a saved account username (`omni start <username>`) —
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
                          f"(omni kioskify {name})"}
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


def _ensure_booted(acct, cfg, label, timeout=None, accel=None):
    """Boot the instance if it isn't up, and block until Android is ready.
    Returns (ok, first_boot)."""
    first = not acct.get("first_boot_done")
    if running_pid(acct["name"]):
        adb_connect(acct)
        if adb_getprop(acct, "sys.boot_completed") == "1":
            return True, first
        print(f"[{label}] instance is up but Android is still booting; waiting")
    else:
        dev = first          # first boot always uses the dev profile
        spawn_qemu(acct, cfg, dev=dev, mode=None if dev else resolve_mode(cfg),
                   accel=accel)
        # Same rule as `start`: the recorder attaches at spawn so a dev session
        # has screenshots of the boot screen itself.
        maybe_start_autocap(acct, label)
    t = timeout or (FIRST_BOOT_TIMEOUT if first else NORMAL_BOOT_TIMEOUT)
    if not wait_for_boot(acct, t, label, first_boot=first):
        return False, first
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
    _devkit_activate(acct, label)
    return True, first


def _token_flag_given(args):
    """True iff a --token/--token-file/--token-stdin flag was explicitly
    passed, even if it resolves to an empty cookie (a blank file/stdin/arg).

    `is not None`, not truthiness: `--token ""` must still count as GIVEN.
    Used by `omni login` to fail fast on an unusable token rather than
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

    Either way: ready to use as `omni start <username> --place`."""
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
        print(f"\nplay as this account:  omni start {r['username']} "
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

    The token always comes from `omni login`; this command only ever touches
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


def main():
    p = argparse.ArgumentParser(prog="omnidroid")
    sub = p.add_subparsers(dest="cmd", required=True)

    def _token_args(parser):
        # No --account flag: the positional IS the account username (from
        # `omni login`), so its cookie is looked up automatically. These flags
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
                            "`omni start <username> [--place <id>]`. The "
                            "instance is auto-created (ephemeral); run two "
                            "for two accounts at once. Dev and production "
                            "alike")
    s.add_argument("name", metavar="username",
                   help="a saved Roblox account username (from `omni login`). "
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
    s.add_argument("--dev", action="store_true",
                   help="launch the instance on the DEV base (frida+Magisk); "
                        "dev-only, refused without OMNI_DEV_MODE")
    s_win = s.add_mutually_exclusive_group()
    s_win.add_argument("--window", action="store_true",
                       help="open a live window even in --json mode (two "
                            "starts = two accounts side by side)")
    s_win.add_argument("--no-window", dest="no_window", action="store_true",
                       help="do not open a window (headless; watch via "
                            "`omni view` or capture)")
    s.add_argument("--mode", choices=list(MODES), default=None,
                   help="RAM/CPU tier (all headless): playable 4G/4c | "
                        "hard 3G/4c | brutal 2G/2c. Default: playable")
    s.add_argument("--mem", type=int, default=None,
                   help="override guest RAM in MB")
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

    bg = sub.add_parser("bake-game",
                        help="bake a game APK into an arm image as a "
                             "pre-installed SYSTEM app, so production ships "
                             "with it. BUILD-machine command (e2fsprogs + "
                             "~6 GiB scratch)")
    bg.add_argument("apk")
    bg.add_argument("--base", default="arm", help="base tag (default: arm)")
    bg.add_argument("--image", default=None,
                    help="operate on this image instead of the base's "
                         "(e.g. the branded production candidate)")
    bg.add_argument("--name", default="OmniGame",
                    help="directory name under /product/app (default OmniGame)")
    bg.add_argument("--json", action="store_true")
    bg.set_defaults(func=cmd_bake_game)

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
                             "omni start <username> --place <id>")
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
                         "e.g. `omni accounts --set-custom-name erin7231 "
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

    a = sub.add_parser("adb")
    a.add_argument("name")
    a.add_argument("rest", nargs=argparse.REMAINDER)
    a.set_defaults(func=cmd_adb)

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

    bdb = sub.add_parser("build-dev-base",
                         help="build the arm dev base = base_arm + the "
                              "base_arm_devkit.qcow2 extra disk (frida + Magisk "
                              "+ omni tools, attached to dev accounts as vdc). "
                              "base_arm stays immutable; current_base unchanged; "
                              "omni-agent only, NOT shipped")
    bdb.add_argument("--frida-version", default=DEFAULT_FRIDA_VERSION,
                     dest="frida_version",
                     help=f"frida-server version (arm64) to stage "
                          f"(default {DEFAULT_FRIDA_VERSION})")
    bdb.add_argument("--frida-port", type=int, default=DEFAULT_FRIDA_PORT,
                     dest="frida_port",
                     help=f"hidden frida-server loopback port "
                          f"(default {DEFAULT_FRIDA_PORT}, deliberately not 27042)")
    bdb.add_argument("--no-magisk", action="store_true", dest="no_magisk",
                     help="skip staging Magisk (root + on-device hiding then "
                          "unavailable until a Magisk APK is dropped in)")
    bdb.add_argument("--patch-boot", action="store_true", dest="patch_boot",
                     help="ALSO Magisk-patch the dev system overlay's boot to "
                          "ROOT it (needed for frida to attach). Edits the boot "
                          "partition — brick-risky; verify on a real boot")
    bdb.add_argument("--keep-builder", action="store_true", dest="keep_builder",
                     help="keep the throwaway builder account dir (debug)")
    bdb.add_argument("--json", action="store_true")
    bdb.set_defaults(func=cmd_build_dev_base)

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
    vw.add_argument("--dev", action="store_true",
                    help="with --start: use the dev boot profile")
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
    # `omni view` as a detached child). Not for direct use.
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
