#!/usr/bin/env python3
"""The window is hidden at boot, restyled, and shown with OUR bar above it.

    python3 -m pytest tests/test_hidden_window_viewer.py -q

On Windows the window is the price of the GPU: every windowless GL display
there takes its context from ANGLE at ES 2.0 and virglrenderer cannot serve a
scanout from it (SET_SCANOUT rejected, 602 rejections in one boot,
totalFrames = 0, screen black), while the GTK path goes through WGL and works.
So the window exists, it is hidden through the boot (qemu_proc spawns it and
then hides it -- GTK re-shows it during early boot, so it has to be re-hidden
for a while), the boot records that it did so (`boot_has_hidden_window`), and
`view` restyles it and shows it with our bar above it.

What is GONE is the reparenting: nothing is ever a child of anything, so the
force-killed-viewer failure that used to be reported here cannot happen any
more and is not reported (see TheRemovedFailureModeIsReallyGone below). The
bar is owned BY the guest window instead -- see tests/test_windowbar.py.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import engine
from omnidroid import hostwin, qemu_proc


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


class WhenTheWindowIsHidden(unittest.TestCase):
    """`_hide_window_if_wanted` decides, and it only ever has three answers.

    Recovered from the pre-Task-5 version of this file (commit ef52be7):
    this is qemu_proc's spawn-time hiding, unrelated to the deleted embedded
    viewer, and Task 5 does not touch it -- `view` restyles and re-shows the
    same window this function hid."""

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
    """`boot_has_hidden_window` is the switch `cmd_view` uses to pick the
    restyle-and-show path over the VNC path -- still live code, still owned
    by this file. Recovered from ef52be7."""

    def _record(self, run):
        with mock.patch.object(engine, "_run_record", return_value=run):
            return engine.boot_has_hidden_window("u1")

    def test_a_hidden_window_boot_is_recognised(self):
        self.assertTrue(self._record({"native_window": True,
                                      "window_hidden": True}))

    def test_a_visible_window_boot_is_not(self):
        # The window was never hidden -- the user asked to see it
        # (`--gpu window`), and it already is.
        self.assertFalse(self._record({"native_window": True,
                                       "window_hidden": False}))

    def test_a_windowless_boot_is_not(self):
        self.assertFalse(self._record({"native_window": False,
                                       "window_hidden": False}))

    def test_an_older_run_record_without_the_field_is_not(self):
        # Boots from before this feature have no `window_hidden` key at all,
        # and must fall through to the VNC path they were written for.
        self.assertFalse(self._record({"native_window": True}))


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
