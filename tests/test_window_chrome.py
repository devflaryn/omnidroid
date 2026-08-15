#!/usr/bin/env python3
"""QEMU's window is restyled in place -- caption off, sizing border kept.

    python3 -m pytest tests/test_window_chrome.py -q

The caption goes because the strip IS the title bar; leaving QEMU's would put
two title bars on screen -- a second one it would be impossible to click.
WS_THICKFRAME stays so the composite can still be resized by dragging the
guest window's edges.

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
        # The brief's own fake writes into `out.left`/`out.top`/... on the
        # object `GetWindowRect` is handed -- which works against THIS fake,
        # but not against the real implementation, which must pass
        # `ctypes.byref(rect)` to the real GetWindowRect (a `byref` object
        # has no settable attributes, so a fake built this way can never
        # stand in for it). `window_geometry` therefore reads the rect
        # through its own `_window_rect(hwnd)` seam instead of going through
        # `_user32()` directly, and that seam is what gets replaced here.
        with mock.patch.object(hostwin, "_window_rect",
                               return_value=(100, 100, 1380, 900)):
            self.assertEqual(hostwin.window_geometry("omni-farm3"),
                             (100, 100, 1280, 800))


class ChromeElsewhere(unittest.TestCase):

    def test_a_non_win32_backend_declines_with_a_reason(self):
        with mock.patch.object(hostwin, "backend",
                               return_value=hostwin.BACKEND_MACOS):
            result = hostwin.apply_chrome("omni-farm3")
        self.assertFalse(result["applied"])
        self.assertIn("macos", result["reason"].lower())


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


if __name__ == "__main__":
    unittest.main()
