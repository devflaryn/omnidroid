# omnidroid/adb.py
"""ADB transport + guest-shell primitives keyed off an account's adb_port."""
import re
import subprocess

from omnidroid.output import fail


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


def adb_state(acct):
    """'device' | 'offline' | 'unknown' | '' — the HOST's view of the endpoint."""
    try:
        return subprocess.run(["adb", "-s",
                               f"127.0.0.1:{_require_adb_port(acct)}",
                               "get-state"],
                              capture_output=True, text=True,
                              timeout=10).stdout.strip()
    except Exception:  # noqa: BLE001 — a probe must never raise
        return ""


def adb_recover(acct, hard=False):
    """Clear an endpoint the host's adb server has stuck in `offline`.

    Why this exists: `adb connect` on an endpoint already in the server's
    table just answers "already connected" and changes nothing, so once the
    entry goes offline -- which happens when something connects while the
    guest is mid-boot, before adbd is accepting -- every later poll sees
    `offline` forever. wait_for_boot then spins to its full timeout against a
    guest that is actually up and idle: `start` hangs for fifteen minutes with
    no output and the UI shows no sign of a boot that already finished.

    Escalates, because the cheap fixes are not reliable: disconnect+connect
    was observed NOT to clear the entry (it still reported "already
    connected"). `adb reconnect offline` is the targeted command for this
    state; `kill-server` is the one verified to always work, so it is the last
    resort -- it is heavy (it drops every other endpoint on the host, which
    then simply reconnect) and must not be the first move.
    """
    port = _require_adb_port(acct)
    serial = f"127.0.0.1:{port}"

    def _run(args, timeout=20):
        try:
            subprocess.run(["adb"] + args, capture_output=True, text=True,
                           timeout=timeout)
        except Exception:  # noqa: BLE001 — recovery is best-effort
            pass

    if hard:
        _run(["kill-server"], timeout=30)
        _run(["start-server"], timeout=30)
    else:
        _run(["disconnect", serial])
        _run(["reconnect", "offline"])
    _run(["connect", serial])


def adb_getprop(acct, prop):
    try:
        r = adb(acct, "shell", "getprop", prop, timeout=8)
        return r.stdout.strip()
    except Exception:
        return ""


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


def _foreground(acct):
    try:
        r = adb(acct, "shell", "dumpsys", "activity", "activities",
                timeout=10)
        m = re.search(r"topResumedActivity=ActivityRecord\{\S+ \S+ (\S+)",
                      r.stdout)
        return m.group(1) if m else None
    except Exception:
        return None
