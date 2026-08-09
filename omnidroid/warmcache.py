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
import shutil
import time
from pathlib import Path

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


def warm_root(images_dir):
    """Directory holding all cache entries, under the images dir."""
    return Path(images_dir) / WARM_DIRNAME


def entry_path(images_dir, key):
    return warm_root(images_dir) / key


def read_meta(entry):
    """Parsed meta.json, or None if absent/unreadable/not an object."""
    try:
        data = json.loads((Path(entry) / META_NAME).read_text())
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def lookup(images_dir, key, qemu_version):
    """The entry for `key`, or None. A None return ALWAYS means "cold boot".

    Validates that the entry is complete and was written by this QEMU build.
    Never raises: an unreadable cache is a miss, not a failed launch.
    """
    try:
        entry = entry_path(images_dir, key)
        meta = read_meta(entry)
        if not meta:
            return None
        if meta.get("key") != key:
            return None
        meta_qemu_version = meta.get("qemu_version")
        if not meta_qemu_version or meta_qemu_version != qemu_version:
            return None
        for name in REQUIRED_FILES:
            path = entry / name
            if not path.is_file() or path.stat().st_size == 0:
                return None
        return entry
    except Exception:      # noqa: BLE001 - a broken cache is a miss, never a crash
        return None


def _staging_path(images_dir, key):
    return warm_root(images_dir) / f".bake-{key}"


def begin_bake(images_dir, key):
    """A clean staging dir for a new entry. Caller writes the payload files
    into it; the entry only becomes visible at commit_bake()."""
    staging = _staging_path(images_dir, key)
    shutil.rmtree(staging, ignore_errors=True)
    staging.mkdir(parents=True, exist_ok=True)
    return staging


def commit_bake(images_dir, key, tmp, meta):
    """Publish a staged bake atomically, replacing any existing entry.

    The rename is the only moment an entry becomes visible, so a crash at any
    earlier point leaves the previous entry (or no entry) intact rather than a
    half-written one a boot would trust.
    """
    tmp = Path(tmp)
    meta = dict(meta, key=key, last_used=time.time())
    (tmp / META_NAME).write_text(json.dumps(meta, indent=2))
    entry = entry_path(images_dir, key)
    entry.parent.mkdir(parents=True, exist_ok=True)
    if entry.exists():
        doomed = warm_root(images_dir) / f".trash-{key}-{int(time.time())}"
        entry.rename(doomed)
        shutil.rmtree(doomed, ignore_errors=True)
    tmp.rename(entry)
    return entry


def discard_bake(tmp):
    """Throw a staged bake away. Idempotent; never raises."""
    shutil.rmtree(Path(tmp), ignore_errors=True)
