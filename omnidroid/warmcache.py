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

    A directory can't be renamed onto an existing non-empty directory on
    POSIX (ENOTEMPTY), so replacing an entry is a three-step dance: move the
    old entry aside (frees the destination, old data still intact on disk),
    rename the new one into place, and only once that succeeds does the old
    one actually get deleted. If the publish rename fails, the old entry is
    renamed back so a previously-working, expensive-to-rebuild entry is never
    lost alongside a failed write.
    """
    tmp = Path(tmp)
    meta = dict(meta, key=key, last_used=time.time())
    (tmp / META_NAME).write_text(json.dumps(meta, indent=2))
    entry = entry_path(images_dir, key)
    entry.parent.mkdir(parents=True, exist_ok=True)
    doomed = None
    if entry.exists():
        doomed = warm_root(images_dir) / f".trash-{key}-{time.time_ns()}"
        entry.rename(doomed)
    try:
        tmp.rename(entry)
    except Exception:
        if doomed is not None:
            try:
                doomed.rename(entry)
            except Exception:      # noqa: BLE001 - never mask the original error
                pass
        raise
    if doomed is not None:
        shutil.rmtree(doomed, ignore_errors=True)
    return entry


def discard_bake(tmp):
    """Throw a staged bake away. Idempotent; never raises."""
    shutil.rmtree(Path(tmp), ignore_errors=True)


DEFAULT_MAX_ENTRIES = 4
DEFAULT_MAX_BYTES = 8 * 2**30
FREE_RESERVE_BYTES = 10 * 2**30


def entry_bytes(entry):
    """Bytes an entry occupies. Best-effort; unreadable files count as 0."""
    total = 0
    for f in Path(entry).glob("*"):
        try:
            total += f.stat().st_size
        except OSError:
            pass
    return total


def has_room(images_dir, projected_bytes, reserve=FREE_RESERVE_BYTES,
             free_fn=None):
    """Is there room for a new entry AND the reserve still left over?

    Below the floor we skip the bake entirely and run as today: a launch is
    never failed or delayed over cache housekeeping, and the engine never
    competes with the user for the last of their disk.
    """
    try:
        free = (free_fn or (lambda p: shutil.disk_usage(p).free))(
            str(images_dir))
        return free >= projected_bytes + reserve
    except Exception:      # noqa: BLE001 - unreadable disk: skip the bake
        return False


def touch(entry):
    """Stamp last_used so eviction can order entries. Never raises."""
    entry = Path(entry)
    meta = read_meta(entry)
    if meta is None:
        return
    meta["last_used"] = time.time()
    try:
        (entry / META_NAME).write_text(json.dumps(meta, indent=2))
    except OSError:
        pass


def list_entries(images_dir):
    """[(key, path, last_used, size_bytes)] for every complete-looking entry."""
    root = warm_root(images_dir)
    if not root.is_dir():
        return []
    out = []
    for d in root.iterdir():
        if not d.is_dir() or d.name.startswith("."):
            continue
        meta = read_meta(d) or {}
        out.append((d.name, d, float(meta.get("last_used") or 0),
                    entry_bytes(d)))
    return out


def _remove(entry):
    shutil.rmtree(Path(entry), ignore_errors=True)


def evict_lru(images_dir, in_use, max_entries=DEFAULT_MAX_ENTRIES,
              max_bytes=DEFAULT_MAX_BYTES):
    """Enforce the entry-count and byte ceilings, oldest first.

    An entry is a pure derived artifact, so eviction costs exactly one cold
    boot -- never data. Entries backing a RUNNING instance are pinned and
    never touched -- and the budget is measured over the evictable
    candidates only, not the pinned ones: a pinned entry alone filling (or
    exceeding) max_entries/max_bytes must never force the eviction of some
    OTHER, still-useful entry, since the pinned one occupies its space no
    matter what we do.
    """
    entries = sorted(list_entries(images_dir), key=lambda r: r[2])
    candidates = [r for r in entries if r[0] not in in_use]
    total = sum(r[3] for r in candidates)
    count = len(candidates)
    removed = []
    for key, path, _, size in candidates:
        if count <= max_entries and total <= max_bytes:
            break
        _remove(path)
        removed.append(key)
        count -= 1
        total -= size
    return removed


def prune(images_dir, valid_keys, in_use):
    """Reclaim entries whose key no longer exists, plus abandoned staging dirs.

    Called from reconcile_runtime(), so a base update reclaims the space its
    stale entries held instead of accumulating alongside the new ones.
    """
    root = warm_root(images_dir)
    if not root.is_dir():
        return []
    removed = []
    for d in root.iterdir():
        if d.is_dir() and d.name.startswith((".bake-", ".trash-")):
            _remove(d)
            continue
        if not d.is_dir() or d.name.startswith("."):
            continue
        if d.name in valid_keys or d.name in in_use:
            continue
        _remove(d)
        removed.append(d.name)
    return removed


def prune_staging(images_dir):
    """Reclaim abandoned `.bake-`/`.trash-` staging directories only.

    Safe to call from anywhere -- unlike prune(), it never needs the full set
    of still-reachable keys, so it can run speculatively (e.g. at process
    startup) without risking wiping live entries over an incomplete
    valid_keys set.
    """
    root = warm_root(images_dir)
    if not root.is_dir():
        return []
    removed = []
    for d in root.iterdir():
        if d.is_dir() and d.name.startswith((".bake-", ".trash-")):
            _remove(d)
            removed.append(d.name)
    return removed
