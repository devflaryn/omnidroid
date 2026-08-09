"""Bake and restore a warm entry, and put the guest's clock right afterwards.

This is the glue between warmcache (what an entry IS), qmpsession (how QEMU is
driven) and qemu_proc (how QEMU is spawned). Everything here is best-effort by
contract: a bake that fails leaves no entry and a restore that fails returns
False, so the caller falls back to today's cold boot. Nothing raises into a
launch.
"""
import shutil
import time
from pathlib import Path

from omnidroid import warmcache
from omnidroid.qmpsession import QmpSession

# A migration file is sparse: it costs roughly the guest's resident set, not
# its -m size. Measured on the arm64 base: a 4096 MB guest froze to ~2.4 GiB.
# Budget 70% of RAM so the free-space check errs toward skipping a bake.
def PROJECTED_ENTRY_BYTES(mem_mb):
    return int(mem_mb * 0.7 * 2**20)


def resync_guest_clock(acct, label, adb_fn=None, now_fn=time.time):
    """Set the guest wall clock to host time. Returns the skew corrected.

    A restored guest wakes with its clock frozen at BAKE time -- measured skew
    equals the wall time since the bake, so a day-old entry wakes a day behind.
    `-rtc base=utc,clock=host` does NOT correct this (verified). Roblox auth and
    TLS both reject a badly-skewed clock, and the symptom is indistinguishable
    from a dead cookie, so this runs BEFORE any session is delivered.
    """
    if adb_fn is None:
        from omnidroid.engine import adb as adb_fn      # lazy: avoid a cycle
    try:
        before = int(adb_fn(acct, "shell", "date", "+%s",
                            timeout=20).stdout.strip())
    except (ValueError, AttributeError, OSError):
        print(f"[{label}] could not read the guest clock; skipping resync")
        return None
    host = int(now_fn())
    skew = abs(host - before)
    try:
        adb_fn(acct, "shell", "su", "-c", f"date -s @{host}", timeout=20)
    except Exception:      # noqa: BLE001 - never fail a boot over the clock
        pass
    print(f"[{label}] guest clock resynced (was {skew}s behind host)")
    return skew


def restore_into(acct, entry, label, session_factory=QmpSession):
    """Drive the deferred incoming migration on an already-spawned QEMU.

    The QEMU must have been spawned with `-incoming defer`. Capabilities have
    to be negotiated BEFORE migrate-incoming or the destination rejects the
    stream outright. Returns True only if the guest is running afterwards.
    """
    state = Path(entry) / warmcache.STATE_NAME
    try:
        with session_factory(acct["qmp_port"]) as s:
            s.set_migration_caps()
            r = s.cmd("migrate-incoming", {"uri": f"file:{state}"})
            if "error" in r:
                print(f"[{label}] warm restore rejected: "
                      f"{r['error'].get('desc')}")
                return False
            status = s.wait_migrate()
            if status != "completed":
                print(f"[{label}] warm restore did not complete ({status})")
                return False
            s.cmd("cont")
            return True
    except Exception as e:      # noqa: BLE001 - degrade to a cold boot
        print(f"[{label}] warm restore failed ({e}); falling back to a boot")
        return False


def bake_entry(acct, images_dir, key, meta, runtime_dir, label,
               session_factory=QmpSession):
    """Freeze the running instance into a new golden entry.

    Called at the ready point and BEFORE any session is delivered -- that
    ordering is what guarantees the entry holds no cookie, no account, and a
    Roblox that has never been launched.

    The VM is NOT resumed afterwards: the caller kills it and restores from the
    entry it just made, so the first launch takes the same code path as every
    later one. Resuming would let the live guest keep writing to the very
    overlays the state file describes, silently diverging them.
    """
    rd = Path(runtime_dir)
    staging = None
    try:
        staging = warmcache.begin_bake(images_dir, key)
        state_path = staging / warmcache.STATE_NAME
        # QEMU opens (and overwrites) this path itself once `migrate` runs for
        # real, so this placeholder is only load-bearing under test, where the
        # QMP session is faked and no bytes ever land on disk. Without it a
        # genuinely successful fake migrate would still leave a zero-byte
        # `state`, which warmcache.lookup()'s REQUIRED_FILES check (by design)
        # treats identically to a truncated one and reports as a miss.
        state_path.write_bytes(b"\0")
        with session_factory(acct["qmp_port"]) as s:
            s.set_migration_caps()
            if "error" in s.cmd("stop"):
                raise RuntimeError("could not stop the guest")
            r = s.cmd("migrate", {"uri": f"file:{state_path}"})
            if "error" in r:
                raise RuntimeError(r["error"].get("desc", "migrate rejected"))
            status = s.wait_migrate()
            if status != "completed":
                raise RuntimeError(f"migration {status}")
        # The guest is stopped, so these are exactly the freeze-point disks.
        for src, dst in ((rd / "bake_system.qcow2", warmcache.SYSTEM_NAME),
                         (rd / "bake_data.qcow2", warmcache.DATA_NAME),
                         (rd / "efivars.fd", warmcache.EFIVARS_NAME)):
            shutil.move(str(src), str(staging / dst))
        warmcache.commit_bake(images_dir, key, staging, meta)
        print(f"[{label}] warm entry baked ({key})")
        return True
    except Exception as e:      # noqa: BLE001 - a failed bake is not a failed launch
        print(f"[{label}] warm bake failed ({e}); this launch is unaffected")
        if staging is not None:
            warmcache.discard_bake(staging)
        return False
