"""The warm-restore boot cache: keys, entry layout, lifecycle, disk budget.

A cache ENTRY is a pre-booted, account-free machine state for one exact
machine shape. Restoring it replaces a ~20 s Android cold boot with a ~3 s
load (measured; see the design spec).

Invalidation is a consequence of the KEY rather than separate bookkeeping: a
base update, a new APK/offset, a mode change, a resize or a QEMU upgrade each
produce a different key, so the stale entry is simply never looked up again
and is later reclaimed by prune()/evict_lru().

NOTHING HERE MAY RAISE INTO A BOOT PATH. Every lookup failure -- missing file,
corrupt JSON, version mismatch -- is a MISS, and a miss just means today's
cold boot.
"""
import hashlib
import json

WARM_DIRNAME = "warm"
STATE_NAME = "state"
SYSTEM_NAME = "system.qcow2"
DATA_NAME = "data.qcow2"
EFIVARS_NAME = "efivars.fd"
META_NAME = "meta.json"
REQUIRED_FILES = (STATE_NAME, SYSTEM_NAME, DATA_NAME, EFIVARS_NAME, META_NAME)


def cache_key(*, arch, base_tag, base_version, offset, mode_name, mem_mb, smp,
              machine, accel, qemu_version):
    """Stable short hash of the exact machine shape an entry describes.

    Keyword-only on purpose: ten positional fields would be trivially
    transposable, and a transposed key silently restores the wrong machine.
    Numeric fields are normalized so "8192" and 8192 are one entry.
    Never raises: bad input produces a distinct key, not an exception.
    """
    def safe_int(value):
        """Try to convert to int; fall back to string representation."""
        try:
            return int(value)
        except (ValueError, TypeError):
            return str(value)

    payload = json.dumps([
        arch, base_tag, safe_int(base_version), offset, mode_name,
        safe_int(mem_mb), safe_int(smp), machine, accel, qemu_version
    ])
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()[:24]
