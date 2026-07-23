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
