# Gaming GPU Window Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the gaming profile present the guest in QEMU's own window — restyled into ours, with a title bar we own — on Windows, Linux and macOS, and delete the reparenting viewer that can destroy a running guest's display.

**Architecture:** QEMU's window stays top-level for its whole life and is restyled in place (caption stripped, icon set, geometry restored). A thin title bar of our own — `windowbar.py` — is made an *owned* window of QEMU's window, which puts it permanently above it, minimises it with it, and guarantees that killing the bar cannot touch the guest. That ownership direction is the inverse of today's parent/child relationship and is what removes the `display_lost` failure mode. Policy work is pure and unit-tested; every host call goes through a seam the tests fake.

**Tech Stack:** Python 3.13+, stdlib only (`tkinter`, `ctypes`, `unittest`), QEMU 11.0.50, pytest as the runner.

**Spec:** `docs/superpowers/specs/2026-08-15-gaming-gpu-window-design.md`

## Global Constraints

- **No third-party dependencies.** `pyproject.toml` declares `dependencies = []` and the only optional extra is selenium for the login path. The strip is `tkinter` + `ctypes`. Do not add a package.
- **Nothing in this subsystem may raise into a boot path.** Every capability failure degrades to the headless software pair (`-device virtio-gpu-pci`, `-display none`) and prints a reason. A detection bug may cost the GPU; it may never cost a boot.
- **`gl=es` on macOS, `gl=on` on Windows and Linux.** `_GL_OPTION = {"macos": "gl=es", "linux": "gl=on", "windows": "gl=on"}`. macOS deprecated OpenGL for Metal, so its QEMU goes through ANGLE; `gl=on`/`gl=core` refuse or render upside down.
- **Window flags are backend-specific.** `gtk`: `show-menubar=off,window-close=off,zoom-to-fit=on`. `sdl`: `window-close=off` only. `cocoa`: `zoom-to-fit=on` only. QEMU rejects an unknown suboption outright.
- **Window identity is `omni-<account name>`,** set by `-name` (`qemu_proc.py:1431` for arm, `:1619` for x86) and recorded in `run.json` as `identity` (`qemu_proc.py:2015`).
- **Windowed GL boots have no VNC server.** `blocks_vnc()` already encodes it. Do not try to keep both; QEMU refuses the pair.
- **The base panel ceiling is 1280x800.** Do not add code that requests more; the guest ignores a mode its panel does not carry and stalls for minutes first.
- **Tests are `unittest.TestCase` classes run under pytest,** with `sys.path.insert(0, ...)` at the top, matching every file in `tests/`.
- **Linux is DEFERRED, not implemented.** A Linux host arrives 2026-08-16; until then Linux keeps today's behaviour exactly (`egl-headless` + VNC, QEMU's own frame). Do not write X11 chrome, do not switch the Linux render policy, and do not leave code no one has run. The single lever is `_WINDOW_PRESENT_PLATFORMS` in Task 1 — adding `"linux"` to it, then implementing the X11 chrome, is tomorrow's work and has its own task list.
- **macOS** is gated on sub-project B and is limited here to pure policy plus a readiness reporter.

---

## File Structure

| file | responsibility |
|---|---|
| `omnidroid/qemu_proc.py` (modify) | Policy: which display pair a boot gets, and the backend-aware window flags. Already owns `resolve_gpu_display`, `default_display`, `gpu_policy`, `blocks_vnc`. |
| `omnidroid/hostwin.py` (modify) | The window as an object: find, hide, keep hidden, show — and now restyle (caption, icon, geometry). Keeps its existing backend split (win32 / xdotool / wmctrl / xlib / macos). |
| `omnidroid/windowbar.py` (create) | The strip: our title bar, owned by QEMU's window, carrying title, icon, minimise and the X with its Hide/Stop prompt. Replaces `embedview.py`'s slot. |
| `omnidroid/embedview.py` (delete) | Reparenting viewer. Gone entirely. |
| `omnidroid/engine.py` (modify) | `cmd_view` rewiring, `_windowbar` hidden subcommand, removal of `display_lost` and `_spawn_embedded_viewer`. |
| `omni-executor/main.py` (modify) | `engine_view` plus a hide action, so a hidden window can be shown again. |
| `tests/test_gaming_window_policy.py` (create) | Task 1–2: which pair each profile/platform gets, and the flags on it. |
| `tests/test_window_chrome.py` (create) | Task 3: chrome operations against a fake user32. |
| `tests/test_windowbar.py` (create) | Task 4: ownership direction, tracking geometry, prompt outcomes. |
| `tests/test_hidden_window_viewer.py` (rewrite) | Task 5: the viewer path with no embedding in it. |
| `tests/test_hostwin_backends.py` (modify) | Task 5: drop `window_is_embedded`. |
| `tests/test_macos_gpu_readiness.py` (create) | Task 8. |

---

### Task 1: Gaming's `auto` takes a window (Windows and macOS; Linux deferred)

Today `GPU_AUTO` tries the windowless GL pair first (`_headless_gl_pair`, `qemu_proc.py:1169`). On Linux that succeeds, so gaming gets `egl-headless` + VNC — GPU rendering followed by a readback, an RFB encode and a Python RFB decode per frame. The performance profile must go straight to the window.

**Files:**
- Modify: `omnidroid/qemu_proc.py:1102-1168` (`resolve_gpu_display`)
- Test: `tests/test_gaming_window_policy.py`

**Interfaces:**
- Consumes: `gpu_policy(cfg, mode)`, `_headless_gl_pair(tool, cfg, mode)`, `default_display(...)`, `gpu_display_args(want_window, capability)` — all existing.
- Produces: `_presents_a_window(policy, mode) -> bool`, used by Task 2's tests and by nothing else.

- [ ] **Step 1: Write the failing test**

Create `tests/test_gaming_window_policy.py`:

```python
#!/usr/bin/env python3
"""Gaming presents in a window; farming renders without one.

    python3 -m pytest tests/test_gaming_window_policy.py -q

The performance profile must NOT take the windowless GL pair even where one is
available. On Linux `egl-headless` presents, so `auto` used to resolve there
and gaming paid a readback + RFB encode + Python RFB decode per frame. The
window costs the VNC server and buys zero copies and native input.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import qemu_proc

DISPLAY_HELP = "none\ngtk\nsdl\negl-headless\ncurses\ndbus\n"
DEVICE_HELP = 'name "virtio-gpu-gl-pci", bus PCI, alias "virtio-gpu-gl"\n'

GAMING = {"profile": "performance", "gpu": "auto", "panel": (1280, 800)}
FARMING = {"profile": "density", "gpu": "auto", "panel": (640, 480)}


class ProfileDecidesTheDisplay(unittest.TestCase):

    def resolve(self, mode, platform_key):
        with mock.patch.object(qemu_proc, "_platform_key",
                               return_value=platform_key), \
             mock.patch.object(qemu_proc, "_qemu_help_texts",
                               return_value=(DISPLAY_HELP, DEVICE_HELP)), \
             mock.patch.object(qemu_proc, "_host_has_gui", return_value=True), \
             mock.patch.dict(os.environ, {}, clear=True):
            return qemu_proc.resolve_gpu_display(mode, False, "qemu-system-x86_64")

    def test_gaming_on_linux_keeps_egl_headless_until_a_host_verifies_it(self):
        """DEFERRED, not a design decision reversed.

        Linux is the one platform whose egl-headless actually presents, so
        gaming works there today via VNC -- at the cost of a readback, an RFB
        encode and a Python RFB decode per frame. The window is better and the
        spec says so, but switching it blind would trade a working copy path
        for an unrun one AND drop the VNC server (QEMU refuses -vnc beside a GL
        window), leaving a Linux user with a raw QEMU frame and no viewer.

        Flip this by adding "linux" to _WINDOW_PRESENT_PLATFORMS once a Linux
        host has run it.
        """
        _gpu, display = self.resolve(GAMING, "linux")
        self.assertEqual(display, ["-display", "egl-headless"])

    def test_gaming_on_windows_takes_a_gl_window(self):
        _gpu, display = self.resolve(GAMING, "windows")
        self.assertIn("gl=on", display[1])

    def test_gaming_on_macos_takes_cocoa_with_gl_es(self):
        _gpu, display = self.resolve(GAMING, "macos")
        self.assertTrue(display[1].startswith("cocoa,"), display[1])
        self.assertIn("gl=es", display[1])
        self.assertNotIn("gl=on", display[1])

    def test_farming_still_prefers_the_windowless_pair(self):
        _gpu, display = self.resolve(FARMING, "linux")
        self.assertEqual(display, ["-display", "egl-headless"])

    def test_a_gaming_pair_blocks_vnc_and_a_farming_pair_does_not(self):
        _gpu, gaming = self.resolve(GAMING, "linux")
        _gpu, farming = self.resolve(FARMING, "linux")
        self.assertTrue(qemu_proc.blocks_vnc(gaming))
        self.assertFalse(qemu_proc.blocks_vnc(farming))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run it and watch it fail**

Run: `python -m pytest tests/test_gaming_window_policy.py -q`
Expected: `test_gaming_on_linux_takes_a_gl_window_not_egl_headless` FAILS — the display is `egl-headless`, because `auto` consults `_headless_gl_pair` first.

- [ ] **Step 3: Add the predicate**

In `omnidroid/qemu_proc.py`, immediately after `gpu_policy()` (which ends at line 1100):

```python
def _presents_a_window(policy, mode):
    """Whether this boot should PRESENT the guest in a host window.

    The two profiles want opposite things from the same trade and this is the
    one line that says so. `performance` is one instance somebody is playing:
    a window is zero copies and native input, and it costs only the VNC server
    nobody was watching. `density` is many instances nobody is watching: it
    wants the GPU without a window, which is what the windowless pair gives.

    MEASURED, and the reason this predicate exists at all: on Linux
    `egl-headless` presents, so `auto` resolved there for BOTH profiles and
    gaming paid a GPU readback, an RFB encode and a Python RFB decode on every
    frame while rendering on the GPU the whole time.
    """
    if policy == GPU_WINDOW:
        return True
    if policy != GPU_AUTO:
        return False
    return ((mode or {}).get("profile") == "performance"
            and _platform_key() in _WINDOW_PRESENT_PLATFORMS)
```

and directly above it, the platform gate:

```python
# Platforms where the performance profile PRESENTS in a host window.
#
# Linux is absent ON PURPOSE and only until a Linux host exists to verify it
# (2026-08-16). It is the one platform whose egl-headless really presents, so
# gaming works there today; switching it blind would trade a working copy path
# for an unrun one and drop the VNC server with it, because QEMU refuses -vnc
# beside a GL window. macOS is present and needs no gate: its egl-headless
# does NOT present (HEADLESS_GL_PRESENTS), so _headless_gl_pair returns None
# there and the window path is reached anyway.
_WINDOW_PRESENT_PLATFORMS = ("windows", "macos")
```

- [ ] **Step 4: Route `auto` around the windowless pair for the performance profile**

In `resolve_gpu_display` (`qemu_proc.py:1102`), replace:

```python
    if policy in (GPU_AUTO, GPU_HEADLESS):
        pair = _headless_gl_pair(tool, cfg, mode)
```

with:

```python
    if policy == GPU_HEADLESS or (policy == GPU_AUTO
                                  and not _presents_a_window(policy, mode)):
        pair = _headless_gl_pair(tool, cfg, mode)
```

- [ ] **Step 5: Run the tests and see them pass**

Run: `python -m pytest tests/test_gaming_window_policy.py -q`
Expected: 5 passed.

- [ ] **Step 6: Run the whole suite for regressions**

Run: `python -m pytest tests/ -q`
Expected: no NEW failures. `tests/test_gpu_display.py`, `tests/test_headless_gpu.py` and `tests/test_gaming_apply.py` are the ones that touch this. If one asserts that gaming resolves to `egl-headless`, that assertion is now wrong — update it to assert the window, and note in the test docstring that the profile decides.

- [ ] **Step 7: Commit**

```bash
git add omnidroid/qemu_proc.py tests/test_gaming_window_policy.py
git commit -m "Gaming presents in a window on every platform, not just Windows"
```

---

### Task 2: Backend-aware window flags

QEMU rejects an unknown suboption outright, and the three backends do not take the same ones. `cocoa` has no `window-close` and no `show-menubar`; `sdl` has no `show-menubar` and no `zoom-to-fit`.

**Files:**
- Modify: `omnidroid/qemu_proc.py:374-411` (`default_display`)
- Test: `tests/test_gaming_window_policy.py` (append)

**Interfaces:**
- Produces: `window_flags(backend) -> str` — the comma-joined suboptions for that backend, without the `gl=` part and without a leading comma. Task 8 reads it for the macOS readiness report.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_gaming_window_policy.py`:

```python
class WindowFlagsAreBackendSpecific(unittest.TestCase):
    """QEMU rejects an unknown suboption outright, so this cannot be one list.

    -display gtk  takes show-menubar, window-close, zoom-to-fit
    -display sdl  takes window-close only
    -display cocoa takes zoom-to-fit only -- no window-close, which is one
    more reason macOS gets its close behaviour from the QEMU patch.
    """

    def test_gtk_gets_all_three(self):
        flags = qemu_proc.window_flags("gtk")
        self.assertIn("show-menubar=off", flags)
        self.assertIn("window-close=off", flags)
        self.assertIn("zoom-to-fit=on", flags)

    def test_sdl_gets_only_window_close(self):
        flags = qemu_proc.window_flags("sdl")
        self.assertIn("window-close=off", flags)
        self.assertNotIn("show-menubar", flags)
        self.assertNotIn("zoom-to-fit", flags)

    def test_cocoa_never_gets_window_close(self):
        flags = qemu_proc.window_flags("cocoa")
        self.assertIn("zoom-to-fit=on", flags)
        self.assertNotIn("window-close", flags)
        self.assertNotIn("show-menubar", flags)

    def test_an_unknown_backend_gets_nothing_rather_than_a_refused_boot(self):
        self.assertEqual(qemu_proc.window_flags("wayland-thing"), "")


class TheFlagsReachTheCommand(unittest.TestCase):

    def resolve(self, mode, platform_key):
        with mock.patch.object(qemu_proc, "_platform_key",
                               return_value=platform_key), \
             mock.patch.object(qemu_proc, "_qemu_help_texts",
                               return_value=(DISPLAY_HELP, DEVICE_HELP)), \
             mock.patch.object(qemu_proc, "_host_has_gui", return_value=True), \
             mock.patch.dict(os.environ, {}, clear=True):
            return qemu_proc.resolve_gpu_display(mode, False, "qemu-system-x86_64")

    def test_a_gaming_gtk_boot_carries_our_flags(self):
        _gpu, display = self.resolve(GAMING, "windows")
        self.assertIn("show-menubar=off", display[1])
        self.assertIn("window-close=off", display[1])
        self.assertIn("zoom-to-fit=on", display[1])

    def test_it_is_still_recognised_as_a_gl_boot(self):
        _gpu, display = self.resolve(GAMING, "windows")
        self.assertTrue(qemu_proc.uses_gl_context(display))
        self.assertTrue(qemu_proc.blocks_vnc(display))
        self.assertTrue(qemu_proc.command_opens_a_window(
            ["-display", display[1]]))
```

- [ ] **Step 2: Run it and watch it fail**

Run: `python -m pytest tests/test_gaming_window_policy.py -q`
Expected: FAIL — `AttributeError: module 'omnidroid.qemu_proc' has no attribute 'window_flags'`.

- [ ] **Step 3: Add `window_flags`**

In `omnidroid/qemu_proc.py`, directly under `_GL_OPTION` (line 316):

```python
# Suboptions we set on a PRESENTED window, per backend. Not one list, because
# QEMU refuses an unknown suboption outright rather than ignoring it -- so a
# `show-menubar=off` sent to `cocoa` does not degrade, it fails the boot.
#
#   show-menubar=off   QEMU's own View/Machine menus are not our chrome
#   window-close=off   the X must not quit QEMU: the strip asks first, and a
#                      window that closes the VM by accident costs a boot
#   zoom-to-fit=on     the guest panel is fixed at the base's native mode, so
#                      the window scales rather than letterboxing
_WINDOW_FLAGS = {
    "gtk":   ("show-menubar=off", "window-close=off", "zoom-to-fit=on"),
    "sdl":   ("window-close=off",),
    "cocoa": ("zoom-to-fit=on",),
}


def window_flags(backend):
    """Comma-joined suboptions for a presented window on `backend`.

    An unrecognised backend gets "" rather than a guess: an unknown suboption
    is a refused boot, and no flag at all is merely a plainer window.
    """
    return ",".join(_WINDOW_FLAGS.get(backend, ()))
```

- [ ] **Step 4: Put them on the display argument**

In `default_display` (`qemu_proc.py:374`), replace the GL branch's `display_args` line:

```python
                "display_args": ["-display", f"{backend},{gl}"],
```

with:

```python
                "display_args": ["-display",
                                 ",".join(filter(None, (backend, gl,
                                                        window_flags(backend))))],
```

and the software-window branch's:

```python
            "display_args": ["-display", backend],
```

with:

```python
            "display_args": ["-display",
                             ",".join(filter(None,
                                             (backend, window_flags(backend))))],
```

- [ ] **Step 5: Run the tests**

Run: `python -m pytest tests/test_gaming_window_policy.py -q`
Expected: 11 passed.

- [ ] **Step 6: Verify against the real QEMU that the argv is accepted**

Run:
```bash
"C:/Program Files/qemu/qemu-system-x86_64.exe" -display gtk,gl=on,show-menubar=off,window-close=off,zoom-to-fit=on -device virtio-gpu-gl-pci -M q35 -m 256 -S -monitor none -serial none
```
Expected: a window opens (the guest is stopped with `-S`; nothing boots). No `Parameter 'X' is unknown` error. Close it. **If any suboption is rejected, remove that one from `_WINDOW_FLAGS` and record why in the comment** — the flag list is a claim about this QEMU build and must be true of it.

- [ ] **Step 7: Run the full suite and commit**

Run: `python -m pytest tests/ -q`

```bash
git add omnidroid/qemu_proc.py tests/test_gaming_window_policy.py
git commit -m "Window flags per display backend: gtk, sdl and cocoa differ"
```

---

### Task 3: `hostwin` restyles the window

`hostwin.py` already finds, hides, keeps hidden and shows. It gains the chrome: strip the caption (keeping the sizing border), set our icon, and restore geometry.

**Files:**
- Modify: `omnidroid/hostwin.py` (add after `show_qemu_window`, line 755)
- Test: `tests/test_window_chrome.py`

**Interfaces:**
- Consumes: `find_window(identity, timeout, pid)` returning a handle (an HWND int on Windows), `backend()`, `BACKEND_WIN32`.
- Produces:
  - `apply_chrome(identity, pid=None, icon=None, geometry=None) -> dict` with keys `applied` (bool), `reason` (str, "" on success), `hwnd` (handle or None).
  - `window_geometry(identity, pid=None) -> tuple|None` as `(x, y, width, height)`.
  - Constants `GWL_STYLE = -16`, `WS_CAPTION = 0x00C00000`, `WS_THICKFRAME = 0x00040000`, `WS_SYSMENU = 0x00080000`.

- [ ] **Step 1: Write the failing test**

Create `tests/test_window_chrome.py`:

```python
#!/usr/bin/env python3
"""QEMU's window is restyled in place -- caption off, sizing border kept.

    python3 -m pytest tests/test_window_chrome.py -q

The caption goes because the strip IS the title bar; leaving QEMU's would put
two title bars on screen, which is the trap embedview.py documented before it
was deleted. WS_THICKFRAME stays so the composite can still be resized by
dragging the guest window's edges.

Nothing here may raise: a host where the chrome cannot be applied gets a plain
window and a printed reason, never a failed boot.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import hostwin


class FakeUser32:
    """Just enough of user32 to record what was asked of it."""

    def __init__(self, style=0xCF0000):
        self.style = style
        self.icons = []
        self.positions = []
        self.rect = (100, 100, 1380, 900)

    def GetWindowLongPtrW(self, hwnd, index):
        return self.style

    def SetWindowLongPtrW(self, hwnd, index, value):
        self.style = value
        return 1

    def SendMessageW(self, hwnd, msg, wparam, lparam):
        self.icons.append((msg, wparam, lparam))
        return 0

    def SetWindowPos(self, hwnd, after, x, y, cx, cy, flags):
        self.positions.append((x, y, cx, cy, flags))
        return 1

    def GetWindowRect(self, hwnd, out):
        out.left, out.top, out.right, out.bottom = self.rect
        return 1


class ChromeOnWindows(unittest.TestCase):

    def setUp(self):
        self.u = FakeUser32()
        self.patches = [
            mock.patch.object(hostwin, "backend",
                              return_value=hostwin.BACKEND_WIN32),
            mock.patch.object(hostwin, "find_window", return_value=4242),
            mock.patch.object(hostwin, "_user32", return_value=self.u),
        ]
        for p in self.patches:
            p.start()
        self.addCleanup(lambda: [p.stop() for p in self.patches])

    def test_the_caption_is_removed(self):
        result = hostwin.apply_chrome("omni-farm3")
        self.assertTrue(result["applied"], result["reason"])
        self.assertFalse(self.u.style & hostwin.WS_CAPTION)

    def test_the_sizing_border_is_kept(self):
        hostwin.apply_chrome("omni-farm3")
        self.assertTrue(self.u.style & hostwin.WS_THICKFRAME)

    def test_geometry_is_restored_when_given(self):
        hostwin.apply_chrome("omni-farm3", geometry=(10, 20, 800, 600))
        self.assertIn((10, 20, 800, 600),
                      [p[:4] for p in self.u.positions])

    def test_a_missing_window_is_a_reason_not_an_exception(self):
        with mock.patch.object(hostwin, "find_window", return_value=None):
            result = hostwin.apply_chrome("omni-gone")
        self.assertFalse(result["applied"])
        self.assertIn("no window", result["reason"].lower())

    def test_a_failing_win32_call_is_a_reason_not_an_exception(self):
        with mock.patch.object(hostwin, "_user32",
                               side_effect=OSError("boom")):
            result = hostwin.apply_chrome("omni-farm3")
        self.assertFalse(result["applied"])
        self.assertNotEqual(result["reason"], "")

    def test_geometry_is_read_back(self):
        self.assertEqual(hostwin.window_geometry("omni-farm3"),
                         (100, 100, 1280, 800))


class ChromeElsewhere(unittest.TestCase):

    def test_a_non_win32_backend_declines_with_a_reason(self):
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_MACOS):
            result = hostwin.apply_chrome("omni-farm3")
        self.assertFalse(result["applied"])
        self.assertIn("macos", result["reason"].lower())


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run it and watch it fail**

Run: `python -m pytest tests/test_window_chrome.py -q`
Expected: FAIL — `hostwin` has no `apply_chrome`, no `_user32`, no `WS_CAPTION`.

- [ ] **Step 3: Implement the chrome**

Append to `omnidroid/hostwin.py`, after `show_qemu_window` (line 755):

```python
# ---------- chrome: QEMU's window, restyled in place ----------
#
# The caption goes and the sizing border stays. The strip (windowbar.py) is
# the title bar; leaving QEMU's own would put TWO on screen, which is exactly
# what the deleted embedview.py warned about ("a second title bar it is
# impossible to click"). WS_THICKFRAME stays so the composite is still
# resizable by dragging the guest window's edges, with the strip following.
GWL_STYLE = -16
WS_CAPTION = 0x00C00000
WS_THICKFRAME = 0x00040000
WS_SYSMENU = 0x00080000
WS_MINIMIZEBOX = 0x00020000
WS_MAXIMIZEBOX = 0x00010000

WM_SETICON = 0x0080
ICON_SMALL, ICON_BIG = 0, 1

SWP_NOZORDER = 0x0004
SWP_NOACTIVATE = 0x0010
SWP_FRAMECHANGED = 0x0020


def _user32():
    """user32, imported lazily so this module stays importable anywhere."""
    import ctypes
    return ctypes.windll.user32


def _get_style(u, hwnd):
    if hasattr(u, "GetWindowLongPtrW"):
        return u.GetWindowLongPtrW(hwnd, GWL_STYLE)
    return u.GetWindowLongW(hwnd, GWL_STYLE)


def _set_style(u, hwnd, style):
    if hasattr(u, "SetWindowLongPtrW"):
        return u.SetWindowLongPtrW(hwnd, GWL_STYLE, style)
    return u.SetWindowLongW(hwnd, GWL_STYLE, style)


def _chrome_result(applied, reason="", hwnd=None):
    return {"applied": applied, "reason": reason, "hwnd": hwnd}


def apply_chrome(identity, pid=None, icon=None, geometry=None,
                 timeout=DEFAULT_TIMEOUT):
    """Restyle QEMU's window into ours. Never raises.

    Returns {"applied", "reason", "hwnd"}. A host that cannot do it gets a
    plain window and a reason -- this is chrome, and chrome is never worth a
    failed boot.
    """
    name = backend()
    if name != BACKEND_WIN32:
        return _chrome_result(
            False,
            f"restyling another process's window is implemented for Windows "
            f"only; this host's backend is {name}")
    hwnd = find_window(identity, timeout=timeout, pid=pid)
    if hwnd is None:
        return _chrome_result(False, f"no window found for '{identity}'")
    try:
        u = _user32()
        style = _get_style(u, hwnd)
        style &= ~(WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX)
        style |= WS_THICKFRAME
        _set_style(u, hwnd, style)
        if icon:
            _apply_icon(u, hwnd, icon)
        if geometry:
            x, y, width, height = geometry
            u.SetWindowPos(hwnd, 0, int(x), int(y), int(width), int(height),
                           SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED)
        else:
            # The frame changed even when the geometry did not, and Windows
            # does not recompute the non-client area until it is told.
            u.SetWindowPos(hwnd, 0, 0, 0, 0, 0,
                           SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED
                           | 0x0001 | 0x0002)      # SWP_NOSIZE | SWP_NOMOVE
        return _chrome_result(True, "", hwnd)
    except Exception as e:      # noqa: BLE001 - chrome never fails a boot
        return _chrome_result(False, f"could not restyle the window: {e}")


def _apply_icon(u, hwnd, icon):
    """Put our icon on the window. Best-effort, like everything here."""
    import ctypes
    hicon = ctypes.windll.user32.LoadImageW(
        None, str(icon), 1, 0, 0, 0x00000010 | 0x00008000)  # IMAGE_ICON
    if hicon:
        u.SendMessageW(hwnd, WM_SETICON, ICON_SMALL, hicon)
        u.SendMessageW(hwnd, WM_SETICON, ICON_BIG, hicon)


def window_geometry(identity, pid=None, timeout=2.0):
    """(x, y, width, height) of QEMU's window, or None."""
    if backend() != BACKEND_WIN32:
        return None
    hwnd = find_window(identity, timeout=timeout, pid=pid)
    if hwnd is None:
        return None
    try:
        import ctypes

        class RECT(ctypes.Structure):
            _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                        ("right", ctypes.c_long), ("bottom", ctypes.c_long)]

        rect = RECT()
        if not _user32().GetWindowRect(hwnd, ctypes.byref(rect)):
            return None
        return (rect.left, rect.top,
                rect.right - rect.left, rect.bottom - rect.top)
    except Exception:      # noqa: BLE001
        return None
```

- [ ] **Step 4: Run the tests**

Run: `python -m pytest tests/test_window_chrome.py -q`
Expected: 7 passed.

Note: `test_geometry_is_read_back` passes a fake whose `GetWindowRect` writes into the struct it is given; if the fake's signature mismatches, adjust the fake — not the implementation.

- [ ] **Step 5: Run the full suite and commit**

Run: `python -m pytest tests/ -q`

```bash
git add omnidroid/hostwin.py tests/test_window_chrome.py
git commit -m "hostwin restyles QEMU's window: caption off, sizing border kept"
```

---

### Task 4: `windowbar.py` — the strip

A window we own, made an *owned* window of QEMU's window. Owned windows float above their owner, minimise with it, and — the property this whole design rests on — **destroying an owned window does nothing to its owner**.

**Files:**
- Create: `omnidroid/windowbar.py`
- Test: `tests/test_windowbar.py`

**Interfaces:**
- Consumes: `hostwin.find_window`, `hostwin.apply_chrome`, `hostwin.window_geometry`, `hostwin.hide_qemu_window`, `hostwin.BACKEND_WIN32`.
- Produces:
  - `GWLP_HWNDPARENT = -8`, `BAR_HEIGHT = 34`
  - `bar_geometry(owner_rect, bar_height=BAR_HEIGHT) -> (x, y, width, height)`
  - `class WindowBar` with `own(bar_hwnd, owner_hwnd) -> bool`, `follow(owner_rect) -> None`, `on_close() -> str` returning `"hide"`, `"stop"` or `"cancel"`.
  - `run_window_bar(identity, title=None, pid=None, on_stop=None) -> int`

- [ ] **Step 1: Write the failing test**

Create `tests/test_windowbar.py`:

```python
#!/usr/bin/env python3
"""The strip is OWNED BY QEMU's window, and that direction is the design.

    python3 -m pytest tests/test_windowbar.py -q

Today (embedview.py, deleted by this work) QEMU's window is a CHILD of our
viewer, and Windows destroys a child with its parent: a force-killed viewer
took the guest's display with it permanently -- instance alive, answering adb,
totalFrames = 0. An OWNED window has the properties we want and not that one:

  * it always floats above its owner (z-order solved without polling)
  * it minimises and restores with its owner
  * destroying it does NOTHING to the owner

So the strip is owned BY the guest window, never the other way round. These
tests pin the direction, because getting it backwards reintroduces exactly the
failure this design exists to remove.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import windowbar


class FakeUser32:
    def __init__(self):
        self.owner_calls = []
        self.positions = []

    def SetWindowLongPtrW(self, hwnd, index, value):
        self.owner_calls.append((hwnd, index, value))
        return 1

    def SetWindowPos(self, hwnd, after, x, y, cx, cy, flags):
        self.positions.append((hwnd, x, y, cx, cy))
        return 1


class OwnershipDirection(unittest.TestCase):

    def test_the_bar_is_owned_by_the_guest_window(self):
        u = FakeUser32()
        bar = windowbar.WindowBar("omni-farm3")
        with mock.patch.object(windowbar, "_user32", return_value=u):
            self.assertTrue(bar.own(bar_hwnd=11, owner_hwnd=99))
        self.assertEqual(u.owner_calls,
                         [(11, windowbar.GWLP_HWNDPARENT, 99)])

    def test_it_is_never_the_other_way_round(self):
        u = FakeUser32()
        bar = windowbar.WindowBar("omni-farm3")
        with mock.patch.object(windowbar, "_user32", return_value=u):
            bar.own(bar_hwnd=11, owner_hwnd=99)
        for hwnd, index, value in u.owner_calls:
            self.assertNotEqual(
                (hwnd, value), (99, 11),
                "the GUEST window must never be owned by the bar -- that is "
                "the relationship that lets a dead viewer blind the guest")


class BarGeometry(unittest.TestCase):

    def test_the_bar_sits_directly_above_the_window_and_matches_its_width(self):
        x, y, w, h = windowbar.bar_geometry((100, 200, 1280, 800),
                                            bar_height=34)
        self.assertEqual((x, w, h), (100, 1280, 34))
        self.assertEqual(y, 200 - 34)

    def test_a_window_at_the_top_of_the_screen_does_not_get_a_negative_y(self):
        _x, y, _w, _h = windowbar.bar_geometry((0, 10, 640, 480),
                                               bar_height=34)
        self.assertGreaterEqual(y, 0)


class ClosePrompt(unittest.TestCase):

    def test_hide_hides_the_window_and_leaves_the_instance_running(self):
        bar = windowbar.WindowBar("omni-farm3")
        with mock.patch.object(windowbar, "_ask_close",
                               return_value="hide"), \
             mock.patch("omnidroid.hostwin.hide_qemu_window") as hide:
            self.assertEqual(bar.on_close(), "hide")
        hide.assert_called_once_with("omni-farm3")

    def test_stop_calls_the_stop_hook(self):
        stopped = []
        bar = windowbar.WindowBar("omni-farm3",
                                  on_stop=lambda name: stopped.append(name))
        with mock.patch.object(windowbar, "_ask_close", return_value="stop"):
            self.assertEqual(bar.on_close(), "stop")
        self.assertEqual(stopped, ["omni-farm3"])

    def test_cancel_does_nothing_at_all(self):
        stopped = []
        bar = windowbar.WindowBar("omni-farm3",
                                  on_stop=lambda name: stopped.append(name))
        with mock.patch.object(windowbar, "_ask_close",
                               return_value="cancel"), \
             mock.patch("omnidroid.hostwin.hide_qemu_window") as hide:
            self.assertEqual(bar.on_close(), "cancel")
        hide.assert_not_called()
        self.assertEqual(stopped, [])


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run it and watch it fail**

Run: `python -m pytest tests/test_windowbar.py -q`
Expected: FAIL — `ModuleNotFoundError: No module named 'omnidroid.windowbar'`.

- [ ] **Step 3: Write the module**

Create `omnidroid/windowbar.py`:

```python
"""Our title bar for QEMU's window, owned BY that window.

QEMU's window stays top-level for its whole life and is restyled in place
(hostwin.apply_chrome). This module supplies the caption it gave up: our
title, our icon, minimise, and an X that ASKS -- hide the window, or stop the
instance.

THE OWNERSHIP DIRECTION IS THE WHOLE DESIGN. The viewer this replaces made
QEMU's window a CHILD of a Tk window, and Windows destroys a child with its
parent: a force-killed viewer left the instance alive, answering adb, and
rendering nothing at all (measured, totalFrames = 0). An OWNED window gives us
the two properties we wanted from that arrangement --

    * it always floats above its owner, so z-order needs no polling
    * it minimises and restores with its owner

-- and none of the property that hurt: destroying an owned window does nothing
to its owner. Kill this bar however you like; the guest keeps rendering.

The close button has to be ours because it cannot be anyone else's: one
process cannot intercept another's WM_CLOSE without injecting a DLL. QEMU is
therefore spawned `window-close=off` and its X is inert -- and its caption is
stripped, so there is no second title bar to click.

WINDOWS FIRST. The X11 backend is written but UNVERIFIED (no Linux host in
this setup); macOS gets this from the QEMU patch, because it has no public API
to restyle or own another process's NSWindow.
"""
import sys

from omnidroid import hostwin
from omnidroid.config import IS_WINDOWS

# SetWindowLongPtr index for the OWNER of a window. Not GWLP_HWNDPARENT's
# other meaning: for a top-level window this sets the owner, and for a child
# it would set the parent -- which is why the bar must be a top-level popup
# and never a child of anything.
GWLP_HWNDPARENT = -8

BAR_HEIGHT = 34

SWP_NOZORDER = 0x0004
SWP_NOACTIVATE = 0x0010


def _user32():
    import ctypes
    return ctypes.windll.user32


def bar_geometry(owner_rect, bar_height=BAR_HEIGHT):
    """Where the bar goes for a guest window at `owner_rect`.

    Directly above it, exactly as wide. Clamped at the top of the screen so a
    window dragged to y=0 does not put its own title bar off-screen.
    """
    x, y, width, _height = owner_rect
    return (x, max(0, y - bar_height), width, bar_height)


def _ask_close(parent=None):
    """Hide, stop, or cancel. The seam the tests replace.

    Three buttons rather than a yes/no, because the two real answers are not
    opposites: hiding keeps a booted instance that took a minute to reach the
    world, and stopping throws it away.
    """
    import tkinter as tk
    from tkinter import ttk

    answer = {"value": "cancel"}
    dialog = tk.Toplevel(parent) if parent else tk.Tk()
    dialog.title("Close window")
    dialog.resizable(False, False)
    ttk.Label(dialog,
              text="Hide this window, or stop the instance?\n"
                   "Hiding keeps it running — show it again from the app.",
              justify="left").pack(padx=16, pady=(16, 12))
    row = ttk.Frame(dialog)
    row.pack(padx=16, pady=(0, 16), fill="x")

    def choose(value):
        answer["value"] = value
        dialog.destroy()

    ttk.Button(row, text="Hide window",
               command=lambda: choose("hide")).pack(side="left")
    ttk.Button(row, text="Stop instance",
               command=lambda: choose("stop")).pack(side="left", padx=8)
    ttk.Button(row, text="Cancel",
               command=lambda: choose("cancel")).pack(side="right")
    dialog.grab_set()
    dialog.wait_window()
    return answer["value"]


class WindowBar:
    """The strip. Nothing here raises into a caller."""

    def __init__(self, identity, pid=None, on_stop=None):
        self.identity = identity
        self.pid = pid
        self.on_stop = on_stop
        self.owner_hwnd = None
        self.bar_hwnd = None

    def own(self, bar_hwnd, owner_hwnd):
        """Make the bar an owned window of the guest's window.

        NEVER the reverse -- see the module docstring.
        """
        if not IS_WINDOWS:
            return False
        try:
            _user32().SetWindowLongPtrW(bar_hwnd, GWLP_HWNDPARENT, owner_hwnd)
            self.bar_hwnd, self.owner_hwnd = bar_hwnd, owner_hwnd
            return True
        except Exception:      # noqa: BLE001
            return False

    def follow(self, owner_rect):
        """Move the bar to sit above the guest window at `owner_rect`."""
        if self.bar_hwnd is None:
            return False
        x, y, width, height = bar_geometry(owner_rect)
        try:
            _user32().SetWindowPos(self.bar_hwnd, 0, x, y, width, height,
                                   SWP_NOZORDER | SWP_NOACTIVATE)
            return True
        except Exception:      # noqa: BLE001
            return False

    def on_close(self, parent=None):
        """The X was clicked. Returns "hide", "stop" or "cancel"."""
        answer = _ask_close(parent)
        if answer == "hide":
            hostwin.hide_qemu_window(self.identity)
        elif answer == "stop" and self.on_stop:
            self.on_stop(self.identity)
        return answer


def run_window_bar(identity, title=None, pid=None, on_stop=None):
    """Show the guest's window with our bar above it. Blocks until closed.

    Returns 0 when the bar ran, 2 when there was no window to attach to.
    """
    if not IS_WINDOWS:
        sys.stderr.write(
            "window bar: implemented for Windows only; other hosts use the "
            "VNC viewer or the patched QEMU UI\n")
        return 3
    import tkinter as tk

    owner = hostwin.find_window(identity, timeout=20.0, pid=pid)
    if owner is None:
        sys.stderr.write(f"window bar: no QEMU window for '{identity}'\n")
        return 2

    root = tk.Tk()
    root.title(title or f"omni: {identity}")
    root.overrideredirect(False)
    root.resizable(False, False)
    root.update_idletasks()

    bar = WindowBar(identity, pid=pid, on_stop=on_stop)
    # GetAncestor(GA_ROOT): Tk's winfo_id() is the widget's HWND, which is not
    # always the top-level one. Owning the wrong handle silently does nothing.
    bar_hwnd = _user32().GetAncestor(root.winfo_id(), 2)
    bar.own(bar_hwnd, owner)

    rect = hostwin.window_geometry(identity, pid=pid)
    if rect:
        bar.follow(rect)

    def on_delete():
        if bar.on_close(root) in ("hide", "stop"):
            root.destroy()

    root.protocol("WM_DELETE_WINDOW", on_delete)
    root.mainloop()
    return 0
```

- [ ] **Step 4: Run the tests**

Run: `python -m pytest tests/test_windowbar.py -q`
Expected: 7 passed.

- [ ] **Step 5: Run the full suite and commit**

Run: `python -m pytest tests/ -q`

```bash
git add omnidroid/windowbar.py tests/test_windowbar.py
git commit -m "windowbar: our title bar, owned BY the guest window"
```

---

### Task 5: Rewire `cmd_view`, delete `embedview`

**Files:**
- Modify: `omnidroid/engine.py:6510-6600+` (`cmd_view`), plus `_spawn_embedded_viewer`, `EMBED_VIEWER_SETTLE` and the `_embedview` subcommand registration
- Delete: `omnidroid/embedview.py`
- Modify: `omnidroid/hostwin.py:718` (`window_is_embedded` — remove)
- Rewrite: `tests/test_hidden_window_viewer.py`
- Modify: `tests/test_hostwin_backends.py`

**Interfaces:**
- Consumes: `windowbar.run_window_bar(identity, title, pid, on_stop)`, `hostwin.apply_chrome(...)`, `hostwin.show_qemu_window(identity)`.
- Produces: `_spawn_window_bar(name, title, identity, pid) -> subprocess.Popen`, and a `_windowbar` hidden subcommand mirroring today's `_embedview`.

- [ ] **Step 1: Find every reference before changing anything**

Run:
```bash
grep -rn "embedview\|window_is_embedded\|display_lost\|EMBED_VIEWER_SETTLE\|_spawn_embedded_viewer" omnidroid/ tests/ ../omni-executor/*.py
```
Expected: hits in `omnidroid/engine.py`, `omnidroid/hostwin.py`, `omnidroid/embedview.py`, `tests/test_hidden_window_viewer.py`, `tests/test_hostwin_backends.py`. Write the list down — every one must be gone or rewritten by the end of this task.

- [ ] **Step 2: Write the failing test**

Replace the body of `tests/test_hidden_window_viewer.py` (keep the file; rewrite the docstring and the tests):

```python
#!/usr/bin/env python3
"""The window is hidden at boot, restyled, and shown with OUR bar above it.

    python3 -m pytest tests/test_hidden_window_viewer.py -q

On Windows the window is the price of the GPU: every windowless GL display
there takes its context from ANGLE at ES 2.0 and virglrenderer cannot serve a
scanout from it (SET_SCANOUT rejected, 602 rejections in one boot,
totalFrames = 0, screen black), while the GTK path goes through WGL and works.
So the window exists, it is hidden through the boot, and `view` shows it.

What is GONE is the reparenting: nothing is ever a child of anything, so the
force-killed-viewer failure (`display_lost`) cannot happen and is not
reported. The bar is owned BY the guest window instead -- see
tests/test_windowbar.py.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import engine


class ViewShowsTheWindowAndItsBar(unittest.TestCase):

    def test_view_applies_chrome_then_shows_then_spawns_the_bar(self):
        calls = []
        with mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242,
                                      "display_kind": "gl-window"}), \
             mock.patch("omnidroid.hostwin.apply_chrome",
                        side_effect=lambda *a, **k: calls.append("chrome")
                        or {"applied": True, "reason": "", "hwnd": 1}), \
             mock.patch("omnidroid.hostwin.show_qemu_window",
                        side_effect=lambda *a, **k: calls.append("show")), \
             mock.patch("omnidroid.engine._spawn_window_bar",
                        side_effect=lambda *a, **k: calls.append("bar")):
            engine.cmd_view(_args(name="farm3"))
        self.assertEqual(calls, ["chrome", "show", "bar"])


class TheRemovedFailureModeIsReallyGone(unittest.TestCase):
    """A force-killed viewer can no longer blind the guest, so the error that
    reported it must not exist -- a dead error path is worse than none: it is
    read as a live hazard by the next person."""

    def test_the_engine_no_longer_mentions_display_lost(self):
        source = open(os.path.join(os.path.dirname(__file__), "..",
                                   "omnidroid", "engine.py"),
                      encoding="utf-8").read()
        self.assertNotIn("display_lost", source)

    def test_embedview_is_gone(self):
        path = os.path.join(os.path.dirname(__file__), "..",
                            "omnidroid", "embedview.py")
        self.assertFalse(os.path.exists(path))

    def test_hostwin_no_longer_exposes_window_is_embedded(self):
        from omnidroid import hostwin
        self.assertFalse(hasattr(hostwin, "window_is_embedded"))


def _args(**kw):
    defaults = {"name": "farm3", "start": False, "native": False,
                "json": False, "debug": False, "mode": None, "offset": None,
                "timeout": 60}
    defaults.update(kw)
    return type("Args", (), defaults)()


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 3: Run it and watch it fail**

Run: `python -m pytest tests/test_hidden_window_viewer.py -q`
Expected: FAIL on all four — `_spawn_window_bar` does not exist, `display_lost` is still in `engine.py`, `embedview.py` still exists, `window_is_embedded` still exists.

- [ ] **Step 4: Replace the embedded-viewer branch in `cmd_view`**

In `omnidroid/engine.py`, replace the whole block that begins at the `if boot_has_hidden_window(args.name) and embedview.available()` test (around line 6551) and ends where the VNC path resumes, with:

```python
    if boot_has_hidden_window(args.name) and not getattr(args, "native", False):
        run = _run_record(args.name)
        identity = run.get("identity") or f"omni-{args.name}"
        qemu_pid = run.get("pid")
        # Restyle first, THEN show. The other order puts an unstyled window
        # with a caption on screen for a frame and then yanks it about, which
        # reads as a glitch in the product rather than as a window being set
        # up.
        chrome = hostwin.apply_chrome(identity, pid=qemu_pid,
                                      geometry=run.get("geometry"))
        if not chrome["applied"]:
            print(f"[view {args.name}] the window keeps QEMU's own chrome: "
                  f"{chrome['reason']}. Rendering and input are unaffected.")
        hostwin.show_qemu_window(identity)
        title = f"omni: {args.name}"
        try:
            _spawn_window_bar(args.name, title, identity, qemu_pid)
        except Exception as e:      # noqa: BLE001
            print(f"[view {args.name}] the title bar did not start ({e}); the "
                  f"window is usable and the app can still hide or stop it.")
        if getattr(args, "json", False):
            emit_json({"name": args.name, "viewer": "window",
                       "chrome": chrome["applied"], "vnc_host": None,
                       "vnc_port": None, "started": started, "ok": True})
        return
```

- [ ] **Step 5: Add the spawn helper and the hidden subcommand**

Next to where `_spawn_embedded_viewer` is defined in `engine.py`, replace it with:

```python
def _spawn_window_bar(name, title, identity, pid):
    """Start the title bar in its own process, detached.

    Detached on purpose: it is a window, not a step of the launch, and `view`
    must return as soon as the guest is on screen. It holds nothing hostage --
    the bar is OWNED BY the guest's window, so killing it however you like
    leaves the guest rendering (windowbar.py).
    """
    argv = [sys.executable, "-m", "omnidroid", "_windowbar", name,
            "--identity", identity, "--title", title]
    if pid:
        argv += ["--pid", str(pid)]
    return subprocess.Popen(argv, **_no_console_kwargs())
```

Register the subcommand where `_embedview` is registered (mirror its flags exactly), and route it to:

```python
def cmd_windowbar(args):
    """Hidden: run the title bar for one instance. Not a user-facing command."""
    from omnidroid import windowbar

    def stop_instance(_identity):
        cmd_stop(type("Args", (), {"name": args.name, "json": False})())

    return windowbar.run_window_bar(args.identity, title=args.title,
                                    pid=args.pid, on_stop=stop_instance)
```

- [ ] **Step 6: Delete the dead code**

```bash
git rm omnidroid/embedview.py
```

Then remove from `omnidroid/engine.py`: the `embedview` import, `EMBED_VIEWER_SETTLE`, `_spawn_embedded_viewer`, the `_embedview` subcommand registration and its handler, and the whole `display_lost` `fail(...)` branch. Remove `window_is_embedded` from `omnidroid/hostwin.py:718` and its test in `tests/test_hostwin_backends.py`.

- [ ] **Step 7: Run the tests**

Run: `python -m pytest tests/test_hidden_window_viewer.py tests/test_hostwin_backends.py -q`
Expected: all pass.

- [ ] **Step 8: Run the whole suite**

Run: `python -m pytest tests/ -q`
Expected: no NEW failures. Any test importing `embedview` must have been rewritten in this task — if one still does, it was missed in Step 1.

- [ ] **Step 9: Commit**

```bash
git add -A omnidroid/ tests/
git commit -m "view shows QEMU's own restyled window; embedview and display_lost are gone"
```

---

### Task 6: `run.json` records the display, and the app can hide and show

**Files:**
- Modify: `omnidroid/qemu_proc.py:1990-2020` (the run record)
- Modify: `omni-executor/main.py:1489-1505` (`engine_view`)
- Test: `tests/test_windowbar.py` (append), `omni-executor/tests/` for the app half

**Interfaces:**
- Consumes: `qemu_proc.command_opens_a_window(cmd)`, `qemu_proc.uses_gl_context(display_args)`.
- Produces: `run.json` keys `display_kind` (`"gl-window"` | `"window"` | `"vnc"` | `"none"`) and `geometry` (`[x, y, w, h]` or absent); app method `engine_hide(name)`.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_windowbar.py`:

```python
class RunRecordSaysWhatTheDisplayIs(unittest.TestCase):
    """`view` must not have to re-derive the boot's display policy: the argv
    IS what the process did, so it is read once at spawn and written down."""

    def test_a_gl_window_boot_is_recorded_as_such(self):
        from omnidroid import qemu_proc
        cmd = ["qemu-system-x86_64", "-display",
               "gtk,gl=on,show-menubar=off,window-close=off,zoom-to-fit=on"]
        self.assertEqual(qemu_proc.display_kind(cmd), "gl-window")

    def test_a_software_window_boot_is_recorded_as_a_window(self):
        from omnidroid import qemu_proc
        self.assertEqual(
            qemu_proc.display_kind(["qemu", "-display", "gtk"]), "window")

    def test_a_headless_boot_with_vnc_is_recorded_as_vnc(self):
        from omnidroid import qemu_proc
        self.assertEqual(
            qemu_proc.display_kind(["qemu", "-display", "egl-headless",
                                    "-vnc", "127.0.0.1:1"]), "vnc")

    def test_a_headless_boot_with_no_vnc_is_none(self):
        from omnidroid import qemu_proc
        self.assertEqual(
            qemu_proc.display_kind(["qemu", "-display", "none"]), "none")
```

- [ ] **Step 2: Run it and watch it fail**

Run: `python -m pytest tests/test_windowbar.py -q`
Expected: FAIL — `qemu_proc` has no `display_kind`.

- [ ] **Step 3: Implement `display_kind`**

In `omnidroid/qemu_proc.py`, next to `command_opens_a_window` (line 1733):

```python
def display_kind(cmd):
    """What this boot put on screen, read off the argv it was spawned with.

    Written into run.json so `view` never re-derives the policy: the argv IS
    what the process did, and a second copy of the decision is a second thing
    that can drift.
    """
    display = []
    for i, arg in enumerate(cmd):
        if arg == "-display" and i + 1 < len(cmd):
            display = [cmd[i + 1]]
            break
    if not command_opens_a_window(["-display"] + display):
        return "vnc" if "-vnc" in cmd else "none"
    return "gl-window" if uses_gl_context(display) else "window"
```

- [ ] **Step 4: Write it into the run record**

In `qemu_proc.py` around line 2015, add to the dict that already carries `"identity"`:

```python
         "display_kind": display_kind(cmd),
```

- [ ] **Step 5: Give the app a hide action**

In `omni-executor/main.py`, beside `engine_view` (line ~1489):

```python
    def engine_hide(self, name):
        """Hide an instance's window without stopping it.

        The window's own X asks (hide or stop); this is the same 'hide' from
        the app side, so a window that was hidden can always be brought back
        by View without the user having to find it on the desktop.
        """
        error = self._bad_name(name)
        if error:
            return error
        return run_engine(["view", name, "--hide", "--json"],
                          timeout=VIEW_TIMEOUT)
```

and add `--hide` to the `view` subcommand in `engine.py`, handled at the top of the branch added in Task 5:

```python
        if getattr(args, "hide", False):
            hostwin.hide_qemu_window(identity)
            if getattr(args, "json", False):
                emit_json({"name": args.name, "viewer": "window",
                           "hidden": True, "ok": True})
            return
```

- [ ] **Step 6: Run the tests**

Run: `python -m pytest tests/ -q`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add omnidroid/qemu_proc.py omnidroid/engine.py tests/test_windowbar.py
git commit -m "run.json records the display kind; the app can hide a window without stopping it"
```

---

### Task 7: Linux stays exactly as it is — DEFERRED to 2026-08-16

**Nothing is implemented for Linux in this plan.** A host arrives tomorrow;
until then Linux keeps today's behaviour (`egl-headless` + VNC, QEMU's own
frame), which works. The only Linux work here is making the *decline* honest,
so a Linux user reads a reason rather than watching chrome silently not happen.

Writing an X11 backend now would mean shipping a `_MOTIF_WM_HINTS` path and a
`WM_TRANSIENT_FOR` path that no one has ever run, in a subsystem whose whole
history is measurements contradicting what looked obvious. That is the trade
this task refuses.

**Files:**
- Modify: `omnidroid/hostwin.py` (`apply_chrome`, the non-Windows branch from Task 3)
- Test: `tests/test_window_chrome.py` (append)

**Interfaces:**
- Consumes: `hostwin.backend()`, `BACKEND_XDOTOOL`, `BACKEND_WMCTRL`, `BACKEND_XLIB`.
- Produces: nothing new.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_window_chrome.py`:

```python
class ChromeOnLinuxIsDeferredAndSaysSo(unittest.TestCase):
    """Linux keeps QEMU's own frame until a host has verified a replacement.

    The reason has to name the state -- 'not implemented yet' -- rather than
    read as a failure, because nothing is broken: the window works, the guest
    renders on the GPU, and the VNC viewer is still there. Only the chrome is
    missing.
    """

    def test_an_x11_backend_declines_with_a_deferral_not_an_error(self):
        for name in (hostwin.BACKEND_XDOTOOL, hostwin.BACKEND_WMCTRL,
                     hostwin.BACKEND_XLIB):
            with mock.patch.object(hostwin, "backend", return_value=name):
                result = hostwin.apply_chrome("omni-farm3")
            self.assertFalse(result["applied"])
            self.assertIn("not implemented", result["reason"].lower())
            self.assertIn("linux", result["reason"].lower())

    def test_the_decline_never_raises_and_never_blocks_a_boot(self):
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_XLIB):
            result = hostwin.apply_chrome("omni-farm3")
        self.assertIsInstance(result, dict)
        self.assertIn("hwnd", result)
```

- [ ] **Step 2: Run it and watch it fail**

Run: `python -m pytest tests/test_window_chrome.py -q`
Expected: FAIL — Task 3's generic reason says "implemented for Windows only"
and does not name Linux or the deferral.

- [ ] **Step 3: Make the decline specific**

In `omnidroid/hostwin.py`, replace the non-Windows early return in
`apply_chrome` with:

```python
    if name in (BACKEND_XDOTOOL, BACKEND_WMCTRL, BACKEND_XLIB):
        return _chrome_result(
            False,
            "window chrome is not implemented on Linux yet: the window keeps "
            "QEMU's own frame. Nothing is broken -- the guest renders on the "
            "GPU and the VNC viewer works. _MOTIF_WM_HINTS is the route and "
            "it will be written against a real host rather than guessed at.")
    if name != BACKEND_WIN32:
        return _chrome_result(
            False,
            f"restyling another process's window is not implemented for "
            f"backend {name}; on macOS the chrome comes from our own QEMU "
            f"build instead")
```

- [ ] **Step 4: Run the tests**

Run: `python -m pytest tests/test_window_chrome.py -q`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add omnidroid/hostwin.py tests/test_window_chrome.py
git commit -m "Linux keeps QEMU's own frame and says why; X11 chrome deferred to a real host"
```

---

### Task 8: macOS resolves correctly and reports what it is missing

macOS cannot be finished here (sub-project B owns the QEMU build; the image needs `ro.hardware.egl=mesa`; Xcode CLT needs a human). What it can do now is resolve to the right arguments and tell the truth about the three blockers instead of rendering black.

**Files:**
- Create: `omnidroid/macgpu.py`
- Test: `tests/test_macos_gpu_readiness.py`

**Interfaces:**
- Consumes: `qemu_proc._qemu_help_texts(tool)`, `qemu_proc.GL_GPU_DEVICE`.
- Produces: `macgpu.readiness(qemu_device_help="", surfaceflinger_gles="") -> dict` with keys `ready` (bool) and `blockers` (list of strings).

- [ ] **Step 1: Write the failing test**

Create `tests/test_macos_gpu_readiness.py`:

```python
#!/usr/bin/env python3
"""macOS says which of its three prerequisites are missing.

    python3 -m pytest tests/test_macos_gpu_readiness.py -q

None of the three is code in this repo, and all three fail the same way from
the user's chair -- a black or 3 fps guest -- so the product has to name them:

  1. a virgl-capable QEMU (Homebrew core has no virtio-gpu-gl-pci at all)
  2. an arm base rebuilt with ro.hardware.egl=mesa (it ships `angle`, so guest
     GL goes ANGLE -> SwiftShader in software whatever the host offers, and
     ro.* is immutable after init)
  3. Xcode CLT new enough for Homebrew to build from source

The SurfaceFlinger line is the acceptance test for (2): want `Mesa, virgl`,
never `ANGLE ... SwiftShader`.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import macgpu

HAS_GL_DEVICE = 'name "virtio-gpu-gl-pci", bus PCI\n'
NO_GL_DEVICE = 'name "virtio-gpu-pci", bus PCI\n'
MESA = "GLES: Mesa, virgl (Apple M1), OpenGL ES 3.2"
ANGLE = "GLES: ANGLE (Apple, Apple M1, SwiftShader), OpenGL ES 3.0"


class Readiness(unittest.TestCase):

    def test_no_gl_device_is_reported_as_the_qemu_build(self):
        result = macgpu.readiness(qemu_device_help=NO_GL_DEVICE)
        self.assertFalse(result["ready"])
        self.assertTrue(any("virglrenderer" in b for b in result["blockers"]))

    def test_angle_in_surfaceflinger_is_reported_as_the_image(self):
        result = macgpu.readiness(qemu_device_help=HAS_GL_DEVICE,
                                  surfaceflinger_gles=ANGLE)
        self.assertFalse(result["ready"])
        self.assertTrue(any("ro.hardware.egl" in b
                            for b in result["blockers"]))

    def test_both_present_is_ready(self):
        result = macgpu.readiness(qemu_device_help=HAS_GL_DEVICE,
                                  surfaceflinger_gles=MESA)
        self.assertTrue(result["ready"], result["blockers"])
        self.assertEqual(result["blockers"], [])

    def test_an_unknown_surfaceflinger_line_is_not_treated_as_a_pass(self):
        result = macgpu.readiness(qemu_device_help=HAS_GL_DEVICE,
                                  surfaceflinger_gles="")
        self.assertFalse(result["ready"])


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run it and watch it fail**

Run: `python -m pytest tests/test_macos_gpu_readiness.py -q`
Expected: FAIL — no module `omnidroid.macgpu`.

- [ ] **Step 3: Write the module**

Create `omnidroid/macgpu.py`:

```python
"""Whether this Mac can render the guest on the GPU, and what is missing.

Three prerequisites, none of them code in this repository, all of them
presenting to the user as the same symptom -- a black or 3 fps guest. So the
product names them rather than letting somebody rediscover them:

1. A virgl-capable QEMU. Homebrew core has no `virtio-gpu-gl-pci` at all
   (`-display help` there is none/curses/cocoa/dbus). The route that works is
   knazarov's libangle + libepoxy-angle + virglrenderer formulae with QEMU
   built against them into a PRIVATE PREFIX; `startergo/qemu-virgl-kosmickrisp`
   names its formula `qemu` and Homebrew evicts the working one.
2. An arm base rebuilt with `ro.hardware.egl=mesa`. The image ships `angle`,
   so guest GL goes ANGLE -> SwiftShader in software no matter what the host
   offers. Mesa (/vendor/lib64/egl/libEGL_mesa.so) and the render node
   (/sys/class/drm/renderD128) are already there; only the property is wrong,
   and ro.* is immutable after init, so setprop cannot fix it.
3. Xcode CLT new enough for Homebrew to build from source. Do NOT probe this
   with `brew install --dry-run` on a bottled formula: a bottle never invokes
   a compiler, so the probe passes on a machine that cannot build anything.

This module is pure. The caller supplies the two host facts.
"""
from omnidroid.qemu_proc import GL_GPU_DEVICE

MESA_MARKER = "mesa"
VIRGL_MARKER = "virgl"


def readiness(qemu_device_help="", surfaceflinger_gles=""):
    """{"ready": bool, "blockers": [str, ...]}. Never raises.

    An EMPTY SurfaceFlinger line is a blocker, not a pass: "we could not ask"
    and "the answer was good" are different states, and treating the first as
    the second is how a software guest gets reported as accelerated.
    """
    blockers = []
    if GL_GPU_DEVICE not in (qemu_device_help or ""):
        blockers.append(
            f"this QEMU has no {GL_GPU_DEVICE} (built without virglrenderer). "
            f"Build one into a private prefix and point config `qemu.dir` at "
            f"it; never replace the Homebrew `qemu` formula.")
    gles = (surfaceflinger_gles or "").lower()
    if not gles:
        blockers.append(
            "could not read the guest's GLES renderer "
            "(`adb shell dumpsys SurfaceFlinger | grep GLES:`), so whether it "
            "is accelerated is unknown")
    elif not (MESA_MARKER in gles and VIRGL_MARKER in gles):
        blockers.append(
            "the guest renders through ANGLE/SwiftShader in software: the arm "
            "base ships ro.hardware.egl=angle and ro.* is immutable after "
            "init, so the base must be REBUILT with ro.hardware.egl=mesa")
    return {"ready": not blockers, "blockers": blockers}
```

- [ ] **Step 4: Run the tests**

Run: `python -m pytest tests/test_macos_gpu_readiness.py -q`
Expected: 4 passed.

- [ ] **Step 5: Run the full suite and commit**

```bash
git add omnidroid/macgpu.py tests/test_macos_gpu_readiness.py
git commit -m "macOS names its three GPU blockers instead of rendering black"
```

---

### Task 9: Measure it on Windows and write the numbers down

Not TDD. This is the tier that catches what unit tests pin wrongly — `tests/test_gpu_display.py` once asserted `gl=on` on every platform, which would have made the first Mac with a GL QEMU look broken.

**Files:**
- Modify: `MODES.md` (the GPU section), `CHANGELOG.md`

- [ ] **Step 1: Boot a gaming instance and confirm the argv**

Run: `python -m omnidroid start <account> --place 8737899170 --mode gaming --json`
Expected: the printed `[gpu]` line names a `gtk,gl=on,...` display. Check `run.json` has `"display_kind": "gl-window"`.

- [ ] **Step 2: Confirm the window is hidden through the boot**

Watch the screen for the whole boot. Expected: no QEMU window appears at any point. If one flashes, `keep_hidden()` needs a longer window, not a re-hide at the end.

- [ ] **Step 3: Show it and check the chrome**

Run: `python -m omnidroid view <account>`
Expected: the guest appears with our title bar above it and **no QEMU menu bar**, **no second title bar**, our title and icon.

- [ ] **Step 4: Measure the frame rate**

Run:
```bash
adb -s 127.0.0.1:16001 shell dumpsys SurfaceFlinger --timestats -clear
# wait 30 seconds with the game in the world
adb -s 127.0.0.1:16001 shell dumpsys SurfaceFlinger --timestats -dump
```
Expected: `totalFrames` well above zero. Record frames/30 s. The existing measured band on this box is 24–58 fps on PS99; a number far below that means the boot fell back to software — check the `[gpu]` line.

- [ ] **Step 5: Prove the hazard is gone**

Run: `taskkill /F /IM python.exe /FI "WINDOWTITLE eq omni: <account>"` (kill the bar, not QEMU), then re-run the timestats from Step 4.
Expected: **the guest is still rendering.** This is the single most important check in the task — it is the failure this design exists to remove. Under the old code the same action left `totalFrames = 0` forever.

- [ ] **Step 6: Exercise the close prompt**

Click the bar's X three times: Cancel (nothing happens), Hide (window disappears, `omnidroid list` still shows it running, `omnidroid view` brings it back), Stop (instance powers off).

- [ ] **Step 7: Write the results into the docs**

Update `MODES.md`'s GPU section: replace the "the window is hosted by our viewer / `SetParent`" description with the restyled-window-plus-owned-bar model, and put the measured fps in the table. Add a `CHANGELOG.md` entry recording what was measured, on what, and what was removed.

- [ ] **Step 8: Commit**

```bash
git add MODES.md CHANGELOG.md
git commit -m "Measured: gaming's restyled window on Windows, and the force-kill hazard is gone"
```

---

## Self-Review

**Spec coverage.** §3 window model → Tasks 3, 4. §3a ownership → Task 4. §3b per-platform → Tasks 1, 2 (flags), 3 (Windows chrome), 7 (Linux declines), 8 (macOS). §3d `--gpu` profile-directed → Task 1. §4 lifecycle → Tasks 5, 6. §5 degradation → Tasks 3 (chrome reason), 5 (bar failure), 7 (Linux), 8 (macOS blockers). §6 deletions → Task 5. §7 verification → every task's tests plus Task 9. §8 dependencies → Tasks 7, 8 carry the honesty requirements.

**Deliberate deviation from the spec, recorded rather than silent.** §3c has Linux moving off `egl-headless` onto a GL window. **This plan does not do that**, because no Linux host exists to verify it and the switch would drop the VNC server on the way (QEMU refuses `-vnc` beside a GL window) — a Linux user would get a raw QEMU frame with no viewer and no chrome. Linux keeps today's behaviour behind `_WINDOW_PRESENT_PLATFORMS`; §3c becomes a one-line change plus an X11 chrome task once a host is available (2026-08-16).

**Gap found and closed:** §4 says `run.json` gains geometry, and Task 6 only writes `display_kind`. Task 3's `window_geometry()` supplies the read and Task 5's `cmd_view` passes `run.get("geometry")` to `apply_chrome`; the **write** happens when the bar exits. Added to Task 6 as a note here rather than a silent omission: `cmd_windowbar`'s `on_stop`/exit path must persist `hostwin.window_geometry(identity)` into `run.json` before returning, so the next `view` restores position. Implement it in Task 6, Step 4, alongside `display_kind`.

**Placeholder scan:** no TBDs; every code step carries the code. Task 9 is measurement, which is why it has commands and expected values instead of code.

**Type consistency:** `apply_chrome` returns `{"applied", "reason", "hwnd"}` in Tasks 3, 5, 7 alike. `bar_geometry` takes `(x, y, width, height)` and returns the same shape in Task 4 and its tests. `display_kind(cmd)` takes the full argv in Task 6's implementation and its tests. `on_close()` returns `"hide" | "stop" | "cancel"` in Task 4 and Task 5's `on_delete`.
