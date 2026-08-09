# Warm-Restore Boot Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `omnidroid start` restore a pre-booted, account-free Android machine state instead of cold-booting it, cutting the Android half of a launch from ~20 s to ~3 s without weakening stability, auto-login, auto-capture, or the omni-executor CLI surface.

**Architecture:** A golden machine state (QEMU file migration: RAM + device state) is baked once per `(arch, base+version, offset, mode, mem/smp, qemu version)` and restored on later launches. Disks stay independent: instances open the entry's warm overlays `snapshot=on` exactly as today, so concurrency and the shared-template model are unchanged. The cache is an optimization layer that always degrades to today's cold boot — no miss, corrupt entry, failed bake, full disk, or unsupported accelerator may fail a launch.

**Tech Stack:** Python 3.13 (stdlib only — the project declares no dependencies), QEMU 11.x (`migrate file:` / `-incoming defer`, `mapped-ram` + `multifd`), QMP over TCP, adb, `unittest` run under `pytest`.

**Source spec:** `docs/superpowers/specs/2026-08-09-warm-restore-boot-cache-design.md`

## Global Constraints

- **Stdlib only.** `pyproject.toml` declares `dependencies = []`. Do not add a package.
- **Python >= 3.13** (`requires-python = ">=3.13"`).
- **Nothing may fail a launch.** Every cache path degrades to a cold boot. Catch broadly in cache code and return a miss; never propagate into the boot flow.
- **Cross-platform.** macOS/HVF, Linux/KVM, Windows/WHPX; x86 and arm. No platform-specific filesystem tricks (no reflink/clonefile). WHPX migration is unverified — it must degrade to a cold boot, not error.
- **Restore uses `-incoming defer` + a QMP handshake.** A plain `-incoming file:<path>` fails with `Capability mapped-ram is off, but received capability is on`.
- **Clock resync is mandatory** and must run **before** `deliver_session`. `-rtc base=utc,clock=host` does NOT fix skew; an explicit `date -s @<epoch>` does.
- **Interim concurrency rule:** restore only when no other running instance uses the same golden entry; otherwise cold-boot (spec §8b).
- **`--debug` boots never read or write the cache** (the devkit vdc disk changes device topology).
- **Free-space reserve: 10 GiB.** Cache limits default to 4 entries / 8 GiB.
- Tests are `unittest.TestCase` classes with `sys.path.insert(0, ...)` at the top, matching `tests/test_mode_scaling.py`.
- **Use `python3.13 -m pytest tests/<file> -q`.** On this machine bare `python3` is 3.14 and has no pytest; only `python3.13` does (pytest 9.1.1). `python3 -m unittest discover -s tests -p "<file>"` also works as a fallback. Wherever a task below says `python3 -m pytest`, run `python3.13 -m pytest`.
- **Full-suite baseline (measured 2026-08-09, commit 41e3644): `539 passed, 76 subtests passed`, zero failures.** "No new failures" means the suite must still finish with zero failures and at least 539 passing.
- Commit after every task.
- **Pre-existing uncommitted WIP.** This branch was cut from a working tree with ~47 modified files. `omnidroid/qemu_proc.py` and `omnidroid/runtime.py` still carry unrelated uncommitted changes authored earlier by someone else (`omnidroid/engine.py` did too, and was swept into the Task 2 commit). When a task edits one of these files, its hunks interleave with that WIP inside the same functions, so `git add <file>` commits both and no safe hunk-level split exists. This is expected — **do not try to separate them, do not revert or "clean up" anything you did not write, and do not reformat the file.** Say so plainly in the commit message: name the file, and state that it contains pre-existing unrelated changes alongside the task's own. An accurate message is the requirement; a surgically clean commit is not.

## File Structure

| File | Responsibility |
| --- | --- |
| `omnidroid/timings.py` (new) | Phase 0: per-stage wall-clock recorder for one launch. Pure, no I/O. |
| `omnidroid/warmcache.py` (new) | Cache key, entry layout, lookup, bake lifecycle, disk budget, eviction, prune. Pure filesystem + hashing; knows nothing about QEMU. |
| `omnidroid/qmpsession.py` (new) | Persistent QMP session supporting the multi-command migration handshake. The existing one-shot `qemu_proc.qmp()` stays untouched. |
| `omnidroid/warmboot.py` (new) | Orchestration: bake an entry, restore into an instance, resync the guest clock. Glue between warmcache, qmpsession and qemu_proc. |
| `omnidroid/qemu_proc.py` (modify) | `qemu_command_arm()` / `qemu_command()` learn `warm=` and `bake=`. |
| `omnidroid/runtime.py` (modify) | Record `warm_key` in `run.json`; expose the in-use key set. |
| `omnidroid/engine.py` (modify) | `_ensure_booted` branches restore vs cold+bake; `cmd_start` emits timings. |

---

### Task 1: Stage timing recorder (Phase 0)

**Files:**
- Create: `omnidroid/timings.py`
- Test: `tests/test_timings.py`

**Interfaces:**
- Consumes: nothing.
- Produces: `Timings(clock=time.monotonic)` with `.mark(stage: str) -> Timings` and `.as_dict() -> dict` returning `{"total_s": float, "stages": {name: float}, "marks": {name: float}}`. `stages` are deltas between consecutive marks; `marks` are absolute offsets from construction.

- [ ] **Step 1: Write the failing test**

Create `tests/test_timings.py`:

```python
#!/usr/bin/env python3
"""Phase 0 instrumentation: per-stage wall-clock timings for one launch.

    python3 -m pytest tests/test_timings.py -q

The recorder is a pure function of an injected clock precisely so the whole
shape is testable without a real boot under it.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid.timings import Timings  # noqa: E402


class StageTimings(unittest.TestCase):
    def test_stages_are_deltas_between_consecutive_marks(self):
        ticks = iter([0.0, 1.0, 3.5, 4.0])
        t = Timings(clock=lambda: next(ticks))
        t.mark("boot")
        t.mark("session")
        t.mark("joined")

        d = t.as_dict()

        self.assertEqual(d["stages"], {"boot": 1.0, "session": 2.5,
                                       "joined": 0.5})
        self.assertEqual(d["marks"], {"boot": 1.0, "session": 3.5,
                                      "joined": 4.0})
        self.assertEqual(d["total_s"], 4.0)

    def test_no_marks_is_an_empty_report_not_a_crash(self):
        # A launch that fails before the first mark must still emit JSON.
        t = Timings(clock=lambda: 0.0)
        self.assertEqual(t.as_dict(),
                         {"total_s": 0.0, "stages": {}, "marks": {}})

    def test_mark_returns_self_so_calls_can_chain(self):
        ticks = iter([0.0, 2.0])
        t = Timings(clock=lambda: next(ticks))
        self.assertIs(t.mark("boot"), t)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_timings.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'omnidroid.timings'`

- [ ] **Step 3: Write minimal implementation**

Create `omnidroid/timings.py`:

```python
"""Per-stage wall-clock timings for one `omnidroid start`.

Phase 0 of the warm-restore work: before optimizing the boot we have to know
which half is slow. Android boot was measured at ~20 s; Roblox's own cold start
and join were never measured at all, so this is permanent instrumentation
rather than a throwaway script.

The clock is injected so the whole shape is unit-testable without a real boot.
"""
import time


class Timings:
    """Records the instant each named stage COMPLETED, relative to creation."""

    def __init__(self, clock=time.monotonic):
        self._clock = clock
        self._start = clock()
        self._marks = []          # [(stage, seconds_since_start)]

    def mark(self, stage):
        """Record that `stage` just finished. Returns self so calls chain."""
        self._marks.append((stage, round(self._clock() - self._start, 3)))
        return self

    def as_dict(self):
        """{'total_s', 'stages' (per-stage deltas), 'marks' (absolute)}."""
        stages, prev = {}, 0.0
        for name, at in self._marks:
            stages[name] = round(at - prev, 3)
            prev = at
        return {"total_s": self._marks[-1][1] if self._marks else 0.0,
                "stages": stages,
                "marks": dict(self._marks)}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_timings.py -q`
Expected: PASS (3 tests)

- [ ] **Step 5: Commit**

```bash
git add omnidroid/timings.py tests/test_timings.py
git commit -m "feat: per-stage launch timing recorder (Phase 0 instrumentation)"
```

---

### Task 2: Emit timings from `omnidroid start`

**Files:**
- Modify: `omnidroid/engine.py` (`cmd_start`, around lines 1333-1520)
- Test: `tests/test_start_timings.py`

**Interfaces:**
- Consumes: `Timings` from Task 1.
- Produces: `cmd_start` result dict gains a `"timings"` key holding `Timings.as_dict()`. Stage names, in order: `boot`, `apk_install` (only when `--apk` was passed), `session_delivered`, `game_foreground`.

- [ ] **Step 1: Write the failing test**

Create `tests/test_start_timings.py`:

```python
#!/usr/bin/env python3
"""`start --json` must report where the wall clock went.

    python3 -m pytest tests/test_start_timings.py -q

Without this, "make boot fast" is unfalsifiable: Android boot and Roblox's own
cold start are both inside one number.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine  # noqa: E402


class StartEmitsTimings(unittest.TestCase):
    def test_result_carries_named_stage_timings(self):
        # The contract the GUI/executor reads: a `timings` block with the
        # stage names the boot path marks.
        self.assertTrue(hasattr(engine, "_start_timings_stages"))
        self.assertEqual(
            engine._start_timings_stages(has_apk=False),
            ["boot", "session_delivered", "game_foreground"])

    def test_apk_install_is_its_own_stage_only_when_an_apk_was_given(self):
        self.assertEqual(
            engine._start_timings_stages(has_apk=True),
            ["boot", "apk_install", "session_delivered", "game_foreground"])


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_start_timings.py -q`
Expected: FAIL — `AssertionError: False is not true` (no `_start_timings_stages`)

- [ ] **Step 3: Write minimal implementation**

In `omnidroid/engine.py`, add near the other `start` helpers (just above `def cmd_start`):

```python
def _start_timings_stages(has_apk):
    """The stage names `cmd_start` marks, in order. Declared separately from
    the marking itself so the emitted contract is testable without a boot."""
    stages = ["boot"]
    if has_apk:
        stages.append("apk_install")
    stages += ["session_delivered", "game_foreground"]
    return stages
```

Then inside `cmd_start`, create the recorder immediately before `_ensure_booted`:

```python
    from omnidroid.timings import Timings
    timings = Timings()
```

Add `timings.mark("boot")` on the line immediately after the `booted, first = _ensure_booted(...)` call returns.

Add `timings.mark("apk_install")` immediately after the `_install_apk` success branch (inside `if getattr(args, "apk", None):`, after the `if not ir.get("ok")` block).

Add `timings.mark("session_delivered")` immediately after the `status = deliver_session(...)` line.

Add `timings.mark("game_foreground")` immediately after the `pin_game_to_top_app(acct, label)` call, and also in the `else` path so the stage is always marked — place it after the `if resolve_mode(...) == "performance":` block closes.

Finally, immediately before each `emit_json(result)` call in `cmd_start`, add:

```python
    result["timings"] = timings.as_dict()
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_start_timings.py -q`
Expected: PASS (2 tests)

Then confirm nothing regressed: `python3 -m pytest tests/ -q`
Expected: no new failures versus the pre-change run.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/engine.py tests/test_start_timings.py
git commit -m "feat: emit per-stage timings from start --json"
```

---

### Task 3: Cache key

**Files:**
- Create: `omnidroid/warmcache.py`
- Test: `tests/test_warmcache.py`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `cache_key(*, arch, base_tag, base_version, offset, mode_name, mem_mb, smp, machine, accel, qemu_version) -> str` (24-char hex, keyword-only).
  - Module constants `STATE_NAME = "state"`, `SYSTEM_NAME = "system.qcow2"`, `DATA_NAME = "data.qcow2"`, `EFIVARS_NAME = "efivars.fd"`, `META_NAME = "meta.json"`, `WARM_DIRNAME = "warm"`, `REQUIRED_FILES` (tuple of the five names above).

- [ ] **Step 1: Write the failing test**

Create `tests/test_warmcache.py`:

```python
#!/usr/bin/env python3
"""The warm-restore cache key.

    python3 -m pytest tests/test_warmcache.py -q

Invalidation in this design is a CONSEQUENCE of the key, not separate
bookkeeping: a base update, a new APK/offset, a mode change, a resize, or a
QEMU upgrade must each produce a different key so the stale entry is simply
never found. These tests are what keeps that property true.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import warmcache  # noqa: E402


BASE = dict(arch="arm64", base_tag="arm", base_version=3, offset="patched",
            mode_name="playable", mem_mb=8192, smp=6, machine="virt",
            accel="hvf", qemu_version="11.0.2")


class TheCacheKey(unittest.TestCase):
    def test_same_inputs_give_the_same_key(self):
        self.assertEqual(warmcache.cache_key(**BASE),
                         warmcache.cache_key(**BASE))

    def test_every_field_changes_the_key(self):
        # If any of these stopped mattering, a stale entry would be restored
        # against a machine it does not describe.
        changes = dict(arch="x86_64", base_tag="x86", base_version=4,
                       offset="arceus", mode_name="farming", mem_mb=4096,
                       smp=4, machine="q35", accel="kvm",
                       qemu_version="11.1.0")
        for field, value in changes.items():
            with self.subTest(field=field):
                other = dict(BASE, **{field: value})
                self.assertNotEqual(warmcache.cache_key(**BASE),
                                    warmcache.cache_key(**other), field)

    def test_key_is_filesystem_safe_and_short(self):
        key = warmcache.cache_key(**BASE)
        self.assertEqual(len(key), 24)
        self.assertTrue(all(c in "0123456789abcdef" for c in key))

    def test_numeric_fields_compare_by_value_not_text(self):
        # "8192" and 8192 must not be two different cache entries.
        self.assertEqual(warmcache.cache_key(**dict(BASE, mem_mb=8192)),
                         warmcache.cache_key(**dict(BASE, mem_mb="8192")))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_warmcache.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'omnidroid.warmcache'`

- [ ] **Step 3: Write minimal implementation**

Create `omnidroid/warmcache.py`:

```python
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
    """
    payload = "|".join(str(x) for x in (
        arch, base_tag, int(base_version), offset, mode_name,
        int(mem_mb), int(smp), machine, accel, qemu_version))
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()[:24]
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_warmcache.py -q`
Expected: PASS (4 tests)

- [ ] **Step 5: Commit**

```bash
git add omnidroid/warmcache.py tests/test_warmcache.py
git commit -m "feat: warm-restore cache key"
```

---

### Task 4: Entry lookup and meta validation

**Files:**
- Modify: `omnidroid/warmcache.py`
- Test: `tests/test_warmcache.py` (append a class)

**Interfaces:**
- Consumes: Task 3 constants and `cache_key`.
- Produces:
  - `warm_root(images_dir) -> Path`
  - `entry_path(images_dir, key) -> Path`
  - `read_meta(entry) -> dict | None`
  - `lookup(images_dir, key, qemu_version) -> Path | None`

- [ ] **Step 1: Write the failing test**

Append to `tests/test_warmcache.py` (before the `if __name__` block):

```python
def _make_entry(tmp, key, qemu_version="11.0.2", missing=()):
    """Build a complete-looking entry on disk; `missing` omits files."""
    e = warmcache.entry_path(tmp, key)
    e.mkdir(parents=True, exist_ok=True)
    for name in warmcache.REQUIRED_FILES:
        if name in missing or name == warmcache.META_NAME:
            continue
        (e / name).write_bytes(b"x")
    if warmcache.META_NAME not in missing:
        (e / warmcache.META_NAME).write_text(json.dumps(
            {"key": key, "qemu_version": qemu_version, "mem_mb": 8192,
             "smp": 6, "last_used": 0}))
    return e


class EntryLookup(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.key = warmcache.cache_key(**BASE)

    def test_complete_entry_is_found(self):
        e = _make_entry(self.tmp, self.key)
        self.assertEqual(warmcache.lookup(self.tmp, self.key, "11.0.2"), e)

    def test_missing_entry_is_a_miss_not_an_error(self):
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.0.2"))

    def test_each_missing_file_is_a_miss(self):
        for name in warmcache.REQUIRED_FILES:
            with self.subTest(missing=name):
                tmp = Path(tempfile.mkdtemp())
                self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
                _make_entry(tmp, self.key, missing=(name,))
                self.assertIsNone(warmcache.lookup(tmp, self.key, "11.0.2"))

    def test_qemu_version_mismatch_is_a_miss(self):
        # The migration stream format is tied to the QEMU build that wrote it.
        _make_entry(self.tmp, self.key, qemu_version="11.0.2")
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.1.0"))

    def test_corrupt_meta_is_a_miss_not_a_crash(self):
        e = _make_entry(self.tmp, self.key)
        (e / warmcache.META_NAME).write_text("{not json")
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.0.2"))

    def test_meta_key_must_match_the_directory_key(self):
        # Guards against a hand-copied or half-renamed entry.
        e = _make_entry(self.tmp, self.key)
        (e / warmcache.META_NAME).write_text(json.dumps(
            {"key": "somethingelse", "qemu_version": "11.0.2"}))
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.0.2"))
```

Add these imports to the top of `tests/test_warmcache.py`:

```python
import json
import shutil
import tempfile
from pathlib import Path
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_warmcache.py -q`
Expected: FAIL — `AttributeError: module 'omnidroid.warmcache' has no attribute 'entry_path'`

- [ ] **Step 3: Write minimal implementation**

Append to `omnidroid/warmcache.py`:

```python
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
        if meta.get("qemu_version") != qemu_version:
            return None
        for name in REQUIRED_FILES:
            if not (entry / name).exists():
                return None
        return entry
    except Exception:      # noqa: BLE001 - a broken cache is a miss, never a crash
        return None
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_warmcache.py -q`
Expected: PASS (10 tests)

- [ ] **Step 5: Commit**

```bash
git add omnidroid/warmcache.py tests/test_warmcache.py
git commit -m "feat: warm cache entry lookup with fail-to-miss validation"
```

---

### Task 5: Bake lifecycle (begin / commit / discard)

**Files:**
- Modify: `omnidroid/warmcache.py`
- Test: `tests/test_warmcache.py` (append a class)

**Interfaces:**
- Consumes: Tasks 3-4.
- Produces:
  - `begin_bake(images_dir, key) -> Path` (a clean temp dir; caller writes the five files into it)
  - `commit_bake(images_dir, key, tmp, meta) -> Path` (writes `meta.json`, then atomically swaps into place, replacing any existing entry)
  - `discard_bake(tmp) -> None`

- [ ] **Step 1: Write the failing test**

Append to `tests/test_warmcache.py`:

```python
class BakeLifecycle(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.key = warmcache.cache_key(**BASE)

    def _fill(self, d):
        for name in warmcache.REQUIRED_FILES:
            if name != warmcache.META_NAME:
                (d / name).write_bytes(b"payload")

    def test_a_partial_bake_is_never_visible_as_an_entry(self):
        # The whole point of staging: a crash mid-bake must not leave an
        # entry that lookup() would hand to a boot.
        staging = warmcache.begin_bake(self.tmp, self.key)
        (staging / warmcache.STATE_NAME).write_bytes(b"half")
        self.assertIsNone(warmcache.lookup(self.tmp, self.key, "11.0.2"))

    def test_commit_makes_the_entry_findable(self):
        staging = warmcache.begin_bake(self.tmp, self.key)
        self._fill(staging)
        warmcache.commit_bake(self.tmp, self.key, staging,
                              {"key": self.key, "qemu_version": "11.0.2"})
        self.assertIsNotNone(warmcache.lookup(self.tmp, self.key, "11.0.2"))

    def test_commit_stamps_key_and_last_used_even_if_caller_forgot(self):
        staging = warmcache.begin_bake(self.tmp, self.key)
        self._fill(staging)
        warmcache.commit_bake(self.tmp, self.key, staging,
                              {"qemu_version": "11.0.2"})
        meta = warmcache.read_meta(warmcache.entry_path(self.tmp, self.key))
        self.assertEqual(meta["key"], self.key)
        self.assertIsInstance(meta["last_used"], (int, float))

    def test_commit_replaces_an_existing_entry(self):
        first = warmcache.begin_bake(self.tmp, self.key)
        self._fill(first)
        warmcache.commit_bake(self.tmp, self.key, first,
                              {"qemu_version": "11.0.2"})
        second = warmcache.begin_bake(self.tmp, self.key)
        self._fill(second)
        (second / warmcache.STATE_NAME).write_bytes(b"newer")
        warmcache.commit_bake(self.tmp, self.key, second,
                              {"qemu_version": "11.0.2"})
        entry = warmcache.entry_path(self.tmp, self.key)
        self.assertEqual((entry / warmcache.STATE_NAME).read_bytes(), b"newer")

    def test_begin_bake_clears_a_stale_staging_dir(self):
        staging = warmcache.begin_bake(self.tmp, self.key)
        (staging / "leftover").write_bytes(b"junk")
        staging2 = warmcache.begin_bake(self.tmp, self.key)
        self.assertFalse((staging2 / "leftover").exists())

    def test_discard_removes_staging_and_is_idempotent(self):
        staging = warmcache.begin_bake(self.tmp, self.key)
        warmcache.discard_bake(staging)
        self.assertFalse(staging.exists())
        warmcache.discard_bake(staging)      # must not raise
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_warmcache.py -q`
Expected: FAIL — `AttributeError: module 'omnidroid.warmcache' has no attribute 'begin_bake'`

- [ ] **Step 3: Write minimal implementation**

Append to `omnidroid/warmcache.py`:

```python
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_warmcache.py -q`
Expected: PASS (16 tests)

- [ ] **Step 5: Commit**

```bash
git add omnidroid/warmcache.py tests/test_warmcache.py
git commit -m "feat: atomic warm cache bake lifecycle"
```

---

### Task 6: Disk budget, LRU eviction and prune

**Files:**
- Modify: `omnidroid/warmcache.py`
- Test: `tests/test_warmcache.py` (append a class)

**Interfaces:**
- Consumes: Tasks 3-5.
- Produces:
  - `DEFAULT_MAX_ENTRIES = 4`, `DEFAULT_MAX_BYTES = 8 * 2**30`, `FREE_RESERVE_BYTES = 10 * 2**30`
  - `entry_bytes(entry) -> int`
  - `has_room(images_dir, projected_bytes, reserve=FREE_RESERVE_BYTES, free_fn=None) -> bool`
  - `touch(entry) -> None`
  - `list_entries(images_dir) -> list[tuple[str, Path, float, int]]` — `(key, path, last_used, size_bytes)`
  - `evict_lru(images_dir, in_use, max_entries=DEFAULT_MAX_ENTRIES, max_bytes=DEFAULT_MAX_BYTES) -> list[str]`
  - `prune(images_dir, valid_keys, in_use) -> list[str]`

- [ ] **Step 1: Write the failing test**

Append to `tests/test_warmcache.py`:

```python
class DiskBudget(unittest.TestCase):
    """The images volume was measured 89% full while an entry costs ~2 GB.
    An unbounded cache would take the product down, so these limits are a
    stability requirement, not tidiness."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def _entry(self, key, last_used, size=1024):
        e = warmcache.entry_path(self.tmp, key)
        e.mkdir(parents=True, exist_ok=True)
        for name in warmcache.REQUIRED_FILES:
            if name != warmcache.META_NAME:
                (e / name).write_bytes(b"0" * size)
        (e / warmcache.META_NAME).write_text(json.dumps(
            {"key": key, "qemu_version": "11.0.2", "last_used": last_used}))
        return e

    def test_bake_is_skipped_when_the_disk_is_near_full(self):
        # Skipping a bake costs one slow launch; filling the disk costs the
        # product. Never fail or delay the launch over housekeeping.
        self.assertFalse(warmcache.has_room(
            self.tmp, projected_bytes=2 * 2**30,
            free_fn=lambda p: 11 * 2**30))     # 11 GiB free, need 2 + 10 reserve
        self.assertTrue(warmcache.has_room(
            self.tmp, projected_bytes=2 * 2**30,
            free_fn=lambda p: 13 * 2**30))

    def test_eviction_removes_least_recently_used_first(self):
        self._entry("old", last_used=100)
        self._entry("mid", last_used=200)
        self._entry("new", last_used=300)
        evicted = warmcache.evict_lru(self.tmp, in_use=set(), max_entries=2,
                                      max_bytes=10**9)
        self.assertEqual(evicted, ["old"])
        self.assertFalse(warmcache.entry_path(self.tmp, "old").exists())
        self.assertTrue(warmcache.entry_path(self.tmp, "new").exists())

    def test_eviction_never_removes_an_entry_in_use(self):
        # Deleting the disks a running instance is backed by would kill it.
        self._entry("old", last_used=100)
        self._entry("new", last_used=300)
        evicted = warmcache.evict_lru(self.tmp, in_use={"old"}, max_entries=1,
                                      max_bytes=10**9)
        self.assertEqual(evicted, [])
        self.assertTrue(warmcache.entry_path(self.tmp, "old").exists())

    def test_eviction_respects_a_byte_ceiling(self):
        self._entry("a", last_used=100, size=4096)
        self._entry("b", last_used=200, size=4096)
        evicted = warmcache.evict_lru(self.tmp, in_use=set(), max_entries=99,
                                      max_bytes=10000)
        self.assertEqual(evicted, ["a"])

    def test_touch_updates_last_used(self):
        e = self._entry("k", last_used=1)
        warmcache.touch(e)
        self.assertGreater(warmcache.read_meta(e)["last_used"], 1)

    def test_prune_reclaims_entries_whose_key_no_longer_exists(self):
        # A base update changes every key; the old entries are pure garbage.
        self._entry("stale", last_used=100)
        self._entry("live", last_used=200)
        removed = warmcache.prune(self.tmp, valid_keys={"live"}, in_use=set())
        self.assertEqual(removed, ["stale"])
        self.assertTrue(warmcache.entry_path(self.tmp, "live").exists())

    def test_prune_removes_abandoned_staging_dirs(self):
        staging = warmcache.begin_bake(self.tmp, "somekey")
        warmcache.prune(self.tmp, valid_keys=set(), in_use=set())
        self.assertFalse(staging.exists())

    def test_prune_keeps_an_in_use_entry_even_if_its_key_went_stale(self):
        self._entry("running", last_used=100)
        removed = warmcache.prune(self.tmp, valid_keys=set(),
                                  in_use={"running"})
        self.assertEqual(removed, [])

    def test_missing_cache_dir_is_not_an_error(self):
        empty = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, empty, ignore_errors=True)
        self.assertEqual(warmcache.list_entries(empty), [])
        self.assertEqual(warmcache.evict_lru(empty, in_use=set()), [])
        self.assertEqual(warmcache.prune(empty, set(), set()), [])
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_warmcache.py -q`
Expected: FAIL — `AttributeError: module 'omnidroid.warmcache' has no attribute 'has_room'`

- [ ] **Step 3: Write minimal implementation**

Append to `omnidroid/warmcache.py`:

```python
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
    boot -- never data. Entries backing a RUNNING instance are never touched.
    """
    entries = sorted(list_entries(images_dir), key=lambda r: r[2])
    keep = [r for r in entries if r[0] in in_use]
    candidates = [r for r in entries if r[0] not in in_use]
    total = sum(r[3] for r in entries)
    count = len(entries)
    removed = []
    for key, path, _, size in candidates:
        if count <= max_entries and total <= max_bytes:
            break
        _remove(path)
        removed.append(key)
        count -= 1
        total -= size
    del keep
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_warmcache.py -q`
Expected: PASS (25 tests)

- [ ] **Step 5: Commit**

```bash
git add omnidroid/warmcache.py tests/test_warmcache.py
git commit -m "feat: warm cache disk budget, LRU eviction and prune"
```

---

### Task 7: Persistent QMP session

**Files:**
- Create: `omnidroid/qmpsession.py`
- Test: `tests/test_qmpsession.py`

**Interfaces:**
- Consumes: nothing.
- Produces: `QmpSession(port, connect_timeout=60.0, timeout=15.0)` with `.cmd(execute, arguments=None) -> dict`, `.set_migration_caps(channels=4) -> None`, `.wait_migrate(timeout=600.0, sleep=0.25) -> str`, `.close() -> None`, and context-manager support. Also `MIGRATION_CAPS = ("mapped-ram", "multifd")`.

Why a new module: `qemu_proc.qmp()` opens a fresh connection per command. The migration handshake requires capabilities to be set and then `migrate-incoming` issued **on the same session**, so it cannot use that helper.

- [ ] **Step 1: Write the failing test**

Create `tests/test_qmpsession.py`:

```python
#!/usr/bin/env python3
"""A persistent QMP session, driven against a fake QMP server.

    python3 -m pytest tests/test_qmpsession.py -q

qemu_proc.qmp() opens one connection per command, which cannot express the
migration handshake: capabilities must be set and `migrate-incoming` issued on
the SAME session, or the load dies with
"Capability mapped-ram is off, but received capability is on".
"""
import json
import os
import socket
import sys
import threading
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid.qmpsession import QmpSession  # noqa: E402


class FakeQmp:
    """Minimal QMP server: greets, then answers each command from a script."""

    def __init__(self, replies):
        self.replies = list(replies)
        self.received = []
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(1)
        self.port = self.sock.getsockname()[1]
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()

    def _serve(self):
        conn, _ = self.sock.accept()
        f = conn.makefile("rw", encoding="utf-8", newline="\n")
        f.write(json.dumps({"QMP": {"version": {}}}) + "\n")
        f.flush()
        while True:
            line = f.readline()
            if not line:
                break
            self.received.append(json.loads(line))
            reply = self.replies.pop(0) if self.replies else {"return": {}}
            f.write(json.dumps(reply) + "\n")
            f.flush()
        conn.close()


class Session(unittest.TestCase):
    def test_capabilities_are_negotiated_once_on_connect(self):
        fake = FakeQmp([{"return": {}}])
        with QmpSession(fake.port) as s:
            s.cmd("query-status")
        self.assertEqual(fake.received[0]["execute"], "qmp_capabilities")

    def test_cmd_returns_the_parsed_reply(self):
        fake = FakeQmp([{"return": {}},
                        {"return": {"status": "paused", "running": False}}])
        with QmpSession(fake.port) as s:
            r = s.cmd("query-status")
        self.assertEqual(r["return"]["status"], "paused")

    def test_migration_caps_enable_mapped_ram_and_multifd_together(self):
        # Both are required: the source writes a mapped-ram stream and the
        # destination refuses it unless it agreed to the same capability.
        fake = FakeQmp([{"return": {}}, {"return": {}}, {"return": {}}])
        with QmpSession(fake.port) as s:
            s.set_migration_caps(channels=4)
        caps = [m for m in fake.received
                if m["execute"] == "migrate-set-capabilities"][0]
        enabled = {c["capability"]: c["state"]
                   for c in caps["arguments"]["capabilities"]}
        self.assertEqual(enabled, {"mapped-ram": True, "multifd": True})
        params = [m for m in fake.received
                  if m["execute"] == "migrate-set-parameters"][0]
        self.assertEqual(params["arguments"]["multifd-channels"], 4)

    def test_wait_migrate_polls_until_a_terminal_status(self):
        fake = FakeQmp([{"return": {}},
                        {"return": {"status": "active"}},
                        {"return": {"status": "active"}},
                        {"return": {"status": "completed"}}])
        with QmpSession(fake.port) as s:
            self.assertEqual(s.wait_migrate(timeout=5, sleep=0), "completed")

    def test_wait_migrate_gives_up_and_reports_rather_than_hanging(self):
        fake = FakeQmp([{"return": {}}] + [{"return": {"status": "active"}}] * 50)
        with QmpSession(fake.port) as s:
            self.assertEqual(s.wait_migrate(timeout=0, sleep=0), "timeout")

    def test_connect_failure_raises_a_clear_error(self):
        # Port 1 is never a QMP server; the caller must be able to catch this
        # and fall back to a cold boot.
        with self.assertRaises(OSError):
            QmpSession(1, connect_timeout=0.5)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_qmpsession.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'omnidroid.qmpsession'`

- [ ] **Step 3: Write minimal implementation**

Create `omnidroid/qmpsession.py`:

```python
"""A persistent QMP session, for handshakes that span several commands.

qemu_proc.qmp() opens one connection per command, which is right for the
fire-and-forget calls the engine already makes (balloon, quit, query-balloon).
It cannot express a migration, where capabilities must be negotiated and THEN
`migrate`/`migrate-incoming` issued on the same connection -- get that wrong
and the destination dies with:

    Capability mapped-ram is off, but received capability is on
"""
import json
import socket
import time

MIGRATION_CAPS = ("mapped-ram", "multifd")


class QmpSession:
    """One long-lived QMP connection. Use as a context manager."""

    def __init__(self, port, connect_timeout=60.0, timeout=15.0):
        deadline = time.monotonic() + connect_timeout
        last = None
        while True:
            try:
                self._sock = socket.create_connection(("127.0.0.1", port),
                                                      timeout=timeout)
                break
            except OSError as e:
                last = e
                if time.monotonic() >= deadline:
                    raise OSError(
                        f"QMP on 127.0.0.1:{port} never accepted a connection "
                        f"within {connect_timeout}s: {last}") from last
                time.sleep(0.25)
        self._sock.settimeout(timeout)
        self._f = self._sock.makefile("rw", encoding="utf-8", newline="\n")
        self._f.readline()                       # greeting
        self.cmd("qmp_capabilities")

    def cmd(self, execute, arguments=None):
        """Send one command, return its parsed reply (return OR error).

        Asynchronous events are skipped: only a reply carries `return`/`error`.
        """
        msg = {"execute": execute}
        if arguments:
            msg["arguments"] = arguments
        self._f.write(json.dumps(msg) + "\n")
        self._f.flush()
        while True:
            line = self._f.readline()
            if not line:
                return {"error": {"desc": "QMP connection closed"}}
            reply = json.loads(line)
            if "return" in reply or "error" in reply:
                return reply

    def set_migration_caps(self, channels=4):
        """Enable the capabilities the warm-restore stream is written with.

        Must be called on BOTH ends, and on the destination BEFORE
        migrate-incoming. direct-io is deliberately not set: the Homebrew QEMU
        build reports "No build-time support for direct-io" and the mechanism
        works fine without it.
        """
        self.cmd("migrate-set-capabilities",
                 {"capabilities": [{"capability": c, "state": True}
                                   for c in MIGRATION_CAPS]})
        self.cmd("migrate-set-parameters", {"multifd-channels": channels})

    def wait_migrate(self, timeout=600.0, sleep=0.25):
        """Poll query-migrate until terminal. Returns the status string, or
        'timeout' -- never hangs, so a stuck migration degrades to a cold boot."""
        deadline = time.monotonic() + timeout
        while True:
            status = (self.cmd("query-migrate").get("return", {})
                      .get("status"))
            if status in ("completed", "failed", "cancelled"):
                return status
            if time.monotonic() >= deadline:
                return "timeout"
            time.sleep(sleep)

    def close(self):
        try:
            self._f.close()
        except Exception:      # noqa: BLE001
            pass
        try:
            self._sock.close()
        except Exception:      # noqa: BLE001
            pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_qmpsession.py -q`
Expected: PASS (6 tests)

- [ ] **Step 5: Commit**

```bash
git add omnidroid/qmpsession.py tests/test_qmpsession.py
git commit -m "feat: persistent QMP session for the migration handshake"
```

---

### Task 8: QEMU command args for bake and restore

**Files:**
- Modify: `omnidroid/qemu_proc.py` (`qemu_command_arm` ~line 558, `qemu_command` ~line 698)
- Test: `tests/test_warm_qemu_args.py`

**Interfaces:**
- Consumes: `warmcache` names from Task 3.
- Produces: `qemu_command_arm(acct, cfg, interactive, mode=None, accel=None, debug=False, warm=None, bake=False)` and the same two new keyword args on `qemu_command`. `warm` is a `Path` to a cache entry; `bake` is a bool.
  - `warm` set: disks are the entry's `system.qcow2` / `data.qcow2` **with `snapshot=on`**, efivars is the entry's copy, and `-incoming defer` is appended.
  - `bake=True`: disks are the caller-provided writable overlays (no `snapshot=on`), no `-incoming`.

- [ ] **Step 1: Write the failing test**

Create `tests/test_warm_qemu_args.py`:

```python
#!/usr/bin/env python3
"""QEMU args for the warm-restore paths.

    python3 -m pytest tests/test_warm_qemu_args.py -q

Two properties are load-bearing and easy to break silently:
  * a RESTORE must open the golden disks snapshot=on, or the first restore
    poisons the entry for every later one;
  * a restore must use `-incoming defer`, never `-incoming file:` -- the
    latter dies with "Capability mapped-ram is off, but received capability
    is on" because caps can only be set over QMP.
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import qemu_proc as qp  # noqa: E402
from omnidroid import warmcache  # noqa: E402


def _cfg(images):
    return {"images_dir": str(images),
            "qemu": {"mem_mb": 4096, "smp": 4, "adb_port_start": 16001,
                     "qmp_port_start": 17001, "vnc_port_start": 18001},
            "bases": {"arm": {"type": "arm-uefi",
                              "system": "base_arm_system_rooted.qcow2",
                              "data": "base_arm_data_rooted.qcow2",
                              "efivars": "base_arm_efivars.fd"}}}


def _acct():
    return {"name": "t", "base": "arm", "ephemeral": True, "adb_port": 16001,
            "qmp_port": 17001, "vnc_port": 18001, "arch": "arm64"}


class WarmRestoreArgs(unittest.TestCase):
    def setUp(self):
        self.images = Path(tempfile.mkdtemp())
        self.entry = warmcache.entry_path(self.images, "abc123")
        self.entry.mkdir(parents=True, exist_ok=True)
        for name in warmcache.REQUIRED_FILES:
            (self.entry / name).write_bytes(b"x")

    def test_restore_opens_the_golden_disks_snapshot_on(self):
        cmd = qp.qemu_command_arm(_acct(), _cfg(self.images), interactive=False,
                                  warm=self.entry)
        drives = [a for a in cmd if a.startswith("file=")]
        golden = [d for d in drives if warmcache.SYSTEM_NAME in d
                  or warmcache.DATA_NAME in d]
        self.assertEqual(len(golden), 2, drives)
        for d in golden:
            self.assertIn("snapshot=on", d)

    def test_restore_defers_incoming_and_never_uses_incoming_file(self):
        cmd = qp.qemu_command_arm(_acct(), _cfg(self.images), interactive=False,
                                  warm=self.entry)
        self.assertIn("-incoming", cmd)
        self.assertEqual(cmd[cmd.index("-incoming") + 1], "defer")
        self.assertFalse(any(str(a).startswith("file:") for a in cmd))

    def test_restore_uses_the_entrys_own_efivars(self):
        cmd = qp.qemu_command_arm(_acct(), _cfg(self.images), interactive=False,
                                  warm=self.entry)
        pflash = [a for a in cmd if "if=pflash,unit=1" in str(a)][0]
        self.assertIn(str(self.entry / warmcache.EFIVARS_NAME), pflash)

    def test_bake_uses_writable_overlays_not_snapshot_on(self):
        # The freeze point must be persistable; snapshot=on would discard it.
        cmd = qp.qemu_command_arm(_acct(), _cfg(self.images), interactive=False,
                                  bake=True)
        drives = [a for a in cmd if a.startswith("file=")]
        self.assertTrue(drives)
        for d in drives:
            self.assertNotIn("snapshot=on", d)
        self.assertNotIn("-incoming", cmd)

    def test_a_normal_boot_is_byte_for_byte_unchanged(self):
        # The cache is an optimization layer: with no warm/bake it must emit
        # exactly what it emitted before this feature existed.
        before = qp.qemu_command_arm(_acct(), _cfg(self.images),
                                     interactive=False)
        after = qp.qemu_command_arm(_acct(), _cfg(self.images),
                                    interactive=False, warm=None, bake=False)
        self.assertEqual(before, after)
        self.assertNotIn("-incoming", before)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_warm_qemu_args.py -q`
Expected: FAIL — `TypeError: qemu_command_arm() got an unexpected keyword argument 'warm'`

- [ ] **Step 3: Write minimal implementation**

In `omnidroid/qemu_proc.py`, change the `qemu_command_arm` signature to:

```python
def qemu_command_arm(acct, cfg, interactive, mode=None, accel=None,
                     debug=False, warm=None, bake=False):
```

Inside it, immediately after the existing `ephemeral = bool(acct.get("ephemeral"))` line, insert the warm/bake branch **before** the existing `if ephemeral:` block, and make that block an `elif`:

```python
    # WARM RESTORE: the disks are the golden entry's frozen overlays, opened
    # snapshot=on exactly like a shared template -- so N instances can share
    # one entry and no restore can ever modify it. efivars is the entry's own
    # copy (pflash needs a real writable file).
    from omnidroid import warmcache
    if warm is not None:
        warm = Path(warm)
        sys_src = warm / warmcache.SYSTEM_NAME
        data_src = warm / warmcache.DATA_NAME
        disk_opts = ",discard=unmap,detect-zeroes=unmap,snapshot=on"
        efivars_src = warm / warmcache.EFIVARS_NAME
    elif bake:
        # BAKE: writable overlays under the instance's runtime dir. The freeze
        # point has to be persistable, so snapshot=on is exactly wrong here.
        sys_src = rd / "bake_system.qcow2"
        data_src = rd / "bake_data.qcow2"
        disk_opts = ",discard=unmap,detect-zeroes=unmap"
        efivars_src = rd / "efivars.fd"
    elif ephemeral:
```

(The body of the original `if ephemeral:` block is unchanged; only its keyword becomes `elif`.)

Then, at the end of the function, immediately before `return cmd`, add:

```python
    if warm is not None:
        # NOT `-incoming file:<path>`. mapped-ram/multifd must be enabled on
        # the destination before the stream is read, and capabilities can only
        # be set over QMP -- so the load is deferred and driven by
        # warmboot.restore_into(). See the design spec, section 2b(a).
        cmd += ["-incoming", "defer"]
```

Next, update `qemu_command` (the dispatcher) to accept and forward the new arguments:

```python
def qemu_command(acct, cfg, interactive, mode=None, accel=None, debug=False,
                 warm=None, bake=False):
    from omnidroid.engine import account_dir
    base = cfg["bases"][acct["base"]]
    if base_type(base) == BASE_TYPE_ARM:
        return qemu_command_arm(acct, cfg, interactive, mode=mode, accel=accel,
                                debug=debug, warm=warm, bake=bake)
```

(keep the rest of the existing x86 body unchanged below that).

Finally, forward the flags from `spawn_qemu`:

```python
def spawn_qemu(acct, cfg, interactive, mode=None, accel=None, debug=False,
               warm=None, bake=False):
```

and change its internal `cmd = qemu_command(...)` call to:

```python
    cmd = qemu_command(acct, cfg, interactive, mode, accel=accel, debug=debug,
                       warm=warm, bake=bake)
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_warm_qemu_args.py -q`
Expected: PASS (5 tests)

Then: `python3 -m pytest tests/ -q`
Expected: no new failures — in particular `tests/test_qemu_footprint.py` and `tests/test_gaming_apply.py` must still pass, since they assert on the command shape.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/qemu_proc.py tests/test_warm_qemu_args.py
git commit -m "feat: qemu command args for warm restore and bake"
```

---

### Task 9: Bake and restore orchestration + clock resync

**Files:**
- Create: `omnidroid/warmboot.py`
- Test: `tests/test_warmboot.py`

**Interfaces:**
- Consumes: `warmcache` (Tasks 3-6), `QmpSession` (Task 7), `spawn_qemu` (Task 8).
- Produces:
  - `bake_entry(acct, images_dir, key, meta, runtime_dir, label, session_factory=QmpSession) -> bool`
  - `restore_into(acct, entry, label, session_factory=QmpSession) -> bool`
  - `resync_guest_clock(acct, label, adb_fn=None, now_fn=time.time) -> int | None` — returns the skew in seconds it corrected, or `None` if it could not read the guest clock.
  - `PROJECTED_ENTRY_BYTES(mem_mb) -> int` helper for the free-space check.

- [ ] **Step 1: Write the failing test**

Create `tests/test_warmboot.py`:

```python
#!/usr/bin/env python3
"""Bake/restore orchestration and the post-restore clock resync.

    python3 -m pytest tests/test_warmboot.py -q

The QMP layer is faked: what matters here is the ORDER of operations and that
every failure degrades to "no entry / cold boot" rather than raising into a
launch.
"""
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import warmboot, warmcache  # noqa: E402


class FakeSession:
    """Records the command order and replays a scripted migration status."""

    def __init__(self, port, migrate_status="completed", fail_on=None,
                 connect_timeout=60.0, timeout=15.0):
        self.calls = []
        self._status = migrate_status
        self._fail_on = fail_on or set()

    def cmd(self, execute, arguments=None):
        self.calls.append(execute)
        if execute in self._fail_on:
            return {"error": {"desc": "nope"}}
        return {"return": {}}

    def set_migration_caps(self, channels=4):
        self.calls.append("migrate-set-capabilities")

    def wait_migrate(self, timeout=600.0, sleep=0.25):
        self.calls.append("wait_migrate")
        return self._status

    def close(self):
        self.calls.append("close")

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False


class ClockResync(unittest.TestCase):
    """A restored guest wakes with the clock frozen at bake time; measured
    skew equals the wall time since the bake. -rtc base=utc,clock=host does
    NOT fix it. A wrong clock breaks TLS and cookie acceptance, which looks
    exactly like "auto-login is broken"."""

    def test_it_sets_the_guest_clock_to_host_time(self):
        sent = []

        class R:
            def __init__(self, out):
                self.stdout = out

        def fake_adb(acct, *args, **kw):
            sent.append(args)
            if args[:2] == ("shell", "date"):
                return R("1000\n" if len(sent) == 1 else "2000\n")
            return R("")

        skew = warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=fake_adb, now_fn=lambda: 2000)

        self.assertEqual(skew, 1000)
        joined = [" ".join(a) for a in sent]
        self.assertTrue(any("date -s @2000" in j for j in joined), joined)

    def test_unreadable_guest_clock_returns_none_instead_of_raising(self):
        class R:
            stdout = "not-a-number"

        self.assertIsNone(warmboot.resync_guest_clock(
            {"name": "t"}, "lbl", adb_fn=lambda *a, **k: R(),
            now_fn=lambda: 1))


class Restore(unittest.TestCase):
    def setUp(self):
        self.images = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.images, ignore_errors=True)
        self.entry = warmcache.entry_path(self.images, "k")
        self.entry.mkdir(parents=True, exist_ok=True)
        for n in warmcache.REQUIRED_FILES:
            (self.entry / n).write_bytes(b"x")

    def test_handshake_order_is_caps_then_incoming_then_cont(self):
        # Caps BEFORE migrate-incoming, or the load is rejected outright.
        sess = FakeSession(0)
        ok = warmboot.restore_into({"name": "t", "qmp_port": 1}, self.entry,
                                   "lbl", session_factory=lambda *a, **k: sess)
        self.assertTrue(ok)
        order = [c for c in sess.calls
                 if c in ("migrate-set-capabilities", "migrate-incoming",
                          "wait_migrate", "cont")]
        self.assertEqual(order, ["migrate-set-capabilities", "migrate-incoming",
                                 "wait_migrate", "cont"])

    def test_failed_migration_reports_false_and_never_conts(self):
        sess = FakeSession(0, migrate_status="failed")
        ok = warmboot.restore_into({"name": "t", "qmp_port": 1}, self.entry,
                                   "lbl", session_factory=lambda *a, **k: sess)
        self.assertFalse(ok)
        self.assertNotIn("cont", sess.calls)

    def test_qmp_that_never_answers_is_false_not_an_exception(self):
        def boom(*a, **k):
            raise OSError("no QMP")

        self.assertFalse(warmboot.restore_into(
            {"name": "t", "qmp_port": 1}, self.entry, "lbl",
            session_factory=boom))


class Bake(unittest.TestCase):
    def setUp(self):
        self.images = Path(tempfile.mkdtemp())
        self.rd = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.images, ignore_errors=True)
        self.addCleanup(shutil.rmtree, self.rd, ignore_errors=True)
        for n in ("bake_system.qcow2", "bake_data.qcow2", "efivars.fd"):
            (self.rd / n).write_bytes(b"x")

    def test_successful_bake_publishes_a_findable_entry(self):
        sess = FakeSession(0)
        ok = warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images,
                                 "k", {"qemu_version": "11.0.2"}, self.rd,
                                 "lbl", session_factory=lambda *a, **k: sess)
        self.assertTrue(ok)
        self.assertIsNotNone(warmcache.lookup(self.images, "k", "11.0.2"))

    def test_it_stops_the_vm_before_migrating(self):
        # Migrating a running guest would capture a torn machine.
        sess = FakeSession(0)
        warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images, "k",
                            {"qemu_version": "11.0.2"}, self.rd, "lbl",
                            session_factory=lambda *a, **k: sess)
        self.assertLess(sess.calls.index("stop"), sess.calls.index("migrate"))

    def test_failed_bake_leaves_no_entry_behind(self):
        sess = FakeSession(0, migrate_status="failed")
        ok = warmboot.bake_entry({"name": "t", "qmp_port": 1}, self.images,
                                 "k", {"qemu_version": "11.0.2"}, self.rd,
                                 "lbl", session_factory=lambda *a, **k: sess)
        self.assertFalse(ok)
        self.assertIsNone(warmcache.lookup(self.images, "k", "11.0.2"))
        self.assertFalse(any(p.name.startswith(".bake-")
                             for p in warmcache.warm_root(self.images).iterdir()))

    def test_bake_never_raises_into_the_caller(self):
        def boom(*a, **k):
            raise OSError("no QMP")

        self.assertFalse(warmboot.bake_entry(
            {"name": "t", "qmp_port": 1}, self.images, "k",
            {"qemu_version": "11.0.2"}, self.rd, "lbl", session_factory=boom))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_warmboot.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'omnidroid.warmboot'`

- [ ] **Step 3: Write minimal implementation**

Create `omnidroid/warmboot.py`:

```python
"""Bake and restore a warm entry, and put the guest's clock right afterwards.

This is the glue between warmcache (what an entry IS), qmpsession (how QEMU is
driven) and qemu_proc (how QEMU is spawned). Everything here is best-effort by
contract: a bake that fails leaves no entry and a restore that fails returns
False, so the caller falls back to today's cold boot. Nothing raises into a
launch.
"""
import shutil
import time
from pathlib import Path

from omnidroid import warmcache
from omnidroid.qmpsession import QmpSession

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
    except (ValueError, AttributeError, OSError):
        print(f"[{label}] could not read the guest clock; skipping resync")
        return None
    host = int(now_fn())
    skew = abs(host - before)
    try:
        adb_fn(acct, "shell", "su", "-c", f"date -s @{host}", timeout=20)
    except Exception:      # noqa: BLE001 - never fail a boot over the clock
        pass
    print(f"[{label}] guest clock resynced (was {skew}s behind host)")
    return skew


def restore_into(acct, entry, label, session_factory=QmpSession):
    """Drive the deferred incoming migration on an already-spawned QEMU.

    The QEMU must have been spawned with `-incoming defer`. Capabilities have
    to be negotiated BEFORE migrate-incoming or the destination rejects the
    stream outright. Returns True only if the guest is running afterwards.
    """
    state = Path(entry) / warmcache.STATE_NAME
    try:
        with session_factory(acct["qmp_port"]) as s:
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
        with session_factory(acct["qmp_port"]) as s:
            s.set_migration_caps()
            if "error" in s.cmd("stop"):
                raise RuntimeError("could not stop the guest")
            r = s.cmd("migrate",
                      {"uri": f"file:{staging / warmcache.STATE_NAME}"})
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
        print(f"[{label}] warm entry baked ({key})")
        return True
    except Exception as e:      # noqa: BLE001 - a failed bake is not a failed launch
        print(f"[{label}] warm bake failed ({e}); this launch is unaffected")
        if staging is not None:
            warmcache.discard_bake(staging)
        return False
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_warmboot.py -q`
Expected: PASS (9 tests)

- [ ] **Step 5: Commit**

```bash
git add omnidroid/warmboot.py tests/test_warmboot.py
git commit -m "feat: warm entry bake/restore orchestration and clock resync"
```

---

### Task 10: Record `warm_key` in run.json and expose in-use keys

**Files:**
- Modify: `omnidroid/runtime.py`
- Test: `tests/test_warm_in_use.py`

**Interfaces:**
- Consumes: nothing.
- Produces: `warm_keys_in_use() -> set[str]` in `omnidroid/runtime.py`, reading `warm_key` from every live `runtime/<name>/run.json`. Used by the interim concurrency rule and by eviction.

- [ ] **Step 1: Write the failing test**

Create `tests/test_warm_in_use.py`:

```python
#!/usr/bin/env python3
"""Which golden entries are currently backing a RUNNING instance.

    python3 -m pytest tests/test_warm_in_use.py -q

Two consumers depend on this: eviction (never delete the disks a live
instance is backed by) and the interim concurrency rule (a second launch
against an in-use entry must cold-boot, because a second concurrent restore
lands `offline` on adb -- see the design spec section 8b).
"""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import runtime  # noqa: E402


class InUseKeys(unittest.TestCase):
    def setUp(self):
        self.tmp = os.environ.get("OMNI_DATA_DIR")

    def _run(self, tmp_path, name, **fields):
        d = tmp_path / "runtime" / name
        d.mkdir(parents=True, exist_ok=True)
        (d / "run.json").write_text(json.dumps(fields))

    def test_collects_keys_from_live_instances(self, ):
        import tempfile
        from pathlib import Path
        tmp = Path(tempfile.mkdtemp())
        os.environ["OMNI_DATA_DIR"] = str(tmp)
        self.addCleanup(lambda: os.environ.pop("OMNI_DATA_DIR", None))
        self._run(tmp, "a", pid=os.getpid(), warm_key="k1")
        self._run(tmp, "b", pid=os.getpid(), warm_key="k2")

        self.assertEqual(runtime.warm_keys_in_use(), {"k1", "k2"})

    def test_instances_without_a_warm_key_contribute_nothing(self):
        import tempfile
        from pathlib import Path
        tmp = Path(tempfile.mkdtemp())
        os.environ["OMNI_DATA_DIR"] = str(tmp)
        self.addCleanup(lambda: os.environ.pop("OMNI_DATA_DIR", None))
        self._run(tmp, "cold", pid=os.getpid())

        self.assertEqual(runtime.warm_keys_in_use(), set())

    def test_a_dead_instance_does_not_hold_its_entry_hostage(self):
        import tempfile
        from pathlib import Path
        tmp = Path(tempfile.mkdtemp())
        os.environ["OMNI_DATA_DIR"] = str(tmp)
        self.addCleanup(lambda: os.environ.pop("OMNI_DATA_DIR", None))
        # PID 1 is not one of ours; running_pid() must reject it.
        self._run(tmp, "dead", pid=999999, warm_key="ghost")

        self.assertEqual(runtime.warm_keys_in_use(), set())

    def test_missing_runtime_dir_is_empty_not_an_error(self):
        import tempfile
        from pathlib import Path
        tmp = Path(tempfile.mkdtemp())
        os.environ["OMNI_DATA_DIR"] = str(tmp)
        self.addCleanup(lambda: os.environ.pop("OMNI_DATA_DIR", None))
        self.assertEqual(runtime.warm_keys_in_use(), set())


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_warm_in_use.py -q`
Expected: FAIL — `AttributeError: module 'omnidroid.runtime' has no attribute 'warm_keys_in_use'`

- [ ] **Step 3: Write minimal implementation**

Append to `omnidroid/runtime.py`:

```python
def warm_keys_in_use():
    """Golden-entry keys backing a RUNNING instance right now.

    Eviction uses it so a live instance's disks are never deleted, and the
    interim concurrency rule uses it so a second launch against an in-use
    entry cold-boots instead of landing `offline` on adb (design spec 8b).
    """
    keys = set()
    root = config.runtime_root()
    if not root.is_dir():
        return keys
    for d in root.iterdir():
        if not d.is_dir():
            continue
        try:
            data = json.loads((d / "run.json").read_text())
        except (OSError, ValueError):
            continue
        key = data.get("warm_key")
        if key and running_pid(d.name):
            keys.add(key)
    return keys
```

Then make `spawn_qemu` record the key. In `omnidroid/qemu_proc.py`, inside `spawn_qemu`, the `run.json` dict written after `Popen` gains one field — add `"warm_key": warm_key,` to that dict, and add a `warm_key=None` keyword parameter to `spawn_qemu`'s signature (alongside the `warm`/`bake` added in Task 8):

```python
def spawn_qemu(acct, cfg, interactive, mode=None, accel=None, debug=False,
               warm=None, bake=False, warm_key=None):
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_warm_in_use.py -q`
Expected: PASS (4 tests)

Then: `python3 -m pytest tests/ -q` — no new failures.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/runtime.py omnidroid/qemu_proc.py tests/test_warm_in_use.py
git commit -m "feat: track which warm entry each running instance uses"
```

---

### Task 11: Wire the cache into `_ensure_booted`

**Files:**
- Modify: `omnidroid/engine.py` (`_ensure_booted` ~line 6874; `reconcile_runtime` import site ~line 1338)
- Test: `tests/test_warm_boot_policy.py`

**Interfaces:**
- Consumes: everything from Tasks 3-10.
- Produces:
  - `_ensure_booted(...)` gains a private `_no_rebake=False` keyword: when True the launch may still RESTORE but must not BAKE, which is what stops the post-bake handoff from recursing forever.
  - `_halt_qemu(acct) -> None` — immediate QMP `quit` + SIGKILL. Used instead of `_shutdown()`, which tries an in-guest power-off that cannot work on a paused guest and would burn its 90 s timeout on every bake.
  - `_warm_key_for(acct, cfg, mode, accel) -> str | None` — `None` when the cache must not be used at all (debug boot, unknown QEMU version).
  - `_warm_entry_for(acct, cfg, mode, accel, debug) -> tuple[Path | None, str | None]` — applies the interim concurrency rule and returns `(entry, key)`.
  - `_qemu_version(tool) -> str` — cached `qemu-system-*  --version` first line.

- [ ] **Step 1: Write the failing test**

Create `tests/test_warm_boot_policy.py`:

```python
#!/usr/bin/env python3
"""When may a launch use the warm cache at all?

    python3 -m pytest tests/test_warm_boot_policy.py -q

The policy is the whole safety story, so it is a pure function tested without
QEMU under it:
  * a --debug boot changes device topology (devkit vdc), so it must never
    read OR write the cache;
  * an entry already backing a running instance must not be restored a second
    time -- the second concurrent restore lands `offline` on adb (spec 8b).
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine  # noqa: E402


class WarmPolicy(unittest.TestCase):
    def test_debug_boots_never_touch_the_cache(self):
        self.assertFalse(engine._warm_cache_allowed(debug=True, in_use=set(),
                                                    key="k"))

    def test_a_normal_boot_may_use_the_cache(self):
        self.assertTrue(engine._warm_cache_allowed(debug=False, in_use=set(),
                                                   key="k"))

    def test_an_entry_already_in_use_is_refused(self):
        # Interim rule: siblings cold-boot until the adb blocker is root-caused.
        self.assertFalse(engine._warm_cache_allowed(debug=False,
                                                    in_use={"k"}, key="k"))

    def test_a_different_entry_being_in_use_is_irrelevant(self):
        self.assertTrue(engine._warm_cache_allowed(debug=False,
                                                   in_use={"other"}, key="k"))

    def test_no_key_means_no_cache(self):
        self.assertFalse(engine._warm_cache_allowed(debug=False, in_use=set(),
                                                    key=None))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest tests/test_warm_boot_policy.py -q`
Expected: FAIL — `AttributeError: module 'omnidroid' has no attribute '_warm_cache_allowed'`

- [ ] **Step 3: Write minimal implementation**

In `omnidroid/engine.py`, add above `_ensure_booted`:

```python
RESTORE_TIMEOUT = 30       # a healthy warm restore is seconds, not minutes
_QEMU_VERSION_CACHE = {}


def _halt_qemu(acct):
    """Kill this instance's QEMU immediately. NOT _shutdown().

    _shutdown() tries a graceful in-guest power-off first, which cannot work
    on a guest that is PAUSED (the bake stops the VM before migrating) and
    would burn its 90 s timeout on every bake. Both callers here -- the
    post-bake handoff and the poisoned-entry fallback -- want the process
    gone now, and neither has any in-guest state worth preserving.
    """
    from omnidroid.qemu_proc import qmp
    name = acct["name"]
    pid = running_pid(name)
    qmp(acct, "quit")
    time.sleep(1)
    if pid and pid_alive(pid):
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    # run.json is deliberately left alone: the next spawn_qemu overwrites it,
    # and wiping it here would strip the port reservation this instance still
    # owns for the restore that follows.


def _qemu_version(tool):
    """First line of `<tool> --version`, cached. Part of the cache key: the
    migration stream format is tied to the QEMU build that wrote it."""
    if tool not in _QEMU_VERSION_CACHE:
        try:
            out = subprocess.run([qemu_bin(tool), "--version"],
                                 capture_output=True, text=True,
                                 timeout=20).stdout.splitlines()
            _QEMU_VERSION_CACHE[tool] = out[0].strip() if out else ""
        except Exception:      # noqa: BLE001 - unknown version = no cache
            _QEMU_VERSION_CACHE[tool] = ""
    return _QEMU_VERSION_CACHE[tool]


def _warm_cache_allowed(debug, in_use, key):
    """May THIS launch use the warm cache?

    False for a debug boot (the devkit vdc disk changes device topology, so a
    restore would not match), for an unknown key, and for an entry already
    backing a running instance -- the second concurrent restore comes up alive
    but `offline` on adb (design spec 8b). Refusing costs one cold boot.
    """
    if debug or not key:
        return False
    return key not in in_use
```

Then rewrite the spawn branch of `_ensure_booted`. Replace the existing `else:` block (the one containing `interactive = first` through `maybe_start_autocap(acct, label)`) with:

```python
    else:
        # `interactive` is the boot PROFILE (full host smp/mem + serial log),
        # historically used for a first/provisioning boot. It is independent of
        # `debug`, which attaches the devkit disk.
        interactive = first
        from omnidroid import warmboot, warmcache
        from omnidroid.runtime import warm_keys_in_use
        images = Path(cfg["images_dir"])
        tool = ("qemu-system-aarch64" if acct_base_is_arm(acct)
                else "qemu-system-x86_64")
        qver = _qemu_version(tool)
        base = cfg["bases"][acct["base"]]
        key = None
        if qver:
            key = warmcache.cache_key(
                arch=acct_arch(acct), base_tag=acct["base"],
                base_version=base.get("version", 0),
                offset=acct.get("offset") or "none",
                mode_name=mode["name"], mem_mb=mode["mem"], smp=mode["smp"],
                machine="virt" if acct_base_is_arm(acct) else "q35",
                accel=accel or default_accel(), qemu_version=qver)
        in_use = warm_keys_in_use()
        entry = None
        if _warm_cache_allowed(debug, in_use, key):
            entry = warmcache.lookup(images, key, qver)

        if entry is not None:
            # FAST PATH: restore a pre-booted machine instead of booting one.
            spawn_qemu(acct, cfg, interactive=False, mode=mode, accel=accel,
                       debug=debug, warm=entry, warm_key=key)
            maybe_start_autocap(acct, label)
            if warmboot.restore_into(acct, entry, label):
                warmcache.touch(entry)
                if wait_for_boot(acct, RESTORE_TIMEOUT, label):
                    warmboot.resync_guest_clock(acct, label)
                    post_boot(acct, label)
                    _enforce_hiding(acct, label)
                    assert_kiosk_game(acct, cfg, label)
                    return True, first
            # POISONED ENTRY: the state file itself is suspect, so delete it
            # and cold-boot. One slow launch, never a failed one.
            print(f"[{label}] warm restore did not come up; discarding the "
                  f"entry and cold-booting")
            _halt_qemu(acct)
            shutil.rmtree(entry, ignore_errors=True)
            entry = None

        # COLD PATH, optionally baking a new entry on the way.
        want_bake = (not _no_rebake
                     and _warm_cache_allowed(debug, in_use, key)
                     and not interactive
                     and warmcache.has_room(
                         images, warmboot.projected_entry_bytes(mode["mem"])))
        if want_bake:
            rd = runtime_dir(acct["name"])
            _stage_bake_overlays(acct, cfg, rd)
        spawn_qemu(acct, cfg, interactive=interactive,
                   mode=None if interactive else mode, accel=accel,
                   debug=debug, bake=want_bake,
                   warm_key=key if want_bake else None)
        maybe_start_autocap(acct, label)
        if want_bake:
            t = timeout or (FIRST_BOOT_TIMEOUT if first else NORMAL_BOOT_TIMEOUT)
            if not wait_for_boot(acct, t, label, first_boot=first):
                return False, first
            meta = {"qemu_version": qver, "mem_mb": mode["mem"],
                    "smp": mode["smp"], "mode": mode["name"],
                    "base": acct["base"], "base_version": base.get("version", 0),
                    "offset": acct.get("offset") or "none"}
            if warmboot.bake_entry(acct, images, key, meta,
                                   runtime_dir(acct["name"]), label):
                warmcache.evict_lru(images, warm_keys_in_use())
                # The bake stopped the VM and moved its disks into the entry;
                # restore from what we just made so the FIRST launch takes the
                # same code path as every later one.
                #
                # _no_rebake guards the recursion: if THAT restore also fails,
                # the entry is discarded and the retry cold-boots WITHOUT
                # baking again -- otherwise a reproducibly-bad bake would loop
                # bake -> restore -> discard -> bake forever.
                _halt_qemu(acct)
                return _ensure_booted(acct, cfg, label, timeout=timeout,
                                      accel=accel, mode_name=mode_name,
                                      mem=mem, smp=smp, balloon=balloon,
                                      quality=quality, debug=debug,
                                      _no_rebake=True)
            return True, first
```

Add the supporting overlay-staging helper above `_ensure_booted`:

```python
def _stage_bake_overlays(acct, cfg, rd):
    """Writable COW overlays for a BAKE boot, plus a fresh efivars.

    A bake must persist its freeze point, so it cannot use the shared
    templates opened snapshot=on the way a normal ephemeral boot does.
    """
    base = cfg["bases"][acct["base"]]
    images = Path(cfg["images_dir"])
    rd.mkdir(parents=True, exist_ok=True)
    pairs = ((images / base["system"], rd / "bake_system.qcow2"),
             (images / (acct.get("data_image") or base["data"]),
              rd / "bake_data.qcow2"))
    for backing, overlay in pairs:
        overlay.unlink(missing_ok=True)
        subprocess.run([qemu_bin("qemu-img"), "create", "-f", "qcow2", "-F", "qcow2",
                        "-b", str(backing), str(overlay)],
                       check=True, capture_output=True)
    shutil.copyfile(images / base.get("efivars", ARM_BASE_EFIVARS),
                    rd / "efivars.fd")
```

Finally, hook housekeeping into the existing sweep. **Only staging/trash dirs may be pruned here.** `prune()` deletes every entry whose key is not in `valid_keys`, and the set of still-reachable keys is only knowable per-launch (it depends on the base, offset and mode being started) — calling it from `reconcile_runtime` with an incomplete `valid_keys` would wipe every entry not currently in use, turning the cache into a permanent miss. Add to the end of `reconcile_runtime()` in `omnidroid/runtime.py`:

```python
    try:
        from omnidroid import warmcache
        from omnidroid.engine import read_config
        from omnidroid.config import images_dir
        warmcache.prune_staging(images_dir(read_config()))
    except Exception:      # noqa: BLE001 - housekeeping never fails a command
        pass
```

and add to `omnidroid/warmcache.py`:

```python
def prune_staging(images_dir):
    """Remove abandoned .bake-/.trash- dirs only. Safe to run any time:
    it never touches a real entry, whose validity is only knowable per-launch."""
    root = warm_root(images_dir)
    if not root.is_dir():
        return []
    removed = []
    for d in root.iterdir():
        if d.is_dir() and d.name.startswith((".bake-", ".trash-")):
            _remove(d)
            removed.append(d.name)
    return removed
```

Add a test for it in `tests/test_warmcache.py`:

```python
class PruneStaging(unittest.TestCase):
    def test_only_staging_dirs_are_removed(self):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        staging = warmcache.begin_bake(tmp, "k")
        entry = warmcache.entry_path(tmp, "real")
        entry.mkdir(parents=True, exist_ok=True)
        (entry / warmcache.STATE_NAME).write_bytes(b"x")

        warmcache.prune_staging(tmp)

        self.assertFalse(staging.exists())
        self.assertTrue(entry.exists())
```

`shutil`, `subprocess`, `os`, `time` and `signal` must all be importable in `engine.py` — verify each at the top of the file and add any that are missing. `qemu_bin("qemu-img")` is the established qemu-img resolver (already used five times in `engine.py`); there is no `qemu_img()` helper.

Also add the `_no_rebake=False` keyword to the `_ensure_booted` signature itself:

```python
def _ensure_booted(acct, cfg, label, timeout=None, accel=None, mode_name=None,
                   mem=None, balloon=None, debug=None, smp=None, quality=None,
                   _no_rebake=False):
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 -m pytest tests/test_warm_boot_policy.py tests/test_warmcache.py -q`
Expected: PASS

Then the full suite: `python3 -m pytest tests/ -q`
Expected: no new failures.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/engine.py omnidroid/runtime.py omnidroid/warmcache.py tests/
git commit -m "feat: restore from the warm cache in _ensure_booted, bake on miss"
```

---

### Task 12: End-to-end verification and the adb concurrency root-cause

**Files:**
- Create: `tools/warm_restore_check.py`
- Test: manual, on a host with real base images

**Interfaces:**
- Consumes: the whole feature.
- Produces: a one-command verifier for a real host, and evidence for the spec §8b blocker.

This task does not add unit tests — it verifies against real images, which CI cannot do. It also carries the open blocker from the spec.

- [ ] **Step 1: Write the verifier**

Create `tools/warm_restore_check.py`:

```python
#!/usr/bin/env python3
"""End-to-end check of the warm-restore cache against real base images.

    python3 tools/warm_restore_check.py <account>

Run 1 cold-boots and bakes; run 2 must restore. Prints the measured split so
the win is evidence rather than assertion. Requires the account to exist and
a base to be installed; it is a host tool, never part of the unit suite.
"""
import json
import subprocess
import sys
import time


def start(name, extra=()):
    t0 = time.time()
    r = subprocess.run([sys.executable, "-m", "omnidroid", "start", name,
                        "--json", "--no-window", *extra],
                       capture_output=True, text=True)
    dt = time.time() - t0
    try:
        payload = json.loads(r.stdout.splitlines()[-1])
    except (ValueError, IndexError):
        print(r.stdout[-2000:], r.stderr[-2000:])
        sys.exit("could not parse `start --json` output")
    return dt, payload


def stop(name):
    subprocess.run([sys.executable, "-m", "omnidroid", "stop", name],
                   capture_output=True, text=True)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    name = sys.argv[1]

    stop(name)
    cold_dt, cold = start(name)
    print(f"run 1 (cold + bake): {cold_dt:.1f}s  "
          f"timings={json.dumps(cold.get('timings', {}))}")
    stop(name)

    warm_dt, warm = start(name)
    print(f"run 2 (warm restore): {warm_dt:.1f}s  "
          f"timings={json.dumps(warm.get('timings', {}))}")
    stop(name)

    ok = warm.get("ok") and warm_dt < cold_dt
    print(f"\nRESULT: {'PASS' if ok else 'FAIL'} - "
          f"cold {cold_dt:.1f}s vs warm {warm_dt:.1f}s")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Run it on a real host**

Run: `python3 tools/warm_restore_check.py <an existing account>`
Expected: run 2 is materially faster than run 1, `ok: true` on both, and the account lands in its place. Record both `timings` blocks — this is the Phase 0 measurement the spec asks for, and it is what decides whether the 15 s end-to-end target is reachable without the deferred warm-Roblox freeze.

- [ ] **Step 3: Verify auto-login and the executor surface survived**

Run, against a warm-restored instance:

```bash
python3 -m omnidroid list --json
python3 -m omnidroid screenshot <account>
python3 -m omnidroid status <account> --json
```

Expected: identical shape to a cold-booted instance. Confirm from the screenshot that the account is logged in and in its place — the golden entry is account-free, so this proves cookie injection still happens per launch.

- [ ] **Step 4: Reproduce and root-cause the concurrency blocker**

Start two accounts that resolve to the SAME golden entry (same base, offset and mode), one after the other.

Expected with the interim rule in place: the first restores, the second **cold-boots**, and both are reachable over adb. Confirm with `adb devices` that neither shows `offline`.

Then, to attack the root cause, temporarily bypass `_warm_cache_allowed`'s in-use check and start both from the entry. The second will come up alive on VNC but `offline` on adb. Evidence already gathered (do not redo): it is not a per-instance defect, not pre-existing (two cold boots are fine), not host-side adb-server dedup (a per-instance `ANDROID_ADB_SERVER_PORT` does not help), and `stop adbd; start adbd` before the freeze makes it worse.

Next hypotheses to test, in order:
1. Inspect the guest's socket state after restore: `adb shell su -c "ss -tanp | grep 5555"` on the working instance versus VNC-only inspection of the broken one.
2. Freeze at a point where adbd has never accepted a host connection — detect readiness from the serial log rather than over adb, so the frozen image contains no adb session at all.
3. Compare `getprop` output between the working and broken instances via the kiosk (which does not need adb) to find a differing identity property.

Record findings in the spec's §8b. Lifting the interim rule requires a root cause plus a passing two-instance restore.

- [ ] **Step 5: Commit**

```bash
git add tools/warm_restore_check.py
git commit -m "test: end-to-end warm-restore verifier and blocker repro steps"
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
| --- | --- |
| §2b(a) `-incoming defer` + QMP handshake | 7, 8, 9 |
| §2b(b) clock resync | 9, 11 |
| §5.1 cache key + invalidation | 3, 11 |
| §5.2 entry layout | 3, 4, 5 |
| §5.3 mem/smp pinning | 11 (key includes resolved mem/smp; restore forces them via `mode`) |
| §5.4 disk budget, LRU, orphan reclaim | 6, 11 |
| §6.1 warmcache API | 3, 4, 5, 6 |
| §6.2 qemu_proc warm/bake args | 8 |
| §6.3 `_ensure_booted` branch | 11 |
| §6.4 bake flow | 9, 11 |
| §7 post-restore step order | 11 |
| §8 cross-platform / WHPX degradation | 11 (empty `_qemu_version` or a failed restore both fall back) |
| §8b interim concurrency rule | 10, 11, 12 |
| §9 farming/KSM reporting | **not implemented** — spec calls it reporting-only; deferred, see below |
| §10 Phase 0 measurement | 1, 2, 12 |
| §11 testing | 3-11 unit, 12 integration |
| §12 out of scope | n/a |

**Known gap:** spec §9 (attribute KSM dedup to warm restore in `footprint`) has no task. It is reporting-only and changes no behaviour, so it is deliberately deferred rather than padding this plan; add it once §8b is resolved and farming actually uses the cache — until then there is nothing to report.

**Placeholder scan:** no TBD/TODO; every code step carries real code.

**Type consistency:** `cache_key` keyword-only signature is identical in Tasks 3 and 11. Entry filename constants (`STATE_NAME`, `SYSTEM_NAME`, `DATA_NAME`, `EFIVARS_NAME`, `META_NAME`) are used consistently in Tasks 3-9. `session_factory` has the same meaning in Task 9's two functions and its fake. `warm`/`bake`/`warm_key` keywords match across `qemu_command_arm`, `qemu_command` and `spawn_qemu` (Tasks 8, 10, 11). `warm_keys_in_use()` returns `set[str]` in Task 10 and is consumed as such in Task 11.

**One risk to flag for the implementer:** Task 11 modifies the busiest function in an 8,000-line file. Run the full suite before and after and diff the failure lists — several existing tests (`test_qemu_footprint`, `test_gaming_apply`, `test_session`) assert on boot behaviour and are the safety net for this change.
