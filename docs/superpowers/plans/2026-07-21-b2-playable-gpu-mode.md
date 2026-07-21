# Omnidroid B2: Playable GPU Mode — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove (via one live experiment) whether the guest can render Roblox with real GPU acceleration in a native window, then — only if it can — build a `playable` GPU mode that opens an accelerated window on the host and degrades safely everywhere else.

**Architecture:** Spike-first. Task 1 adds a tiny reversible experiment apparatus (an env-var arg-swap). Task 2 is the LIVE spike the user runs on their Mac — **the gate**. Its result decides the branch: GREEN → Tasks 3–5 build the real capability-detected, gracefully-degrading playable-GL mode; NOT GREEN → Task 3-ALT documents the finding and opens a B3 spec. Compatibility is the top priority — every host either accelerates or falls back to today's headless+VNC, never crashes.

**Tech Stack:** Python 3, `unittest` (omnidroid convention: `from omnidroid import engine as omni`, run `python3 tests/test_x.py`), QEMU (arm-uefi + x86), adb.

## Global Constraints

- **Tests ARE git-tracked here** (omnidroid convention; `.gitignore` ignores only `.pytest_cache/`). Commit tests WITH source: `git add omnidroid/... tests/...`.
- Test convention: `unittest.TestCase`; header `sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))` then `from omnidroid import engine as omni`; run `python3 tests/test_<name>.py`. Mock engine internals with `mock.patch.object(omni, "<name>", ...)` and platform constants with `mock.patch.object(omni, "IS_MACOS", True)`. See `tests/test_ephemeral_boot.py` for the `qemu_command_arm` mocking pattern.
- Engine anchors: `qemu_command_arm(acct, cfg, dev, mode=None, accel=None)` at engine.py:1191; the arm GPU/display block `"-device","virtio-gpu-pci","-display","none",` at **1259-1260**; x86 GPU at ~1315-1327; `default_accel()` at 1109; platform constants `IS_MACOS`/`IS_LINUX`/`IS_WINDOWS` (imported ~line 39). The existing `--window` flag opens omnidroid's VNC VIEWER (`_spawn_builtin_viewer` ~2212) — playable-GL is a SEPARATE path (a native accelerated QEMU window), do not conflate them.
- Compatibility is the TOP priority. Every matrix cell `{macOS-arm, Linux-x86, Linux-arm, Windows-x86} × {accel, none}` must degrade safely; NEVER crash or regress a boot. Default behavior (no play request) stays byte-for-byte today's headless path.
- Measurement rig: the pre-installed Roblox is flagged and black-screens — install the bootstrap APK `~/Desktop/overnight tests/update test/roblox-v2.726-bootstrap.apk`, log in, join place `8737899170`, assess there.
- Bases live external in `~/OmniImages`, never committed. Work on `main`.

## File Structure

| File | Responsibility | Status |
|---|---|---|
| `omnidroid/engine.py` | Spike env swap (T1); `default_display()` detector (T3); decision fn + wiring (T4) | Modify (arm block 1259-1260) |
| `tests/test_gl_spike.py` | Spike arg-swap is applied only when requested (T1) | Create |
| `tests/test_default_display.py` | Capability detector per matrix cell (T3) | Create |
| `tests/test_gl_decision.py` | Accelerated-vs-headless decision fn (T4) | Create |
| `docs/superpowers/runbooks/B2-spike.md` | The plain-language live experiment (T2) | Create |
| `docs/superpowers/runbooks/B2-playable-confirm.md` | Live confirm of the real mode (T5, if green) | Create |
| `docs/superpowers/specs/2026-07-2X-b3-guest-gpu-driver-design.md` | B3 stub (T3-ALT, if not green) | Create (conditional) |

---

## Task 1: Spike apparatus — `OMNI_GL_WINDOW` arg-swap (OFFLINE, TDD)

**Why first, before the runbook:** the user is not technical and cannot hand-paste a ~40-arg QEMU command. This tiny, reversible env-var swap turns the spike into a one-line `OMNI_GL_WINDOW=1 omni start ...`. It changes NOTHING unless the env var is set.

**Files:**
- Modify: `omnidroid/engine.py` (arm block 1259-1260, + a helper)
- Test: `tests/test_gl_spike.py`

**Interfaces:**
- Produces: `_gl_window_requested() -> bool` (True iff env `OMNI_GL_WINDOW` is a truthy value). In `qemu_command_arm`, when `_gl_window_requested()` AND `IS_MACOS` AND not `dev`, the GPU/display args become `["-device","virtio-gpu-gl","-display","cocoa,gl=on"]` instead of `["-device","virtio-gpu-pci","-display","none"]`. Everything else (VNC line included) unchanged. When the env var is unset, the command is byte-for-byte today's.

- [ ] **Step 1: Write the failing test**

Create `tests/test_gl_spike.py`:

```python
#!/usr/bin/env python3
"""OMNI_GL_WINDOW swaps the arm GPU/display to accelerated — only when set.

    python3 tests/test_gl_spike.py
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


def _acct():
    return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
            "vnc_port": 18001, "base": "arm"}


def _cfg():
    return {"images_dir": "/imgs", "current_base": "arm",
            "bases": {"arm": {"type": "arm-uefi", "system": "base_arm_system.qcow2",
                              "data": "base_arm_data.qcow2",
                              "base_disk": "base_arm_v2.qcow2",
                              "efivars": "base_arm_efivars.fd"}},
            "qemu": {"mem_mb": 4096, "smp": 4}}


def _cmd(gl_env, is_mac=True):
    env = {"OMNI_GL_WINDOW": "1"} if gl_env else {}
    with mock.patch.dict(os.environ, env, clear=False), \
         mock.patch.object(omni, "IS_MACOS", is_mac), \
         mock.patch.object(omni, "qemu_bin", side_effect=lambda x: x), \
         mock.patch.object(omni, "default_accel", return_value="hvf"), \
         mock.patch.object(omni, "resolve_mode",
                           return_value={"smp": 4, "mem": 4096, "name": "playable"}), \
         mock.patch.object(omni, "_assert_port_triple", return_value=1), \
         mock.patch.object(omni, "runtime_dir", side_effect=lambda n: __import__("pathlib").Path(f"/RT/{n}")), \
         mock.patch.object(omni, "account_dir", side_effect=lambda n: __import__("pathlib").Path(f"/AC/{n}")):
        if not gl_env:
            os.environ.pop("OMNI_GL_WINDOW", None)
        return " ".join(omni.qemu_command_arm(_acct(), _cfg(), dev=False))


class GlSpikeSwap(unittest.TestCase):
    def test_gl_env_swaps_to_accelerated(self):
        cmd = _cmd(gl_env=True)
        self.assertIn("virtio-gpu-gl", cmd)
        self.assertIn("cocoa,gl=on", cmd)
        self.assertNotIn("virtio-gpu-pci", cmd)

    def test_no_env_is_unchanged_headless(self):
        cmd = _cmd(gl_env=False)
        self.assertIn("virtio-gpu-pci", cmd)
        self.assertIn("-display none", cmd)
        self.assertNotIn("virtio-gpu-gl", cmd)

    def test_helper_reads_env(self):
        with mock.patch.dict(os.environ, {"OMNI_GL_WINDOW": "1"}):
            self.assertTrue(omni._gl_window_requested())
        os.environ.pop("OMNI_GL_WINDOW", None)
        self.assertFalse(omni._gl_window_requested())


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 tests/test_gl_spike.py`
Expected: FAIL — `AttributeError: module 'omnidroid.engine' has no attribute '_gl_window_requested'`.

- [ ] **Step 3: Write minimal implementation**

Add a helper near `default_accel` (engine.py ~1109):

```python
def _gl_window_requested():
    """B2 spike apparatus: env OMNI_GL_WINDOW=1 asks a start to open a native
    GPU-accelerated window instead of the headless VNC path. Reversible and
    off by default — this is the experiment switch, replaced by the real
    capability-gated playable mode once the spike proves feasibility."""
    return os.environ.get("OMNI_GL_WINDOW", "").strip() not in ("", "0", "false", "False")
```

In `qemu_command_arm`, replace the fixed block at 1259-1260:

```python
        "-device", "virtio-gpu-pci",
        "-display", "none",       # headless ALWAYS (same rule as x86)
```

with a computed pair (build it just before the args list, then splice it in — keep the surrounding `-drive`/`-vnc` lines exactly as they are):

```python
        *(["-device", "virtio-gpu-gl", "-display", "cocoa,gl=on"]
          if (_gl_window_requested() and IS_MACOS and not dev)
          else ["-device", "virtio-gpu-pci", "-display", "none"]),
```

(If the surrounding literal-list style makes an inline `*(...)` awkward, instead compute `gpu_display = [...]` above the return list and splice `*gpu_display,` in place of the two lines. Either is fine; the test only checks the resulting command string.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `python3 tests/test_gl_spike.py`
Expected: PASS — `Ran 3 tests ... OK`.

- [ ] **Step 5: Full suite (no regressions)**

Run: `python3 -m pytest tests/ -q`
Expected: 180 baseline (after B1) + 3 new, all green. Record baseline first.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/engine.py tests/test_gl_spike.py
git commit -m "feat(b2-spike): OMNI_GL_WINDOW arg-swap apparatus (arm/macOS)

Reversible experiment switch: with the env set, an arm start on macOS opens
a native virtio-gpu-gl + cocoa,gl=on window instead of headless. Off by
default; everything unchanged when unset. This is the spike apparatus."
```

---

## Task 2: THE SPIKE — live experiment runbook (MANUAL, the GATE)

**This is the decision gate. The user runs it on the ARM Mac. It is NOT a test.**
Deliverable: `docs/superpowers/runbooks/B2-spike.md` with the RESULT recorded.

- [ ] **Step 1: Write the runbook doc**

Create `docs/superpowers/runbooks/B2-spike.md` with this plain-language content (fill the RESULT section after running):

```markdown
# B2 Spike — does the guest render with a real GPU? (run on the ARM Mac)

Goal: find out, with ONE experiment, whether Roblox renders with real graphics
acceleration in a native window. The result decides the rest of B2.

## What you'll do
Boot one instance with the experimental accelerated window turned on, get Roblox
running and joined to a place, and look at how it renders.

## Steps (copy/paste each command)

1. Pick a logged-in account name you already have (from `omni login`). Call it
   ACCT below. If you have none, run `omni login` first.

2. Start it WITH the accelerated window (the `OMNI_GL_WINDOW=1` prefix is the
   only difference from a normal start):

       OMNI_GL_WINDOW=1 python3 -m omnidroid start ACCT --mode playable

   A QEMU window should open on your Mac. If NO window opens, or QEMU errors
   about `cocoa`/`gl`/`virtio-gpu-gl`, write that down in RESULT (that itself is
   a finding) and skip to "Recording the result".

3. Install the bootstrap APK (NOT the flagged pre-installed one). In another
   terminal:

       adb -s 127.0.0.1:16001 install -r "$HOME/Desktop/overnight tests/update test/roblox-v2.726-bootstrap.apk"

   (16001 is the default adb port of the first instance; if you started a second
   one it's 16002, etc. `omni list` shows ports.)

4. In the window, let Roblox open and log in (the bootstrap cookie handles it),
   then join place id **8737899170** (the account/session should deep-link; if it
   lands on home, open that place).

5. LOOK at the game in the window and answer:
   - Does it render smoothly / look like real 3D?  → likely ACCELERATED.
   - Does it render but choppy/laggy like slideshow? → SOFTWARE (no accel).
   - Black screen / never draws the game?           → BLACK / broken.

## Recording the result
Fill this in and commit the file:

- Window opened? (yes/no + any QEMU error):
- Roblox installed + logged in + joined 8737899170? (yes/no):
- Render verdict (ACCELERATED / SOFTWARE / BLACK):
- Notes (fps feel, anything odd, anti-cheat behavior):

## What the result means
- ACCELERATED → the guest already has what it needs. Proceed to B2 Tasks 3–5
  (build the real playable mode). 
- SOFTWARE or BLACK → the guest is missing a GPU driver. STOP B2 here; do Task
  3-ALT (write the B3 spec for adding a guest GPU driver stack) instead.
```

- [ ] **Step 2: (User, live) run the experiment** exactly as the runbook says, on the Mac.

- [ ] **Step 3: Record the verdict** in the runbook's RESULT section and commit:

```bash
git add docs/superpowers/runbooks/B2-spike.md
git commit -m "docs(b2): spike runbook + recorded result (ACCELERATED/SOFTWARE/BLACK)"
```

- [ ] **Step 4: BRANCH.** If verdict = ACCELERATED → do Tasks 3, 4, 5. Otherwise → do Task 3-ALT and STOP.

---

## Task 3: `default_display()` capability detector (OFFLINE, TDD) — only if spike GREEN

**Files:**
- Modify: `omnidroid/engine.py` (add `default_display`, near `default_accel` 1109)
- Test: `tests/test_default_display.py`

**Interfaces:**
- Produces: `default_display(qemu_display_help="", qemu_device_help="", has_gui=True) -> dict` returning `{"available": bool, "display_args": list, "gpu_args": list, "reason": str}`. Per platform picks the backend (`cocoa` macOS, `gtk`/`sdl` Linux+Windows) + `virtio-gpu-gl`. `available=False` (with a reason) when: no GUI/display present, OR the qemu help text lacks the chosen display or `virtio-gpu-gl`. Pure — all host facts are passed in, so it is fully unit-testable.

- [ ] **Step 1: Write the failing test**

Create `tests/test_default_display.py`:

```python
#!/usr/bin/env python3
"""default_display() picks the right accelerated backend or degrades safely.

    python3 tests/test_default_display.py
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402

MAC_HELP_DISPLAY = "cocoa\ngtk\nsdl\nvnc\nnone\negl-headless\n"
MAC_HELP_DEVICE = "virtio-gpu-pci\nvirtio-gpu-gl\nvirtio-vga\n"


class Detector(unittest.TestCase):
    def test_macos_accelerated(self):
        with mock.patch.object(omni, "IS_MACOS", True), \
             mock.patch.object(omni, "IS_LINUX", False), \
             mock.patch.object(omni, "IS_WINDOWS", False):
            d = omni.default_display(MAC_HELP_DISPLAY, MAC_HELP_DEVICE, has_gui=True)
        self.assertTrue(d["available"])
        self.assertIn("cocoa,gl=on", " ".join(d["display_args"]))
        self.assertIn("virtio-gpu-gl", " ".join(d["gpu_args"]))

    def test_no_gui_degrades(self):
        with mock.patch.object(omni, "IS_MACOS", True):
            d = omni.default_display(MAC_HELP_DISPLAY, MAC_HELP_DEVICE, has_gui=False)
        self.assertFalse(d["available"])
        self.assertTrue(d["reason"])

    def test_missing_gl_device_degrades(self):
        with mock.patch.object(omni, "IS_MACOS", True):
            d = omni.default_display(MAC_HELP_DISPLAY, "virtio-gpu-pci\n", has_gui=True)
        self.assertFalse(d["available"])

    def test_linux_uses_gtk_or_sdl(self):
        with mock.patch.object(omni, "IS_MACOS", False), \
             mock.patch.object(omni, "IS_LINUX", True), \
             mock.patch.object(omni, "IS_WINDOWS", False):
            d = omni.default_display("gtk\nsdl\nvnc\nnone\n",
                                     "virtio-gpu-gl\n", has_gui=True)
        self.assertTrue(d["available"])
        self.assertRegex(" ".join(d["display_args"]), r"gtk,gl=on|sdl,gl=on")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 tests/test_default_display.py`
Expected: FAIL — `AttributeError: ... no attribute 'default_display'`.

- [ ] **Step 3: Write minimal implementation**

Add near `default_accel` (engine.py ~1118):

```python
def default_display(qemu_display_help="", qemu_device_help="", has_gui=True):
    """Host GL-display capability, mirroring default_accel(). Pure: all host
    facts are passed in (help text from `qemu-system-* -display help` /
    `-device help`, and whether a GUI/display is present). Returns a descriptor;
    available=False (with a reason) whenever accelerated GL isn't usable, so the
    caller degrades to the headless+VNC path. Compatibility over smoothness."""
    if not has_gui:
        return {"available": False, "display_args": [], "gpu_args": [],
                "reason": "no host GUI/display (headless session)"}
    if "virtio-gpu-gl" not in qemu_device_help:
        return {"available": False, "display_args": [], "gpu_args": [],
                "reason": "this QEMU build lacks virtio-gpu-gl"}
    if IS_MACOS:
        backend = "cocoa"
    elif "gtk" in qemu_display_help:
        backend = "gtk"
    elif "sdl" in qemu_display_help:
        backend = "sdl"
    else:
        return {"available": False, "display_args": [], "gpu_args": [],
                "reason": "no gl-capable display backend (gtk/sdl/cocoa)"}
    if backend not in qemu_display_help:
        return {"available": False, "display_args": [], "gpu_args": [],
                "reason": f"QEMU build lacks the {backend} display"}
    return {"available": True,
            "display_args": ["-display", f"{backend},gl=on"],
            "gpu_args": ["-device", "virtio-gpu-gl"],
            "reason": f"{backend},gl=on + virtio-gpu-gl"}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `python3 tests/test_default_display.py`
Expected: PASS — `Ran 4 tests ... OK`.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/engine.py tests/test_default_display.py
git commit -m "feat(b2): default_display() host GL capability detector

Mirrors default_accel(); pure (host facts passed in). Picks cocoa/gtk/sdl
gl=on + virtio-gpu-gl when usable, else available=False with a reason so the
caller degrades to headless+VNC. Every matrix cell covered."
```

---

## Task 4: Decision fn + wire into the boot path (OFFLINE, TDD) — only if spike GREEN

**Files:**
- Modify: `omnidroid/engine.py` (`gpu_display_args()` + use it in `qemu_command_arm`, replacing the Task-1 spike swap)
- Test: `tests/test_gl_decision.py`

**Interfaces:**
- Produces: `gpu_display_args(want_play, capability) -> (gpu_args, display_args, vnc_ok)` — pure. When `want_play` AND `capability["available"]`: returns the accelerated `gpu_args`/`display_args` and `vnc_ok=False` (the window replaces VNC). Otherwise: today's `["-device","virtio-gpu-pci"]` / `["-display","none"]` and `vnc_ok=True`. NEVER raises. `qemu_command_arm` calls it (real capability from `default_display(...)`; `want_play` from the play request) and splices the result — superseding the Task-1 env swap while KEEPING `OMNI_GL_WINDOW` as an alias for `want_play` so the spike command still works.

- [ ] **Step 1: Write the failing test**

Create `tests/test_gl_decision.py`:

```python
#!/usr/bin/env python3
"""gpu_display_args: accelerated when playable+available, else headless; safe.

    python3 tests/test_gl_decision.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402

AVAIL = {"available": True, "display_args": ["-display", "cocoa,gl=on"],
         "gpu_args": ["-device", "virtio-gpu-gl"], "reason": "ok"}
NOPE = {"available": False, "display_args": [], "gpu_args": [], "reason": "no gui"}


class Decision(unittest.TestCase):
    def test_play_and_available_is_accelerated(self):
        gpu, disp, vnc_ok = omni.gpu_display_args(True, AVAIL)
        self.assertIn("virtio-gpu-gl", " ".join(gpu))
        self.assertIn("cocoa,gl=on", " ".join(disp))
        self.assertFalse(vnc_ok)

    def test_play_but_unavailable_degrades(self):
        gpu, disp, vnc_ok = omni.gpu_display_args(True, NOPE)
        self.assertIn("virtio-gpu-pci", " ".join(gpu))
        self.assertIn("none", " ".join(disp))
        self.assertTrue(vnc_ok)

    def test_no_play_is_headless(self):
        gpu, disp, vnc_ok = omni.gpu_display_args(False, AVAIL)
        self.assertIn("virtio-gpu-pci", " ".join(gpu))
        self.assertTrue(vnc_ok)

    def test_never_raises_on_junk(self):
        # a malformed capability must degrade, not crash
        gpu, disp, vnc_ok = omni.gpu_display_args(True, {})
        self.assertTrue(vnc_ok)
        self.assertIn("virtio-gpu-pci", " ".join(gpu))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 tests/test_gl_decision.py`
Expected: FAIL — `AttributeError: ... no attribute 'gpu_display_args'`.

- [ ] **Step 3: Write minimal implementation**

Add near `default_display`:

```python
def gpu_display_args(want_play, capability):
    """Choose accelerated vs headless GPU/display args. Pure, never raises.

    Accelerated only when a play window is requested AND the host capability
    says it's available; otherwise today's headless virtio-gpu-pci + -display
    none (and VNC stays on). Any malformed capability degrades safely."""
    if want_play and isinstance(capability, dict) and capability.get("available"):
        return (list(capability.get("gpu_args") or []),
                list(capability.get("display_args") or []),
                False)
    return (["-device", "virtio-gpu-pci"], ["-display", "none"], True)
```

Then in `qemu_command_arm`, replace the Task-1 spike splice with a call to this. Compute above the args list:

```python
    want_play = _gl_window_requested() and not dev
    cap = default_display(*_qemu_help_texts(), has_gui=_host_has_gui()) \
        if want_play else {"available": False}
    gpu_args, display_args, vnc_ok = gpu_display_args(want_play, cap)
```

and splice `*gpu_args, *display_args,` where the fixed `virtio-gpu-pci`/`-display none` pair was, and make the `-vnc` line conditional on `vnc_ok` (keep VNC when headless; drop it when the accelerated window is up). Add two tiny host-fact helpers next to `default_display` (kept trivial; the live runbook exercises them for real):

```python
def _qemu_help_texts():
    """(display_help, device_help) from the resolved arm QEMU binary; ('','')
    on any failure so default_display just reports unavailable."""
    import subprocess
    try:
        b = qemu_bin("qemu-system-aarch64")
        d = subprocess.run([b, "-display", "help"], capture_output=True,
                           text=True, timeout=10).stdout
        v = subprocess.run([b, "-device", "help"], capture_output=True,
                           text=True, timeout=10).stdout
        return d, v
    except Exception:
        return "", ""


def _host_has_gui():
    """True if a host GUI/display is plausibly present (macOS always; Linux via
    $DISPLAY/$WAYLAND_DISPLAY; Windows always). Conservative — false negatives
    just degrade to headless, which is safe."""
    if IS_MACOS or IS_WINDOWS:
        return True
    return bool(os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY"))
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `python3 tests/test_gl_decision.py && python3 tests/test_gl_spike.py`
Expected: both PASS. The Task-1 `test_gl_spike.py` still passes because `OMNI_GL_WINDOW` still drives `want_play` and the accelerated args still appear on macOS (given a real/faked capability). If the spike test now needs the capability faked, mock `default_display` to return `AVAIL` in that test — update it in this task and note the change.

- [ ] **Step 5: Full suite**

Run: `python3 -m pytest tests/ -q`
Expected: all green (183 + Task 3/4 new), no regressions.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/engine.py tests/test_gl_decision.py tests/test_gl_spike.py
git commit -m "feat(b2): capability-gated playable-GL args + graceful degrade

gpu_display_args picks accelerated GPU/window when play is requested AND the
host supports it, else today's headless virtio-gpu-pci + -display none + VNC.
Never raises. Wires default_display into qemu_command_arm, superseding the
raw spike swap; OMNI_GL_WINDOW still drives the play request."
```

---

## Task 5: Live confirm of the real mode (MANUAL) — only if spike GREEN

Deliverable: `docs/superpowers/runbooks/B2-playable-confirm.md`.

- [ ] **Step 1** — Write the runbook: (a) `OMNI_GL_WINDOW=1 omni start ACCT --mode playable` opens the accelerated window and Roblox renders accelerated joined to place 8737899170 (bootstrap APK); (b) a NORMAL `omni start ACCT --mode farming` still boots headless (no window); (c) forcing unavailability (e.g. `omni start` on a headless SSH session, or a QEMU without virtio-gpu-gl) DEGRADES to headless+VNC with the honest reason line and does NOT crash. Record all three.
- [ ] **Step 2** — Run it on the Mac; record results.
- [ ] **Step 3** — Commit the runbook: `git add docs/superpowers/runbooks/B2-playable-confirm.md && git commit -m "docs(b2): playable-GL live confirm (accel + farming-headless + degrade)"`.

---

## Task 3-ALT: Document + open B3 (only if spike NOT green) — then STOP

- [ ] **Step 1** — In `B2-spike.md`, ensure the RESULT records exactly what was seen (software/black, any QEMU/guest errors, whether login+join worked).
- [ ] **Step 2** — Write `docs/superpowers/specs/2026-07-2X-b3-guest-gpu-driver-design.md` (stub): the problem (guest lacks a GPU driver for virtio-gpu-gl), the evidence (the spike result), and the candidate directions to brainstorm later (guest Mesa/virgl driver; or gfxstream/ranchu path; per-arch feasibility; keep compatibility-first + degradation). Do NOT design it here — that's a fresh brainstorm.
- [ ] **Step 3** — Commit: `git add docs/superpowers/specs/*b3*.md && git commit -m "docs(b3): open guest GPU-driver spec (B2 spike showed the guest needs it)"`. B2 ends here; the Task-1 spike apparatus stays (harmless, off by default) as the reproduction switch for B3.

---

## Self-review notes (for the executor)

- **Task order is deliberate:** the tiny spike apparatus (T1) precedes the runbook (T2) because a non-technical user cannot run a raw QEMU command — T1 makes the experiment a one-liner. T2 is the true GATE; T3–5 vs T3-ALT depends entirely on its recorded verdict.
- **Compatibility is enforced by structure:** `default_display` returns `available=False` for every non-accelerable case, and `gpu_display_args` degrades on `available=False` OR any malformed input and NEVER raises — so no host can crash/regress. Default (no play request) is byte-for-byte today's headless path.
- **Tests are git-tracked** — commit tests with source.
- **x86 wiring is intentionally deferred:** the spike + arm wiring prove the model on the primary host (the Mac); the x86 GPU block (~1315-1327) gets the same `gpu_display_args` treatment as a fast-follow only after the arm path is confirmed green, to avoid doubling live-test surface before feasibility is known. Noted, not a gap.
- **Never conflate with `--window`** (VNC viewer). playable-GL is the QEMU-native accelerated window; they can even coexist (viewer disabled when the native window is up, via `vnc_ok`).
