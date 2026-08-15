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
        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
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
