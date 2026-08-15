"""Bake and restore a warm entry, and put any guest's clock right before use.

This is the glue between warmcache (what an entry IS), qmpsession (how QEMU is
driven) and qemu_proc (how QEMU is spawned). Everything here is best-effort by
contract: a bake that fails leaves no entry and a restore that fails returns
False, so the caller falls back to today's cold boot. Nothing raises into a
launch.

The clock half is NOT specific to a restored guest -- see
resync_guest_clock() -- because the warm POOL (pool.py) hands out guests that
have been live for hours across host sleeps, and a skewed clock fails the same
way there.
"""
import shlex
import shutil
import subprocess
import time
from pathlib import Path

from omnidroid import migfile
from omnidroid import warmcache
from omnidroid.qmpsession import QmpSession

# The whole restore is budgeted at RESTORE_TIMEOUT (engine.py, 30s -- a
# healthy warm restore is seconds, not minutes). QmpSession's own connect
# retry defaults to 60s, which alone would blow that budget 2x over on a
# restored QEMU that never opens its QMP port, before wait_for_boot even
# gets a turn. Kept well under 30s so the boot-completion wait that follows
# still has a meaningful budget left.
RESTORE_CONNECT_TIMEOUT = 20.0

# The loopback port the TCP relay prefers, derived from the instance's own
# reserved triple the same way every other port is: adb 16001+i, qmp 17001+i,
# vnc 18001+i, relay 19001+i. Kept 1000 above vnc so it cannot collide below
# 1000 concurrent instances, exactly like the other three. It is transient --
# open only while a bake or a restore is streaming -- so it is never recorded
# in run.json, and migfile falls back to an ephemeral port if it is taken.
RELAY_PORT_OFFSET = 1000


def _relay_port(acct):
    port = (acct or {}).get("vnc_port")
    return port + RELAY_PORT_OFFSET if port else None

# A migration file is sparse: it costs roughly the guest's resident set, not
# its -m size. Measured on the arm64 base: a 4096 MB guest froze to ~2.4 GiB.
# Budget 70% of RAM so the free-space check errs toward skipping a bake.
def projected_entry_bytes(mem_mb):
    return int(mem_mb * 0.7 * 2**20)


# Below this, leave the clock alone. TLS tolerates minutes of skew and Roblox's
# auth is no tighter, so a second or two buys nothing -- and this function is
# now on EVERY launch, not just a warm restore. Correcting unconditionally
# would spend a second adb round trip and a log line per launch to move a
# clock that was already right, which is how a diagnostic becomes noise nobody
# reads. Read-then-decide costs exactly one round trip in the common case.
CLOCK_SKEW_THRESHOLD_S = 2

# Reasons a resync_guest_clock() result carries. `corrected` says whether the
# clock moved; these say why it did not.
CLOCK_OK = "corrected"
CLOCK_WITHIN_THRESHOLD = "within_threshold"
CLOCK_UNREADABLE = "unreadable"
CLOCK_NO_ROOT = "no_root"
CLOCK_SET_FAILED = "set_failed"


def _clock_result(skew, corrected, reason, residual=None):
    """The structured answer resync_guest_clock returns, always this shape.

        skew_s      host minus guest, whole seconds, POSITIVE when the guest
                    is behind. None only when the clock could not be read.
        corrected   did the guest clock actually get set
        reason      one of the CLOCK_* constants above
        residual_s  skew re-measured AFTER the correction, or None if not
                    verified. See the auto_time note in resync_guest_clock.
    """
    return {"skew_s": skew, "corrected": corrected, "reason": reason,
            "residual_s": residual}


def _read_guest_epoch(acct, adb_fn):
    """The guest's wall clock as a unix epoch, or None. Never raises.

    Takes the LAST line rather than the whole output: adb interleaves its own
    chatter with command output often enough that engine.resolve_root_shell
    parses `id -u` the same way, and a stray banner line here would read as
    "unreadable clock" on a guest that answered perfectly well.
    """
    try:
        out = adb_fn(acct, "shell", "date", "+%s", timeout=20).stdout
        return int((out or "").strip().splitlines()[-1])
    # subprocess.SubprocessError covers subprocess.TimeoutExpired, which is
    # what the real adb() raises when a still-booting guest never answers --
    # the single most likely real-world cause of an unreadable clock, and
    # NOT an OSError, so it must be caught here explicitly or it propagates
    # straight into the boot path.
    except (ValueError, IndexError, AttributeError, OSError,
            subprocess.SubprocessError):
        return None


def guest_clock_skew(acct, adb_fn=None, now_fn=time.time):
    """How far the guest's wall clock is behind the host's, in seconds.

    Read-only: issues exactly one `adb shell date +%s` and sets nothing, so it
    is safe to call on an instance somebody is playing on. Positive means the
    guest is BEHIND the host, which is the only direction the failures here
    actually occur (a frozen or slept guest loses time, it never gains it).
    None means the clock could not be read at all -- which is NOT zero skew,
    and callers must not treat it as such."""
    if adb_fn is None:
        from omnidroid.engine import adb as adb_fn      # lazy: avoid a cycle
    guest = _read_guest_epoch(acct, adb_fn)
    if guest is None:
        return None
    return int(now_fn()) - guest


def _set_clock_argv(root_mode, epoch):
    """The adb argv that sets the guest clock, for `root_mode` from
    engine.resolve_root_shell. ONE argv element after `shell`, quoted exactly
    as engine.root_shell does it: adb re-joins argv with spaces and the guest
    shell re-parses the result, so a script handed over as separate elements
    loses everything after the first metacharacter and still exits 0."""
    script = f"date -s @{epoch}"
    if root_mode == "":
        # `""` is a VALID root mode, not "no root": the x86 Bliss base has no
        # su binary at all but its adbd already runs as uid 0. Prefixing `su`
        # here is what made every root-gated tune skip on x86 (see
        # engine.resolve_root_shell) -- and a clock left uncorrected is
        # reported to the user as a dead cookie.
        return ("shell", f"sh -c {shlex.quote(script)}")
    return ("shell", f"{root_mode} 0 sh -c {shlex.quote(script)}")


def resync_guest_clock(acct, label, adb_fn=None, now_fn=time.time,
                       threshold_s=CLOCK_SKEW_THRESHOLD_S, root_fn=None,
                       verify=True):
    """Put the guest wall clock right before a session is delivered.

    Cheap and safe on ANY instance, not just a restored one, which is why it
    is not called "post_restore_...":

      * A RESTORED guest wakes with its clock frozen at BAKE time -- measured
        skew equals the wall time since the bake, so a day-old entry wakes a
        day behind. `-rtc base=utc,clock=host` does NOT correct this
        (verified).
      * A POOLED guest (pool.py) has been running for hours by the time it is
        handed out. Its clock ticks, so it is usually fine -- until the host
        sleeps or hibernates, which is the normal life of a desktop. The guest
        does not tick through that; the host does.

    Both fail identically: Roblox auth and TLS reject a skewed clock with a
    symptom indistinguishable from a dead cookie. So this runs BEFORE any
    session is delivered, on every path that delivers one.

    Returns the _clock_result() dict -- never a bare number, never None, never
    an exception. A boot must not fail over the clock.

    `root_fn(acct)` is the injected root-shell resolver (pass
    engine.resolve_root_shell). Its `""` answer means "adbd is already uid 0",
    so every gate on it is `is None`, never a truth test. With no resolver the
    legacy `su -c` form is used unchanged, so existing callers behave exactly
    as before.

    NOT DONE HERE, deliberately: `settings put global auto_time 0`. Android's
    time detector can re-apply a network time suggestion after a manual
    `date -s`, which would silently undo this -- but whether that actually
    happens on these bases is UNVERIFIED. It needs a live guest (nothing in
    this repo records a measurement, and the bases cannot be inspected from
    the host), and disabling auto_time is not free either: it also disables
    the mechanism that would fix the clock without us. So instead of guessing,
    `verify` re-reads the clock after correcting and reports `residual_s` --
    a residual that keeps coming back roughly equal to the original skew IS
    the auto_time fight, and settles the question with a measurement. It costs
    one extra round trip only on the (rare) path that actually corrected.
    """
    if adb_fn is None:
        from omnidroid.engine import adb as adb_fn      # lazy: avoid a cycle
    skew = guest_clock_skew(acct, adb_fn=adb_fn, now_fn=now_fn)
    if skew is None:
        print(f"[{label}] could not read the guest clock; skipping resync")
        return _clock_result(None, False, CLOCK_UNREADABLE)
    if abs(skew) <= threshold_s:
        return _clock_result(skew, False, CLOCK_WITHIN_THRESHOLD)
    # Read AFTER the probe, not before: the probe cost a round trip, and the
    # clock should land on now-at-set-time rather than now-at-read-time.
    host = int(now_fn())
    if root_fn is None:
        argv = ("shell", "su", "-c", f"date -s @{host}")
    else:
        try:
            mode = root_fn(acct)
        except Exception:      # noqa: BLE001 - a probe must never break a boot
            mode = None
        if mode is None:
            print(f"[{label}] guest clock is {skew}s off but this guest "
                  f"offers no root; clock left uncorrected")
            return _clock_result(skew, False, CLOCK_NO_ROOT)
        argv = _set_clock_argv(mode, host)
    try:
        adb_fn(acct, *argv, timeout=20)
    except Exception:      # noqa: BLE001 - never fail a boot over the clock
        print(f"[{label}] guest clock read ({skew}s skew) but the "
              f"correction command failed; clock left uncorrected")
        return _clock_result(skew, False, CLOCK_SET_FAILED)
    residual = (guest_clock_skew(acct, adb_fn=adb_fn, now_fn=now_fn)
                if verify else None)
    if residual is not None and abs(residual) > threshold_s:
        # `date -s` returned 0 and the clock did not move. Android's time
        # detector re-applying its own suggestion is the leading suspect (see
        # the auto_time note above); a read-only /proc or a non-root shell
        # that silently no-ops is the other.
        print(f"[{label}] guest clock did NOT take: still {residual}s off "
              f"after setting it (was {skew}s)")
        return _clock_result(skew, False, CLOCK_SET_FAILED, residual)
    print(f"[{label}] guest clock resynced (was {skew}s behind host)")
    return _clock_result(skew, True, CLOCK_OK, residual)


def restore_into(acct, entry, label, session_factory=QmpSession,
                 connect_timeout=RESTORE_CONNECT_TIMEOUT):
    """Drive the deferred incoming migration on an already-spawned QEMU.

    The QEMU must have been spawned with `-incoming defer`. Capabilities have
    to be negotiated BEFORE migrate-incoming or the destination rejects the
    stream outright. Returns True only if the guest is running afterwards.

    The TRANSPORT is read back out of the entry's own meta.json rather than
    recomputed, because the two formats are not interchangeable: a mapped-ram
    file fed to a QEMU that did not enable the capability is rejected, and the
    rejection looks exactly like a corrupt entry. An entry written before the
    field existed is a `file` entry, which is what every entry written before
    this change actually was.

    `connect_timeout` is bounded well under RESTORE_TIMEOUT -- see
    RESTORE_CONNECT_TIMEOUT above -- unlike bake_entry(), which keeps
    QmpSession's own generous default: a bake is not raced against a 30s
    budget the way a restore is.
    """
    entry = Path(entry)
    state = entry / warmcache.STATE_NAME
    transport = (warmcache.read_meta(entry) or {}).get(
        "transport", migfile.TRANSPORT_FILE)
    try:
        with session_factory(acct["qmp_port"],
                             connect_timeout=connect_timeout) as s:
            s.set_migration_caps(caps=migfile.transport_caps(transport))
            ok, detail = migfile.load_state(
                s, state, transport=transport,
                preferred_port=_relay_port(acct))
            if not ok:
                print(f"[{label}] warm restore rejected: {detail}")
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
        transport = migfile.default_transport()
        with session_factory(acct["qmp_port"]) as s:
            s.set_migration_caps(caps=migfile.transport_caps(transport))
            if "error" in s.cmd("stop"):
                raise RuntimeError("could not stop the guest")
            ok, detail = migfile.save_state(
                s, state_path, transport=transport,
                preferred_port=_relay_port(acct))
            if not ok:
                raise RuntimeError(f"migration {detail}")
        meta = dict(meta, transport=transport)
        # The guest is stopped, so these are exactly the freeze-point disks.
        for src, dst in ((rd / "bake_system.qcow2", warmcache.SYSTEM_NAME),
                         (rd / "bake_data.qcow2", warmcache.DATA_NAME),
                         (rd / "efivars.fd", warmcache.EFIVARS_NAME)):
            # efivars is arm-only (UEFI pflash). _stage_bake_overlays does not
            # create one for an x86-bliss boot, and warmcache.required_files()
            # does not demand one back, so its absence is expected rather than
            # a failed bake.
            if dst == warmcache.EFIVARS_NAME and not src.exists():
                continue
            shutil.move(str(src), str(staging / dst))
        warmcache.commit_bake(images_dir, key, staging, meta)
        staging = None      # committed: nothing left at the old path to discard
        # QMP can report "completed" while the write never actually
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
        print(f"[{label}] warm entry baked ({key}, {transport} transport, "
              f"{detail})")
        return True
    except Exception as e:      # noqa: BLE001 - a failed bake is not a failed launch
        print(f"[{label}] warm bake failed ({e}); this launch is unaffected")
        if staging is not None:
            warmcache.discard_bake(staging)
        return False
