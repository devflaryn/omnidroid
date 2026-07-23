# omnidroid/bases.py
"""Base-image registry, arch resolution, and the dev-base gate."""
import json
import os
import re
from pathlib import Path

from omnidroid import config
from omnidroid.config import CONFIG_PATH, IS_ARM64_HOST, images_dir
from omnidroid.output import fail


BASE_TYPE_X86 = "x86-bliss"


BASE_TYPE_ARM = "arm-uefi"


def base_type(base):
    return base.get("type", BASE_TYPE_X86)


def acct_base_is_arm(acct):
    """True if this account's base is arm-uefi (reads config; safe/cheap)."""
    from omnidroid.engine import read_config
    try:
        b = (read_config().get("bases") or {}).get(acct.get("base"), {})
        return base_type(b) == BASE_TYPE_ARM
    except Exception:
        return False


def base_is_dev(base):
    """True if a base entry carries the dev devkit disk (frida + Magisk tools).
    The dev base is arm-uefi + a 'devkit' field naming the extra vdc disk."""
    return bool(base.get("devkit"))


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
    from omnidroid.engine import read_config
    if acct.get("dev"):
        return True
    try:
        b = (read_config().get("bases") or {}).get(acct.get("base"), {})
        return base_is_dev(b)
    except Exception:
        return False


def arch_of_base(base):
    """Canonical arch token for a base entry: 'x86' | 'arm'."""
    return "arm" if base_type(base) == BASE_TYPE_ARM else "x86"


def acct_arch(acct):
    """Canonical arch token for an account: 'x86' | 'arm'."""
    return "arm" if acct_base_is_arm(acct) else "x86"


ARM_BASE_DISK = "base_arm.qcow2"            # pristine system, shared backing


ARM_BASE_SYSTEM = "base_arm_system.qcow2"   # provisioned overlay (FBE keys)


ARM_BASE_DATA = "base_arm_data.qcow2"       # provisioned /data (kiosk + DO)


ARM_BASE_EFIVARS = "base_arm_efivars.fd"    # provisioned UEFI vars


ARM_BASE_TAG = "arm"


X86_BASE_DISK = "base_x86.qcow2"


X86_BASE_KERNEL = "base_x86.kernel"


X86_BASE_INITRD = "base_x86.initrd.img"


X86_BASE_TAG = "x86"


DEV_BASE_TAG = "dev"


ARM_DEVKIT_DISK = "base_arm_devkit.qcow2"


ARM_DEVSYSTEM_DISK = "base_arm_devsystem.qcow2"


ROOTED_MARKER = " [rooted]"


ROOT_PENDING_MARKER = " [root pending: --patch-boot]"


ARM_DEVDATA_DISK = "base_arm_devdata.qcow2"


DEVKIT_MOUNT = "/mnt/omni-devkit"          # ro mount of vdc (source of truth)


DEVKIT_WORK = "/data/local/tmp/omni-devkit"  # exec-capable activated copy


DEVKIT_MANIFEST_GUEST = DEVKIT_WORK + "/manifest.json"


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
    from omnidroid.engine import read_config
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
    from omnidroid.engine import read_config, DEFAULT_SRC
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


def _next_base_tag(cfg):
    """Next build tag. Counts legacy vN tags AND the internal 'version'
    field of versionless entries (base_x86), so a rebuild on the canonical
    base continues its lineage (x86 at version 5 -> next build is v6)."""
    nums = [int(k[1:]) for k in cfg["bases"] if re.fullmatch(r"v\d+", k)]
    nums += [b["version"] for b in cfg["bases"].values()
             if isinstance(b.get("version"), int)]
    return f"v{max(nums, default=0) + 1}"
