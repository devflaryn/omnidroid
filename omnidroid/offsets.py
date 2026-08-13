# omnidroid/offsets.py
"""Roblox version OFFSETS — many baked game versions on ONE clean base.

An **offset** is a named, versioned Roblox build, baked into a THIN qcow2 COW
overlay of the base's pristine /data. Offsets coexist; exactly one is the
DEFAULT, and a bare `omnidroid start <user>` boots that one.

    images_dir/arm/
      base_arm_data_rooted.qcow2                 <- the base's PRISTINE /data
      base_arm_data_offset_2.731.944.qcow2       <- offset "2.731.944"  (thin)
      base_arm_data_offset_2.740.101.qcow2       <- offset "2.740.101"  (thin)
      base_arm_data_offset_arceus-test.qcow2     <- offset "arceus-test" (thin)

WHY THIS REPLACES THE OLD SINGLE BAKE. `bake-data-game` baked into ONE fixed
filename (`base_arm_data_game.qcow2`) and then pointed `bases.<tag>.data` at
it. That has two consequences the product could not live with:

  1. the BASE stopped being clean — it carried a Roblox version, so "which
     Roblox am I running?" was a property of the base image rather than of the
     launch, and
  2. a second version could not exist. Baking a new APK overwrote the only
     slot, so testing build B meant destroying build A and re-baking to get it
     back (~2 minutes and the original APK, if you still had it).

Offsets fix both by making the version a NAMED SIBLING rather than the base:
the base's `data` stays pristine forever, each bake writes its own overlay,
and switching versions is a launch-time choice (`--offset`) or a one-line
default change (`omnidroid offset default <name>`).

WHAT AN OFFSET IS NOT. It is not per-account: accounts do not own offsets and
nothing about an account selects one. Cookie injection into the bootstrapped
Roblox is completely unchanged — the offset decides only WHICH Roblox binary
is on the instance, never who logs into it.

ANTI-CHAINING, same rule as the bake it replaces. Every offset overlays the
PRISTINE /data (`bases.data_bake_source`), never another offset. Two offsets
are therefore siblings of equal cost, and deleting one can never affect
another. Chaining them would make offset N carry every superseded APK from
1..N-1 and turn a delete into a corruption.

This module is PURE — no subprocess, no QEMU, no adb — so the whole naming /
resolution / registry surface is unit-testable without an image directory. The
one thing that touches a file is `apk_version_info`, which reads an APK's own
manifest with the stdlib and never raises.
"""
import json
import re
import zipfile
from pathlib import Path

from omnidroid.bases import (ARM_DIR, BASE_TYPE_X86, X86_DIR, base_type,
                             data_bake_source)


# An offset name lands in a FILENAME, so it is restricted to characters that
# are safe on every host filesystem and need no quoting in a shell. Dots are
# allowed on purpose — "2.731.944" is the name a human actually wants — but a
# leading dot/dash is refused (hidden files, and names argparse would read as
# a flag).
OFFSET_NAME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,47}")


# The reserved name for "no offset at all" — a deliberately clean boot with
# whatever the base ships (i.e. nothing). Never a registry key.
NO_OFFSET = "none"


def valid_offset_name(name):
    """True if `name` is usable as an offset name (and as a filename part)."""
    return bool(name) and name != NO_OFFSET \
        and bool(OFFSET_NAME_RE.fullmatch(str(name)))


def offset_image_name(name, base=None):
    """Path of the /data overlay backing offset `name`, relative to images_dir.

    Offsets live in the SAME arch subfolder (images_dir/arm/, images_dir/x86/)
    as the pristine /data they overlay. That co-location is the requirement,
    not flatness: a qcow2's backing reference resolves relative to the
    overlay's OWN directory, so the bake can still rewrite it to a bare
    filename (`qemu-img rebase -u -b <bare name>`) and keep the image
    directory relocatable. An offset in a DIFFERENT directory from its backing
    file is what would break that — which is exactly why this has to know the
    arch: an x86 offset written to arm/ would look for its backing template in
    arm/ and simply fail to open.

    `base` omitted means "the caller did not say", and answers arm — every
    pre-offsets caller passes a bare name and has always meant the arm base.
    A base that IS passed is typed by the engine's own rule (bases.base_type),
    under which an entry carrying no explicit "type" is x86."""
    if base is not None and base_type(base) == BASE_TYPE_X86:
        return X86_DIR + f"base_x86_data_offset_{name}.qcow2"
    return ARM_DIR + f"base_arm_data_offset_{name}.qcow2"


def offsets_of(base):
    """The offset registry of a base entry ({} when it has none)."""
    return dict((base or {}).get("offsets") or {})


def default_offset_name(base):
    """Which offset a bare launch on this base uses, or None.

    Falls back to THE only offset when `default_offset` was never set: with
    exactly one version baked there is no ambiguity to resolve, and forcing a
    `offset default` call before the first launch would be ceremony. With two
    or more and no default recorded, this returns None and the caller must
    say so rather than guess — picking one at random is how you spend an hour
    debugging the wrong Roblox."""
    offs = offsets_of(base)
    name = (base or {}).get("default_offset")
    if name and name in offs:
        return name
    if len(offs) == 1:
        return next(iter(offs))
    return None


def resolve_offset(base, requested=None):
    """(name, entry, reason) for the offset THIS launch should boot.

    Three outcomes, and the third is why this returns a reason instead of
    raising: the caller (cmd_start) has to distinguish "the user asked for a
    version that does not exist" (a hard error) from "nothing is baked yet"
    (fine when `--apk` is supplying the build instead).

        requested == NO_OFFSET  -> (None, None, "explicit")   clean base
        found                   -> (name, entry, "explicit"|"default")
        not found               -> (None, None, "unknown"|"none"|"ambiguous")
    """
    offs = offsets_of(base)
    if requested == NO_OFFSET:
        return None, None, "explicit"
    if requested:
        if requested in offs:
            return requested, offs[requested], "explicit"
        return None, None, "unknown"
    name = default_offset_name(base)
    if name:
        return name, offs[name], "default"
    return None, None, ("ambiguous" if offs else "none")


def offset_data_image(base, name):
    """The /data image filename for offset `name` on `base`.

    Reads the RECORDED filename rather than recomputing it, so an offset baked
    by an older/newer naming convention still boots; falls back to the
    convention only when the entry does not carry one."""
    entry = offsets_of(base).get(name) or {}
    return entry.get("data") or offset_image_name(name, base=base)


def register_offset(base, name, entry, make_default=False):
    """Add/replace offset `name` on a RAW base entry, in place.

    Also promotes it to default when asked, or when it is the first offset on
    a base that had none — the first version baked onto a clean base is
    unambiguously the one a bare launch means."""
    offs = base.setdefault("offsets", {})
    first = not offs
    offs[name] = entry
    if make_default or first or not base.get("default_offset"):
        base["default_offset"] = name
    return base


def unregister_offset(base, name):
    """Remove offset `name` from a RAW base entry, in place.

    Returns the removed entry (or None). When the DEFAULT is removed, the
    default is re-pointed at the single remaining offset if there is exactly
    one, and cleared otherwise — never left dangling at a name that no longer
    resolves, which would fail every subsequent bare launch."""
    offs = base.setdefault("offsets", {})
    entry = offs.pop(name, None)
    if base.get("default_offset") == name:
        base["default_offset"] = next(iter(offs)) if len(offs) == 1 else None
    if not base.get("default_offset"):
        base.pop("default_offset", None)
    return entry


def offset_rows(base, images_dir=None):
    """Display/JSON rows for every offset, newest-looking first is NOT applied
    (registry order is insertion order, which is bake order). `images_dir`
    makes each row report whether its image is actually present on disk —
    a registry entry whose file was deleted by hand must show as MISSING
    rather than silently fail at boot."""
    default = default_offset_name(base)
    rows = []
    for name, entry in offsets_of(base).items():
        img = entry.get("data") or offset_image_name(name, base=base)
        row = {"name": name, "default": name == default, "data": img,
               "package": entry.get("package"),
               "apk": entry.get("apk"),
               "version_name": entry.get("version_name"),
               "version_code": entry.get("version_code"),
               "created": entry.get("created"),
               "notes": entry.get("notes")}
        if images_dir is not None:
            p = Path(images_dir) / img
            row["present"] = p.exists()
            row["size_mb"] = (p.stat().st_size // (1024 * 1024)
                              if p.exists() else None)
        rows.append(row)
    return rows


# ---------------------------------------------------------------- APK probing

_VERSION_NAME_RE = re.compile(r'versionName="([^"]*)"')
_VERSION_CODE_RE = re.compile(r'versionCode="([^"]*)"')
_PACKAGE_RE = re.compile(r'package="([^"]*)"')


def apk_version_info(apk_path):
    """{'package', 'version_name', 'version_code'} read from the APK itself.

    Stdlib only, via tools/axml.py — deliberately NOT aapt2. The engine
    already learned that lesson once (see bases.resolve_bake_package): aapt2 is
    part of the Android SDK, is not installed on every host that runs
    omnidroid, and making a bake depend on it fails the common case for no
    benefit. This is only ever used to LABEL an offset, so it is best-effort:
    every failure returns empty fields rather than raising, and the caller
    falls back to the name the user gave."""
    out = {"package": None, "version_name": None, "version_code": None}
    try:
        with zipfile.ZipFile(apk_path) as z:
            raw = z.read("AndroidManifest.xml")
    except Exception:  # noqa: BLE001 — not an apk / unreadable / no manifest
        return out
    import tempfile
    import sys
    tools = Path(__file__).resolve().parent.parent / "tools"
    if str(tools) not in sys.path:
        sys.path.insert(0, str(tools))
    try:
        import axml  # noqa: PLC0415 — optional, repo-local, imported lazily
        with tempfile.NamedTemporaryFile(suffix=".xml", delete=False) as f:
            f.write(raw)
            tmp = f.name
        try:
            text = axml.decode(tmp)
        finally:
            Path(tmp).unlink(missing_ok=True)
    except Exception:  # noqa: BLE001 — axml is a reader, not a full impl
        return out
    # Only the FIRST <manifest ...> line carries these; a later match would be
    # some nested element that happens to share an attribute name.
    head = text.split("\n", 1)[0] if text else ""
    m = _PACKAGE_RE.search(head)
    out["package"] = m.group(1) if m else None
    m = _VERSION_NAME_RE.search(head)
    out["version_name"] = m.group(1) if m else None
    m = _VERSION_CODE_RE.search(head)
    if m:
        try:
            out["version_code"] = int(m.group(1))
        except ValueError:
            out["version_code"] = m.group(1)
    return out


def suggest_offset_name(apk_path, info=None):
    """A sensible offset name for this APK, or None.

    Its versionName if the APK carries a usable one ("2.731.944"), else the
    APK's stem. Purely a convenience for `offset create --apk X` with no
    name; the caller must still validate the result."""
    info = info if info is not None else apk_version_info(apk_path)
    cand = (info or {}).get("version_name")
    if cand and valid_offset_name(cand):
        return cand
    stem = Path(apk_path).stem
    stem = re.sub(r"[^A-Za-z0-9._-]+", "-", stem).strip("-.")[:48]
    return stem if valid_offset_name(stem) else None


# ------------------------------------------------------------ base migration

# The single-slot image the pre-offsets `bake-data-game` wrote. Only ever read
# now, by migrate_legacy_bake.
LEGACY_GAME_DATA = ARM_DIR + "base_arm_data_game.qcow2"

LEGACY_OFFSET_NAME = "legacy"


def migrate_legacy_bake(base, name=LEGACY_OFFSET_NAME):
    """Turn a pre-offsets baked base into a clean base + one offset, in place.

    An existing install has `bases.arm.data = base_arm_data_game.qcow2` — the
    old single bake — which is exactly the "base carries a Roblox version"
    state offsets exist to end. Rather than orphan that image (it is the
    Roblox the user is running today), it is ADOPTED as an offset and the base
    is pointed back at its pristine /data.

    Returns the offset name when it migrated, else None. Idempotent: a base
    whose `data` is already pristine is left untouched.
    """
    data = base.get("data")
    pristine = data_bake_source(base)
    if not data or data == pristine or not str(data).endswith(".qcow2"):
        return None
    if data != LEGACY_GAME_DATA and "offset" not in data:
        # Some other non-pristine /data (a hand-built image). Still clean the
        # base — the invariant is "the base ships no game" — but say so by
        # naming the offset after the file rather than "legacy". The recorded
        # value carries its arch subfolder, so name from the BASENAME.
        name = re.sub(r"^base_arm_data_|\.qcow2$", "",
                      Path(data).name) or name
        name = name if valid_offset_name(name) else LEGACY_OFFSET_NAME
    entry = dict(base.get("game_baked") or {})
    register_offset(base, name, {
        "data": data,
        "package": entry.get("package"),
        "apk": entry.get("apk"),
        "version_name": None,
        "version_code": None,
        "created": None,
        "notes": "adopted from the pre-offsets single bake "
                 "(`bake-data-game`); the base is clean again",
    }, make_default=True)
    base["data"] = pristine
    base.pop("game_baked", None)
    return name


def offsets_summary(base):
    """One-line human summary of a base's offsets, for `bases`/`doctor`."""
    offs = offsets_of(base)
    if not offs:
        return "no Roblox baked (clean base)"
    default = default_offset_name(base)
    names = ", ".join(f"{n}*" if n == default else n for n in offs)
    return f"{len(offs)} offset(s): {names}" + ("" if default
                                                else "  [NO DEFAULT SET]")


def dumps(obj):
    """Stable JSON for anything this module writes into the config."""
    return json.dumps(obj, indent=2, sort_keys=True)
