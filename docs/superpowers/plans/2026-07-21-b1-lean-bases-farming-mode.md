# Omnidroid B1: Lean Bases + Farming Mode — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `farming` runtime mode (low mem/smp + a post-boot squeeze) and a measurement harness, then surgically trim the three bases (prod arm, prod x86, dev arm) — proving a joined-idle instance can reach <~400MB, measured at every step.

**Architecture:** Measure-first. Tasks 1–5 are OFFLINE, TDD, committable engine code (the farming mode, the squeeze-sequence builder, the measurement harness + stale-QEMU guard, the trim base-version registration). Tasks 6–9 are LIVE VERIFICATION RUNBOOKS — they run on the real engine against external images and are **manual checklists, not pytest** (a trim/boot/RSS measurement cannot be a unit test). The code tasks build the tools the runbooks use.

**Tech Stack:** Python 3, `unittest` (omnidroid convention: `from omnidroid import engine as omni`, run `python3 tests/test_x.py`), QEMU (arm-uefi + x86-bliss), adb.

## Global Constraints

- **Tests ARE git-tracked here** (14 tracked; `.gitignore` ignores only `.pytest_cache/`). Commit tests WITH source. This is the OPPOSITE of the omni-agent repos — do `git add tests/...`.
- Test convention: `unittest.TestCase`; header `sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))` then `from omnidroid import engine as omni`; run `python3 tests/test_<name>.py`. Mock engine internals with `mock.patch.object(omni, "<name>", ...)` (see `tests/test_ephemeral_boot.py`).
- Engine is `omnidroid/engine.py` (~6641 lines). Anchors: `MODES` at ~1155, `DEFAULT_MODE` at ~1160, `resolve_mode(cfg, name=None, mem=None)` at ~1163, `adb(acct, *args, timeout=20, check=False)` at ~1063, the base-modify/flatten flow at ~2522–2558, `cmd_ksm`/`cmd_bench_ksm` at ~4263/4324. `configs/paths.json` is the base registry (`bases`, `images_dir`, `current_base`).
- **Bases live external in `~/OmniImages` (Linux) — NEVER committed.** Trims produce NEW versioned images (`base_arm_v3`, `base_x86_v6`, `base_arm_devsystem_v3`); prior versions are ALWAYS retained (account overlays are COW-backed by them).
- Per-arch asymmetry: **x86** kiosk in `/system` (`adb root` + remount-rw + `/system` flatten). **arm** is a `user` build with **no `adb root`**; kiosk/apps live in the `/data` template (`base_arm_data.qcow2`) refreshed via the `update_kiosk_arm` copy-back pattern; matched-pair FBE must still decrypt after any arm flatten.
- `playable` and `DEFAULT_MODE` are UNCHANGED by this work (playable is where B2's GPU work lands later).
- Farming target: a **joined-but-idle** Roblox instance under **~400MB** guest RAM, phrased "reach <400MB or as-low-as-stable". `mem: 512` is a STARTING point measurement will tune, not a fixed value.
- Work directly on `main`. Every offline task ends with a commit of source + tests.

## File Structure

| File | Responsibility | Status |
|---|---|---|
| `omnidroid/engine.py` | `MODES` farming entry; wire squeeze into farming start | Modify (`MODES` ~1155) |
| `omnidroid/farming.py` | Pure squeeze-command-sequence builder | Create (Task 2) |
| `omnidroid/measure.py` | Measurement harness: stale-QEMU ps-guard, RSS/boot parse, JSON row | Create (Tasks 3–4) |
| `omnidroid/trimreg.py` | Trim base-version registration (bump + retain-prior, per-arch) | Create (Task 5) |
| `tests/test_farming_mode.py` | MODES/resolve_mode farming (Task 1) | Create |
| `tests/test_farming_squeeze.py` | Squeeze-sequence shape (Task 2) | Create |
| `tests/test_measure_guard.py` | ps-guard + boot-time parse (Task 3) | Create |
| `tests/test_measure_row.py` | Measurement JSON row shape (Task 4) | Create |
| `tests/test_trim_registration.py` | Trim version-bump + retain-prior (Task 5) | Create |
| `docs/superpowers/runbooks/B1-*.md` | Live verification runbooks (Tasks 6–9) | Create |

---

## Task 1: Farming MODES entry + resolve_mode (OFFLINE, TDD)

**Files:**
- Modify: `omnidroid/engine.py` (`MODES` ~1155)
- Test: `tests/test_farming_mode.py`

**Interfaces:**
- Consumes: existing `resolve_mode(cfg, name=None, mem=None)`.
- Produces: `MODES["farming"]` = `{"mem": 512, "smp": 2}`; `resolve_mode(cfg, "farming")` returns `{"mem": 512, "smp": 2, "name": "farming"}`. `playable`/`DEFAULT_MODE` unchanged.

- [ ] **Step 1: Write the failing test**

Create `tests/test_farming_mode.py`:

```python
#!/usr/bin/env python3
"""farming mode: a low mem/smp MODES entry; playable/DEFAULT unchanged.

    python3 tests/test_farming_mode.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


class FarmingMode(unittest.TestCase):
    def test_farming_in_modes_low_footprint(self):
        self.assertIn("farming", omni.MODES)
        self.assertLessEqual(omni.MODES["farming"]["mem"], 1024)
        self.assertLessEqual(omni.MODES["farming"]["smp"], 2)

    def test_resolve_farming(self):
        m = omni.resolve_mode({}, "farming")
        self.assertEqual(m["name"], "farming")
        self.assertEqual(m["mem"], omni.MODES["farming"]["mem"])
        self.assertEqual(m["smp"], omni.MODES["farming"]["smp"])

    def test_playable_and_default_unchanged(self):
        self.assertEqual(omni.DEFAULT_MODE, "playable")
        self.assertEqual(omni.MODES["playable"], {"mem": 4096, "smp": 4})


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 tests/test_farming_mode.py`
Expected: FAIL — `KeyError: 'farming'` / `AssertionError` (no farming entry).

- [ ] **Step 3: Write minimal implementation**

In `omnidroid/engine.py`, extend the `MODES` dict (~1155) — add the farming line, leave the rest untouched:

```python
MODES = {
    "playable": {"mem": 4096, "smp": 4},
    "hard":     {"mem": 3072, "smp": 4},
    "brutal":   {"mem": 2048, "smp": 2},
    # farming: headless, joined-idle, squeezed as small as stable. mem is a
    # STARTING point the live measurement (Task 9) tunes; the runtime squeeze
    # (farming.py) does the rest after boot.
    "farming":  {"mem": 512, "smp": 2},
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 tests/test_farming_mode.py`
Expected: PASS — `Ran 3 tests ... OK`.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/engine.py tests/test_farming_mode.py
git commit -m "feat(farming): add farming MODES entry (low mem/smp)

Headless joined-idle mode. mem=512 is a measurement starting point; the
runtime squeeze does the rest. playable/DEFAULT_MODE unchanged."
```

---

## Task 2: Farming squeeze-sequence builder (OFFLINE, TDD)

**Files:**
- Create: `omnidroid/farming.py`
- Test: `tests/test_farming_squeeze.py`

**Interfaces:**
- Consumes: nothing (pure).
- Produces: `build_squeeze_sequence() -> list[list[str]]` — the ordered list of adb `shell` argument-vectors that squeeze a joined-idle instance (stop residual services, throttle the backgrounded game, enable zram, tune lmkd). Pure and side-effect-free so it is unit-testable; a later live task applies it.

**Why a pure builder:** the squeeze must run over adb on a live guest, which isn't unit-testable — but its SHAPE (right commands, right order, no missing step) is. Splitting the builder from the applier makes the logic testable now and the live wiring trivial.

- [ ] **Step 1: Write the failing test**

Create `tests/test_farming_squeeze.py`:

```python
#!/usr/bin/env python3
"""The farming runtime-squeeze command sequence has the right shape.

    python3 tests/test_farming_squeeze.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import farming  # noqa: E402


class SqueezeSequence(unittest.TestCase):
    def setUp(self):
        self.seq = farming.build_squeeze_sequence()

    def test_returns_nonempty_list_of_argv(self):
        self.assertIsInstance(self.seq, list)
        self.assertGreater(len(self.seq), 0)
        for cmd in self.seq:
            self.assertIsInstance(cmd, list)
            self.assertTrue(all(isinstance(a, str) for a in cmd))
            self.assertEqual(cmd[0], "shell")  # every step is an adb shell cmd

    def test_flat_text_covers_the_four_levers(self):
        flat = " ".join(" ".join(c) for c in self.seq).lower()
        self.assertIn("zram", flat)                  # memory: zram swap
        self.assertIn("lmk", flat)                   # lmkd / lowmemorykiller tune
        self.assertRegex(flat, r"cpu|cgroup|cpuset") # game CPU throttle
        self.assertRegex(flat, r"idle|stop|disable") # quiesce residual work

    def test_deterministic_order(self):
        self.assertEqual(self.seq, farming.build_squeeze_sequence())
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 tests/test_farming_squeeze.py`
Expected: FAIL — `ModuleNotFoundError: No module named 'omnidroid.farming'`.

- [ ] **Step 3: Write minimal implementation**

Create `omnidroid/farming.py`:

```python
"""Farming-mode runtime squeeze.

A joined-but-idle Roblox instance is pushed as small as stable AFTER boot, over
adb, without a separate image. This module only BUILDS the command sequence
(pure, unit-testable); the engine applies it on a farming-mode boot.

Levers (each a step below):
  - stop residual services the trim left running but farming doesn't need;
  - throttle the backgrounded game process via a CPU cgroup/cpuset cap so a
    joined-idle instance does minimal work (safe: instances are headless, no
    active render surface);
  - enable zram swap so the guest can reclaim under the low mem cap;
  - tune lmkd (lowmemorykiller) thresholds to reclaim hard WITHOUT OOM-killing
    the game itself.

The exact service list / thresholds are refined by live measurement (the B1
runbooks); the shape here is the contract."""

# The Roblox package the farming instance keeps joined-idle.
GAME_PKG = "com.roblox.client"


def build_squeeze_sequence():
    """Ordered list of adb `shell` argv vectors for the farming squeeze."""
    return [
        # 1) Quiesce residual background work farming doesn't need. Safe on a
        #    headless kiosk instance; the live runbook extends this list with
        #    specific services proven idle-safe by measurement.
        ["shell", "cmd", "activity", "idle-maintenance"],
        ["shell", "settings", "put", "global", "window_animation_scale", "0"],
        # 2) zram swap on, so the guest reclaims under the low mem cap.
        ["shell", "sh", "-c",
         "swapon /dev/block/zram0 2>/dev/null || "
         "(zramctl -f -s 256M 2>/dev/null; mkswap /dev/block/zram0 2>/dev/null; "
         "swapon /dev/block/zram0 2>/dev/null); true"],
        # 3) lmkd: reclaim aggressively but keep the game alive.
        ["shell", "sh", "-c",
         "setprop ro.lmk.use_psi true; setprop ro.lmk.critical_upgrade true; "
         "setprop ctl.restart lmkd; true"],
        # 4) Throttle the backgrounded game via a cpuset (background cores).
        ["shell", "sh", "-c",
         f"PID=$(pidof {GAME_PKG} 2>/dev/null); "
         f"[ -n \"$PID\" ] && echo $PID > /dev/cpuset/background/tasks "
         f"2>/dev/null; true"],
    ]
```

Note: the service/threshold specifics (which services to quiesce, exact lmkd values) are refined during the live runbook (Task 9) — the SHAPE tested here is the contract. Keep the sequence a flat, deterministic list of `["shell", ...]` argv vectors.

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 tests/test_farming_squeeze.py`
Expected: PASS — `Ran 3 tests ... OK`.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/farming.py tests/test_farming_squeeze.py
git commit -m "feat(farming): pure runtime-squeeze command-sequence builder

Stop residual services, zram, lmkd tune, background-cpuset throttle for the
backgrounded game. Pure/testable; the engine applies it on a farming boot."
```

---

## Task 3: Stale-QEMU ps-guard + boot-time parse (OFFLINE, TDD)

**Files:**
- Create: `omnidroid/measure.py`
- Test: `tests/test_measure_guard.py`

**Interfaces:**
- Consumes: nothing (pure parsers).
- Produces:
  - `parse_boot_minutes(log_text) -> float | None` — extract the "boot completed after X min" value from an engine boot log.
  - `is_suspect_boot(minutes) -> bool` — True when minutes is ~0.0 (the stale-QEMU tell: attached to an already-running instance, not a real boot).
  - `stray_qemu_pids(ps_text, known_pids) -> list[int]` — from `ps` output, the `qemu-system-*` PIDs NOT in `known_pids` (instances the engine lost track of).

**Why:** a measurement taken against a mis-attached/stray instance is worthless. These are the pure guards; the live harness (Task 4) refuses to record a measurement that trips them.

- [ ] **Step 1: Write the failing test**

Create `tests/test_measure_guard.py`:

```python
#!/usr/bin/env python3
"""Measurement guards: stale-QEMU detection + boot-time parse.

    python3 tests/test_measure_guard.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import measure  # noqa: E402


class BootParse(unittest.TestCase):
    def test_parses_minutes(self):
        self.assertEqual(
            measure.parse_boot_minutes("... boot completed after 0.5 min"), 0.5)

    def test_none_when_absent(self):
        self.assertIsNone(measure.parse_boot_minutes("no such line here"))

    def test_zero_boot_is_suspect(self):
        self.assertTrue(measure.is_suspect_boot(0.0))
        self.assertTrue(measure.is_suspect_boot(0.02))

    def test_real_boot_not_suspect(self):
        self.assertFalse(measure.is_suspect_boot(0.5))


class StrayQemu(unittest.TestCase):
    PS = (
        "USER  PID  COMMAND\n"
        "berat 100 qemu-system-aarch64 -name omni-alice ...\n"
        "berat 200 qemu-system-x86_64 -name omni-bob ...\n"
        "berat 300 python engine.py\n"
    )

    def test_flags_unknown_qemu(self):
        self.assertEqual(measure.stray_qemu_pids(self.PS, {100}), [200])

    def test_none_when_all_known(self):
        self.assertEqual(measure.stray_qemu_pids(self.PS, {100, 200}), [])

    def test_ignores_nonqemu(self):
        self.assertNotIn(300, measure.stray_qemu_pids(self.PS, set()))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 tests/test_measure_guard.py`
Expected: FAIL — `ModuleNotFoundError: No module named 'omnidroid.measure'`.

- [ ] **Step 3: Write minimal implementation**

Create `omnidroid/measure.py`:

```python
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 tests/test_measure_guard.py`
Expected: PASS — `Ran 7 tests ... OK`.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/measure.py tests/test_measure_guard.py
git commit -m "feat(measure): stale-QEMU ps-guard + boot-time parse

Pure guards so a B1 measurement taken against a stray/mis-attached qemu
(the 'boot completed after 0.0 min' tell) is discarded, not recorded."
```

---

## Task 4: Measurement row (RSS + boot) JSON shape (OFFLINE, TDD)

**Files:**
- Modify: `omnidroid/measure.py`
- Test: `tests/test_measure_row.py`

**Interfaces:**
- Consumes: `parse_boot_minutes`, `is_suspect_boot` (Task 3).
- Produces:
  - `parse_guest_used_kb(meminfo_text) -> int | None` — guest used RAM = `MemTotal - MemAvailable` from `/proc/meminfo` text.
  - `measurement_row(base, mode, arch, boot_minutes, host_rss_kb, guest_used_kb) -> dict` — a comparable JSON row: `{base, mode, arch, boot_minutes, host_rss_mb, guest_used_mb, suspect: bool, ts}`. `suspect` is True if `is_suspect_boot(boot_minutes)`.

**Why:** Phase 0/1/2 deltas are only comparable if every measurement is the same shape. This is the row the live runbooks emit.

- [ ] **Step 1: Write the failing test**

Create `tests/test_measure_row.py`:

```python
#!/usr/bin/env python3
"""Measurement row shape + guest-meminfo parse.

    python3 tests/test_measure_row.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import measure  # noqa: E402

MEMINFO = "MemTotal:  524288 kB\nMemFree: 100000 kB\nMemAvailable: 424288 kB\n"


class GuestUsed(unittest.TestCase):
    def test_used_is_total_minus_available(self):
        self.assertEqual(measure.parse_guest_used_kb(MEMINFO), 524288 - 424288)

    def test_none_when_unparseable(self):
        self.assertIsNone(measure.parse_guest_used_kb("garbage"))


class Row(unittest.TestCase):
    def test_row_shape_and_units(self):
        row = measure.measurement_row(
            base="base_arm_v3", mode="farming", arch="arm",
            boot_minutes=0.5, host_rss_kb=800_000, guest_used_kb=380_000)
        self.assertEqual(row["base"], "base_arm_v3")
        self.assertEqual(row["mode"], "farming")
        self.assertEqual(row["arch"], "arm")
        self.assertEqual(row["boot_minutes"], 0.5)
        self.assertEqual(row["guest_used_mb"], round(380_000 / 1024, 1))
        self.assertEqual(row["host_rss_mb"], round(800_000 / 1024, 1))
        self.assertFalse(row["suspect"])
        self.assertIn("ts", row)

    def test_suspect_flag_set_on_zero_boot(self):
        row = measure.measurement_row(
            base="b", mode="farming", arch="arm",
            boot_minutes=0.0, host_rss_kb=1, guest_used_kb=1)
        self.assertTrue(row["suspect"])


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 tests/test_measure_row.py`
Expected: FAIL — `AttributeError: module 'omnidroid.measure' has no attribute 'parse_guest_used_kb'`.

- [ ] **Step 3: Write minimal implementation**

Append to `omnidroid/measure.py`:

```python
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 tests/test_measure_row.py`
Expected: PASS — `Ran 4 tests ... OK`.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/measure.py tests/test_measure_row.py
git commit -m "feat(measure): comparable measurement row + guest-meminfo parse

guest_used = MemTotal-MemAvailable; row normalizes to MB and carries the
suspect flag so Phase 0/1/2 deltas are directly comparable."
```

---

## Task 5: Trim base-version registration (OFFLINE, TDD)

**Files:**
- Create: `omnidroid/trimreg.py`
- Test: `tests/test_trim_registration.py`

**Interfaces:**
- Consumes: the `configs/paths.json` shape (`bases`, `version`, `changelog`).
- Produces: `register_trim(cfg, base_key, new_disk, new_system, note) -> dict` — returns a NEW cfg with the trimmed base bumped to the next version, recording `new_disk`/`new_system` and appending `note` to `changelog`, WITHOUT mutating or deleting the prior version's image references. Mirrors the existing `test_dev_base_registration.py` preserve-prior discipline.

**Why:** a trim MUST bump the version and retain the prior image (accounts are COW-backed by it). This is pure dict logic, unit-testable, distinct from the live flatten.

- [ ] **Step 1: Write the failing test**

Create `tests/test_trim_registration.py`:

```python
#!/usr/bin/env python3
"""A trim registration bumps the version and RETAINS the prior image refs.

    python3 tests/test_trim_registration.py
"""
import copy
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import trimreg  # noqa: E402

CFG = {
    "current_base": "arm",
    "bases": {
        "arm": {"type": "arm-uefi", "base_disk": "base_arm_v2.qcow2",
                "system": "base_arm_system.qcow2", "version": 2,
                "changelog": {"2": "branded"}},
    },
}


class TrimRegistration(unittest.TestCase):
    def test_bumps_version_and_records_new_images(self):
        out = trimreg.register_trim(
            copy.deepcopy(CFG), "arm",
            new_disk="base_arm_v3.qcow2", new_system="base_arm_system_v3.qcow2",
            note="trim: removed stock browser/gallery/telephony")
        self.assertEqual(out["bases"]["arm"]["version"], 3)
        self.assertEqual(out["bases"]["arm"]["base_disk"], "base_arm_v3.qcow2")
        self.assertEqual(out["bases"]["arm"]["system"], "base_arm_system_v3.qcow2")
        self.assertIn("3", out["bases"]["arm"]["changelog"])
        self.assertIn("trim", out["bases"]["arm"]["changelog"]["3"])

    def test_prior_changelog_retained(self):
        out = trimreg.register_trim(
            copy.deepcopy(CFG), "arm", new_disk="d", new_system="s", note="n")
        self.assertEqual(out["bases"]["arm"]["changelog"]["2"], "branded")

    def test_does_not_mutate_input(self):
        cfg = copy.deepcopy(CFG)
        trimreg.register_trim(cfg, "arm", new_disk="d", new_system="s", note="n")
        self.assertEqual(cfg["bases"]["arm"]["version"], 2)  # input untouched

    def test_unknown_base_raises(self):
        with self.assertRaises(KeyError):
            trimreg.register_trim(copy.deepcopy(CFG), "nope",
                                  new_disk="d", new_system="s", note="n")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 tests/test_trim_registration.py`
Expected: FAIL — `ModuleNotFoundError: No module named 'omnidroid.trimreg'`.

- [ ] **Step 3: Write minimal implementation**

Create `omnidroid/trimreg.py`:

```python
"""Register a trimmed base as a NEW version without disturbing the prior one.

A trim flatten produces a new image; the prior image MUST stay referenced-safe
because existing account overlays are COW-backed by it. This is pure cfg logic
(the live flatten is a runbook step); it mirrors the preserve-prior discipline
of test_dev_base_registration."""
import copy


def register_trim(cfg, base_key, new_disk, new_system, note):
    """Return a NEW cfg with base_key bumped a version, prior refs retained."""
    if base_key not in cfg.get("bases", {}):
        raise KeyError(f"no base {base_key!r}")
    out = copy.deepcopy(cfg)
    base = out["bases"][base_key]
    new_ver = int(base.get("version", 0)) + 1
    base["version"] = new_ver
    base["base_disk"] = new_disk
    base["system"] = new_system
    changelog = base.setdefault("changelog", {})
    changelog[str(new_ver)] = note
    return out
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 tests/test_trim_registration.py`
Expected: PASS — `Ran 4 tests ... OK`.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/trimreg.py tests/test_trim_registration.py
git commit -m "feat(trim): base-version registration that retains the prior image

A trim bumps version + records the new disk/system + appends a changelog
note, without mutating or dropping the prior version (accounts COW-back it)."
```

---

## Task 6: Wire the squeeze into a farming-mode boot (OFFLINE where possible, TDD)

**Files:**
- Modify: `omnidroid/engine.py` (the post-boot path that runs after `boot_completed` on `start`)
- Test: `tests/test_farming_apply.py`

**Interfaces:**
- Consumes: `farming.build_squeeze_sequence` (Task 2), `resolve_mode` (Task 1), `adb` (~1063).
- Produces: `apply_farming_squeeze(acct)` in `engine.py` — iterates `build_squeeze_sequence()` and runs each via `adb(acct, *cmd)`. Called from the post-boot path ONLY when the resolved mode name is `"farming"`. A non-farming boot never calls it.

- [ ] **Step 1: Write the failing test**

Create `tests/test_farming_apply.py`:

```python
#!/usr/bin/env python3
"""apply_farming_squeeze runs each built step over adb; only in farming mode.

    python3 tests/test_farming_apply.py
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import farming  # noqa: E402


class ApplySqueeze(unittest.TestCase):
    def test_runs_every_step_over_adb(self):
        acct = {"name": "u1"}
        with mock.patch.object(omni, "adb") as adb:
            omni.apply_farming_squeeze(acct)
        expected = len(farming.build_squeeze_sequence())
        self.assertEqual(adb.call_count, expected)
        # first positional arg of each call is the account
        for call in adb.call_args_list:
            self.assertIs(call.args[0], acct)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 tests/test_farming_apply.py`
Expected: FAIL — `AttributeError: module 'omnidroid.engine' has no attribute 'apply_farming_squeeze'`.

- [ ] **Step 3: Write minimal implementation**

Add to `omnidroid/engine.py` (near the other adb helpers), and import `farming` at the top with the other `from omnidroid import ...` lines:

```python
from omnidroid import farming  # (with the other omnidroid imports)


def apply_farming_squeeze(acct):
    """Run the farming runtime squeeze over adb. Call only on a farming boot."""
    for cmd in farming.build_squeeze_sequence():
        adb(acct, *cmd, timeout=20)
```

Then, in the post-`boot_completed` path of the start flow, gate the call on the resolved mode name (find where the boot completes and the mode is known — the `mode` dict from `resolve_mode` carries `name`):

```python
    if (mode or {}).get("name") == "farming":
        apply_farming_squeeze(acct)
```

Locate the exact insertion point by searching for where `boot completed` is logged / where post-boot provisioning runs in `cmd_start`; place the gated call after the instance is confirmed booted (adb `device` state), so the squeeze runs on a live guest. Do NOT call it for any other mode.

- [ ] **Step 4: Run test to verify it passes**

Run: `python3 tests/test_farming_apply.py`
Expected: PASS — `Ran 1 test ... OK`.

- [ ] **Step 5: Run the full offline suite (no regressions)**

Run: `python3 -m pytest tests/ -q` (or run each `tests/test_*.py` if pytest isn't configured)
Expected: the 5 new B1 test files pass; pre-existing tests unaffected. Record any pre-existing failures BEFORE this task so they aren't misattributed.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/engine.py tests/test_farming_apply.py
git commit -m "feat(farming): apply the runtime squeeze on a farming-mode boot

Gated on resolved mode name == farming; runs each built adb step post-boot.
No other mode calls it."
```

---

## LIVE VERIFICATION RUNBOOKS (Tasks 7–9) — manual, NOT pytest

These run on the real engine against external `~/OmniImages` images. They are
**checklists an operator/agent executes and records**, not automated tests. Each
produces a committed runbook doc under `docs/superpowers/runbooks/` capturing the
measured numbers (the deliverable is the recorded evidence + the new trimmed
image registered via Task 5's `register_trim`). A subagent CANNOT complete these
without a live engine — mark them BLOCKED-until-live if run in a sandbox.

### Task 7: Phase 0 baseline + Phase 1 PROD ARM trim

**Deliverable:** `docs/superpowers/runbooks/B1-prod-arm-trim.md` with recorded rows.

- [ ] **Step 1: Baseline measure (Phase 0).** Boot `base_arm` (playable), join Roblox to a place, let it idle. Record a `measurement_row` (boot_minutes, host qemu RSS via `ps`, guest used via `adb shell cat /proc/meminfo`). Run `stray_qemu_pids` first; if suspect, kill strays and re-measure. This row is the arm baseline.
- [ ] **Step 2: Trim batch — arm /data template.** Via the `update_kiosk_arm` copy-back pattern, remove unused `/data` apps (stock browser/gallery/email/etc. if present there). Boot a throwaway account, remove, copy `/data` back.
- [ ] **Step 3: PROD floor smoke-test.** kiosk boots → launches Roblox APK → adb reachable on a FRESH boot → cookie login works → VNC view works → Lock-Task kiosk intact. If any fail, REVERT this batch.
- [ ] **Step 4: Trim batch — arm /system/product bloat** (constrained: no `adb root`; keep `/product/app/Roblox`). Flatten. **Re-run the PROD floor AND confirm the matched-pair FBE still decrypts** (clean boot to a usable kiosk). Revert if the pair desyncs.
- [ ] **Step 5: Register + re-measure.** `register_trim(cfg, "arm", new_disk=..., new_system=..., note="trim: <what was removed>")`, write the new image to `~/OmniImages`, retain the prior. Re-measure post-trim RSS + boot time. Record the delta vs Step 1.
- [ ] **Step 6: Commit the runbook** (source doc only — images are never committed): `git add docs/superpowers/runbooks/B1-prod-arm-trim.md && git commit -m "docs(runbook): B1 prod arm trim — measured baseline vs trimmed"`.

### Task 8: Phase 1 PROD X86 trim (from ARM, slow TCG)

**Deliverable:** `docs/superpowers/runbooks/B1-prod-x86-trim.md`.

- [ ] **Step 1: Baseline measure.** Boot `base_x86` (playable) — on the ARM Mac this runs under slow TCG; that's accepted. Join Roblox, idle, record the row (guard against strays first).
- [ ] **Step 2: Trim via the /system flow.** x86 supports `adb root` + `mount -o remount,rw /` + `/system` edits + flatten (engine.py ~2522–2558). Remove unused apps/services in batches.
- [ ] **Step 3: PROD floor smoke-test** (same six checks). Revert any batch that fails.
- [ ] **Step 4: Register + re-measure.** `register_trim(cfg, "x86", ...)`, retain prior, record delta.
- [ ] **Step 5: Commit the runbook.**

### Task 9: Phase 1 DEV ARM trim + Phase 2 farming measure + Phase 3 decision gate

**Deliverable:** `docs/superpowers/runbooks/B1-dev-trim-and-farming.md`.

- [ ] **Step 1: DEV ARM trim** on `base_arm_devsystem` (lowest priority, done last). Trim in batches.
- [ ] **Step 2: DEV floor smoke-test** — PROD floor PLUS: frida-server reachable on 27142, Magisk `su` works, devkit vdc mounts + activates (`_devkit_activate`), dev-UI/kiosk toggle works, always-on screenshots work. A trim that breaks `su`/frida is REVERTED.
- [ ] **Step 3: Register + measure dev.** `register_trim(cfg, "dev", ...)`, retain prior.
- [ ] **Step 4: Farming measure (Phase 2).** On the trimmed `base_arm` (and `base_x86`), `start --mode farming` (which now applies the squeeze). Join Roblox, idle, confirm the session STAYS connected after the squeeze (not AFK-kicked). Record farming joined-idle RSS + boot time. Guard against strays.
- [ ] **Step 5: Phase 3 decision gate.** Compare farming `guest_used_mb` to 400. **If ≤400 (or as-low-as-stable):** DONE — one base per arch; record the achieved number. **If >400:** record the shortfall and open a follow-up spec for a dedicated stripped farming image (do NOT build it here — that's a new decision with data).
- [ ] **Step 6: Commit the runbook** with the full measurement table (baseline → trimmed → farming, both arches) and the decision-gate verdict.

---

## Self-review notes (for the executor)

- **Offline vs live is the spine.** Tasks 1–6 are real TDD engine code (farming mode, squeeze builder+applier, measurement guards+row, trim registration). Tasks 7–9 are live runbooks — they CANNOT be unit tests and must run on the real engine; a sandboxed subagent should report BLOCKED-until-live, not fake them.
- **Tests are tracked here** — every offline task commits `tests/...` WITH source (unlike the omni-agent repos).
- **`mem: 512` is a starting point**, not a guarantee — Task 9 Step 5 sets the real floor. The plan never claims <400MB is achieved; it MEASURES whether it is.
- **Never delete a prior base version** — `register_trim` only adds; the runbooks write new images and retain old ones.
- **Task 6 insertion point** (the farming gate in `cmd_start`) needs the real post-boot location — the implementer must find where `boot_completed` is confirmed and the `mode` dict is in scope, and place the gated call there. Flagged as the one spot needing live-code orientation.
