# omnidroid/adb.py
"""ADB transport + guest-shell primitives keyed off an account's adb_port."""
import os
import re
import subprocess
import tempfile

from omnidroid.output import fail


def _run_adb(cmd, timeout):
    """Run an adb command WITHOUT giving it a pipe to inherit.

    `adb` forks a long-lived SERVER daemon the first time it is used, and that
    daemon inherits whatever stdout/stderr it was handed. With
    capture_output=True those are pipes that stay open for the daemon's whole
    life, so:

      * the pipe never reaches EOF, and
      * subprocess.run's timeout handling — which kills the child and then
        calls communicate() again to reap it — blocks in that second
        communicate() FOREVER, timeout or no timeout.

    Observed exactly that: `omnidroid start` sat in adb_connect for 12+
    minutes against a QEMU that had already exited, with a 15 s timeout set.
    Temp files break the inheritance chain: the daemon may keep them open, but
    nothing here waits on EOF, so the timeout means what it says.
    """
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        try:
            proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL,
                                    stdout=out, stderr=err)
        except OSError as exc:
            raise FileNotFoundError(f"could not run adb: {exc}") from None
        try:
            code = proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            raise
        out.seek(0)
        err.seek(0)
        decode = lambda b: b.decode("utf-8", "replace")   # noqa: E731
        return subprocess.CompletedProcess(cmd, code, decode(out.read()),
                                           decode(err.read()))


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
    r = _run_adb(cmd, timeout)
    if check and r.returncode != 0:
        raise subprocess.CalledProcessError(r.returncode, cmd, r.stdout, r.stderr)
    return r


def adb_connect(acct):
    port = _require_adb_port(acct)
    try:
        _run_adb(["adb", "connect", f"127.0.0.1:{port}"], timeout=15)
    # TimeoutExpired is the expected one (a closed loopback port TIMES OUT on
    # this Windows host instead of refusing). OSError is not: it means the adb
    # binary could not be run at all, and letting that escape from a probe
    # turned "adb is missing" into a traceback from whatever loop called it.
    except (subprocess.TimeoutExpired, OSError):
        pass


def adb_state(acct):
    """'device' | 'offline' | 'unknown' | '' — the HOST's view of the endpoint.

    Reads BOTH streams. `adb get-state` prints the state to stdout only when
    it HAS one ("device"); for an endpoint the server is holding in the
    offline state it writes "error: device offline" to STDERR and leaves
    stdout empty. Checking stdout alone therefore reported "" for exactly the
    condition this exists to detect, so wait_for_boot's recovery never fired
    and a launch sat at its full timeout against a guest that was up.
    """
    try:
        r = _run_adb(["adb", "-s", f"127.0.0.1:{_require_adb_port(acct)}",
                      "get-state"], timeout=10)
    except Exception:  # noqa: BLE001 — a probe must never raise
        return ""
    out = (r.stdout or "").strip()
    if out:
        return out
    err = (r.stderr or "").lower()
    if "offline" in err:
        return "offline"
    if "not found" in err or "no devices" in err:
        return "unknown"
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
            _run_adb(["adb"] + args, timeout)
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
