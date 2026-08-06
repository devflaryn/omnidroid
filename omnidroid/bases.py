# omnidroid/bases.py
"""Base-image registry, arch resolution, and the debug (devkit) attachment.

There is no separate "dev base". Every shipped base is DUAL-USE: it boots as
the production image, and omni-agent can debug on that same image. The three
capabilities that used to be fused into one `dev` base tag are now separate:

  root     a property of the BASE IMAGE (Magisk-patched boot, baked in, always
           present). Marked by `"rooted": true` on the base entry.
  hiding   a property of the shipped /data + an idempotent per-boot enforce
           step (Zygisk + Enforce DenyList). Active in production too.
  toolkit  the devkit disk (frida-server + the omni-* scripts) — an ATTACHABLE
           disk, opt-in per boot via `omni start --debug` / agent debug=true.

So "debug" is a BOOT OPTION, not a base. The same account can boot production
on one run and debug on the next.
"""
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


def base_is_rooted(base):
    """True if this base image ships a Magisk-patched (rooted) boot.

    Root is baked into the shipped image, so this is a property of the BASE,
    not of the account or of how it was booted. Root-needing operations check
    this to give an actionable 'this base is not rooted yet' message instead of
    failing obscurely."""
    return bool(base.get("rooted"))


def _truthy_env(name):
    return str(os.environ.get(name, "")).strip().lower() in (
        "1", "true", "yes", "on")


DEBUG_ENV = "OMNI_DEBUG_BOOT"


def _debug_boot_requested(args=None):
    """Whether THIS boot should attach the devkit disk (frida + omni-* tools).

    A per-boot option, never a property of the account or the base: `--debug`
    on the command, or OMNI_DEBUG_BOOT=1 to default a whole session to debug
    boots. Production is the default in both cases."""
    if getattr(args, "debug", False):
        return True
    return _truthy_env(DEBUG_ENV)


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


# The ROOTED shipped images. `base_arm_system_rooted.qcow2` is a THIN COW
# overlay of the current production system overlay carrying only the Magisk-
# patched boot partition (vda6) — the production lineage is preserved, not
# flattened. `base_arm_data_rooted.qcow2` is the production /data plus the
# Magisk policy (root_access=3, zygisk=1, denylist=1, adb shell granted
# Forever, the game on the DenyList) so root is headless from first boot and
# hidden from the game in production. Both are preferred when present; a
# deployment without them registers the unrooted images and still works.
ARM_ROOTED_SYSTEM = "base_arm_system_rooted.qcow2"


ARM_ROOTED_DATA = "base_arm_data_rooted.qcow2"


# The GAME-BAKED /data: a THIN COW overlay of the rooted /data carrying the
# game APK (installed as an updated system app, so it lands in /data/app) and
# `omni_game_package` in the settings database.
#
# Why /data and not the system image: `omni bake-game` writes the APK into
# /product/app inside the 2.3 GB system image, which needs ~6 GiB of scratch
# and produces a new base — per Roblox update. Roblox updates often. An
# updated system app in /data does the same job, the package name never
# changes across versions, and the overlay is only as big as the APK.
ARM_GAME_DATA = "base_arm_data_game.qcow2"

# Echoed by the in-guest bake script only after every step has been verified,
# so the caller can refuse to capture a /data where a step silently did
# nothing. The two markers are deliberately NOT substrings of one another —
# the caller tests with `in`, and a failure marker that contained the success
# marker would report the opposite of what happened.
GAME_BAKE_OK = "OMNI_GAME_BAKE_OK"
GAME_BAKE_INSTALL_FAILED = "OMNI_GAME_BAKE_APK_REJECTED"


def data_bake_source(base):
    """The /data image a game bake must overlay — always the PRISTINE one.

    Anti-chaining rule, and the whole reason this is a function. The bake's
    output becomes the base's `data`, so re-baking (which is what a Roblox
    update is) would otherwise overlay the previous bake, and every update
    would add a link to the chain that still carries the superseded APK. Going
    back to the rooted image each time keeps a re-bake exactly as cheap as the
    first one and leaves no dead versions behind.

    `root_manifest.rooted_data` is the recorded pristine image on a rooted
    base; an unrooted deployment falls back to its own `data`, which has never
    been baked over."""
    manifest = base.get("root_manifest") or {}
    return manifest.get("rooted_data") or base.get("data")


def resolve_bake_package(explicit, tag, cfg):
    """Which package the bake should register as the game.

    Deliberately does NOT read the APK. apk_package_name() shells out to the
    Android SDK's aapt2, which is not installed on every host, and a Roblox
    update never changes the package name anyway — so making the bake depend
    on it would fail the common case for no benefit. An explicit --package
    still wins for a genuinely different app."""
    if explicit:
        return explicit
    try:
        return (cfg or {}).get("base_game", {}).get(tag)
    except Exception:  # noqa: BLE001 — no/!dict config
        return None


def build_game_bake_script(pkg, apk_guest_path):
    """The in-guest script the bake runs as root.

    `apk_guest_path` None bakes ONLY the setting (the kiosk fix) and touches
    no APK — worth keeping separable, because that half needs no 131 MB push
    and is the part that actually stops the Magisk guess.

    `pm install -r -d`: -r REPLACES the pre-installed system app instead of
    failing with INSTALL_FAILED_ALREADY_EXISTS, and -d permits a downgrade so
    rolling back a bad Roblox build does not need a base rebuild either.

    The install result is CHECKED. `pm install` exits 0 and prints its verdict
    on stdout, so the exit status says nothing — the first version of this
    script therefore captured a /data whose install had been rejected:

        Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package
        com.roblox.client signatures do not match newer version; ignoring!]
        OMNI_GAME_BAKE_OK

    A replacement APK must be signed with the SAME key as the one baked into
    the system image; an officially-signed Roblox build will not install over
    a re-signed one, and vice versa. That is a real constraint on the update
    workflow, and it has to surface as a failed bake rather than as a silently
    unchanged image."""
    steps = []
    if apk_guest_path:
        steps.append(
            f"pm install -r -d {apk_guest_path} 2>&1 | grep -q Success "
            f"|| {{ echo {GAME_BAKE_INSTALL_FAILED}; "
            f"pm install -r -d {apk_guest_path} 2>&1; exit 1; }}")
        steps.append(f"rm -f {apk_guest_path}")
    steps.append(f"settings put global omni_game_package {pkg}")
    # Read it back before claiming success: `settings put` is asynchronous
    # through system_server and a silent failure here would be captured into
    # the image and ship.
    steps.append(f'[ "$(settings get global omni_game_package)" = "{pkg}" ] '
                 f"&& echo {GAME_BAKE_OK}")
    return "; ".join(steps)


X86_BASE_DISK = "base_x86.qcow2"


X86_BASE_KERNEL = "base_x86.kernel"


X86_BASE_INITRD = "base_x86.initrd.img"


X86_ROOTED_INITRD = "base_x86_rooted.initrd.img"


X86_BASE_TAG = "x86"


# The devkit disk: frida-server + the omni-* device scripts, per architecture.
# It belongs to NO base entry — it is attached as vdc only on a `--debug` boot,
# so a production instance's hardware profile is unchanged.
DEVKIT_DISKS = {"arm": "base_arm_devkit.qcow2",
                "x86": "base_x86_devkit.qcow2"}


ARM_DEVKIT_DISK = DEVKIT_DISKS["arm"]


X86_DEVKIT_DISK = DEVKIT_DISKS["x86"]


ROOTED_MARKER = " [rooted]"


ROOT_PENDING_MARKER = " [root pending: run `omni root-base`]"


DEVKIT_MOUNT = "/mnt/omni-devkit"          # ro mount of vdc (source of truth)


DEVKIT_WORK = "/data/local/tmp/omni-devkit"  # exec-capable activated copy


DEVKIT_MANIFEST_GUEST = DEVKIT_WORK + "/manifest.json"


def devkit_disk_name(arch):
    """Devkit disk filename for a canonical arch token ('arm' | 'x86')."""
    return DEVKIT_DISKS.get(arch, DEVKIT_DISKS["arm"])


def devkit_disk_for_base(images, base):
    """Absolute path to the devkit disk this base would use, or None if that
    disk has not been built yet. images may be str or Path."""
    p = Path(images) / devkit_disk_name(arch_of_base(base))
    return p if p.exists() else None


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
        # Prefer the ROOTED initrd when it has been built: same shipped disk,
        # a Magisk-patched ramdisk, so the x86 base is dual-use too.
        rooted = (images / X86_ROOTED_INITRD).exists()
        bases[X86_BASE_TAG] = {"type": BASE_TYPE_X86,
                               "disk": X86_BASE_DISK,
                               "kernel": X86_BASE_KERNEL,
                               "initrd": (X86_ROOTED_INITRD if rooted
                                          else X86_BASE_INITRD),
                               "rooted": rooted,
                               "src": raw.get("default_src", DEFAULT_SRC),
                               "notes": "auto-registered canonical x86 base "
                                        "from images_dir"
                                        + (ROOTED_MARKER if rooted
                                           else ROOT_PENDING_MARKER)}
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
        # Prefer the ROOTED matched pair when it has been built. It is the same
        # production lineage — the system overlay is a thin COW child of the
        # unrooted one carrying only the Magisk-patched boot — so this is a
        # dual-use base, not a second "dev" base.
        rooted = ((images / ARM_ROOTED_SYSTEM).exists()
                  and (images / ARM_ROOTED_DATA).exists())
        bases[ARM_BASE_TAG] = {
            "type": BASE_TYPE_ARM,
            "base_disk": ARM_BASE_DISK,
            "system": ARM_ROOTED_SYSTEM if rooted else ARM_BASE_SYSTEM,
            "data": ARM_ROOTED_DATA if rooted else ARM_BASE_DATA,
            "efivars": ARM_BASE_EFIVARS,
            "rooted": rooted,
            "src": "https://github.com/jqssun/android-lineage-qemu "
                   "(LineageOS 23.2 arm64, virtio_arm64only)",
            "notes": "auto-registered arm64/UEFI base (LineageOS 23.2, "
                     "kiosk+device-owner provisioned matched pair)"
                     + (ROOTED_MARKER if rooted else ROOT_PENDING_MARKER)}
        new.append(ARM_BASE_TAG)
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
        return [str(images / base[k]) for k in keys
                if base.get(k) and not (images / base[k]).exists()]
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
            fail("no_base", f"no base '{base_tag}'. Known: {list(bases)}")
        if arch and arch_of_base(bases[base_tag]) != arch:
            fail("arch_boundary",
                 f"--base {base_tag} is {arch_of_base(bases[base_tag])} but "
                 f"--arch {arch} was requested")
        return base_tag
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
        fail("no_base",
             f"default base '{default}' is not registered "
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
