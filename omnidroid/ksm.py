# omnidroid/ksm.py
"""Kernel Same-page Merging stats + controls (Linux /sys/kernel/mm/ksm)."""
import sys
import time
from pathlib import Path

from omnidroid.config import IS_LINUX

KSM_DIR = Path("/sys/kernel/mm/ksm")
PAGE_SIZE = 4096


# ---------- KSM (Linux kernel samepage merging) ----------

def ksm_available():
    return IS_LINUX and KSM_DIR.exists()


def ksm_stats():
    """Read all /sys/kernel/mm/ksm/* values (ints where possible).
    None when KSM is not available (non-Linux or kernel without KSM)."""
    if not ksm_available():
        return None
    out = {}
    for f in sorted(KSM_DIR.iterdir()):
        try:
            v = f.read_text().strip()
            out[f.name] = int(v) if v.lstrip("-").isdigit() else v
        except OSError:
            pass
    return out


def ksm_write(name, value):
    """Write one KSM sysfs knob; exits with sudo advice on EPERM."""
    try:
        (KSM_DIR / name).write_text(str(value))
    except PermissionError:
        sys.exit(f"error: no permission to write {KSM_DIR / name} - "
                 f"run with sudo (or install/enable ksmtuned)")


def ksm_saved_mb(stats):
    """Approx MB deduplicated: each page in pages_sharing points at a
    shared page instead of owning its own copy."""
    return stats.get("pages_sharing", 0) * PAGE_SIZE / (1024 * 1024)


def pid_ksm_merged_mb(pid):
    """Per-process KSM-merged pages (kernel >= 6.1 exposes
    ksm_merging_pages). None if unsupported."""
    try:
        n = int(Path(f"/proc/{pid}/ksm_merging_pages").read_text())
        return n * PAGE_SIZE / (1024 * 1024)
    except Exception:
        return None


def _ksm_wait_settle(settle_secs, timeout=600):
    """Block until KSM pages_sharing stops moving (<1% drift held for
    settle_secs). Returns the settled pages_sharing value."""
    last = None
    stable_since = None
    start = time.time()
    while time.time() - start < timeout:
        cur = ksm_stats().get("pages_sharing", 0)
        if last is not None and abs(cur - last) <= max(last, 100) * 0.01:
            stable_since = stable_since or time.time()
            if time.time() - stable_since >= settle_secs:
                return cur
        else:
            stable_since = None
        last = cur
        time.sleep(10)
    return last or 0
