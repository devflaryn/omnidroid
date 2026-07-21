"""B1 measurement harness — pure guards + parsers (this file) plus the live
capture path (Task 4). Every base/mode measurement is bracketed by these so a
number taken against a stray/mis-attached QEMU is discarded, not recorded.

Background: a known open bug leaves a live qemu that `list` reports as stopped;
its adb port is re-issued and `start` silently attaches to the wrong guest. The
tell is `boot completed after 0.0 min` (a real arm boot is ~0.4-0.6 min)."""
import re

_BOOT_RE = re.compile(r"boot completed after\s+([0-9]+(?:\.[0-9]+)?)\s*min")
_QEMU_RE = re.compile(r"\bqemu-system-\S+")
_SUSPECT_BOOT_MIN = 0.05   # below this = "attached to something already there"


def parse_boot_minutes(log_text):
    """The 'boot completed after X min' value, or None if not present."""
    m = _BOOT_RE.search(log_text or "")
    return float(m.group(1)) if m else None


def is_suspect_boot(minutes):
    """True when a boot time is implausibly fast (stale-QEMU attach tell)."""
    return minutes is not None and minutes < _SUSPECT_BOOT_MIN


def stray_qemu_pids(ps_text, known_pids):
    """qemu-system-* PIDs in `ps_text` that are NOT in `known_pids`."""
    known = set(known_pids)
    out = []
    for line in (ps_text or "").splitlines():
        if not _QEMU_RE.search(line):
            continue
        m = re.search(r"\b(\d+)\b", line)
        if not m:
            continue
        pid = int(m.group(1))
        if pid not in known:
            out.append(pid)
    return out


import re as _re
import time as _time


def parse_guest_used_kb(meminfo_text):
    """Guest used RAM (kB) = MemTotal - MemAvailable from /proc/meminfo text."""
    def _kb(key):
        m = _re.search(rf"^{key}:\s+(\d+)\s*kB", meminfo_text or "", _re.M)
        return int(m.group(1)) if m else None
    total, avail = _kb("MemTotal"), _kb("MemAvailable")
    if total is None or avail is None:
        return None
    return total - avail


def measurement_row(base, mode, arch, boot_minutes, host_rss_kb, guest_used_kb):
    """One comparable measurement row (units normalized to MB)."""
    return {
        "base": base,
        "mode": mode,
        "arch": arch,
        "boot_minutes": boot_minutes,
        "host_rss_mb": round(host_rss_kb / 1024, 1) if host_rss_kb else None,
        "guest_used_mb": round(guest_used_kb / 1024, 1) if guest_used_kb else None,
        "suspect": is_suspect_boot(boot_minutes),
        "ts": int(_time.time()),
    }
