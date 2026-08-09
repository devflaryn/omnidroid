"""Bake and restore a warm entry, and put the guest's clock right afterwards.

This is the glue between warmcache (what an entry IS), qmpsession (how QEMU is
driven) and qemu_proc (how QEMU is spawned). Everything here is best-effort by
contract: a bake that fails leaves no entry and a restore that fails returns
False, so the caller falls back to today's cold boot. Nothing raises into a
launch.
"""
import shutil
import subprocess
import time
from pathlib import Path

from omnidroid import warmcache
from omnidroid.qmpsession import QmpSession

# The whole restore is budgeted at RESTORE_TIMEOUT (engine.py, 30s -- a
# healthy warm restore is seconds, not minutes). QmpSession's own connect
# retry defaults to 60s, which alone would blow that budget 2x over on a
# restored QEMU that never opens its QMP port, before wait_for_boot even
# gets a turn. Kept well under 30s so the boot-completion wait that follows
# still has a meaningful budget left.
RESTORE_CONNECT_TIMEOUT = 20.0

# A migration file is sparse: it costs roughly the guest's resident set, not
# its -m size. Measured on the arm64 base: a 4096 MB guest froze to ~2.4 GiB.
# Budget 70% of RAM so the free-space check errs toward skipping a bake.
def projected_entry_bytes(mem_mb):
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
    # subprocess.SubprocessError covers subprocess.TimeoutExpired, which is
    # what the real adb() raises when a still-booting guest never answers --
    # the single most likely real-world cause of an unreadable clock, and
    # NOT an OSError, so it must be caught here explicitly or it propagates
    # straight into the boot path.
    except (ValueError, AttributeError, OSError, subprocess.SubprocessError):
        print(f"[{label}] could not read the guest clock; skipping resync")
        return None
    host = int(now_fn())
    skew = abs(host - before)
    try:
        adb_fn(acct, "shell", "su", "-c", f"date -s @{host}", timeout=20)
    except Exception:      # noqa: BLE001 - never fail a boot over the clock
        print(f"[{label}] guest clock read ({skew}s skew) but the "
              f"correction command failed; clock left uncorrected")
        return skew
    print(f"[{label}] guest clock resynced (was {skew}s behind host)")
    return skew


def restore_into(acct, entry, label, session_factory=QmpSession,
                 connect_timeout=RESTORE_CONNECT_TIMEOUT):
    """Drive the deferred incoming migration on an already-spawned QEMU.

    The QEMU must have been spawned with `-incoming defer`. Capabilities have
    to be negotiated BEFORE migrate-incoming or the destination rejects the
    stream outright. Returns True only if the guest is running afterwards.

    `connect_timeout` is bounded well under RESTORE_TIMEOUT -- see
    RESTORE_CONNECT_TIMEOUT above -- unlike bake_entry(), which keeps
    QmpSession's own generous default: a bake is not raced against a 30s
    budget the way a restore is.
    """
    state = Path(entry) / warmcache.STATE_NAME
    try:
        with session_factory(acct["qmp_port"],
                             connect_timeout=connect_timeout) as s:
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
        staging = None      # committed: nothing left at the old path to discard
        # QMP can report "completed" while the `file:` write never actually
        # landed (an I/O error not surfaced through the migration state
        # machine, a disk that filled mid-write, ...). Trusting that status
        # alone would leave every future launch paying the stop+migrate
        # cost, logging a false success, and still cold-booting -- forever,
        # silently. Re-run the same check a restore would trust.
        if warmcache.lookup(images_dir, key, meta.get("qemu_version")) is None:
            shutil.rmtree(warmcache.entry_path(images_dir, key),
                          ignore_errors=True)
            print(f"[{label}] warm bake produced an unusable entry "
                  f"(failed post-commit validation); discarded")
            return False
        print(f"[{label}] warm entry baked ({key})")
        return True
    except Exception as e:      # noqa: BLE001 - a failed bake is not a failed launch
        print(f"[{label}] warm bake failed ({e}); this launch is unaffected")
        if staging is not None:
            warmcache.discard_bake(staging)
        return False
