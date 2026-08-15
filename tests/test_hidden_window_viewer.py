#!/usr/bin/env python3
"""The window is hidden, and the viewer HOSTS it instead of copying it.

    python3 -m pytest tests/test_hidden_window_viewer.py -q

The product's rule is "you only ever see OUR viewer, never a QEMU window". On
most hosts that is free: the guest renders windowless and QEMU serves a VNC
framebuffer. On Windows it is not, and the reason is measured rather than
assumed (2026-08-15, QEMU 11.0.50, RTX 4060):

  * every windowless GL display there -- `egl-headless` AND `dbus,gl=on` --
    takes its context from ANGLE at ES 2.0, and virglrenderer cannot serve a
    scanout from it. The guest's SET_SCANOUT is rejected
    (`ctrl 0x103, error 0x1203`; 602 rejections in one boot), SurfaceFlinger
    presents nothing, `totalFrames = 0`, screen black.
  * the GTK path goes through WGL, gets desktop GL, gives the guest ES 3.2 --
    and QEMU refuses `-vnc` beside it.

So on Windows the window is the price of the GPU. Three measured facts make the
product's rule survivable anyway, and these tests pin the code that rests on
them:

  1. a HIDDEN window keeps rendering (303 frames / 30 s while invisible),
  2. hiding it at spawn does not break the boot -- but GTK re-shows it during
     early boot, so it has to be re-hidden for a while,
  3. reparenting it into a Tk window works and is fast: 702 frames / 12.1 s
     (58 fps) with the guest living inside our viewer.

And one hazard, also measured: a FORCE-killed viewer destroys the window with
its parent and the guest then renders nothing at all (`totalFrames = 0`) while
still answering adb. That case must be reported, never silently shown as an
empty viewer.
"""
import json
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import embedview, hostwin, qemu_proc  # noqa: E402
from omnidroid import engine as omni  # noqa: E402


class PlatformScope(unittest.TestCase):
    """Both modules are Windows-only, and everything must be a safe no-op
    elsewhere -- Linux renders windowless for real and macOS has no virgl, so
    neither ever has a window to hide."""

    def test_embedding_is_offered_only_where_a_window_is_forced(self):
        self.assertEqual(embedview.available(), qemu_proc.IS_WINDOWS)

    def test_hiding_is_a_no_op_off_windows(self):
        with mock.patch.object(hostwin, "IS_WINDOWS", False):
            self.assertIsNone(hostwin.find_window("omni-x"))
            self.assertFalse(hostwin.hide_qemu_window("omni-x"))
            self.assertFalse(hostwin.window_is_visible("omni-x"))
            self.assertIsNone(hostwin.keep_hidden("omni-x"))

    def test_nothing_is_attempted_without_an_identity(self):
        self.assertIsNone(hostwin.find_window(""))
        self.assertIsNone(hostwin.keep_hidden(""))


class WhenTheWindowIsHidden(unittest.TestCase):
    """`_hide_window_if_wanted` decides, and it only ever has three answers."""

    WINDOWED = ["qemu", "-display", "gtk,gl=on"]
    HEADLESS = ["qemu", "-display", "none"]

    def _hide(self, cmd, cfg=None, env=None):
        calls = []
        with mock.patch.dict(os.environ, env or {}, clear=False), \
             mock.patch.object(hostwin, "hide_qemu_window",
                               side_effect=lambda i, **k: calls.append(i) or True), \
             mock.patch.object(hostwin, "keep_hidden",
                               side_effect=lambda i, **k: calls.append("keep")):
            for stale in ("OMNI_GPU", "OMNI_GL_WINDOW", "OMNI_NO_WINDOW"):
                if stale not in (env or {}):
                    os.environ.pop(stale, None)
            hidden = qemu_proc._hide_window_if_wanted(cmd, "omni-u1", cfg or {})
        return hidden, calls

    def test_a_boot_that_opens_no_window_has_nothing_to_hide(self):
        hidden, calls = self._hide(self.HEADLESS)
        self.assertFalse(hidden)
        self.assertEqual(calls, [])

    def test_the_default_policy_hides_the_window_it_had_to_open(self):
        hidden, calls = self._hide(self.WINDOWED)
        self.assertTrue(hidden)
        self.assertIn("omni-u1", calls)

    def test_it_keeps_hiding_because_gtk_puts_it_back(self):
        # Measured: hiding once at spawn is undone during early boot, and the
        # window was visible again by the time the guest had joined a place.
        _hidden, calls = self._hide(self.WINDOWED)
        self.assertIn("keep", calls)

    def test_an_explicit_window_request_is_honoured(self):
        # `--gpu window` is the debugging configuration: the one case where
        # someone means "put it on my screen".
        hidden, calls = self._hide(self.WINDOWED, env={"OMNI_GPU": "window"})
        self.assertFalse(hidden)
        self.assertEqual(calls, [])


class TheBootRecordsWhatItDid(unittest.TestCase):
    def _record(self, run):
        with mock.patch.object(omni, "_run_record", return_value=run):
            return omni.boot_has_hidden_window("u1")

    def test_a_hidden_window_boot_is_recognised(self):
        self.assertTrue(self._record({"native_window": True,
                                      "window_hidden": True}))

    def test_a_visible_window_boot_is_not(self):
        # Nothing to embed: the user asked to see it, and it is on screen.
        self.assertFalse(self._record({"native_window": True,
                                       "window_hidden": False}))

    def test_a_windowless_boot_is_not(self):
        self.assertFalse(self._record({"native_window": False,
                                       "window_hidden": False}))

    def test_an_older_run_record_without_the_field_is_not(self):
        # Boots from before this feature have no `window_hidden` key at all,
        # and must fall through to the VNC path they were written for.
        self.assertFalse(self._record({"native_window": True}))


class ReleaseIsAlwaysSafe(unittest.TestCase):
    """A window left parented to a viewer that has gone is unreachable, and
    the guest behind it renders nothing. release() therefore has to work from
    every state, including 'never attached'."""

    def test_release_without_an_attach_does_nothing_and_says_so(self):
        win = embedview.EmbeddedQemuWindow("omni-u1")
        self.assertFalse(win.release())

    def test_release_is_idempotent(self):
        win = embedview.EmbeddedQemuWindow("omni-u1")
        win.hwnd = 1234
        win._old_parent = 0
        win._old_style = 0
        with mock.patch.object(embedview.EmbeddedQemuWindow, "_user32",
                               return_value=mock.MagicMock()), \
             mock.patch.object(embedview.EmbeddedQemuWindow, "_set_style"):
            self.assertTrue(win.release())
        self.assertIsNone(win.hwnd)
        self.assertFalse(win.release())

    def test_attach_reports_false_when_there_is_no_window(self):
        win = embedview.EmbeddedQemuWindow("omni-u1")
        with mock.patch.object(embedview, "available", return_value=True), \
             mock.patch.object(hostwin, "find_window", return_value=None):
            self.assertFalse(win.attach(container=1, timeout=0))

    def test_attach_reports_false_off_windows(self):
        win = embedview.EmbeddedQemuWindow("omni-u1")
        with mock.patch.object(embedview, "available", return_value=False):
            self.assertFalse(win.attach(container=1, timeout=0))


if __name__ == "__main__":
    unittest.main()
