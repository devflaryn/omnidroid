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

Durability: a slot outlives the process that booted it
------------------------------------------------------
Slots are spawned DETACHED (see _spawn_pool_manager / spawn_qemu), so the
QEMU keeps running when the manager -- or the whole app -- exits. Nothing
about that is automatic, though: what makes a slot findable again afterwards
is that its liveness is re-derived from `run.json` every time, by
`runtime.instance_live`, which checks the recorded pid AND matches the
recorded `identity` against QMP `query-name`. There is deliberately no second
liveness test here; a bare pid check would re-adopt a recycled pid, i.e. hand
a launch someone else's process.

Two things do NOT survive on their own, and both are handled here:

  * A slot interrupted MID-BOOT is a live guest stuck at `state=booting`
    forever -- never handed out (not ready), never swept (live), and holding
    its `_pool<n>` name against free_slot_name(). `owner_pid` (stamped by
    write_slot_meta) is what lets slot_state() call that out as `orphaned`.
  * "Keep this pool topped up" was previously implied by a manager process
    being alive. `persistent` states it on disk instead, so a manager started
    after a restart knows to refill rather than sit on an empty pool.

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
# runtime root (beside the slots), not in paths.json, because it is a
# statement about THIS host's runtime rather than user configuration.
#
# The record outlives the app; the slots do not necessarily. A power cycle
# kills every QEMU, so the slots read dead and sweep() clears them -- but the
# record stays, and `persistent` is what tells the next manager to boot them
# again instead of reporting an empty pool and doing nothing.
POOL_FILE = "pool.json"

# The manager's liveness beacon. SEPARATE from pool.json on purpose: the
# manager rewriting pool.json every tick races `pool stop`, which clears the
# config precisely so the manager exits -- a heartbeat landing microseconds
# later would RESURRECT the config and keep the pool refilling after a stop.
# A stray beacon file means nothing on its own (read_pool() is still the
# authority on whether a pool exists), so that race is now harmless.
POOL_HEARTBEAT = "pool.hb"

# How stale the beacon may be before the manager is presumed gone. The manager
# ticks every POOL_TICK_SECS (engine.py, 5 s) but a tick that starts a boot
# blocks for the whole boot -- 47-190 s measured -- so anything under a few
# minutes would declare a perfectly healthy manager dead mid-boot and spawn a
# second one alongside it.
MANAGER_STALE_S = 600

# Per-slot files (inside runtime/<slot>/).
SLOT_META = "pool.json"
SLOT_ADOPTED = "adopted.json"


def slot_name(i):
    return f"{SLOT_PREFIX}{i}"


def is_slot(name):
    return bool(name) and str(name).startswith(SLOT_PREFIX)


def pool_file():
    return config.runtime_root() / POOL_FILE


def heartbeat_file():
    return config.runtime_root() / POOL_HEARTBEAT


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
    """The pool's declared shape, or None when no pool is configured.

    `persistent` is normalised on the way OUT as well as in, because the
    records already on disk predate the flag: absent has to mean True, or the
    first manager to run after this change would read every existing pool as
    "fill once, never top up" and quietly stop replacing slots."""
    try:
        data = json.loads(pool_file().read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    data.setdefault("persistent", True)
    return data


def write_pool(data):
    p = pool_file()
    p.parent.mkdir(parents=True, exist_ok=True)
    data = dict(data)
    data.setdefault("persistent", True)
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data, indent=2))
    os.replace(tmp, p)
    return p


def desired():
    """What the pool should be maintaining RIGHT NOW, normalised.

        {"size": int, "key": str|None, "spec": dict, "persistent": bool}

    `size` is 0 when no pool is configured, which is the manager's exit
    condition. `persistent` is the refill POLICY, not the target: a manager
    tops up while it is True, and a False pool is one somebody filled once
    (`pool fill`) and is content to let drain as launches take slots. So the
    manager's condition is `persistent and len(ready_slots(key)) < size`, not
    `size` alone -- reading size alone is what would refill a one-shot pool
    forever.

    Everything is coerced here rather than at the four call sites, because a
    hand-edited or truncated pool.json must degrade to "no pool" instead of
    raising a TypeError inside the manager loop."""
    cur = read_pool() or {}
    try:
        size = int(cur.get("size") or 0)
    except (TypeError, ValueError):
        size = 0
    spec = cur.get("spec")
    return {"size": max(size, 0),
            "key": cur.get("key"),
            "spec": spec if isinstance(spec, dict) else {},
            "persistent": bool(cur.get("persistent", True))}


def clear_pool():
    """Forget the pool. The beacon goes too: leaving it behind would make the
    next `manager_alive()` answer True for a manager that is about to exit
    because this very call removed its reason to live."""
    try:
        heartbeat_file().unlink()
    except OSError:
        pass
    try:
        pool_file().unlink()
        return True
    except OSError:
        return False


# ------------------------------------------------------------ the manager

def manager_heartbeat():
    """Beacon: THIS process is the pool manager, and it is still ticking.

    Call at the TOP of every manager tick, before any boot -- a tick that
    boots a slot blocks for 47-190 s, which is why MANAGER_STALE_S is minutes
    rather than ticks. Returns the timestamp written, or None if it could not
    be written (housekeeping never fails a manager)."""
    now = time.time()
    p = heartbeat_file()
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".hb.tmp")
        tmp.write_text(json.dumps({"pid": os.getpid(), "at": now}))
        os.replace(tmp, p)
    except OSError:
        return None
    return now


def manager_alive(stale_s=MANAGER_STALE_S, now=None):
    """Is a pool manager running right now?

    BOTH halves are load-bearing. The pid alone is what `_spawn_pool_manager`
    used to check, and a pid recorded before an app restart can be handed to
    an unrelated process by the OS -- on that host a stale record reads as
    "a manager is already running", no manager is ever spawned, and the pool
    silently never refills again. Freshness alone is not enough either: a
    manager killed one second ago still has a fresh beacon, and its slots
    would go untended for the whole stale window."""
    from omnidroid.runtime import pid_alive
    beat = _read_json(heartbeat_file())
    if not beat:
        return False
    at = beat.get("at")
    if not isinstance(at, (int, float)):
        return False
    if (now or time.time()) - at > stale_s:
        return False
    return pid_alive(beat.get("pid"))


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


def slot_state(d, now=None):
    """Everything known about one slot directory.

    `live` is verified liveness (pid AND identity), not a bare pid check --
    the same standard the rest of the runtime uses, so a recycled pid can
    never make a dead slot look warm. It is also what re-attaches the pool to
    its slots after the manager (or the whole app) has exited and restarted:
    the QEMU was spawned detached and is still there, and instance_live reads
    run.json fresh every time, so a restarted manager sees a warm slot exactly
    as the one that booted it did. Nothing is remembered in memory, so there
    is nothing to lose across a restart.

    `orphaned` is the one case that does NOT heal itself: a slot still marked
    `booting` whose booting process is gone. Its guest may be up, but no
    process is left to finish the post-boot pipeline or write `ready`, so it
    can never be handed out (ready_slots skips it) and, while live, can never
    be swept -- it would sit there holding 2.2 GB of host RAM and its
    `_pool<n>` name forever. Surfaced rather than acted on: only the caller
    knows whether to shut it down or wait."""
    from omnidroid.runtime import instance_live, pid_alive
    d = Path(d)
    meta = _read_json(d / SLOT_META) or {}
    run = _read_json(d / "run.json") or {}
    adopted = _read_json(d / SLOT_ADOPTED)
    state = meta.get("state", "unknown")
    owner = meta.get("owner_pid")
    # `started` is the fallback so a slot that never reached `ready` still has
    # an age -- a boot wedged for an hour is exactly what a caller wants to
    # find, and it is the one with no ready_at.
    since = meta.get("ready_at") or meta.get("started")
    return {
        "slot": d.name,
        "key": meta.get("key"),
        "spec": meta.get("spec"),
        "state": state,
        "ready_at": meta.get("ready_at"),
        "booted_s": meta.get("booted_s"),
        "error": meta.get("error"),
        "owner_pid": owner,
        "age_s": (None if not since
                  else max(0.0, (now or time.time()) - since)),
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
        # Records written before owner_pid existed have None here and are
        # never called orphaned -- "unknown owner" must not become "delete me"
        # for a pool that predates this field.
        "orphaned": (state == "booting" and owner is not None
                     and not pid_alive(owner)),
        "adopted_by": (adopted or {}).get("account"),
    }


def list_slots(now=None):
    return [slot_state(d, now=now) for d in slot_dirs()]


def free_slot_name(taken=None):
    """The lowest `_pool<n>` not already on disk (or in `taken`)."""
    used = {d.name for d in slot_dirs()} | set(taken or ())
    i = 0
    while slot_name(i) in used:
        i += 1
    return slot_name(i)


def write_slot_meta(name, **fields):
    """Merge `fields` into the slot's record, stamping who wrote it.

    `owner_pid` is the pid of the writing process. It is only INTERPRETED for
    a slot still in `booting` -- there it is the process actually performing
    the boot, and its death is the difference between "a boot in flight" and
    "a boot nobody will ever finish" (see slot_state's `orphaned`). Stamped
    unconditionally rather than only on the booting write, because the writer
    is a fact about the record and pool_boot_slot -- which owns the booting
    write -- is in engine.py, out of this module's reach."""
    d = config.runtime_root() / name
    d.mkdir(parents=True, exist_ok=True)
    meta = _read_json(d / SLOT_META) or {}
    meta.update(fields)
    meta["owner_pid"] = os.getpid()
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


def stale_slots(max_age_s, key=None, now=None):
    """Live, unadopted slots that have been warm longer than `max_age_s`.

    Age is a liability, not just untidiness. A slot is a LIVE guest, and the
    longer one sits the more ways it has drifted from what a launch expects:
    its wall clock skews across a host sleep/hibernate (the desktop norm --
    see warmboot.resync_guest_clock, where a skewed guest fails Roblox auth
    and TLS with a symptom indistinguishable from a dead cookie), and its
    guest-side caches and tmpfs have had hours to fill. Recycling an old slot
    costs one boot the user is not waiting for; handing it out costs a launch
    that fails in a way nobody traces back to the pool.

    ADOPTED slots are excluded on purpose: that guest belongs to an account
    and is very likely mid-game, so "old" there is not a reason to touch it.
    Oldest first, so a caller recycling one slot per tick takes the worst
    first. Slots with no age at all (no ready_at, no started) are never
    stale -- unknown is not old."""
    now = now if now is not None else time.time()
    out = [s for s in list_slots(now=now)
           if s["live"] and not s["adopted_by"]
           and s["age_s"] is not None and s["age_s"] > max_age_s
           and (key is None or s["key"] == key)]
    out.sort(key=lambda s: -s["age_s"])
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

    Three cases, and only the first is removable:

      DEAD                 no live QEMU -> rmtree.
      LIVE, unclaimed      a warm slot waiting for a launch -> leave.
      LIVE, adopted        an account is playing on it -> leave. Adoption
                           COPIES run.json (adopt_run_json), so this directory
                           is still the one QEMU holds `qemu.log` open in;
                           deleting it pulls the log out from under a running
                           guest, and on Windows would not even succeed.

    Both "leave" cases fall out of the single `live` check, which is the point
    -- there is one gate on deletion, and it is verified liveness. A live slot
    is never deleted, whoever it belongs to."""
    import shutil
    removed = []
    for d in slot_dirs():
        s = slot_state(d)
        if s["live"]:
            continue
        # A slot that is mid-boot has a run.json written by spawn_qemu (real
        # QEMU pid) or none at all; `booting` with no run.json yet is not a
        # corpse, it is a slot whose QEMU has not been spawned yet.
        #
        # ...unless the process that was booting it has exited, which is what
        # an app killed between "register the slot" and "spawn QEMU" leaves
        # behind. Nothing will ever spawn that QEMU, and without this the
        # directory holds `_pool0` against free_slot_name() permanently: every
        # later pool starts at `_pool1`, and the leak survives every restart.
        # `orphaned` is False for records with no owner_pid, so pools that
        # predate the field keep the old never-sweep-a-booting-slot rule.
        if s["state"] == "booting" and s["pid"] is None and not s["orphaned"]:
            continue
        shutil.rmtree(d, ignore_errors=True)
        removed.append(d.name)
    return removed


def summary(key=None, max_age_s=None):
    slots = list_slots()
    ready = [s for s in slots if s["state"] == "ready" and s["live"]
             and not s["adopted_by"]]
    out = {
        "configured": read_pool(),
        "desired": desired(),
        "manager_alive": manager_alive(),
        "slots": slots,
        "total": len(slots),
        "ready": len(ready),
        "ready_matching": len([s for s in ready
                               if key is None or s["key"] == key]),
        "booting": len([s for s in slots if s["state"] == "booting"]),
        "adopted": len([s for s in slots if s["adopted_by"]]),
        "dead": len([s for s in slots if not s["live"]]),
        # A non-zero count here is the "app was killed mid-boot" signature:
        # the guest is up and costing RAM, and nothing will ever finish it.
        "orphaned": len([s for s in slots if s["orphaned"]]),
    }
    if max_age_s is not None:
        out["stale"] = len(stale_slots(max_age_s, key=key))
    return out
