# Sub-project D — Remove the tkinter viewer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete every tkinter code path from the `omnidroid` package — the Tk+RFB viewer and the Tk title bar — while keeping the RFB protocol engine that `screenshot` / `capture` / autocap are built on, extracted into a UI-free `omnidroid/rfb.py`.

**Architecture:** `omnidroid/vncview.py` is two things welded together: a pure-stdlib RFB 3.x protocol client (`RFBClient`, lines 55–284) and a Tk front-end (`run_viewer`/`main`, lines 287–446). Only the front-end is dead product; `capture.py:280` constructs `RFBClient` and is how every screenshot and keyframe in the product gets taken. So the protocol half moves verbatim into a new `omnidroid/rfb.py` that imports nothing but `socket`/`struct`/`threading`/`time`, `capture.py` is repointed at it, and then the Tk half, `omnidroid/windowbar.py`, the two hidden subcommands that launch them (`_vncview`, `_windowbar`), the engine plumbing that spawned and killed them, their tests, and their `--hidden-import` lines in the two build scripts are all deleted. `omnidroid view` on a boot whose pixels live in a VNC framebuffer resolves an OS/native VNC client instead, and says so honestly when the host has none.

**Tech Stack:** Python 3.13 stdlib only (`pyproject.toml` declares `dependencies = []`), `unittest` + `pytest` runner, PyInstaller (build scripts only), `argparse` subcommands.

**Spec:** `docs/superpowers/specs/2026-08-19-omni-qemu-and-density-design.md` (§7 "Sub-project D — remove the tkinter viewer"; §3f names what sub-project E deletes, which this plan must not touch)

## Global Constraints

- **Python 3.13, stdlib only.** `pyproject.toml`: `requires-python = ">=3.13"`, `dependencies = []`. Nothing added in this plan may introduce a third-party import. Pillow stays an optional runtime dependency of `capture.py` only, imported lazily inside functions (`capture._lazy_pil`), never at module import.
- **`omnidroid/rfb.py` imports no tkinter and no PIL** — spec §7: "pure protocol, no tkinter, no Pillow". This is enforced by a test, not by review.
- **No tkinter remains in the package** (spec §7, final line). Enforced by an AST scan of `omnidroid/**/*.py` in Task 7.
- **`tests/engine_public_names.json` is a facade contract** enforced by `tests/test_facade_equivalence.py`: every name in that JSON must still resolve as `engine.<name>`. Deleting a public engine name REQUIRES deleting its line from that JSON in the same commit. Two names in this plan are listed: `"_run_vncview"` (line 148) and `"_spawn_builtin_viewer"` (line 156). None of the window-bar names are listed — verified by grep.
- **Never run `git add -A`, `git checkout`, `git stash`, or `git reset` in this repo.** It carries ~60 uncommitted tracked files of another author's in-flight work. Every commit step below lists its files explicitly; `git add` exactly those and nothing else.
- **The working test command** (from `docs/HANDOFF-WINDOWS.md:2691-2696`), run from the repo root:
  ```bash
  cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
  OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
    OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
    python -m pytest tests/ -q
  ```
  **Copy the config fresh every run** — the engine writes back to `OMNIDROID_CONFIG_PATH` and has been seen to drop the base registration, so the second run fails for a reason the first one caused. If the config has no base registered, collection dies with `INTERNALERROR ... SystemExit: error: no base image is registered` (`tests/test_qemu_accepts_devices.py` calls `load_config()` at import time) — that is a config problem, not a code problem.
- **There is no pytest config in `pyproject.toml`.** Single-file runs need the same env: `OMNIDROID_CONFIG_PATH=... OMNI_IMAGES_DIR=... python -m pytest tests/test_x.py -q`.
- **Baseline is ~11 failed / 932 passed.** Capture your own baseline before Task 1 (Task 1 Step 0) and **diff the `FAILED` lines** against it after every task. Never count them.
- **Do not touch `--window` / `--no-window` / `--native` / `--viewer` CLI flags or subcommand names other than `_vncview` and `_windowbar`.** `omni-executor` is a separate repo that builds argv against this parser (`tests/test_contract_commands.py::ExecutorContract` is the guard); removing a flag it passes turns into `unrecognized arguments` in the GUI.

## Not in scope — do NOT delete these (sub-project E owns them)

Sub-project E (`docs/superpowers/specs/2026-08-19-omni-qemu-and-density-design.md` §3f) deletes the window-lock machinery. **This plan must leave all of it alone**, or the two plans collide:

- the `_windowlock` subcommand (`engine.py:12086`) and its handler `_run_windowlock` (`engine.py:6791`) — **not tkinter**; it is `hostwin.aspect_lock`, pure ctypes.
- `_spawn_window_lock`, `_ensure_window_lock`, `_running_window_lock_pid`, `_window_lock_pid_path`, and the `aspect_locked` key in `cmd_view`'s JSON.
- `omnidroid/hostwin.py` in its entirety — ctypes only, no tkinter. It stays. (Its comments *mention* `windowbar.py`; Task 6 fixes the prose, nothing else.)
- `_WINDOW_FLAGS` / `window-close=off` in `qemu_proc.py`.

Also out of scope, in a **sibling repo**: `omni-executor`'s PyInstaller `.spec` files carry the same `tkinter` / `PIL.ImageTk` hiddenimports (`docs/HANDOFF-WINDOWS.md:2830-2839` — `tests/test_packaging.py` lives there, not here). Task 7 records this as a handoff line; do not attempt to edit another repo.

---

## File Structure

**Created**

| file | responsibility |
|---|---|
| `omnidroid/rfb.py` | The RFB 3.x protocol client. `RFBClient` and the protocol constants, moved verbatim out of `vncview.py`. Stdlib only: `socket`, `struct`, `threading`, `time`. No UI, no imaging. |
| `tests/test_rfb.py` | Offline protocol tests for `rfb.RFBClient` — raw rect placement, copyrect, the `on_frame` immutable-copy contract capture.py depends on, DesktopSize resize — plus a subprocess purity check that importing it loads neither tkinter nor PIL. |
| `tests/test_native_viewer_only.py` | What `view` and `start` do now that there is no viewer of ours: `view` on a framebuffer boot resolves a native VNC client or fails with a typed error; `start` opens no viewer process at all. |
| `tests/test_tkinter_is_gone.py` | The absence guards, grown across Tasks 3/4/5/7: the deleted modules do not exist, the deleted engine names do not resolve, the hidden subcommands do not parse, and no module under `omnidroid/` imports tkinter (AST scan). Modelled on the existing `TheRemovedFailureModeIsReallyGone` class in `tests/test_hidden_window_viewer.py:1008`. |

**Deleted**

| file | why |
|---|---|
| `omnidroid/vncview.py` (446 lines) | Its protocol half becomes `rfb.py`; its Tk half (`run_viewer`, `main`, `_KEYSYMS`) is the thing being removed. |
| `omnidroid/windowbar.py` (636 lines) | Retired: `engine._spawn_window_bar` has had zero production callers since `cmd_view` hardcoded `bar_ok = False`. Its docstrings describe an `apply_chrome` shape that no longer exists, so reviving it would give two title bars (spec §7). |
| `tests/test_windowbar.py` (705 lines) | Tests only `windowbar`. |

**Modified**

| file | change |
|---|---|
| `omnidroid/capture.py` | `from omnidroid import vncview` → `from omnidroid import rfb`; the two docstring/comment references and the constructor call at line 280. |
| `omnidroid/engine.py` | Delete `_spawn_builtin_viewer`, `_run_vncview`, the `_vncview` parser, `_spawn_window_bar`, `_run_windowbar`, the `_windowbar` parser, `_window_bar_pid_path`, `_running_window_bar_pid`, `_write_window_bar_pid`, `_clear_window_bar_pid`, `_kill_window_bar`, `_persist_window_bar_geometry`, `WINDOW_BAR_SETTLE`, `_window_bar_settled`. Rewrite `cmd_view`'s viewer branch and docstring, `cmd_start`'s viewer block, `_view_hide`'s bar teardown, and three dangling comments. |
| `omnidroid/config.py:151` | The `self_argv_prefix` docstring's `_vncview` example. |
| `tests/engine_public_names.json` | Drop `"_run_vncview"` and `"_spawn_builtin_viewer"`. |
| `tests/test_pixel_format.py:51-70` | Retarget `test_request_shifts_match_the_decoder` at `rfb.RFBClient` (this test is KEPT — it pins the RGBX shifts). |
| `tests/test_self_argv_prefix.py` | Delete both spawned-child classes and rewrite the module docstring around the surviving child (the autocap recorder). |
| `tests/test_session.py:466` | Drop the `_spawn_builtin_viewer` patch. |
| `tests/test_hidden_window_viewer.py` (1038 lines) | Drop every `_spawn_window_bar` / `_spawn_builtin_viewer` / bar-pid patch and the two classes that exist only to test the bar. |
| `tests/test_window_at_boot.py:543,564` | Drop the `_running_window_bar_pid` patches. |
| `build-exe.ps1:14-27`, `build-linux.sh:18-32` | Drop `--hidden-import vncview`, `--hidden-import tkinter`, `--hidden-import PIL.ImageTk` and rewrite the comment. **`PIL.Image` / `PIL.ImageChops` / `PIL.ImageStat` STAY** — `capture.py` needs all three. |
| `MODES.md`, `HANDOFF.md`, `docs/HANDOFF-WINDOWS.md`, `CHANGELOG.md` | Prose that documents the Tk viewer/bar as shipping behaviour. |

---

## Decisions this plan makes beyond the approved deletion list

Both are forced by deleting `_spawn_builtin_viewer`, which — unlike `_spawn_window_bar` — **has two live production callers**. Neither is optional; they are recorded here so a reviewer can reject them explicitly.

1. **`cmd_view`'s framebuffer branch becomes native-client-or-typed-error.** `engine.py:7280-7288` spawns the Tk viewer whenever `--native` was not passed. With it gone, `view` always resolves through `_vnc_viewer_command()` (which already handles `--viewer`, config `qemu.vnc_viewer`, macOS Screen Sharing, Windows `vncviewer.exe`/shell handler, Linux TigerVNC/remmina/gvncviewer) and, when that returns `None`, calls `fail("no_vnc_client", ...)` rather than pretending. `--native` keeps its other meaning (`engine.py:7170` — "do not use QEMU's own window") so the flag is not inert.
2. **`cmd_start` stops opening a viewer** (`engine.py:2249-2266`). It reports where the screen is instead; the existing success line (`f"vnc 127.0.0.1:{acct['vnc_port']} to watch"`) already says exactly that when `viewer_pid` is None. `_want_vnc_viewer` and its six tests in `tests/test_gaming_window_handoff.py::TheViewerDecision` are **kept** — the predicate still decides whether to name the screen — and `--window` / `--no-window` are **kept** because `omni-executor` builds argv against this parser. `result["viewer_pid"]` disappears from `start --json` (it was only ever present when a Tk viewer opened).

---

### Task 1: `omnidroid/rfb.py` — the protocol engine, with no UI in it

**Files:**
- Create: `omnidroid/rfb.py`
- Create: `tests/test_rfb.py`
- Read (do not modify yet): `omnidroid/vncview.py:1-285`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `omnidroid.rfb.RFBClient(host: str, port: int, on_frame=None)` — identical constructor signature to today's `vncview.RFBClient`.
  - Instance attributes later tasks and `capture.py` rely on: `.width: int`, `.height: int`, `.fb: bytearray` (w*h*4, RGBX), `.name: str`, `.update_count: int`, `.last_update_ns: int|None`, `.dirty: threading.Event`, `.closed: threading.Event`, `.error: Exception|None`.
  - Methods: `.connect() -> None`, `.run() -> None`, `.close() -> None`, `.snapshot() -> (width, height, bytes, completed_ns, sequence)`, `.request_update(incremental=True)`, `.pointer(button_mask, x, y)`, `.key(keysym, down)`, `._set_pixel_format()`, `._set_encodings(encs)`, `._resize(w, h)`, `._raw_rect(x, y, w, h)`, `._copy_rect(x, y, w, h, sx, sy)`, `._framebuffer_update()`, `._recvn(n)`, `._send(data)`, `._security(proto)`, `._server_init()`.
  - `on_frame` callback contract: `on_frame(width: int, height: int, bgrx: bytes, completed_ns: int, sequence: int)`, called once per COMPLETED framebuffer update, from the receive thread, with an **immutable copy** of the framebuffer taken under `_fblock`.
  - Module constants: `_SET_PIXEL_FORMAT=0`, `_SET_ENCODINGS=2`, `_FB_UPDATE_REQUEST=3`, `_KEY_EVENT=4`, `_POINTER_EVENT=5`, `_ENC_RAW=0`, `_ENC_COPYRECT=1`, `_ENC_DESKTOPSIZE=-223`.
  - **Not** produced: `_KEYSYMS`. That dict maps *Tk* `event.keysym` strings to X11 keysym codes and is used only by `run_viewer`'s `keysym_for`. It is Tk-shaped input handling, so it goes with the Tk half in Task 3.

- [ ] **Step 0: Record the baseline before touching anything**

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/ -q 2>&1 | tee /tmp/omni-baseline.txt
grep '^FAILED' /tmp/omni-baseline.txt | sort > /tmp/omni-baseline-failed.txt
wc -l /tmp/omni-baseline-failed.txt
```

Expected: roughly 11 lines, and a summary line near `11 failed, 932 passed`. These are pre-existing failures from another author's in-flight work. **Every later "run the suite" step means: re-run this and `diff /tmp/omni-baseline-failed.txt <(grep '^FAILED' new.txt | sort)`.** An empty diff is the pass condition. Do not commit this file anywhere.

- [ ] **Step 1: Write the failing test**

Create `tests/test_rfb.py`:

```python
#!/usr/bin/env python3
"""The RFB protocol client, exercised offline — no VNC server, no VM, no UI.

`omnidroid/rfb.py` is what `capture.py` (and through it `screenshot`,
`capture` and autocap) is built on, so two things are pinned here:

  * the decoder actually puts bytes where the wire said to put them, and
  * `on_frame` hands out an IMMUTABLE COPY of every completed update. That is
    the whole reason capture.py can promise "two consecutive updates can never
    collapse into one frame" — a viewer may coalesce redraws, a recorder may
    not.

Plus the constraint the module exists for: importing it must not drag in a GUI
toolkit or an imaging library. The Tk viewer that used to live beside this code
is deleted.

    python3 -m pytest tests/test_rfb.py -q
"""
import json
import os
import struct
import subprocess
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import rfb  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _feeder(payload):
    """A stand-in for RFBClient._recvn backed by one flat buffer.

    Same contract as the socket read it replaces: hand back exactly `n` bytes
    or blow up. A short read here means the test's wire bytes are wrong, and
    that must fail loudly rather than silently decode garbage.
    """
    box = {"buf": bytes(payload)}

    def recvn(n):
        chunk, box["buf"] = box["buf"][:n], box["buf"][n:]
        assert len(chunk) == n, f"fake wire ran out: wanted {n}, had {len(chunk)}"
        return chunk

    return recvn


def _client(width=1, height=1, on_frame=None):
    """A connected-looking client with no socket behind it. __init__ does no
    I/O, so this is the real object with a real lock and a real framebuffer."""
    c = rfb.RFBClient("127.0.0.1", 18001, on_frame=on_frame)
    c._resize(width, height)
    return c


class TheDecoderPutsBytesWhereTheWireSaid(unittest.TestCase):

    def test_a_raw_rect_lands_at_its_own_offset_not_at_the_origin(self):
        c = _client(4, 2)
        pixels = bytes(range(1, 9))          # 2 pixels x 4 bytes, RGBX
        c._recvn = _feeder(pixels)
        c._raw_rect(1, 1, 2, 1)
        # row 1 starts at 1 * (4 px * 4 bytes) = 16; x=1 adds 4 => 20.
        self.assertEqual(bytes(c.fb[20:28]), pixels)
        self.assertEqual(bytes(c.fb[:20]), bytes(20))
        self.assertEqual(bytes(c.fb[28:]), bytes(len(c.fb) - 28))

    def test_a_copyrect_moves_pixels_inside_the_framebuffer(self):
        c = _client(2, 2)
        c.fb[:8] = bytes([1, 2, 3, 4, 5, 6, 7, 8])
        c._copy_rect(0, 1, 2, 1, 0, 0)       # copy row 0 down onto row 1
        self.assertEqual(bytes(c.fb[8:16]), bytes([1, 2, 3, 4, 5, 6, 7, 8]))

    def test_a_downward_copyrect_of_overlapping_rows_is_not_smeared(self):
        # Rows are copied bottom-up when the source is ABOVE the destination,
        # so an overlapping scroll cannot read a row it has already written.
        c = _client(1, 3)
        c.fb[:] = bytes([1, 1, 1, 1, 2, 2, 2, 2, 0, 0, 0, 0])
        c._copy_rect(0, 1, 1, 2, 0, 0)       # rows 0..1 -> rows 1..2
        self.assertEqual(bytes(c.fb[4:8]), bytes([1, 1, 1, 1]))
        self.assertEqual(bytes(c.fb[8:12]), bytes([2, 2, 2, 2]))


class EveryCompletedUpdateReachesTheRecorder(unittest.TestCase):

    def test_on_frame_gets_the_size_bytes_and_a_sequence_number(self):
        seen = []
        c = _client(1, 1, on_frame=lambda *a: seen.append(a))
        c._recvn = _feeder(
            b"\x00"                                   # padding
            + struct.pack("!H", 1)                    # one rectangle
            + struct.pack("!HHHHi", 0, 0, 1, 1, 0)    # x,y,w,h, Raw
            + bytes([9, 8, 7, 0]))                    # one RGBX pixel
        c._framebuffer_update()
        self.assertEqual(len(seen), 1)
        width, height, frame, completed_ns, sequence = seen[0]
        self.assertEqual((width, height), (1, 1))
        self.assertEqual(frame, bytes([9, 8, 7, 0]))
        self.assertEqual(sequence, 1)
        self.assertIsInstance(completed_ns, int)
        self.assertEqual(c.update_count, 1)
        self.assertEqual(c.last_update_ns, completed_ns)

    def test_the_frame_handed_out_is_a_copy_the_next_update_cannot_edit(self):
        # THE contract capture.py is built on. If this were a view of the live
        # bytearray, a 1 ms loading screen would be overwritten by the next
        # update before the worker thread ever decoded it.
        seen = []
        c = _client(1, 1, on_frame=lambda *a: seen.append(a))
        c._recvn = _feeder(b"\x00" + struct.pack("!H", 1)
                           + struct.pack("!HHHHi", 0, 0, 1, 1, 0)
                           + bytes([9, 8, 7, 0]))
        c._framebuffer_update()
        c.fb[0] = 255
        self.assertEqual(seen[0][2], bytes([9, 8, 7, 0]))

    def test_no_callback_means_no_copy_is_made_at_all(self):
        # A viewer that only redraws pays for the Event, not for a full
        # framebuffer copy per update.
        c = _client(1, 1, on_frame=None)
        c._recvn = _feeder(b"\x00" + struct.pack("!H", 1)
                           + struct.pack("!HHHHi", 0, 0, 1, 1, 0)
                           + bytes([9, 8, 7, 0]))
        c._framebuffer_update()
        self.assertTrue(c.dirty.is_set())
        self.assertEqual(c.update_count, 1)

    def test_a_desktopsize_pseudo_encoding_resizes_and_asks_again_in_full(self):
        c = _client(2, 2)
        sent = []
        c._send = lambda data: sent.append(data)
        c._recvn = _feeder(b"\x00" + struct.pack("!H", 1)
                           + struct.pack("!HHHHi", 0, 0, 8, 4, -223))
        c._framebuffer_update()
        self.assertEqual((c.width, c.height), (8, 4))
        self.assertEqual(len(c.fb), 8 * 4 * 4)
        # incremental=0 at the new size: a resized desktop has no valid
        # previous contents to be incremental against.
        self.assertEqual(sent, [struct.pack("!BBHHHH", 3, 0, 0, 0, 8, 4)])

    def test_an_unsupported_encoding_is_an_error_not_a_silent_skip(self):
        c = _client(1, 1)
        c._recvn = _feeder(b"\x00" + struct.pack("!H", 1)
                           + struct.pack("!HHHHi", 0, 0, 1, 1, 16))
        with self.assertRaises(ConnectionError):
            c._framebuffer_update()


class SnapshotIsConsistent(unittest.TestCase):

    def test_snapshot_reports_size_bytes_and_the_update_it_belongs_to(self):
        c = _client(1, 1)
        c._recvn = _feeder(b"\x00" + struct.pack("!H", 1)
                           + struct.pack("!HHHHi", 0, 0, 1, 1, 0)
                           + bytes([4, 5, 6, 0]))
        c._framebuffer_update()
        width, height, frame, completed_ns, sequence = c.snapshot()
        self.assertEqual((width, height), (1, 1))
        self.assertEqual(frame, bytes([4, 5, 6, 0]))
        self.assertEqual(sequence, 1)
        self.assertEqual(completed_ns, c.last_update_ns)


class TheModuleHasNoUiInIt(unittest.TestCase):
    """Spec 2026-08-19 §7: 'pure protocol, no tkinter, no Pillow'.

    Checked in a SUBPROCESS on purpose: another test in this suite may have
    already imported tkinter or PIL into this interpreter, which would mask a
    real regression in an in-process sys.modules check.
    """

    def test_importing_rfb_pulls_in_neither_tkinter_nor_pil(self):
        code = (
            "import sys, json\n"
            "import omnidroid.rfb\n"
            "bad = sorted(m for m in sys.modules\n"
            "             if m.split('.')[0] in ('tkinter', '_tkinter', 'PIL'))\n"
            "print(json.dumps(bad))\n"
        )
        env = dict(os.environ, PYTHONPATH=ROOT)
        proc = subprocess.run([sys.executable, "-c", code], cwd=ROOT, env=env,
                              capture_output=True, text=True, timeout=120)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(json.loads(proc.stdout.strip()), [],
                         "rfb.py must import on a host with no GUI toolkit "
                         "and no imaging library")


if __name__ == "__main__":
    unittest.main(verbosity=2)
```

- [ ] **Step 2: Run test to verify it fails**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_rfb.py -q
```
Expected: collection error — `ModuleNotFoundError: No module named 'omnidroid.rfb'`.

- [ ] **Step 3: Write the implementation — `omnidroid/rfb.py`**

This is a **move, not a rewrite**. Everything from `class RFBClient` down is `omnidroid/vncview.py` lines 55–284 byte-for-byte, with exactly one edit: the class docstring's "that a Tk front-end reads" becomes "that a recorder or a viewer reads", because there is no Tk front-end any more. Do not reformat, do not "improve", do not renumber the comments — the `_set_pixel_format` comment block is a measured result and its wording is the record of that measurement.

Create `omnidroid/rfb.py`:

```python
#!/usr/bin/env python3
"""omnidroid's RFB (VNC) protocol client — protocol only, no UI.

A minimal RFB 3.x client against a QEMU built-in VNC server on 127.0.0.1 (no
auth — safe ONLY on the loopback bind, per the port-scheme hard rule). It
maintains an RGBX framebuffer and offers:

  - Raw + CopyRect + DesktopSize pseudo-encoding,
  - pointer and key events (X11 keysyms) toward the guest,
  - ``on_frame(width, height, bgrx, completed_ns, sequence)``, fired from the
    receive loop for every COMPLETED update with an immutable copy of the
    framebuffer.

STDLIB ONLY, ON PURPOSE. This module is what ``omnidroid/capture.py`` — and
through it ``screenshot``, ``capture`` and autocap — is built on, so it has to
import on a host with no GUI toolkit and no imaging library: no tkinter, no
Pillow, no window handles. It was extracted from ``vncview.py``, whose other
half was a Tk window; that half is deleted, because the product's viewer is
QEMU's own window (design 2026-08-19 §7).

Threading: ``run()`` is the receive loop and belongs on its own thread. Sends
(input events and update requests) are serialised by ``_wlock``, so a caller
may inject pointer/key events from another thread while the loop runs.
"""
import socket
import struct
import threading
import time

# ---------- RFB protocol constants ----------
_SET_PIXEL_FORMAT = 0
_SET_ENCODINGS = 2
_FB_UPDATE_REQUEST = 3
_KEY_EVENT = 4
_POINTER_EVENT = 5

_ENC_RAW = 0
_ENC_COPYRECT = 1
_ENC_DESKTOPSIZE = -223


class RFBClient:
    """Minimal RFB 3.x client. Maintains an RGBX framebuffer bytearray that a
    recorder or a viewer reads. Thread model: recv loop in its own thread; sends
    (input + update requests) guarded by _wlock."""

    def __init__(self, host, port, on_frame=None):
        self.host, self.port = host, port
        self.sock = None
        self.width = self.height = 0
        self.fb = bytearray()          # width*height*4, RGBX
        self.name = ""
        self._wlock = threading.Lock()
        # The viewer is allowed to coalesce redraw notifications (`dirty`),
        # but recorders are not: a state which exists for one completed RFB
        # update must be observable before the backing framebuffer is changed
        # by the next update.  `on_frame` therefore receives an immutable copy
        # of every completed update directly from the receive loop, timestamped
        # with the host's monotonic high-resolution clock.
        self._fblock = threading.Lock()
        self.on_frame = on_frame
        self.update_count = 0
        self.last_update_ns = None
        self.dirty = threading.Event()
        self.closed = threading.Event()
        self.error = None

    # --- low-level io ---
    def _recvn(self, n):
        buf = bytearray()
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("server closed connection")
            buf += chunk
        return bytes(buf)

    def _send(self, data):
        with self._wlock:
            self.sock.sendall(data)

    # --- handshake ---
    def connect(self):
        self.sock = socket.create_connection((self.host, self.port),
                                             timeout=10)
        self.sock.settimeout(None)
        server_ver = self._recvn(12)          # e.g. b"RFB 003.008\n"
        try:
            major, minor = int(server_ver[4:7]), int(server_ver[8:11])
        except ValueError:
            major, minor = 3, 8
        proto = b"RFB 003.008\n" if (major, minor) >= (3, 8) else \
            (b"RFB 003.007\n" if (major, minor) >= (3, 7) else b"RFB 003.003\n")
        self.sock.sendall(proto)
        self._security(proto)
        self.sock.sendall(struct.pack("!B", 1))     # ClientInit: shared=1
        self._server_init()
        self._set_pixel_format()
        self._set_encodings([_ENC_RAW, _ENC_COPYRECT, _ENC_DESKTOPSIZE])
        self.request_update(incremental=False)

    def _security(self, proto):
        if proto >= b"RFB 003.007\n":
            n = self._recvn(1)[0]
            if n == 0:                          # failure
                reason = self._recvn(struct.unpack("!I", self._recvn(4))[0])
                raise ConnectionError(f"VNC security failed: {reason!r}")
            types = self._recvn(n)
            if 1 not in types:                  # 1 = None
                raise ConnectionError(
                    "server requires VNC auth/password; this client only "
                    "supports no-auth localhost servers (type 'None'). "
                    f"offered: {list(types)}")
            self.sock.sendall(struct.pack("!B", 1))
        else:                                    # RFB 3.3: server dictates
            sec = struct.unpack("!I", self._recvn(4))[0]
            if sec != 1:
                raise ConnectionError(f"server security type {sec} "
                                      "unsupported (need None)")
        # SecurityResult (present for None in 3.8; in 3.3 too)
        if proto >= b"RFB 003.008\n":
            res = struct.unpack("!I", self._recvn(4))[0]
            if res != 0:
                raise ConnectionError("VNC auth rejected")

    def _server_init(self):
        w, h = struct.unpack("!HH", self._recvn(4))
        self._recvn(16)                          # server pixel format (ignored)
        nlen = struct.unpack("!I", self._recvn(4))[0]
        self.name = self._recvn(nlen).decode("latin-1", "replace")
        self._resize(w, h)

    def _resize(self, w, h):
        self.width, self.height = w, h
        self.fb = bytearray(w * h * 4)           # RGBX, opaque black

    def snapshot(self):
        """Return a consistent immutable framebuffer snapshot.

        Shape: ``(width, height, bgrx_bytes, completed_ns, sequence)``.
        ``completed_ns`` uses :func:`time.perf_counter_ns`; it is the time the
        most recent RFB framebuffer update completed on the host, not a guest
        wall-clock or an assertion that the display updates at 1 kHz.
        """
        with self._fblock:
            return (self.width, self.height, bytes(self.fb),
                    self.last_update_ns, self.update_count)

    def _set_pixel_format(self):
        # 32bpp, depth 24, little-endian, true-colour. Do NOT change these shift
        # values without re-running the ground-truth check below — QEMU DOES
        # honour this request, so the shifts and the frame decoder are a matched
        # pair. This exact request (16/8/0) combined with a PIL "RGBX" decode was
        # VERIFIED against `adb screencap` ground truth: exact match (mean channel
        # diff 0.00). The old code paired the SAME request with a "BGRX" decode,
        # which is R/B-swapped (diff ~23) — the "red looks purple / colours a bit
        # off" bug. (Changing these shifts to 0/8/16 was measured to make QEMU
        # emit a different byte order that RGBX then decodes WRONG, i.e. QEMU is
        # honouring the request, not ignoring it — hence "leave the shifts, fix
        # the decoder".)
        pf = struct.pack("!BBBB HHH BBB xxx",
                         32, 24, 0, 1, 255, 255, 255, 16, 8, 0)
        self._send(struct.pack("!Bxxx", _SET_PIXEL_FORMAT) + pf)

    def _set_encodings(self, encs):
        msg = struct.pack("!BxH", _SET_ENCODINGS, len(encs))
        msg += b"".join(struct.pack("!i", e) for e in encs)
        self._send(msg)

    # --- client -> server messages ---
    def request_update(self, incremental=True):
        self._send(struct.pack("!BBHHHH", _FB_UPDATE_REQUEST,
                               1 if incremental else 0,
                               0, 0, self.width, self.height))

    def pointer(self, button_mask, x, y):
        x = max(0, min(x, self.width - 1))
        y = max(0, min(y, self.height - 1))
        self._send(struct.pack("!BBHH", _POINTER_EVENT, button_mask & 0xFF,
                               x, y))

    def key(self, keysym, down):
        self._send(struct.pack("!BBHI", _KEY_EVENT, 1 if down else 0, 0,
                               keysym & 0xFFFFFFFF))

    # --- server -> client loop ---
    def run(self):
        try:
            while not self.closed.is_set():
                msg_type = self._recvn(1)[0]
                if msg_type == 0:
                    self._framebuffer_update()
                    self.request_update(incremental=True)
                elif msg_type == 1:              # SetColourMapEntries
                    self._recvn(3)
                    n = struct.unpack("!H", self._recvn(2))[0]
                    self._recvn(n * 6)
                elif msg_type == 2:              # Bell
                    pass
                elif msg_type == 3:              # ServerCutText
                    self._recvn(3)
                    n = struct.unpack("!I", self._recvn(4))[0]
                    self._recvn(n)
                else:
                    raise ConnectionError(f"unknown server msg {msg_type}")
        except Exception as e:                   # noqa: BLE001
            if not self.closed.is_set():
                self.error = e
        finally:
            self.closed.set()
            try:
                self.sock.close()
            except Exception:
                pass

    def _framebuffer_update(self):
        with self._fblock:
            self._recvn(1)                       # padding
            nrects = struct.unpack("!H", self._recvn(2))[0]
            for _ in range(nrects):
                x, y, w, h, enc = struct.unpack("!HHHHi", self._recvn(12))
                if enc == _ENC_RAW:
                    self._raw_rect(x, y, w, h)
                elif enc == _ENC_COPYRECT:
                    sx, sy = struct.unpack("!HH", self._recvn(4))
                    self._copy_rect(x, y, w, h, sx, sy)
                elif enc == _ENC_DESKTOPSIZE:
                    self._resize(w, h)
                    self.request_update(incremental=False)
                    break
                else:
                    raise ConnectionError(f"unsupported encoding {enc}")
            completed_ns = time.perf_counter_ns()
            self.update_count += 1
            self.last_update_ns = completed_ns
            sequence = self.update_count
            width, height = self.width, self.height
            # Copy under the framebuffer lock.  The callback can enqueue this
            # immutable value and return quickly; unlike Event-based redraws,
            # two consecutive updates can never collapse into one frame.
            frame = bytes(self.fb) if self.on_frame else None
        self.dirty.set()
        if self.on_frame:
            self.on_frame(width, height, frame, completed_ns, sequence)

    def _raw_rect(self, x, y, w, h, ):
        data = self._recvn(w * h * 4)
        row = w * 4
        fbw = self.width * 4
        mv = memoryview(self.fb)
        for r in range(h):
            dst = (y + r) * fbw + x * 4
            mv[dst:dst + row] = data[r * row:(r + 1) * row]

    def _copy_rect(self, x, y, w, h, sx, sy):
        fbw = self.width * 4
        row = w * 4
        src = bytearray(self.fb)                 # snapshot (regions may overlap)
        mv = memoryview(self.fb)
        rows = range(h - 1, -1, -1) if sy < y else range(h)
        for r in rows:
            s = (sy + r) * fbw + sx * 4
            d = (y + r) * fbw + x * 4
            mv[d:d + row] = src[s:s + row]

    def close(self):
        self.closed.set()
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except Exception:
            pass
```

- [ ] **Step 4: Run test to verify it passes**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_rfb.py -q
```
Expected: PASS, 11 passed.

- [ ] **Step 5: Prove the move was faithful**

The point of a verbatim move is that the two protocol implementations are the same one. Diff the class bodies:

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
sed -n '55,285p' omnidroid/vncview.py > /tmp/old-rfb.txt
sed -n '/^class RFBClient:/,$p' omnidroid/rfb.py > /tmp/new-rfb.txt
diff /tmp/old-rfb.txt /tmp/new-rfb.txt
```
Expected: exactly two differing lines — the class docstring's "that a Tk front-end reads" → "that a recorder or a viewer reads", and the `_security` message's "this viewer only" → "this client only". Nothing else. If anything else differs, you edited the protocol; put it back.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/rfb.py tests/test_rfb.py
git commit -m "refactor: extract the RFB protocol client into omnidroid/rfb.py

vncview.py is a protocol client welded to a Tk window. The protocol half is
what screenshot/capture/autocap are built on and stays; the Tk half is being
deleted. rfb.py imports socket/struct/threading/time and nothing else, which
tests/test_rfb.py checks in a subprocess so an already-imported tkinter cannot
mask a regression."
```

---

### Task 2: Move `capture.py` onto `rfb`, and retarget the pixel-format test

**Files:**
- Modify: `omnidroid/capture.py:1-45` (module docstring + import), `omnidroid/capture.py:84`, `omnidroid/capture.py:280`
- Modify: `tests/test_pixel_format.py:51-70`
- Test: `tests/test_pixel_format.py`, `tests/test_keyframe_thresholds.py`, `tests/test_rfb.py`

**Interfaces:**
- Consumes: `omnidroid.rfb.RFBClient(host, port, on_frame=...)` with `.connect()`, `.run()`, `.close()`, `.closed`, `.error`, and the `on_frame(width, height, bgrx, completed_ns, sequence)` callback — all from Task 1.
- Produces: `omnidroid/capture.py` no longer references `vncview` anywhere. `run_capture(...)` keeps its exact signature and return shape (`metadata` dict) — nothing downstream of it changes.

`omnidroid/vncview.py` is deliberately left on disk by this task. Task 3 deletes it, once the engine has stopped calling it too. That way this commit is a pure, revertible repointing and the suite is green at both ends of it.

- [ ] **Step 1: Write the failing test — retarget the surviving assertion at `rfb`**

`tests/test_pixel_format.py` is KEPT, not deleted: it pins the SetPixelFormat shifts that the RGBX decode depends on, and that pairing was measured against `adb screencap` ground truth. Replace `test_request_shifts_match_the_decoder` (lines 51–70) with:

```python
    def test_request_shifts_match_the_decoder(self):
        """rfb requests the exact shifts that make QEMU emit what the RGBX
        decoder expects. Measured: changing these makes QEMU emit a different
        byte order the decoder then gets wrong, so request and decode are a
        matched pair — this guards the request half."""
        from omnidroid import rfb
        sent = {}
        client = rfb.RFBClient.__new__(rfb.RFBClient)
        client._send = lambda data: sent.setdefault("pf", data)
        client._wlock = None
        rfb.RFBClient._set_pixel_format(client)
        pf = sent["pf"]
        # message: type(1) + pad(3) + PIXEL_FORMAT(16)
        self.assertEqual(len(pf), 20)
        bpp, depth, big_endian, true_colour = struct.unpack_from("!BBBB", pf, 4)
        r_max, g_max, b_max = struct.unpack_from("!HHH", pf, 8)
        r_shift, g_shift, b_shift = struct.unpack_from("!BBB", pf, 14)
        self.assertEqual((bpp, depth, true_colour), (32, 24, 1))
        self.assertEqual((r_max, g_max, b_max), (255, 255, 255))
        self.assertEqual((r_shift, g_shift, b_shift), (16, 8, 0))
```

And add, in the same class, the test that pins the repointing itself:

```python
    def test_capture_reads_the_framebuffer_through_rfb_not_vncview(self):
        """capture.py used to import the Tk viewer module to get at its
        protocol client. The viewer is gone; the protocol client is not."""
        from omnidroid import capture as _capture
        from omnidroid import rfb
        self.assertIs(_capture.rfb, rfb)
        self.assertFalse(hasattr(_capture, "vncview"))
```

- [ ] **Step 2: Run test to verify it fails**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_pixel_format.py -q
```
Expected: `test_capture_reads_the_framebuffer_through_rfb_not_vncview` FAILS with `AttributeError: module 'omnidroid.capture' has no attribute 'rfb'`. `test_request_shifts_match_the_decoder` should already pass (it now targets `rfb`, which exists).

- [ ] **Step 3: Repoint `omnidroid/capture.py`**

Three edits. First, the module docstring — replace line 6 and lines 32–34:

```python
through :class:`vncview.RFBClient`'s ``on_frame`` hook. Each update carries the
```
becomes
```python
through :class:`rfb.RFBClient`'s ``on_frame`` hook. Each update carries the
```

and
```python
This module is deliberately self-contained (stdlib + Pillow + the sibling
vncview) so it works identically from a source checkout and the frozen exe.
"""
```
becomes
```python
This module is deliberately self-contained (stdlib + Pillow + the sibling
rfb) so it works identically from a source checkout and the frozen exe.
"""
```

Second, the import at lines 43–45:

```python
# vncview is a sibling module (omnidroid/); the frozen exe adds --hidden-import
# vncview, and a checkout runs with the repo root on sys.path (see omni.py).
from omnidroid import vncview
```
becomes
```python
# rfb is a sibling module (omnidroid/) holding the RFB protocol client that
# used to live inside the Tk viewer. It imports stdlib only, so this module's
# only optional dependency is Pillow, and only for the decode/diff below.
from omnidroid import rfb
```

Third, line 84 (`_lazy_pil`'s docstring) and line 280:

```python
    """Import Pillow lazily with a clear message (mirrors vncview.run_viewer)."""
```
becomes
```python
    """Import Pillow lazily so importing this module never needs it."""
```

```python
    client = vncview.RFBClient(host, port, on_frame=sink.on_frame)
```
becomes
```python
    client = rfb.RFBClient(host, port, on_frame=sink.on_frame)
```

- [ ] **Step 4: Run test to verify it passes**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_pixel_format.py tests/test_keyframe_thresholds.py tests/test_rfb.py -q
```
Expected: PASS, all three files green. `test_keyframe_thresholds.py` exercises `capture.KeyframeSelector` on top of the same module and must not have moved.

- [ ] **Step 5: Confirm nothing else in the package still reaches for `vncview` at runtime**

```bash
grep -rn "vncview" omnidroid/ --include=*.py
```
Expected: matches only inside `omnidroid/vncview.py` itself, plus `omnidroid/engine.py` (`_spawn_builtin_viewer`, `_run_vncview`, the `_vncview` parser, `cmd_view`'s docstring) and `omnidroid/config.py:151`. Task 3 removes all of those. If `capture.py` still appears, Step 3 was incomplete.

- [ ] **Step 6: Commit**

```bash
git add omnidroid/capture.py tests/test_pixel_format.py
git commit -m "refactor: capture reads the framebuffer through omnidroid.rfb

Same protocol client, no longer imported by way of the Tk viewer module.
capture/screenshot/autocap behaviour is unchanged; test_pixel_format still
pins the SetPixelFormat shifts (16/8/0) the RGBX decode was measured against."
```

---

### Task 3: Delete the built-in Tk viewer — engine, subcommand, and `vncview.py`

**Files:**
- Delete: `omnidroid/vncview.py`
- Modify: `omnidroid/engine.py` — `cmd_start`'s viewer block (~2249-2266), `_spawn_builtin_viewer` (6609-6637), `cmd_view` docstring + viewer branch (7113-7122, 7275-7297), `_run_vncview` (7326-7330), `_spawn_pool_manager` docstring (~10800), the `_vncview` parser (12060-12066)
- Modify: `omnidroid/config.py:151`
- Modify: `tests/engine_public_names.json` (drop lines 148 and 156)
- Modify: `tests/test_self_argv_prefix.py` (delete `SpawnedViewerCommand`, lines 47-97; rewrite the module docstring)
- Modify: `tests/test_session.py:466`
- Modify: `tests/test_hidden_window_viewer.py:496-497`
- Create: `tests/test_native_viewer_only.py`
- Create: `tests/test_tkinter_is_gone.py`

**Interfaces:**
- Consumes: `engine._vnc_viewer_command(host, port, viewer=None) -> (argv_list, shell_bool) | None` (already exists, `engine.py:6562`); `engine.fail(code, message)` from `omnidroid.output`, which raises `SystemExit` after emitting `{"ok": false, "error": code, "message": msg}` in `--json` mode.
- Produces:
  - `engine._spawn_builtin_viewer`, `engine._run_vncview` and the `_vncview` subcommand no longer exist.
  - `cmd_view` on a framebuffer boot emits `{"name", "vnc_host", "vnc_port", "viewer", "viewer_pid", "started", "ok"}` exactly as before — only the value of `viewer` changes, from `"built-in (Tk+RFB)"` to the resolved client's argv[0].
  - New typed error code `"no_vnc_client"` from `cmd_view`.
  - `cmd_start --json` no longer emits a `viewer_pid` key.
  - `tests/test_tkinter_is_gone.py` exists with a `PKG` constant (`pathlib.Path(__file__).resolve().parents[1] / "omnidroid"`) and a `_subcommands()` helper; Tasks 4, 5 and 7 add classes to it.

- [ ] **Step 1: Write the failing tests — the behaviour**

Create `tests/test_native_viewer_only.py`:

```python
#!/usr/bin/env python3
"""What `view` and `start` do now that there is no viewer of ours.

The product's viewer is QEMU's own window. A boot that has one is served by
cmd_view's window branch (tests/test_hidden_window_viewer.py). A boot whose
pixels live in the VNC framebuffer instead -- farming, or a gaming boot that
degraded to software -- used to get a Tk+RFB window spawned out of
`omnidroid/vncview.py`. That is deleted (design 2026-08-19 §7), so `view` now
hands the port to an OS/native client and says so honestly when the host has
none. `start` opens nothing at all and names the port in its success line.

    python3 -m pytest tests/test_native_viewer_only.py -q
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import engine  # noqa: E402

FRAMEBUFFER_BOOT = {"identity": "omni-farm3", "pid": 4242,
                    "native_window": False, "gpu": "software",
                    "display_kind": "vnc"}


def _args(**kw):
    defaults = {"name": "farm3", "start": False, "native": False,
                "json": False, "debug": False, "mode": None, "offset": None,
                "timeout": 60, "hide": False, "viewer": None}
    defaults.update(kw)
    return type("Args", (), defaults)()


class ViewOpensANativeClient(unittest.TestCase):

    def _view(self, resolved, **kw):
        """cmd_view against a framebuffer boot, with the client resolver
        pinned. Returns (popen_argv_or_None, emitted_json, raised)."""
        spawned = {}
        emitted = {}

        class FakeProc:
            pid = 5150

        def fake_popen(argv, **kwargs):
            spawned["argv"] = argv
            spawned["kwargs"] = kwargs
            return FakeProc()

        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine._run_record",
                        return_value=FRAMEBUFFER_BOOT), \
             mock.patch("omnidroid.engine.boot_shows_in_a_window",
                        return_value=False), \
             mock.patch("omnidroid.engine.vnc_unavailable_reason",
                        return_value=None), \
             mock.patch("omnidroid.engine._port_open", return_value=True), \
             mock.patch("omnidroid.engine._vnc_viewer_command",
                        return_value=resolved), \
             mock.patch.object(engine.subprocess, "Popen", fake_popen), \
             mock.patch("omnidroid.engine.emit_json",
                        side_effect=lambda d: emitted.update(d)):
            try:
                engine.cmd_view(_args(**kw))
                raised = None
            except SystemExit as e:
                raised = e
        return spawned.get("argv"), emitted, raised

    def test_a_framebuffer_boot_launches_the_resolved_client(self):
        argv, emitted, raised = self._view(
            (["vncviewer", "127.0.0.1::18001"], False), json=True)
        self.assertIsNone(raised)
        self.assertEqual(argv, ["vncviewer", "127.0.0.1::18001"])
        self.assertEqual(emitted["viewer"], "vncviewer")
        self.assertEqual(emitted["viewer_pid"], 5150)
        self.assertEqual(emitted["vnc_port"], 18001)
        self.assertTrue(emitted["ok"])

    def test_no_client_on_this_host_is_a_typed_error_not_a_silent_nothing(self):
        codes = []
        with mock.patch("omnidroid.engine.fail",
                        side_effect=lambda code, msg=None, **k:
                            codes.append(code) or (_ for _ in ()).throw(
                                SystemExit(1))):
            argv, _emitted, raised = self._view(None)
        self.assertIsNone(argv, "nothing may be launched when nothing resolved")
        self.assertEqual(codes, ["no_vnc_client"])
        self.assertIsInstance(raised, SystemExit)

    def test_the_explicit_viewer_template_still_reaches_the_resolver(self):
        seen = {}

        def resolver(host, port, viewer=None):
            seen["viewer"] = viewer
            return ["myclient", f"{host}:{port}"], False

        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine._run_record",
                        return_value=FRAMEBUFFER_BOOT), \
             mock.patch("omnidroid.engine.boot_shows_in_a_window",
                        return_value=False), \
             mock.patch("omnidroid.engine.vnc_unavailable_reason",
                        return_value=None), \
             mock.patch("omnidroid.engine._port_open", return_value=True), \
             mock.patch("omnidroid.engine._vnc_viewer_command",
                        side_effect=resolver), \
             mock.patch.object(engine.subprocess, "Popen",
                               return_value=mock.Mock(pid=1)):
            engine.cmd_view(_args(viewer="myclient {host}:{port}"))
        self.assertEqual(seen["viewer"], "myclient {host}:{port}")


class StartOpensNoViewer(unittest.TestCase):
    """`start` used to spawn the Tk viewer for any interactive boot without a
    native window. There is no such viewer; the success line already names the
    VNC port, which is what `omnidroid view` and `capture` attach to."""

    def test_the_engine_has_no_viewer_to_spawn(self):
        self.assertFalse(hasattr(engine, "_spawn_builtin_viewer"))

    def test_the_viewer_decision_predicate_survives_for_the_message(self):
        # --window / --no-window are part of the CLI contract omni-executor
        # builds argv against; the predicate that reads them stays.
        self.assertTrue(engine._want_vnc_viewer(
            native_window=False, explicit_window=False, json_mode=False,
            no_window=False))
        self.assertFalse(engine._want_vnc_viewer(
            native_window=False, explicit_window=False, json_mode=False,
            no_window=True))


if __name__ == "__main__":
    unittest.main(verbosity=2)
```

Create `tests/test_tkinter_is_gone.py`:

```python
#!/usr/bin/env python3
"""The Tk viewer and the Tk title bar are gone, and stay gone.

Design 2026-08-19 §7: the product's viewer is QEMU's own customised window.
The Tk+RFB viewer (`vncview.run_viewer`) and the Tk title bar (`windowbar.py`)
were the outdated way, and `windowbar` had had zero production callers since
`cmd_view` hardcoded `bar_ok = False`. A dead UI path is worse than no path:
the next person reads it as live.

These are absence tests. They fail loudly if any of it comes back by accident
-- a merge, a revert, or a copy-paste from an old branch.

    python3 -m pytest tests/test_tkinter_is_gone.py -q
"""
import os
import pathlib
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import engine  # noqa: E402

PKG = pathlib.Path(__file__).resolve().parents[1] / "omnidroid"


def _subcommands():
    """Every subcommand the parser accepts, hidden ones included."""
    parser = engine.build_parser()
    for action in parser._actions:
        if getattr(action, "dest", None) == "cmd" and action.choices:
            return set(action.choices)
    raise AssertionError("no subcommand action on the parser")


class TheTkViewerIsGone(unittest.TestCase):

    def test_the_vncview_module_does_not_exist(self):
        self.assertFalse((PKG / "vncview.py").exists())

    def test_the_engine_cannot_spawn_or_run_it(self):
        for name in ("_spawn_builtin_viewer", "_run_vncview"):
            with self.subTest(name=name):
                self.assertFalse(hasattr(engine, name))

    def test_the_hidden_vncview_subcommand_is_unregistered(self):
        self.assertNotIn("_vncview", _subcommands())

    def test_the_window_lock_subcommand_is_untouched(self):
        # NOT tkinter, and NOT this sub-project's to delete: `_windowlock` is
        # hostwin.aspect_lock (ctypes) and belongs to sub-project E. If this
        # fails, the two plans have collided.
        self.assertIn("_windowlock", _subcommands())


if __name__ == "__main__":
    unittest.main(verbosity=2)
```

- [ ] **Step 2: Run tests to verify they fail**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_native_viewer_only.py tests/test_tkinter_is_gone.py -q
```
Expected: FAIL. `test_a_framebuffer_boot_launches_the_resolved_client` fails because `cmd_view` still calls `_spawn_builtin_viewer`; `test_the_vncview_module_does_not_exist`, `test_the_engine_cannot_spawn_or_run_it` and `test_the_hidden_vncview_subcommand_is_unregistered` all fail on things that still exist. `test_the_window_lock_subcommand_is_untouched` passes now and must keep passing.

- [ ] **Step 3: Delete `_spawn_builtin_viewer` from `omnidroid/engine.py`**

Delete the whole function, `engine.py:6609-6637` — from `def _spawn_builtin_viewer(name, host, port, title):` down to and including its `return subprocess.Popen(cmd, **kwargs)`, plus the blank lines separating it from `_spawn_window_bar` below.

- [ ] **Step 4: Rewrite `cmd_start`'s viewer block**

At `engine.py:2246-2266`, replace:

```python
    # Open a live WINDOW onto this instance so you can watch/play it, and so two
    # `omnidroid start` runs give two accounts side by side. Each viewer is its own
    # detached process bound to this instance's own VNC port, so N windows for N
    # accounts just work. Default ON for interactive use; suppressed by
    # --no-window and by --json (a machine/automation caller drives via capture).
    want_window = _want_vnc_viewer(
        native_window=_booted_with_native_window(args.name),
        explicit_window=getattr(args, "window", False),
        json_mode=json_mode,
        no_window=getattr(args, "no_window", False))
    viewer_pid = None
    if result["ok"] and want_window:
        try:
            if _wait_for_vnc("127.0.0.1", acct["vnc_port"], timeout=10):
                title = (f"omni: {args.name}  (place {sess['place_id']})"
                         if is_join else f"omni: {args.name}  (home)")
                viewer_pid = _spawn_builtin_viewer(
                    args.name, "127.0.0.1", acct["vnc_port"], title).pid
                result["viewer_pid"] = viewer_pid
        except Exception as e:  # noqa: BLE001 — a window failure must not fail start
            print(f"[{label}] could not open a window: {e}")
```

with:

```python
    # There is no viewer of OURS to open any more. The product's viewer is
    # QEMU's own window -- a watched (gaming) boot puts it on screen at spawn
    # (qemu_proc.place_window) -- and a boot that has no window renders into
    # the VNC framebuffer, which `omnidroid view` hands to a native client and
    # which `capture`/`screenshot` read directly. The Tk+RFB window that used
    # to open here is deleted (design 2026-08-19 §7); its RFB half lives on in
    # omnidroid/rfb.py, where the recorders use it.
    #
    # The predicate stays because --window/--no-window stay: it now decides
    # whether to NAME the screen in the success line below, which is the only
    # thing left that "put a window on my screen" can honestly mean here.
    want_window = _want_vnc_viewer(
        native_window=_booted_with_native_window(args.name),
        explicit_window=getattr(args, "window", False),
        json_mode=json_mode,
        no_window=getattr(args, "no_window", False))
    if result["ok"] and want_window:
        result["vnc"] = f"127.0.0.1:{acct['vnc_port']}"
```

Then, at the success-line block just below (`engine.py:2282-2284`), replace:

```python
        where = (f"window opened (pid {viewer_pid})" if viewer_pid
                 else f"vnc 127.0.0.1:{acct['vnc_port']} to watch")
```

with:

```python
        where = (f"vnc 127.0.0.1:{acct['vnc_port']} to watch "
                 f"(`omnidroid view {args.name}`)")
```

Also update `_want_vnc_viewer`'s docstring at `engine.py:569-581` — its first line currently says "Whether `omnidroid start` should also spawn the built-in Tk/RFB viewer.". Replace the whole docstring with:

```python
    """Whether `omnidroid start` should point at this instance's VNC screen.

    The instance always RUNS a VNC server — screenshot, autocap and the
    omnidroid-input skill attach to it in every mode. This decides only
    whether `start` says so, and the gaming case is why it exists: a boot with
    a native QEMU window already HAS its screen on the user's desktop, so
    naming a framebuffer port beside it points at the wrong one of the two.

    (It used to gate spawning a Tk+RFB viewer window here. That viewer is
    deleted; the flags it read are part of the CLI contract omni-executor
    builds argv against, so the predicate stays and its answer is now a line
    of output rather than a process.)

    Order matters: --no-window is absolute, then an explicit --window (the
    user asked; do not second-guess), then the native window stands it down,
    then today's rule (on interactively, off under --json)."""
```

- [ ] **Step 5: Rewrite `cmd_view`'s docstring and its framebuffer branch**

At `engine.py:7113-7122`, replace the first paragraph of `cmd_view`'s docstring:

```python
    """Open a LIVE window onto an instance — real-time screen with mouse and
    keyboard control — launched straight from the terminal.

    Default: the SELF-CONTAINED Python viewer (manager/vncview.py: Tk + a
    minimal RFB client) — identical on Windows/macOS/Linux, no OS
    screen-sharing app. `--native` instead launches the OS/native VNC client
    (macOS Screen Sharing, or --viewer/config qemu.vnc_viewer, or a client on
    PATH). Instance must be running; --start boots it first and waits for the
    VNC port. Localhost-only: the viewer connects to 127.0.0.1 — the server
    has no auth, safe ONLY on the loopback bind (port-scheme HARD RULE)."""
```

with:

```python
    """Open a LIVE window onto an instance — real-time screen with mouse and
    keyboard control — launched straight from the terminal.

    Two paths, chosen by where the instance's pixels actually are:

      * the pixels are in a WINDOW (every GPU boot): QEMU's own window is the
        window. It is restyled, shown, and brought forward. Nothing is copied,
        encoded or decoded per frame, and input goes straight into the guest's
        usb-tablet. `--native` opts out of this path.
      * the pixels are in a VNC FRAMEBUFFER (farming, or a gaming boot that
        degraded to software): an OS/native VNC client is launched against the
        port — macOS Screen Sharing, `--viewer`/config `qemu.vnc_viewer`, or a
        client on PATH. If the host has none, this says so (`no_vnc_client`)
        rather than pretending; `screenshot` and `capture` read the same
        framebuffer without a client.

    Instance must be running; --start boots it first and waits for the VNC
    port. Localhost-only: the client connects to 127.0.0.1 — the server has no
    auth, safe ONLY on the loopback bind (port-scheme HARD RULE)."""
```

Then, at `engine.py:7274-7288`, replace:

```python
    title = f"omni: {args.name}  ({host}:{port})"
    use_native = getattr(args, "native", False) or args.viewer \
        or cfg.get("qemu", {}).get("vnc_viewer")
    if use_native:
        resolved = _vnc_viewer_command(host, port, viewer=args.viewer)
        if not resolved:
            sys.exit("error: no native VNC client found. Drop --native to use "
                     "the built-in viewer, or set --viewer 'client "
                     "{host}::{port}'. Screen is at " f"{host}:{port}.")
        argv, use_shell = resolved
        try:
            subprocess.Popen(argv, shell=use_shell)
        except Exception as e:
            sys.exit(f"error: failed to launch native viewer {argv}: {e}")
        viewer_desc, vpid = argv[0], None
    else:
        try:
            proc = _spawn_builtin_viewer(args.name, host, port, title)
        except Exception as e:
            sys.exit(f"error: failed to launch built-in viewer: {e}")
        viewer_desc, vpid = "built-in (Tk+RFB)", proc.pid
```

with:

```python
    # An OS/native client, always. There is no viewer of ours to fall back to:
    # the built-in Tk+RFB window is deleted (design 2026-08-19 §7). `--native`
    # is therefore not a switch between two viewers any more -- it is the
    # switch that sent us down this branch instead of QEMU's own window above.
    resolved = _vnc_viewer_command(host, port, viewer=args.viewer)
    if not resolved:
        # fail(), not sys.exit(): the app calls this through `view --json` and
        # a bare exit gives it a non-zero status with nothing to show.
        return fail(
            "no_vnc_client",
            f"'{args.name}' renders into a VNC framebuffer, and this host has "
            f"no VNC client to show it with. Its screen is at {host}:{port} — "
            f"point any client at it, or name one durably with `--viewer "
            f"'client {{host}}::{{port}}'` or config qemu.vnc_viewer. "
            f"`omnidroid screenshot {args.name}` and `omnidroid capture "
            f"{args.name}` read that same framebuffer with no client at all.")
    argv, use_shell = resolved
    try:
        proc = subprocess.Popen(argv, shell=use_shell)
    except Exception as e:
        sys.exit(f"error: failed to launch VNC client {argv}: {e}")
    viewer_desc, vpid = argv[0], proc.pid
```

- [ ] **Step 6: Delete `_run_vncview` and its parser, and fix two dangling comments**

Delete `engine.py:7326-7330` entirely:

```python
def _run_vncview(a):
    """Internal: run the built-in viewer in THIS process (invoked as the
    hidden `_vncview` subcommand by _spawn_builtin_viewer)."""
    from omnidroid import vncview
    return vncview.run_viewer(a.host, a.port, a.title)
```

Delete `engine.py:12060-12066` (the parser registration):

```python
    # Hidden internal: run the built-in Tk+RFB viewer in-process (spawned by
    # `omnidroid view` as a detached child). Not for direct use.
    vv = sub.add_parser("_vncview")
    vv.add_argument("--host", default="127.0.0.1")
    vv.add_argument("--port", type=int, required=True)
    vv.add_argument("--title", default=None)
    vv.set_defaults(func=lambda a: sys.exit(_run_vncview(a)))
```

In `_spawn_pool_manager`'s docstring (`engine.py:10800`), replace:

```python
    Same detached shape as the viewer/recorder (see _spawn_builtin_viewer),
    including self_argv_prefix() -- without it an embedding host relaunches
    its own GUI instead of the child."""
```
with (note: it must NOT name `_spawn_window_lock`, which sub-project E deletes):
```python
    Same detached shape as every other detached child in this file --
    self_argv_prefix(), DETACHED_PROCESS on Windows, stdio into a log --
    including self_argv_prefix() itself: without it an embedding host
    relaunches its own GUI instead of the child."""
```

In `omnidroid/config.py:148-151`, inside `self_argv_prefix`'s docstring, replace:

```python
    The engine re-invokes itself for detached children (the VNC viewer, the
    autocap recorder) as `[sys.executable, "<subcommand>", ...]`. That is
    correct for the standalone omnidroid.exe, whose frozen entry point IS the
    engine CLI. It is WRONG when the engine is embedded in a host app:
    omni-exec.exe's entry point is the GUI, which only routes to the engine
    when argv[1] is "--omnidroid". Without the prefix, `omni-exec.exe
    _vncview ...` falls through and launches a SECOND COPY OF THE GUI instead
    of the viewer -- which is exactly what clicking "Open viewer" did.
```
with:
```python
    The engine re-invokes itself for detached children (the autocap recorder,
    the pool manager) as `[sys.executable, "<subcommand>", ...]`. That is
    correct for the standalone omnidroid.exe, whose frozen entry point IS the
    engine CLI. It is WRONG when the engine is embedded in a host app:
    omni-exec.exe's entry point is the GUI, which only routes to the engine
    when argv[1] is "--omnidroid". Without the prefix, `omni-exec.exe
    capture ... --auto` falls through and launches A SECOND COPY OF THE GUI
    instead of the recorder -- which is exactly what every launch did.
```

- [ ] **Step 7: Delete `omnidroid/vncview.py` and drop the two facade names**

```bash
git rm omnidroid/vncview.py
```

In `tests/engine_public_names.json`, delete the two lines (and only those two — keep the JSON list valid, i.e. mind the trailing commas):

```
  "_run_vncview",
```
```
  "_spawn_builtin_viewer",
```

Verify the file still parses:
```bash
python -c "import json; d=json.load(open('tests/engine_public_names.json')); print(len(d)); assert '_run_vncview' not in d and '_spawn_builtin_viewer' not in d"
```

- [ ] **Step 8: Update the three tests that patch the deleted viewer**

In `tests/test_self_argv_prefix.py`, delete the whole `SpawnedViewerCommand` class (lines 47-97, from `class SpawnedViewerCommand(unittest.TestCase):` through `self.assertIn("18001", cmd)`). Then replace the module docstring (lines 1-14) with:

```python
# omnidroid/tests/test_self_argv_prefix.py
"""Re-invoking the frozen binary for detached children.

The engine spawns children by re-running itself -- the autocap recorder
(`capture ... --auto`) and the pool manager (`_poolmgr`). It built the command
as `[sys.executable, "<subcommand>", ...]`, which is correct for the standalone
omnidroid.exe -- its frozen entry point IS the engine CLI.

It is WRONG when the engine is embedded. omni-exec.exe's entry point is the
GUI, which routes to the engine only when argv[1] == "--omnidroid", so
`omni-exec.exe capture ... --auto` fell through to the GUI: every launch
silently spawned another copy of the GUI in place of the recorder.

(This file used to pin the same shape for two Tk children, `_vncview` and
`_windowbar`. Both are deleted -- design 2026-08-19 §7.)
"""
```

In `tests/test_session.py:466`, delete the patch line and fix the continuation on the line above it:

```python
             mock.patch.object(omni, "deliver_session",
                              return_value={"delivered": True}) as deliver_mock, \
             mock.patch.object(omni, "_spawn_builtin_viewer"):
```
becomes
```python
             mock.patch.object(omni, "deliver_session",
                              return_value={"delivered": True}) as deliver_mock:
```

In `tests/test_hidden_window_viewer.py:496-497`, inside `HideNeverOpensAViewer._view`, delete:

```python
             mock.patch("omnidroid.engine._spawn_builtin_viewer",
                        side_effect=lambda *a, **k: opened.append("vnc")), \
```

(The `_spawn_window_bar` patch two lines below stays for now; Task 4 removes it. `opened` still collects `"bar"` and `"show"`, so the assertions in that class are unaffected.)

- [ ] **Step 9: Run the new tests, then the full suite**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_native_viewer_only.py tests/test_tkinter_is_gone.py \
    tests/test_facade_equivalence.py tests/test_self_argv_prefix.py \
    tests/test_session.py tests/test_hidden_window_viewer.py \
    tests/test_contract_commands.py -q
```
Expected: PASS across all seven files.

Then the whole suite, diffed against the baseline:
```bash
cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/ -q 2>&1 | tee /tmp/omni-t3.txt
diff /tmp/omni-baseline-failed.txt <(grep '^FAILED' /tmp/omni-t3.txt | sort)
```
Expected: the diff is empty.

- [ ] **Step 10: Commit**

```bash
git add omnidroid/engine.py omnidroid/config.py omnidroid/vncview.py \
  tests/engine_public_names.json tests/test_self_argv_prefix.py \
  tests/test_session.py tests/test_hidden_window_viewer.py \
  tests/test_native_viewer_only.py tests/test_tkinter_is_gone.py
git commit -m "feat: delete the built-in Tk+RFB viewer

vncview.py, _spawn_builtin_viewer, _run_vncview and the hidden _vncview
subcommand are gone. A boot whose pixels live in the VNC framebuffer now gets
an OS/native client from _vnc_viewer_command, and a typed no_vnc_client error
when the host has none -- instead of a Tk window nobody asked for. `start`
names the VNC port rather than opening a second window onto it.

The protocol half survives as omnidroid/rfb.py, which capture/screenshot/
autocap use unchanged."
```

---

### Task 4: Delete `windowbar.py` and everything that launched it

**Files:**
- Delete: `omnidroid/windowbar.py`, `tests/test_windowbar.py`
- Modify: `omnidroid/engine.py` — `_spawn_window_bar` (6640-6667), `_run_windowbar` (7299-7323), the `_windowbar` parser (12068-12080)
- Modify: `tests/test_self_argv_prefix.py` (delete `SpawnedWindowBarCommand`, lines 100-171 as they stand after Task 3's edit)
- Modify: `tests/test_hidden_window_viewer.py` (the `_spawn_window_bar` patches at 81-83, 106, 210-211, 341, 498-499, and the whole `TheBarsPidFileTracksItsLifetime` methods that call `_run_windowbar`)
- Modify: `tests/test_tkinter_is_gone.py` (add a class)

**Interfaces:**
- Consumes: `PKG` and `_subcommands()` from `tests/test_tkinter_is_gone.py` (Task 3).
- Produces: `engine._spawn_window_bar` and `engine._run_windowbar` no longer exist; `_windowbar` is no longer a subcommand. `engine._persist_window_bar_geometry` and `engine._clear_window_bar_pid` still exist after this task — Task 5 removes them.

Spec §7 states the case: `windowbar.py` is 636 lines with 705 lines of tests, and `engine._spawn_window_bar` has had **zero production callers** since `cmd_view` hardcoded `bar_ok = False` (`engine.py:7239`). Verify that before deleting:

- [ ] **Step 1: Confirm there are no production callers**

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
grep -rn "_spawn_window_bar\|_window_bar_settled" omnidroid/
grep -n "bar_ok" omnidroid/engine.py
grep -rn "^from omnidroid import windowbar\|import windowbar" omnidroid/
```
Expected: `_spawn_window_bar` appears only as its own `def` (and inside `_running_window_bar_pid`'s prose); `_window_bar_settled` appears only as its own `def`; `bar_ok` appears exactly twice, as `bar_ok = False` and `"bar": bar_ok`; `windowbar` is imported only inside `_run_windowbar`. If any of these shows a real call site, **stop** and report it — the premise of this task is wrong and the plan needs revising.

- [ ] **Step 2: Write the failing test**

Append to `tests/test_tkinter_is_gone.py`, after `TheTkViewerIsGone`:

```python
class TheTkTitleBarIsGone(unittest.TestCase):
    """windowbar.py was 636 lines with 705 lines of tests and no production
    caller at all: cmd_view hardcoded `bar_ok = False`, so nothing ever
    spawned it. Its docstrings also described an `apply_chrome` shape that no
    longer exists, so reviving it would have produced two title bars."""

    def test_the_windowbar_module_does_not_exist(self):
        self.assertFalse((PKG / "windowbar.py").exists())

    def test_its_tests_are_gone_with_it(self):
        self.assertFalse(
            (pathlib.Path(__file__).with_name("test_windowbar.py")).exists())

    def test_the_engine_cannot_spawn_or_run_a_bar(self):
        for name in ("_spawn_window_bar", "_run_windowbar"):
            with self.subTest(name=name):
                self.assertFalse(hasattr(engine, name))

    def test_the_hidden_windowbar_subcommand_is_unregistered(self):
        self.assertNotIn("_windowbar", _subcommands())

    def test_importing_it_fails_rather_than_finding_a_stale_copy(self):
        import importlib
        with self.assertRaises(ImportError):
            importlib.import_module("omnidroid.windowbar")
```

- [ ] **Step 3: Run test to verify it fails**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_tkinter_is_gone.py -q
```
Expected: FAIL — all five methods of `TheTkTitleBarIsGone` fail; `TheTkViewerIsGone` still passes.

- [ ] **Step 4: Delete the two files**

```bash
git rm omnidroid/windowbar.py tests/test_windowbar.py
```

- [ ] **Step 5: Delete `_spawn_window_bar` from `omnidroid/engine.py`**

Delete `engine.py:6640-6667` in full — from `def _spawn_window_bar(name, title, identity, pid):` (whose docstring begins "Start the title bar in its own process, detached.") down to and including its `return subprocess.Popen(cmd, **kwargs)`, plus the blank lines before `def _window_lock_pid_path(name):`.

**Leave `_window_lock_pid_path`, `_running_window_lock_pid` and `_spawn_window_lock` exactly as they are** — they are the aspect lock, sub-project E's. Note that `_running_window_lock_pid`'s docstring says "Same shape and the same reasoning as `_running_window_bar_pid`"; Task 5 fixes that sentence.

- [ ] **Step 6: Delete `_run_windowbar` and its parser**

Delete `engine.py:7299-7323` in full:

```python
def _run_windowbar(a):
    """Hidden subcommand: run the title bar for one instance, detached from
    `view` (see _spawn_window_bar). Not a user-facing command."""
    from omnidroid import windowbar

    def stop_instance(_identity):
        cmd_stop(type("Args", (), {"name": a.name, "json": False,
                                   "timeout": 90})())

    try:
        return windowbar.run_window_bar(a.identity, title=a.title,
                                        pid=getattr(a, "pid", None),
                                        on_stop=stop_instance)
    finally:
        # However this exits -- hide, stop, or the window closed some other
        # way -- persist where the user left the window BEFORE clearing the
        # pid file, so the NEXT `view` restores it instead of QEMU's own
        # default position. Same `finally`, so it happens on every exit path,
        # not just the clean one.
        _persist_window_bar_geometry(a.name, a.identity,
                                     getattr(a, "pid", None))
        # The pid file that says "a bar is already open" must not outlive
        # the bar, or the NEXT `view` believes a bar is up when it is not
        # and never opens a new one.
        _clear_window_bar_pid(a.name)
```

Delete `engine.py:12068-12080` (the parser registration):

```python
    # Hidden internal: the title bar for one instance, owned BY the guest's
    # QEMU window (see windowbar.py). Spawned by `omnidroid view` on a boot
    # whose pixels live in a window (hidden through boot, shown by `view`).
    # Not for direct use.
    wb = sub.add_parser("_windowbar")
    wb.add_argument("name")
    wb.add_argument("--identity", required=True,
                    help="the QEMU window title, i.e. omni-<account>")
    wb.add_argument("--title", default=None)
    wb.add_argument("--pid", type=int, default=None,
                    help="the QEMU pid, so the window is found even if its "
                         "title is not what we expect")
    wb.set_defaults(func=lambda a: sys.exit(_run_windowbar(a)))
```

**Do NOT touch the `_windowlock` parser block immediately below it.**

- [ ] **Step 7: Delete the tests that drove the bar process**

In `tests/test_self_argv_prefix.py`, delete the whole `SpawnedWindowBarCommand` class — everything from `class SpawnedWindowBarCommand(unittest.TestCase):` through `self.assertEqual(cmd[2], "_windowbar")` at the end of `test_the_non_frozen_dev_shape_reinvokes_this_file_not_dash_m`. After Task 3 and this step, the file contains only the `SelfArgvPrefix` class, its four tests, the docstring, and the `if __name__ == "__main__":` footer. Drop the now-unused `from unittest import mock` import.

In `tests/test_hidden_window_viewer.py`, delete these `_spawn_window_bar` patches and adjust the surrounding line continuations:

1. Lines 81-83, in `test_view_applies_chrome_then_shows_and_spawns_no_bar` — remove the patch and the `"bar"` collector; the preceding `show_qemu_window` patch takes the closing `:`. Rename the test to `test_view_applies_chrome_then_shows` and update its assertion comment. `self.assertEqual(calls, ["chrome", "show", "lock"])` is unchanged.
2. Line 106, in `test_show_is_given_the_pid_...` — remove `mock.patch("omnidroid.engine._spawn_window_bar"):` and close the `with` on the `show_qemu_window` patch above it.
3. Lines 210-211, in `test_a_missing_window_fails_instead_of_opening_an_empty_desktop` — remove the patch, close the `with` on `show_qemu_window`, and change the comment `# Chrome/show/bar must never run against a window that is not there.` to `# Chrome/show must never run against a window that is not there.`
4. Line 341 (`... as spawn, \`) in `test_hide_hides_the_window_and_touches_nothing_else` — remove the patch, and remove `spawn.assert_not_called()` from the assertions (line 360). Also amend the comment above the assertions: "show it, or spawn a bar" → "or show it".
5. Lines 498-499, in `HideNeverOpensAViewer._view` — remove the `_spawn_window_bar` patch and its `opened.append("bar")`. `opened` then collects only `"show"`, which is still what the four tests in that class assert against (they assert `opened == []`).
6. In `TheBarsPidFileTracksItsLifetime`, delete the three tests that call `engine._run_windowbar`: `test_run_windowbar_clears_the_pid_file_on_the_way_out` (877-886), `test_run_windowbar_persists_geometry_before_it_returns` (888-908) and `test_a_window_that_is_already_gone_leaves_the_record_untouched` (910-928). The rest of that class is deleted in Task 5.

Also update the class docstring at line 317 (`TheAppCanHideAWindowWithoutStopping`): "The window's own X already only offers hide-or-stop (windowbar.py);" → "The window's own X is inert today (QEMU is spawned `window-close=off`);".

- [ ] **Step 8: Run test to verify it passes**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_tkinter_is_gone.py tests/test_self_argv_prefix.py \
    tests/test_hidden_window_viewer.py tests/test_contract_commands.py \
    tests/test_facade_equivalence.py -q
```
Expected: PASS across all five.

- [ ] **Step 9: Commit**

```bash
git add omnidroid/engine.py omnidroid/windowbar.py tests/test_windowbar.py \
  tests/test_self_argv_prefix.py tests/test_hidden_window_viewer.py \
  tests/test_tkinter_is_gone.py
git commit -m "feat: delete windowbar.py and the _windowbar subcommand

636 lines of Tk title bar with 705 lines of tests and zero production callers
since cmd_view hardcoded bar_ok = False. Its docstrings described an
apply_chrome shape that no longer exists, so reviving it would have given two
title bars. QEMU's own window is the window."
```

---

### Task 5: Delete the window-bar pid, geometry and kill plumbing

**Files:**
- Modify: `omnidroid/engine.py` — `_write_run_record` docstring (~456-459), `_running_window_lock_pid` docstring (~6680), `_window_bar_pid_path`/`_running_window_bar_pid`/`_write_window_bar_pid`/`_clear_window_bar_pid`/`_kill_window_bar`/`_persist_window_bar_geometry` (6897-7017), `WINDOW_BAR_SETTLE` + `_window_bar_settled` (6848-6895), `_view_hide` (7057-7073), `cmd_view`'s `bar_ok` (7239, 7245)
- Modify: `tests/test_hidden_window_viewer.py` (the remaining bar patches and the two bar-only classes)
- Modify: `tests/test_window_at_boot.py:543,564`
- Modify: `tests/test_tkinter_is_gone.py` (add a class)

**Interfaces:**
- Consumes: `PKG`, `_subcommands()` and the two classes from Tasks 3 and 4 in `tests/test_tkinter_is_gone.py`.
- Produces: `engine` no longer defines `_window_bar_pid_path`, `_running_window_bar_pid`, `_write_window_bar_pid`, `_clear_window_bar_pid`, `_kill_window_bar`, `_persist_window_bar_geometry`, `WINDOW_BAR_SETTLE` or `_window_bar_settled`. `cmd_view`'s window-path JSON loses its `"bar"` key; every other key (`name`, `viewer`, `chrome`, `aspect_locked`, `vnc_host`, `vnc_port`, `started`, `ok`) is unchanged. `_view_hide` keeps its exact contract: same `no_window_to_hide` failure, same `hostwin.hide_qemu_window(identity, pid=..., timeout=2)` call, same `_record_window_visible` write, same JSON.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_tkinter_is_gone.py`:

```python
class TheBarsPlumbingWentWithIt(unittest.TestCase):
    """A pid file, a killer, a geometry persister and a settle check, all for
    a process that no longer exists. Dead plumbing reads as live plumbing."""

    NAMES = ("_window_bar_pid_path", "_running_window_bar_pid",
             "_write_window_bar_pid", "_clear_window_bar_pid",
             "_kill_window_bar", "_persist_window_bar_geometry",
             "_window_bar_settled", "WINDOW_BAR_SETTLE")

    def test_none_of_it_resolves_on_the_engine_any_more(self):
        for name in self.NAMES:
            with self.subTest(name=name):
                self.assertFalse(hasattr(engine, name))

    def test_none_of_it_is_still_promised_by_the_facade_contract(self):
        import json
        snapshot = json.loads(
            (pathlib.Path(__file__).with_name("engine_public_names.json"))
            .read_text())
        for name in self.NAMES + ("_run_vncview", "_spawn_builtin_viewer",
                                  "_spawn_window_bar", "_run_windowbar"):
            with self.subTest(name=name):
                self.assertNotIn(name, snapshot)

    def test_the_window_lock_plumbing_is_untouched(self):
        # Sub-project E's, not ours. If this fails, the plans have collided.
        for name in ("_window_lock_pid_path", "_running_window_lock_pid",
                     "_spawn_window_lock", "_run_windowlock"):
            with self.subTest(name=name):
                self.assertTrue(hasattr(engine, name))


class HideStillWorksWithoutABarToKill(unittest.TestCase):
    """`view --hide` used to persist the bar's geometry, kill the bar, clear
    its pid file and only then hide the window. With no bar, it is one call."""

    def test_hide_is_now_just_hide(self):
        import unittest.mock as _mock
        calls = []
        with _mock.patch("omnidroid.engine.load_config", return_value={}), \
             _mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             _mock.patch("omnidroid.engine.load_account",
                         return_value={"name": "farm3", "vnc_port": 18001}), \
             _mock.patch("omnidroid.engine._run_record",
                         return_value={"identity": "omni-farm3", "pid": 4242,
                                       "native_window": True}), \
             _mock.patch("omnidroid.engine._record_window_visible"), \
             _mock.patch("omnidroid.hostwin.hide_qemu_window",
                         side_effect=lambda *a, **k:
                             calls.append((a, k)) or True):
            result = engine.cmd_view(type("A", (), {
                "name": "farm3", "hide": True, "json": False, "start": False,
                "native": False, "mode": None, "debug": False, "offset": None,
                "timeout": 60, "viewer": None})())
        self.assertIsNone(result)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][0], ("omni-farm3",))
        self.assertEqual(calls[0][1].get("pid"), 4242)
        self.assertEqual(calls[0][1].get("timeout"), 2)
```

- [ ] **Step 2: Run test to verify it fails**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_tkinter_is_gone.py -q
```
Expected: `test_none_of_it_resolves_on_the_engine_any_more` FAILS (all eight names still resolve). `test_hide_is_now_just_hide` passes already — `_running_window_bar_pid` returns `None` for a name with no pid file — and must keep passing after the deletion.

- [ ] **Step 3: Delete the settle check and the pid/geometry/kill helpers**

Delete these contiguous blocks from `omnidroid/engine.py`, in this order (top-down, so earlier deletions do not shift the later ones you are still reading):

1. `engine.py:6848-6866` — the `WINDOW_BAR_SETTLE` comment block, the constant itself, and `def _window_bar_settled(name, proc):` through its closing `return False` (ends at line 6895).
2. `engine.py:6897-6898` — `def _window_bar_pid_path(name):` and its `return runtime_dir(name) / "windowbar.pid"`.
3. `engine.py:6901-6920` — `def _running_window_bar_pid(name):` through `return pid if pid_alive(pid) else None`.
4. `engine.py:6922-6931` — `def _write_window_bar_pid(name, pid):` through its `pass`.
5. `engine.py:6934-6938` — `def _clear_window_bar_pid(name):` through its `pass`.
6. `engine.py:6941-6959` — `def _kill_window_bar(pid):` through its `pass`.
7. `engine.py:6962-7017` — `def _persist_window_bar_geometry(name, identity, pid):` through its `pass`.

After this, `def _record_window_client(...)`/`def boot_shows_in_a_window(...)` (whichever precedes) should run straight into `def _view_hide(args):` with two blank lines between.

- [ ] **Step 4: Simplify `_view_hide`**

In `engine.py:7057-7073`, delete the whole comment block and the branch:

```python
    # A live bar is OWNED by QEMU's window, not a CHILD of it -- Windows
    # only cascades DESTROY to an OWNED window when the OWNER is destroyed
    # (that is what lets `stop` clean the bar up for free); a bare hide of
    # the owner gives no such guarantee, so an untouched bar would be left
    # on screen, captioning nothing. It has to be killed here. And killing
    # it does NOT run `_run_windowbar`'s `finally` -- Windows does not run
    # them on termination -- so the geometry that `finally` would have
    # persisted must be captured and written FIRST, while the window is
    # still visible, or it is lost for good. Order is load-bearing: persist,
    # then kill, then clear its pid file (so the next `view` does not
    # believe a bar only WE just killed is still open), then hide.
    bar_pid = _running_window_bar_pid(args.name)
    if bar_pid is not None:
        _persist_window_bar_geometry(args.name, identity, qemu_pid)
        _kill_window_bar(bar_pid)
        _clear_window_bar_pid(args.name)
```

`identity` is now unused in `_view_hide` (it was only read by the deleted branch); delete its assignment too:

```python
    identity = run.get("identity") or f"omni-{args.name}"
```

`qemu_pid` is still used by the `hide_qemu_window` call below, so it stays.

- [ ] **Step 5: Drop `bar_ok` from `cmd_view`**

At `engine.py:7239`, delete the line:
```python
        bar_ok = False
```
and at `engine.py:7245`, change:
```python
            emit_json({"name": args.name, "viewer": "window",
                       "chrome": chrome["applied"], "bar": bar_ok,
                       "aspect_locked": bool(locked),
                       "vnc_host": None,
                       "vnc_port": None, "started": started, "ok": True})
```
to:
```python
            emit_json({"name": args.name, "viewer": "window",
                       "chrome": chrome["applied"],
                       "aspect_locked": bool(locked),
                       "vnc_host": None,
                       "vnc_port": None, "started": started, "ok": True})
```

- [ ] **Step 6: Fix the two docstrings that referenced the deleted helpers**

In `_write_run_record` (`engine.py:456-460`), replace:

```python
    exists: `_persist_window_bar_geometry` rewrites it MID-LIFE, from a
    detached bar process that this product FORCE-KILLS (`_kill_window_bar`),
    and `view --hide` rewrites it from a second process moments before
    killing the first. A `write_text` truncates the file and then fills it,
    so a reader — or a kill — landing in that gap sees an empty or partial
    file.
```
with:
```python
    exists: `view --hide` and `_record_window_visible` rewrite it MID-LIFE,
    from a second process, while the first is still running and readers are
    polling it. A `write_text` truncates the file and then fills it, so a
    reader — or a kill — landing in that gap sees an empty or partial file.
```

In `_running_window_lock_pid` (`engine.py:6678-6685` as it stands), replace:

```python
    Same shape and the same reasoning as `_running_window_bar_pid`: a stale
    file (the process was killed, the host rebooted) reads as dead, and a
    missed "already running" costs one harmless duplicate rather than a
    command that hangs.
```
with:
```python
    A stale file (the process was killed, the host rebooted) reads as dead,
    and a missed "already running" costs one harmless duplicate rather than a
    command that hangs.
```

- [ ] **Step 7: Strip the bar out of the remaining tests**

In `tests/test_hidden_window_viewer.py`:

1. Delete the whole `TheBarIsVerifiedToHaveComeUp` class (lines 660-730 as they stand: the class docstring, `setUp`, `_Proc`, and its four tests) — it tests only `_window_bar_settled`.
2. Delete the whole `TheBarsPidFileTracksItsLifetime` class (lines 831 onwards; Task 4 already removed its last three tests, so what remains is `setUp` plus `test_no_file_means_no_bar`, `test_a_live_pid_is_reported`, `test_a_dead_pid_reads_as_no_bar`, `test_a_corrupt_file_reads_as_no_bar_not_a_crash` and `test_clearing_removes_the_file_and_is_safe_when_there_is_none`).
3. Delete `test_hide_while_a_bar_is_live_persists_geometry_kills_it_then_hides` in full (lines 366-408).
4. Delete `test_persisting_geometry_uses_it` (lines 614-620) from `TheRunRecordIsRewrittenAtomically`. Keep the class and its three other tests — they test `_write_run_record`'s atomicity, which is still load-bearing. Amend the class docstring's second sentence: "`_persist_window_bar_geometry` rewrites it MID-LIFE from a detached bar process this product FORCE-KILLS (`_kill_window_bar`), and `view --hide` rewrites it from a second process" → "`view --hide` and `_record_window_visible` rewrite it MID-LIFE from a second process".
5. Delete the remaining `_running_window_bar_pid` / `_persist_window_bar_geometry` / `_kill_window_bar` / `_clear_window_bar_pid` patches from the four surviving hide tests (lines 334, 342-345, 421, 444, 466, 494), fixing the `with` continuations each time. `test_hide_hides_the_window_and_touches_nothing_else` loses `persist.assert_not_called()`, `kill.assert_not_called()` and `clear.assert_not_called()`.
6. Update the module docstring's last paragraph (line 30-31): "The separate title bar is gone too -- QEMU's own window is the window." is already correct; leave it.

In `tests/test_window_at_boot.py`, delete the two patches:
```python
             mock.patch.object(engine, "_running_window_bar_pid",
                               return_value=None), \
```
at lines 543-544 and 564-565, closing the `with` continuations on the `load_account` patches above them.

- [ ] **Step 8: Run test to verify it passes**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_tkinter_is_gone.py tests/test_hidden_window_viewer.py \
    tests/test_window_at_boot.py tests/test_facade_equivalence.py \
    tests/test_native_viewer_only.py -q
```
Expected: PASS across all five.

Then the whole suite, diffed against the baseline:
```bash
cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/ -q 2>&1 | tee /tmp/omni-t5.txt
diff /tmp/omni-baseline-failed.txt <(grep '^FAILED' /tmp/omni-t5.txt | sort)
```
Expected: empty diff.

- [ ] **Step 9: Commit**

```bash
git add omnidroid/engine.py tests/test_hidden_window_viewer.py \
  tests/test_window_at_boot.py tests/test_tkinter_is_gone.py
git commit -m "refactor: remove the window-bar pid, geometry and kill plumbing

A pid file, a force-killer, a geometry persister and a settle check, all for a
process that no longer exists. view --hide is now one call to
hostwin.hide_qemu_window, and cmd_view's JSON drops its always-false 'bar' key.

The window-LOCK plumbing (_windowlock, _spawn_window_lock, aspect_lock) is
deliberately untouched -- it is not tkinter and it belongs to sub-project E."
```

---

### Task 6: Take tkinter out of the two build scripts, and out of the docs

**Files:**
- Modify: `build-exe.ps1:14-27`
- Modify: `build-linux.sh:18-32`
- Modify: `MODES.md` (lines ~137, ~144-171), `HANDOFF.md` (lines ~388-411), `docs/HANDOFF-WINDOWS.md` (lines ~2777, ~2837), `omnidroid/hostwin.py` (comment references at 30, 334, 920, 1221), `CHANGELOG.md` (new entry)
- Test: `tests/test_tkinter_is_gone.py` (add a class)

**Interfaces:**
- Consumes: `PKG` from `tests/test_tkinter_is_gone.py`.
- Produces: neither build script mentions `tkinter`, `vncview` or `PIL.ImageTk`. **`--hidden-import PIL.Image`, `--hidden-import PIL.ImageChops` and `--hidden-import PIL.ImageStat` stay in both** — `capture.py` imports all three lazily (`capture._lazy_pil`, `capture._frame_metrics`, `capture._mean_brightness`), and PyInstaller cannot see a lazy import that also ships native binaries (`docs/HANDOFF-WINDOWS.md:2830-2839`).

- [ ] **Step 1: Write the failing test**

Append to `tests/test_tkinter_is_gone.py`:

```python
class TheFrozenBuildsDoNotAskForTk(unittest.TestCase):
    """The build scripts declared --hidden-import tkinter / vncview /
    PIL.ImageTk because the Tk viewer was imported lazily by name. There is no
    Tk viewer. PIL.Image / ImageChops / ImageStat STAY: capture.py imports all
    three lazily and PyInstaller cannot resolve a lazy import that also ships
    native binaries."""

    ROOT = pathlib.Path(__file__).resolve().parents[1]
    SCRIPTS = ("build-exe.ps1", "build-linux.sh")
    GONE = ("tkinter", "vncview", "PIL.ImageTk")
    KEPT = ("PIL.Image", "PIL.ImageChops", "PIL.ImageStat")

    def test_neither_script_asks_for_a_tk_dependency(self):
        for script in self.SCRIPTS:
            text = (self.ROOT / script).read_text(encoding="utf-8")
            for token in self.GONE:
                with self.subTest(script=script, token=token):
                    self.assertNotIn(token, text)

    def test_both_scripts_still_ask_for_the_imaging_capture_needs(self):
        for script in self.SCRIPTS:
            text = (self.ROOT / script).read_text(encoding="utf-8")
            for token in self.KEPT:
                with self.subTest(script=script, token=token):
                    self.assertIn(token, text)
```

- [ ] **Step 2: Run test to verify it fails**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_tkinter_is_gone.py::TheFrozenBuildsDoNotAskForTk -q
```
Expected: FAIL on `test_neither_script_asks_for_a_tk_dependency` — both scripts still contain all three tokens.

- [ ] **Step 3: Edit `build-exe.ps1`**

Replace lines 14-27:

```powershell
# The built-in VNC viewer (manager\vncview.py) and the capture engine
# (manager\capture.py) are imported lazily by name, so PyInstaller can't
# auto-detect them — add them and their GUI/imaging deps explicitly.
# (Windows python.org builds ship tkinter; Pillow via `pip install pillow`.)
py -3 -m PyInstaller --onefile --name omnidroid `
    --distpath "$root\dist" --workpath "$root\build\pyi" `
    --specpath "$root\build" `
    --paths "$root\manager" `
    --hidden-import vncview `
    --hidden-import capture `
    --hidden-import tkinter `
    --hidden-import PIL.Image --hidden-import PIL.ImageTk `
    --hidden-import PIL.ImageChops --hidden-import PIL.ImageStat `
    "$root\manager\omni.py"
```

with:

```powershell
# The capture engine is imported lazily by name, so PyInstaller can't
# auto-detect it — add it and its imaging deps explicitly. Pillow via
# `pip install pillow`.
#
# NO tkinter and NO PIL.ImageTk: the Tk viewer and the Tk title bar are
# deleted (design 2026-08-19 §7). The RFB protocol client that survived them
# (omnidroid\rfb.py) is stdlib-only and statically imported by capture.py, so
# it needs no flag of its own. PIL.Image/ImageChops/ImageStat DO stay —
# capture.py imports all three lazily, and a lazy import that ships native
# binaries is exactly what PyInstaller cannot resolve unaided.
py -3 -m PyInstaller --onefile --name omnidroid `
    --distpath "$root\dist" --workpath "$root\build\pyi" `
    --specpath "$root\build" `
    --paths "$root\manager" `
    --hidden-import capture `
    --hidden-import PIL.Image `
    --hidden-import PIL.ImageChops --hidden-import PIL.ImageStat `
    "$root\manager\omni.py"
```

- [ ] **Step 4: Edit `build-linux.sh`**

Replace lines 18-32:

```sh
# The built-in VNC viewer (manager/vncview.py) and the capture engine
# (manager/capture.py) are imported lazily by name, so PyInstaller can't
# auto-detect them — add them and their GUI/imaging deps explicitly.
# (tkinter is usually picked up by PyInstaller's hooks; keep it listed to be
# safe. Linux needs the system Tk: sudo apt install python3-tk.)
python3 -m PyInstaller --onefile --name omnidroid \
    --distpath "$root/dist" --workpath "$root/build/pyi" \
    --specpath "$root/build" \
    --paths "$root/manager" \
    --hidden-import vncview \
    --hidden-import capture \
    --hidden-import tkinter \
    --hidden-import PIL.Image --hidden-import PIL.ImageTk \
    --hidden-import PIL.ImageChops --hidden-import PIL.ImageStat \
    "$root/manager/omni.py"
```

with:

```sh
# The capture engine is imported lazily by name, so PyInstaller can't
# auto-detect it — add it and its imaging deps explicitly.
#
# NO tkinter and NO PIL.ImageTk, and Linux no longer needs `apt install
# python3-tk`: the Tk viewer and the Tk title bar are deleted (design
# 2026-08-19 §7). The RFB protocol client that survived them
# (omnidroid/rfb.py) is stdlib-only and statically imported by capture.py, so
# it needs no flag of its own. PIL.Image/ImageChops/ImageStat DO stay —
# capture.py imports all three lazily, and a lazy import that ships native
# binaries is exactly what PyInstaller cannot resolve unaided.
python3 -m PyInstaller --onefile --name omnidroid \
    --distpath "$root/dist" --workpath "$root/build/pyi" \
    --specpath "$root/build" \
    --paths "$root/manager" \
    --hidden-import capture \
    --hidden-import PIL.Image \
    --hidden-import PIL.ImageChops --hidden-import PIL.ImageStat \
    "$root/manager/omni.py"
```

- [ ] **Step 5: Run test to verify it passes**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_tkinter_is_gone.py -q
```
Expected: PASS, all four classes.

- [ ] **Step 6: Correct the prose that documents the Tk viewer as shipping behaviour**

`MODES.md` — in the table around line 137, delete the row:
```markdown
| a thin bar of ours is spawned | a separate process (`windowbar.py`), its window made an OWNER of QEMU's window via `GWLP_HWNDPARENT` — never the reverse |
```
Then, in the paragraphs that follow (through the "Known issue" paragraph that ends around line 175), the whole ownership-direction discussion and the `bar_geometry()` known issue describe a bar that no longer exists. Replace that whole run of text — from "QEMU's window stays top-level for its whole life" through the end of the `windowbar.bar_geometry()` known-issue paragraph — with:

```markdown
QEMU's window stays top-level for its whole life; only its *style bits* change
(caption/sysmenu/minimize/maximize cleared, `WS_THICKFRAME` kept — confirmed
2026-08-16 by reading the live style word off a running instance:
`0x16040000` = `WS_VISIBLE|WS_CLIPSIBLINGS|WS_CLIPCHILDREN|WS_THICKFRAME`, no
`WS_CAPTION`, no `WS_SYSMENU`). Nothing is copied, encoded or decoded per frame
(there was never a framebuffer in this design, restyled or not), and input goes
into the guest's `usb-tablet`/`usb-kbd` directly.

**There is no bar, and no viewer of ours.** A separate Tk strip
(`windowbar.py`) used to be spawned above the guest window, owned BY it via
`GWLP_HWNDPARENT`. It was retired when `cmd_view` hardcoded `bar_ok = False`,
shipped un-called for months, and is deleted (design 2026-08-19 §7) along with
the Tk+RFB viewer (`vncview.py`). The window's own chrome — title, icon, aspect
lock and eventually the close prompt — is QEMU's, out of the patch series
sub-project E owns.
```

`HANDOFF.md` — replace the "Live viewer" bullet's first two sub-bullets (lines ~389-404) with:

```markdown
- **Live viewer:** `omnidroid view <account> [--start]`. Where it looks depends
  on where the pixels are:
  - **The pixels are in a WINDOW** (every GPU boot): QEMU's own window IS the
    viewer. `view` restyles it in place (`hostwin.apply_chrome`), shows it, and
    brings it forward. Nothing is copied or encoded per frame; input goes into
    the guest's `usb-tablet`/`usb-kbd` directly. `view <name> --hide` takes it
    off screen without stopping the instance.
  - **The pixels are in a VNC FRAMEBUFFER** (farming, or a gaming boot that
    degraded to software): `view` launches an OS/native VNC client against the
    account's localhost `vnc_port`, and fails with the typed error
    `no_vnc_client` if the host has none. There is no built-in viewer any more
    — the Tk+RFB window (`vncview.py`) is deleted (design 2026-08-19 §7). Its
    protocol half lives on in `omnidroid/rfb.py`, which is what `screenshot`,
    `capture` and autocap read the framebuffer through.
```
Keep the `--native` sub-bullet and the "No password anywhere" sub-bullet that follow; in the `--native` one, delete the parenthetical "(implies --native)" only if it now reads wrong in context, otherwise leave it.

`docs/HANDOFF-WINDOWS.md` — at line ~2777, change:
```markdown
   `vncview.RFBClient` (and `connect()` only handshakes — `run()` is the receive
```
to
```markdown
   `rfb.RFBClient` (and `connect()` only handshakes — `run()` is the receive
```
and at line ~2837, change:
```markdown
    any hiddenimports entry. What it cannot resolve unaided is a dependency that
    is imported conditionally AND ships native binaries or data: selenium (whose
    Selenium Manager is an executable), tkinter and PIL. Those are the
    hiddenimports that matter, and `tests/test_packaging.py` asserts BOTH specs
    declare the same set — the macOS spec had been missing them for months.
```
to
```markdown
    any hiddenimports entry. What it cannot resolve unaided is a dependency that
    is imported conditionally AND ships native binaries or data: selenium (whose
    Selenium Manager is an executable) and PIL. Those are the hiddenimports that
    matter, and `tests/test_packaging.py` asserts BOTH specs declare the same
    set — the macOS spec had been missing them for months. (tkinter used to be
    on that list for the Tk viewer; that viewer is deleted, and omni-executor's
    two .spec files still need the same line removed — see the handoff.)
```

`omnidroid/hostwin.py` — four comments name `windowbar.py` as a live thing. Change each to past tense so nobody goes looking for the file:
- line 30: `` `windowbar.py`'s bar, owned BY the window `` → `` a bar owned BY the window (deleted; see design 2026-08-19 §7) ``
- line 334: `` window, never a child (windowbar.py). `` → `` window, never a child. ``
- line 920: `` The strip (windowbar.py) is `` → `` The strip that used to sit above it (deleted) was ``
- line 1221: `` (windowbar.py) becomes the title bar -- `` → `` (deleted) used to become the title bar -- ``

Read each line in context and keep the sentence grammatical; these are comments, and a mangled one is worse than the stale one it replaced.

`CHANGELOG.md` — add a new entry at the top, directly under the `> **Resuming...` blockquote and the "All notable base-image and manager changes." paragraph, above `## 2026-08-17 (night) — ...`:

```markdown
## 2026-08-19 — the tkinter viewer is deleted

There were two Tk windows in the tree and neither was the product's viewer.

`vncview.py` was an RFB client welded to a Tk window: `omnidroid view` spawned
it as a detached `_vncview` child, and so did `omnidroid start` on any
interactive boot without a native QEMU window. `windowbar.py` was 636 lines of
Tk title bar with 705 lines of tests and **zero production callers** — `cmd_view`
had hardcoded `bar_ok = False` — plus docstrings describing an `apply_chrome`
shape that no longer exists, so reviving it would have produced two title bars.

The product's viewer is QEMU's own customised window. Both Tk paths are gone,
with `_spawn_builtin_viewer`, `_run_vncview`, `_spawn_window_bar`,
`_run_windowbar`, the `_vncview` / `_windowbar` subcommands, the window-bar pid
file, killer, geometry persister and settle check, and the
`--hidden-import tkinter` / `vncview` / `PIL.ImageTk` lines in `build-exe.ps1`
and `build-linux.sh`.

**What survived, and why.** `RFBClient` is now `omnidroid/rfb.py` — the same
protocol client, stdlib only, no tkinter and no Pillow, checked in a subprocess
by `tests/test_rfb.py`. `capture.py` is built on it, so `screenshot`, `capture`
and autocap are unchanged, and `tests/test_pixel_format.py` still pins the
SetPixelFormat shifts (16/8/0) that the RGBX decode was measured against.

**What changed for a user.** `omnidroid view` on a boot whose pixels live in the
VNC framebuffer launches an OS/native client and reports the typed error
`no_vnc_client` when the host has none, instead of opening a Tk window. `start`
names the VNC port rather than opening a second window onto it, and
`start --json` no longer emits `viewer_pid`. `view --json` on the window path no
longer emits the always-false `bar` key.

**Not touched:** `_windowlock` / `aspect_lock` / `hostwin.py` — ctypes, not
tkinter, and sub-project E's to remove.
```

- [ ] **Step 7: Commit**

```bash
git add build-exe.ps1 build-linux.sh MODES.md HANDOFF.md \
  docs/HANDOFF-WINDOWS.md omnidroid/hostwin.py CHANGELOG.md \
  tests/test_tkinter_is_gone.py
git commit -m "chore: drop tkinter from the frozen builds and the docs

--hidden-import tkinter / vncview / PIL.ImageTk go from both build scripts;
PIL.Image/ImageChops/ImageStat stay because capture.py imports all three
lazily. MODES.md, HANDOFF.md and HANDOFF-WINDOWS.md documented the Tk viewer
and the Tk bar as shipping behaviour."
```

---

### Task 7: Prove no tkinter is left anywhere, and hand off

**Files:**
- Modify: `tests/test_tkinter_is_gone.py` (add the AST scan)
- Test: the whole suite

**Interfaces:**
- Consumes: `PKG` from `tests/test_tkinter_is_gone.py` (Task 3).
- Produces: a standing guard. Spec §7's closing claim ("No tkinter remains in the package") becomes a test rather than an assertion.

- [ ] **Step 1: Write the failing test**

Append to `tests/test_tkinter_is_gone.py` — note the `import ast` at the top of the file, which you must add to the import block:

```python
class NoModuleInThePackageImportsTk(unittest.TestCase):
    """Design 2026-08-19 §7 closes with 'No tkinter remains in the package.'
    That is a claim about every file, so it is checked against every file
    rather than against the two we happened to delete.

    Parsed, not grepped: the word 'tkinter' appears in comments and CHANGELOG
    prose describing what was removed, and prose must not fail a build."""

    BANNED_ROOTS = ("tkinter", "_tkinter", "Tkinter", "ttk")

    def _offenders(self, predicate):
        found = []
        for path in sorted(PKG.rglob("*.py")):
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
            for node in ast.walk(tree):
                found += [f"{path.name}:{node.lineno} {what}"
                          for what in predicate(node)]
        return found

    def test_nothing_imports_tkinter(self):
        def banned(node):
            if isinstance(node, ast.Import):
                return [f"import {a.name}" for a in node.names
                        if a.name.split(".")[0] in self.BANNED_ROOTS]
            if isinstance(node, ast.ImportFrom):
                root = (node.module or "").split(".")[0]
                return ([f"from {node.module} import ..."]
                        if root in self.BANNED_ROOTS else [])
            return []

        self.assertEqual(self._offenders(banned), [])

    def test_nothing_imports_the_tk_half_of_pillow(self):
        # PIL.ImageTk is the bridge from a PIL image to a Tk widget. capture.py
        # legitimately uses PIL.Image/ImageChops/ImageStat; ImageTk has no
        # non-Tk use, so its presence would mean a Tk window came back.
        def banned(node):
            if isinstance(node, ast.Import):
                return [f"import {a.name}" for a in node.names
                        if a.name in ("PIL.ImageTk", "ImageTk")]
            if isinstance(node, ast.ImportFrom):
                if (node.module or "") in ("PIL", "PIL.ImageTk"):
                    return [f"from {node.module} import ImageTk"
                            for a in node.names if a.name == "ImageTk"]
            return []

        self.assertEqual(self._offenders(banned), [])

    def test_the_package_still_has_the_modules_that_replaced_them(self):
        # A guard that passes because the package is empty is not a guard.
        self.assertTrue((PKG / "rfb.py").exists())
        self.assertTrue((PKG / "capture.py").exists())
        self.assertTrue((PKG / "hostwin.py").exists())
```

- [ ] **Step 2: Run test to verify it passes for the right reason**

Run:
```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_tkinter_is_gone.py -q
```
Expected: PASS.

Then prove the guard actually bites, rather than passing vacuously. Temporarily add `import tkinter` as the first line of `omnidroid/rfb.py`, re-run, and confirm `test_nothing_imports_tkinter` FAILS with `['rfb.py:1 import tkinter']`. **Remove that line again** and re-run to confirm PASS. Do not commit the temporary line.

```bash
python - <<'PY'
import pathlib
p = pathlib.Path("omnidroid/rfb.py")
p.write_text("import tkinter\n" + p.read_text(encoding="utf-8"), encoding="utf-8")
PY
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_tkinter_is_gone.py::NoModuleInThePackageImportsTk -q
python - <<'PY'
import pathlib
p = pathlib.Path("omnidroid/rfb.py")
p.write_text(p.read_text(encoding="utf-8").replace("import tkinter\n", "", 1), encoding="utf-8")
PY
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/test_tkinter_is_gone.py::NoModuleInThePackageImportsTk -q
```

- [ ] **Step 3: Final sweep — nothing in the package still names the deleted modules**

```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
grep -rn "vncview\|windowbar\|window_bar\|_spawn_builtin_viewer\|Tk+RFB" omnidroid/ tests/ --include=*.py
```
Expected: matches only in prose that deliberately records the removal — `tests/test_tkinter_is_gone.py`, `tests/test_native_viewer_only.py`, `tests/test_rfb.py`, `tests/test_self_argv_prefix.py`'s docstring, and the past-tense comments in `omnidroid/hostwin.py`. **No `import`, no attribute access, no `mock.patch` target.** If any live reference remains, it belongs to whichever earlier task owned that file — go back and finish it there.

```bash
grep -rn "vncview\|windowbar" omnidroid.egg-info/SOURCES.txt
```
`omnidroid.egg-info/` is generated build metadata and is regenerated on the next build; leave it alone.

- [ ] **Step 4: Run the full suite and diff against the baseline**

Run:
```bash
cd "C:/Users/berat/Desktop/Omni Apps/omnidroid"
cp "$LOCALAPPDATA/OmniExec/paths.json" /tmp/test-paths.json
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m pytest tests/ -q 2>&1 | tee /tmp/omni-final.txt
diff /tmp/omni-baseline-failed.txt <(grep '^FAILED' /tmp/omni-final.txt | sort)
tail -3 /tmp/omni-final.txt
```
Expected: the diff is **empty** — the same 11 pre-existing failures, no new ones. The passed count drops by roughly the number of tests deleted with `test_windowbar.py` (~40) and rises by the ~30 added across Tasks 1, 3, 4, 5, 6 and 7; the absolute number is not the check, the FAILED diff is.

- [ ] **Step 5: Confirm the CLI still starts and still advertises a coherent command list**

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m omnidroid version --json | python -c "import json,sys; d=json.load(sys.stdin); c=d['commands']; print(sorted(c)); assert '_vncview' not in c and '_windowbar' not in c; assert '_windowlock' in c; assert 'view' in c and 'capture' in c and 'screenshot' in c"
```
Expected: exit 0, and the printed list contains `view`, `capture`, `screenshot` and `_windowlock` but neither `_vncview` nor `_windowbar`.

```bash
OMNIDROID_CONFIG_PATH=/tmp/test-paths.json \
  OMNI_IMAGES_DIR="$LOCALAPPDATA/OmniExec/images" \
  python -m omnidroid view --help
```
Expected: exit 0; the `--native` and `--viewer` help text renders; no traceback.

- [ ] **Step 6: Commit**

```bash
git add tests/test_tkinter_is_gone.py
git commit -m "test: guard that no module under omnidroid/ imports tkinter

Parsed with ast, not grepped, so the comments and CHANGELOG prose that record
the removal do not fail the build. Verified to bite by temporarily adding
'import tkinter' to rfb.py and watching it fail."
```

- [ ] **Step 7: Write the handoff note**

Report to the requester, in the completion message (not a new file):

1. **Suite result**: the FAILED diff against the baseline (expected empty), and the final `N failed, M passed` line.
2. **Behaviour changes a client can see** — `view` on a framebuffer boot now errors `no_vnc_client` where a host has no VNC client; `start --json` no longer emits `viewer_pid`; `view --json` on the window path no longer emits `bar`; `version --json`'s `commands` list lost `_vncview` and `_windowbar`.
3. **The cross-repo follow-up**: `omni-executor`'s two PyInstaller `.spec` files still declare `tkinter` and `PIL.ImageTk` hiddenimports, and `tests/test_packaging.py` over there asserts both specs declare the same set (`docs/HANDOFF-WINDOWS.md:2830-2839`). That is a separate repo and a separate change; the frozen exe is bigger than it needs to be until it lands. **Verify the frozen build, not the source** — the engine is frozen in from a sibling checkout at build time, so "the source is fixed" and "the shipped exe is fixed" are different claims (spec §8).
4. **Untouched for sub-project E**: `_windowlock`, `_run_windowlock`, `_spawn_window_lock`, `_running_window_lock_pid`, `_window_lock_pid_path`, `_ensure_window_lock`, `hostwin.py`, `window-close=off`.

---

## Self-Review

**1. Spec coverage.** Spec §7 makes five claims; each has a task.

| §7 requirement | task |
|---|---|
| "`RFBClient` moves out of `vncview.py` into `omnidroid/rfb.py` — pure protocol, no tkinter, no Pillow" | Task 1 (create + purity test), Task 2 (`capture.py` repointed) |
| "because `capture.py:280` is built on it and screenshot/capture/autocap must keep working" | Task 2 Steps 3-4; `tests/test_pixel_format.py` and `tests/test_keyframe_thresholds.py` are run as the check |
| "Then `run_viewer` ... and the `_vncview` ... subcommand go" | Task 3 |
| "`windowbar.py` ... go" (636 lines, 705 lines of tests, zero production callers since `bar_ok = False`) | Task 4 (module + tests + spawner + subcommand), Task 5 (the pid/kill/geometry/settle plumbing it left behind) |
| "along with `--hidden-import tkinter` / `PIL` in `build-exe.ps1` and `build-linux.sh`" | Task 6 — narrowed to `tkinter` / `vncview` / `PIL.ImageTk`, because `capture.py` needs `PIL.Image`/`ImageChops`/`ImageStat`; the narrowing is stated in the task and pinned by `test_both_scripts_still_ask_for_the_imaging_capture_needs` |
| "No tkinter remains in the package." | Task 7 (AST scan over `omnidroid/**/*.py`, proved to bite) |
| §3f's `_windowlock` (E's, not D's) | "Not in scope" section, plus three positive assertions that it still exists (`test_the_window_lock_subcommand_is_untouched`, `test_the_window_lock_plumbing_is_untouched`) so a collision fails loudly |

The user's brief item 4 also named `_spawn_builtin_viewer` for deletion; research found it has **two** production callers (`cmd_view:7285` and `cmd_start:2262`, the latter not mentioned in the brief). Both are handled in Task 3 and the two consequent design decisions are recorded above the tasks rather than buried in a step.

**2. Placeholder scan.** No "TBD", no "similar to Task N", no "add error handling", no "write tests for the above". Every code step carries the literal text to write; every deletion step carries the literal text being deleted so the engineer can confirm they are cutting the right lines; every run step carries the exact command and the expected result. The one place a step says "read each line in context" (Task 6 Step 6, the four `hostwin.py` comments) shows the exact before/after for each of the four and asks only that the surrounding sentence stay grammatical — that is a judgement about prose, not a missing instruction.

**3. Type consistency.** `RFBClient(host, port, on_frame=None)` is defined in Task 1's Interfaces and used with that exact signature in Task 2 (`rfb.RFBClient(host, port, on_frame=sink.on_frame)`) and in Task 1's own tests. The `on_frame(width, height, bgrx, completed_ns, sequence)` five-tuple matches `capture._FrameSink.on_frame`'s existing five parameters. `_vnc_viewer_command(host, port, viewer=None) -> (argv, shell) | None` is used in Task 3 exactly as `engine.py:6562` defines it. `fail(code, message)` matches `output.py:69`. `PKG` and `_subcommands()` are defined once, in Task 3's `tests/test_tkinter_is_gone.py`, and consumed by name in Tasks 4, 5, 6 and 7 — Task 7 also notes the `import ast` those later additions require. The `"no_vnc_client"` error code is introduced in Task 3 and asserted with that same spelling in `tests/test_native_viewer_only.py`.
