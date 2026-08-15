"""Warm POOL: instances pre-booted to the ready point, waiting for an account.

Why this exists at all, and why it is not the warm-boot CACHE
------------------------------------------------------------
The cache (`warmcache.py` + `warmboot.py`) freezes a booted machine to disk and
restores it in seconds. It cannot work on Windows: QEMU/WHPX registers a
migration blocker at CPU realize time, so there is nothing to serialise ---

    warm bake failed (migration State blocked due to non-migratable CPUID
    feature support,dirty memory tracking support, and XSAVE/XRSTOR support)

No capability, transport or flag changes it (measured 2026-08-15; see
MODES.md "Boot time"). The pool sidesteps the whole problem: nothing is
serialised, because the machine is never stopped. It is simply already up.

The idea the cache proved is the one this reuses: **there IS an account-free
ready point.** `bake_entry` froze exactly that state — Android booted, kiosk
up, DNS/consent/awake/mode tuning applied, and NO session delivered. Instances
are diskless (`snapshot=on`, see qemu_proc.qemu_command), so every account on
one offset boots byte-identical disks; what makes an instance *someone's* is
the session broadcast, which arrives over adb long after boot. So a pooled
instance is not "an instance belonging to nobody" — it is the same instance
every launch produces, stopped one step before the cookie.

    cold   spawn -> 47-190 s boot -> deliver session (10 s) -> playing
    pool                             deliver session (10 s) -> playing

What a slot IS
--------------
A slot is an ordinary instance whose name is `_pool<n>`. That is deliberate:
`runtime/_pool0/run.json` is a normal runtime record, so `allocate_ports`
counts its ports, `running_instances`/`instance_live` see it, `reconcile_runtime`
GCs it when it dies, and `omnidroid stop _pool0` works with no special case.
The pool adds two files beside it:

    runtime/_pool0/pool.json      what this slot is (key + spec) and when it
                                  became ready
    runtime/_pool0/adopted.json   written EXCLUSIVELY (O_EXCL) by the launch
                                  that claimed it -- see claim()

Adoption copies the slot's run.json to `runtime/<account>/run.json` rather than
moving anything: QEMU holds `qemu.log` open, and on Windows an open handle is a
locked file. `identity` is copied VERBATIM (`omni-_pool0`, not
`omni-<account>`), because identity is what `instance_live` compares against
QMP `query-name` on a process that was started under the slot's name and cannot
be renamed. Getting that wrong makes an adopted instance read as dead.

Compatibility
-------------
A slot can only be handed to a launch that would have booted the same machine.
`slot_key()` hashes everything that is decided AT SPAWN or applied BEFORE the
session lands: arch, base + version, offset (name AND image identity), mode,
mem, smp, accel, gpu policy, panel, quality, guest display, debug. Anything
that differs after the key matches is delivered per-launch anyway (the place,
the cookie), so it does not belong in the key.

The offset is keyed by its image's (size, mtime) as well as its name for the
same reason `warmcache.cache_key` does it: `offset delete X` + `offset create X
<other apk>` reuses the name for a different Roblox, and a name-only key would
hand out a slot running the old build.
"""

import hashlib
import json
import os
import time
from pathlib import Path

from omnidroid import config

# Slot names are `_pool<n>`. build_acct() enforces [A-Za-z0-9_-]+ on every
# instance name, which a leading underscore satisfies, and no Roblox username
# can start with one -- so a slot can never collide with an account's runtime
# directory.
SLOT_PREFIX = "_pool"

# Pool-wide config: what the manager is trying to keep warm. Lives in the
# runtime root (beside the slots), not in paths.json, because it is live state
# rather than configuration -- a host that is power-cycled has no pool, and
# should not think it has one.
POOL_FILE = "pool.json"

# Per-slot files (inside runtime/<slot>/).
SLOT_META = "pool.json"
SLOT_ADOPTED = "adopted.json"


def slot_name(i):
    return f"{SLOT_PREFIX}{i}"


def is_slot(name):
    return bool(name) and str(name).startswith(SLOT_PREFIX)


def pool_file():
    return config.runtime_root() / POOL_FILE


# ---------------------------------------------------------------- the key

# The spec keys that describe a bootable machine. Kept as an explicit tuple
# rather than "whatever is in the dict" so that adding a knob to the CLI
# cannot silently start (or stop) mattering to slot compatibility.
SPEC_KEYS = ("mode", "mem", "smp", "balloon", "gpu", "panel", "quality",
             "guest_display", "offset", "debug")


def normalize_spec(spec):
    """The spec dict with exactly SPEC_KEYS, missing values as None.

    Normalising rather than trusting the caller is what makes the key stable:
    `{"mode": "gaming"}` and `{"mode": "gaming", "mem": None}` must hash the
    same, or a pool filled by the manager would never match a launch."""
    spec = spec or {}
    return {k: spec.get(k) for k in SPEC_KEYS}


def slot_key(*, arch, base_tag, base_version, offset, offset_image_stat,
             accel, spec):
    """Stable hash of everything that decides WHAT a warm slot is.

    Anything not in here is either delivered per-launch (the place, the
    cookie) or is a property of the host rather than the guest."""
    payload = {
        "v": 1,
        "arch": arch,
        "base": base_tag,
        "base_version": base_version,
        "offset": offset or "none",
        # (size, mtime) of the offset's /data image -- see the module
        # docstring on why the name alone is not enough.
        "offset_image": list(offset_image_stat) if offset_image_stat else None,
        "accel": accel,
        "spec": normalize_spec(spec),
    }
    blob = json.dumps(payload, sort_keys=True, separators=(",", ":"))
    return hashlib.sha1(blob.encode()).hexdigest()[:16]


# ------------------------------------------------------------- pool config

def read_pool():
    """The pool's declared shape, or None when no pool is configured."""
    try:
        data = json.loads(pool_file().read_text())
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def write_pool(data):
    p = pool_file()
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data, indent=2))
    os.replace(tmp, p)
    return p


def clear_pool():
    try:
        pool_file().unlink()
        return True
    except OSError:
        return False


# -------------------------------------------------------------- the slots

def _read_json(p):
    try:
        data = json.loads(Path(p).read_text())
        return data if isinstance(data, dict) else None
    except (OSError, ValueError):
        return None


def slot_dirs():
    root = config.runtime_root()
    if not root.is_dir():
        return []
    try:
        return sorted(d for d in root.iterdir()
                      if d.is_dir() and is_slot(d.name))
    except OSError:
        return []


def slot_state(d):
    """Everything known about one slot directory.

    `live` is verified liveness (pid AND identity), not a bare pid check --
    the same standard the rest of the runtime uses, so a recycled pid can
    never make a dead slot look warm."""
    from omnidroid.runtime import instance_live
    d = Path(d)
    meta = _read_json(d / SLOT_META) or {}
    run = _read_json(d / "run.json") or {}
    adopted = _read_json(d / SLOT_ADOPTED)
    return {
        "slot": d.name,
        "key": meta.get("key"),
        "spec": meta.get("spec"),
        "state": meta.get("state", "unknown"),
        "ready_at": meta.get("ready_at"),
        "booted_s": meta.get("booted_s"),
        "error": meta.get("error"),
        "pid": run.get("pid"),
        "adb_port": run.get("adb_port"),
        "qmp_port": run.get("qmp_port"),
        "vnc_port": run.get("vnc_port"),
        "base": run.get("base"),
        "offset": run.get("offset"),
        "data_image": run.get("data_image"),
        "identity": run.get("identity"),
        "mode": run.get("mode"),
        "live": bool(run) and not run.get("reserving") and instance_live(run),
        "adopted_by": (adopted or {}).get("account"),
    }


def list_slots():
    return [slot_state(d) for d in slot_dirs()]


def free_slot_name(taken=None):
    """The lowest `_pool<n>` not already on disk (or in `taken`)."""
    used = {d.name for d in slot_dirs()} | set(taken or ())
    i = 0
    while slot_name(i) in used:
        i += 1
    return slot_name(i)


def write_slot_meta(name, **fields):
    d = config.runtime_root() / name
    d.mkdir(parents=True, exist_ok=True)
    meta = _read_json(d / SLOT_META) or {}
    meta.update(fields)
    meta["updated"] = time.time()
    tmp = d / (SLOT_META + ".tmp")
    tmp.write_text(json.dumps(meta, indent=2))
    os.replace(tmp, d / SLOT_META)
    return meta


def ready_slots(key=None):
    """Live, ready, unadopted slots — oldest first, so a slot that has been
    warm longest is handed out first (FIFO keeps every slot's age bounded
    rather than letting one sit forever while a newer one is reused)."""
    out = [s for s in list_slots()
           if s["state"] == "ready" and s["live"] and not s["adopted_by"]
           and (key is None or s["key"] == key)]
    out.sort(key=lambda s: s.get("ready_at") or 0)
    return out


def claim(key):
    """Atomically take one ready slot, or None.

    The claim is an O_EXCL create of `adopted.json`: the filesystem decides
    the winner, so two concurrent launches can never be handed the same
    instance. (A lock file would need the same primitive anyway, and
    `_launch_lock` degrades to a no-op on Windows.)"""
    for s in ready_slots(key):
        d = config.runtime_root() / s["slot"]
        try:
            fd = os.open(str(d / SLOT_ADOPTED),
                         os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        except FileExistsError:
            continue          # another launch got there first
        except OSError:
            continue
        with os.fdopen(fd, "w") as fh:
            json.dump({"claimed_at": time.time()}, fh)
        return s
    return None


def release(slot, reason="unclaimed"):
    """Undo a claim that could not be completed, so the slot is reusable."""
    p = config.runtime_root() / slot / SLOT_ADOPTED
    try:
        p.unlink()
    except OSError:
        return False
    write_slot_meta(slot, released=reason)
    return True


def mark_adopted(slot, account):
    d = config.runtime_root() / slot
    try:
        data = _read_json(d / SLOT_ADOPTED) or {}
        data.update({"account": account, "at": time.time()})
        (d / SLOT_ADOPTED).write_text(json.dumps(data, indent=2))
    except OSError:
        pass
    write_slot_meta(slot, state="adopted", adopted_by=account)


def adopt_run_json(slot, account):
    """Point `runtime/<account>/run.json` at the slot's LIVE QEMU.

    Copy, never move: QEMU holds `qemu.log` open in the slot directory and
    Windows will not rename a directory out from under an open handle. The
    copy carries the slot's `identity` verbatim -- the QEMU process was named
    `omni-<slot>` at spawn and cannot be renamed, and `instance_live` compares
    the RECORDED identity against QMP `query-name`, so rewriting it here would
    make the adopted instance read as dead."""
    from omnidroid.runtime import runtime_dir
    src = config.runtime_root() / slot / "run.json"
    run = _read_json(src)
    if not run:
        return None
    run = dict(run)
    run["pool_slot"] = slot
    run["adopted_at"] = time.time()
    d = runtime_dir(account)
    d.mkdir(parents=True, exist_ok=True)
    tmp = d / "run.json.tmp"
    tmp.write_text(json.dumps(run))
    os.replace(tmp, d / "run.json")
    return run


def acct_from_slot(account, slot_rec, run):
    """The launch handle for an adopted instance.

    Same shape build_acct() returns, but with the SLOT's ports and offset
    instead of freshly allocated ones -- the instance already exists, so
    nothing here allocates or reserves anything."""
    return {"name": account,
            "base": run.get("base") or slot_rec.get("base"),
            "adb_port": run["adb_port"], "qmp_port": run["qmp_port"],
            "vnc_port": run["vnc_port"],
            "ephemeral": True, "debug": bool(run.get("debug")),
            "offset": run.get("offset"), "data_image": run.get("data_image"),
            "game_package": "com.roblox.client", "first_boot_done": True,
            "pool_slot": slot_rec["slot"]}


def sweep():
    """Remove slot directories whose QEMU is gone, and slots already adopted
    whose adopting instance has also gone. Returns the names removed.

    Never touches a LIVE slot, and never touches an adopted-and-still-live
    one: the account's own `stop` kills that process, and this only clears the
    directory afterwards (which is also when Windows finally releases the
    `qemu.log` handle that made an in-place move impossible in the first
    place)."""
    import shutil
    removed = []
    for d in slot_dirs():
        s = slot_state(d)
        if s["live"]:
            continue
        # A slot that is mid-boot has a run.json written by spawn_qemu (real
        # QEMU pid) or none at all; `booting` with no run.json yet is not a
        # corpse, it is a slot whose QEMU has not been spawned yet.
        if s["state"] == "booting" and s["pid"] is None:
            continue
        shutil.rmtree(d, ignore_errors=True)
        removed.append(d.name)
    return removed


def summary(key=None):
    slots = list_slots()
    ready = [s for s in slots if s["state"] == "ready" and s["live"]
             and not s["adopted_by"]]
    return {
        "configured": read_pool(),
        "slots": slots,
        "total": len(slots),
        "ready": len(ready),
        "ready_matching": len([s for s in ready
                               if key is None or s["key"] == key]),
        "booting": len([s for s in slots if s["state"] == "booting"]),
        "adopted": len([s for s in slots if s["adopted_by"]]),
        "dead": len([s for s in slots if not s["live"]]),
    }
