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
import json
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
             mock.patch("omnidroid.hostwin.find_window", return_value=1), \
             mock.patch("omnidroid.engine._running_window_bar_pid",
                        return_value=None), \
             mock.patch("omnidroid.hostwin.apply_chrome",
                        side_effect=lambda *a, **k: calls.append("chrome")
                        or {"applied": True, "reason": "", "hwnd": 1}), \
             mock.patch("omnidroid.hostwin.show_qemu_window",
                        side_effect=lambda *a, **k: calls.append("show")), \
             mock.patch("omnidroid.engine._spawn_window_bar",
                        side_effect=lambda *a, **k: calls.append("bar")):
            engine.cmd_view(_args(name="farm3"))
        self.assertEqual(calls, ["chrome", "show", "bar"])

    def test_show_is_given_the_pid_so_a_title_substring_cannot_hit_the_wrong_window(self):
        # e.g. omni-farm3 vs omni-farm30 with several gaming instances up.
        seen = {}
        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.hostwin.find_window", return_value=1), \
             mock.patch("omnidroid.engine._running_window_bar_pid",
                        return_value=None), \
             mock.patch("omnidroid.hostwin.apply_chrome",
                        return_value={"applied": True, "reason": "",
                                      "hwnd": 1}), \
             mock.patch("omnidroid.hostwin.show_qemu_window",
                        side_effect=lambda *a, **k: seen.update(
                            args=a, kwargs=k)), \
             mock.patch("omnidroid.engine._spawn_window_bar"):
            engine.cmd_view(_args(name="farm3"))
        self.assertEqual(seen["kwargs"].get("pid"), 4242)


class AWindowThatIsGenuinelyNotThereFailsFast(unittest.TestCase):
    """A missing window and a window whose STYLING failed are different,
    honest problems -- conflating them told the user "cosmetic problem,
    rendering unaffected" and then opened an empty desktop."""

    def _fail_recorder(self):
        """Wrap engine.fail so the CODE it was called with can be asserted
        on, not just that some SystemExit happened -- same pattern as
        test_session.py's _fail_recorder."""
        calls = []
        orig_fail = engine.fail

        def _wrapped(code, *a, **k):
            calls.append(code)
            return orig_fail(code, *a, **k)
        return calls, mock.patch.object(engine, "fail", side_effect=_wrapped)

    def test_a_missing_window_fails_instead_of_opening_an_empty_desktop(self):
        fail_calls, fail_patch = self._fail_recorder()
        touched = []
        with fail_patch, \
             mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.hostwin.find_window", return_value=None), \
             mock.patch("omnidroid.hostwin.apply_chrome",
                        side_effect=lambda *a, **k: touched.append("chrome")), \
             mock.patch("omnidroid.hostwin.show_qemu_window",
                        side_effect=lambda *a, **k: touched.append("show")), \
             mock.patch("omnidroid.engine._spawn_window_bar",
                        side_effect=lambda *a, **k: touched.append("bar")):
            with self.assertRaises(SystemExit):
                engine.cmd_view(_args(name="farm3"))
        self.assertEqual(fail_calls, ["no_window"])
        # Chrome/show/bar must never run against a window that is not there.
        self.assertEqual(touched, [])

    def test_the_probe_uses_a_short_timeout_not_apply_chromes_twenty_second_default(self):
        # The deleted embedded-viewer probe used timeout=2; apply_chrome's
        # own DEFAULT_TIMEOUT is 20s, sized for "wait at spawn", not for a
        # `view` that must fail fast when the window is simply gone.
        seen = {}

        def fake_find_window(_identity, **kwargs):
            seen["timeout"] = kwargs.get("timeout")
            return None

        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.hostwin.find_window",
                        side_effect=fake_find_window):
            with self.assertRaises(SystemExit):
                engine.cmd_view(_args(name="farm3"))
        self.assertEqual(seen.get("timeout"), 2)


class ASecondViewDoesNotStackASecondBar(unittest.TestCase):
    """The old embedded-viewer path answered a second `view` with "a viewer
    already has this instance's window; bringing it forward" -- removing the
    reparenting hazard is not a reason to remove that handling."""

    def test_a_live_bar_is_brought_forward_and_no_second_one_spawns(self):
        shown = []
        spawned = []
        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.hostwin.find_window", return_value=1), \
             mock.patch("omnidroid.engine._running_window_bar_pid",
                        return_value=9999), \
             mock.patch("omnidroid.hostwin.show_qemu_window",
                        side_effect=lambda *a, **k: shown.append((a, k))), \
             mock.patch("omnidroid.hostwin.apply_chrome") as chrome, \
             mock.patch("omnidroid.engine._spawn_window_bar",
                        side_effect=lambda *a, **k: spawned.append(1)):
            engine.cmd_view(_args(name="farm3"))
        self.assertEqual(len(shown), 1)
        self.assertEqual(shown[0][1].get("pid"), 4242)
        self.assertEqual(spawned, [])   # no second bar
        chrome.assert_not_called()      # no need to restyle an already-open window

    def test_a_dead_pid_file_falls_through_to_a_normal_spawn(self):
        spawned = []
        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.hostwin.find_window", return_value=1), \
             mock.patch("omnidroid.engine._running_window_bar_pid",
                        return_value=None), \
             mock.patch("omnidroid.hostwin.apply_chrome",
                        return_value={"applied": True, "reason": "",
                                      "hwnd": 1}), \
             mock.patch("omnidroid.hostwin.show_qemu_window"), \
             mock.patch("omnidroid.engine._spawn_window_bar",
                        side_effect=lambda *a, **k: spawned.append(1)):
            engine.cmd_view(_args(name="farm3"))
        self.assertEqual(spawned, [1])


class TheAppCanHideAWindowWithoutStopping(unittest.TestCase):
    """The window's own X already only offers hide-or-stop (windowbar.py);
    `--hide` is the same 'hide' reachable from the app side, without the
    user having to find the window on the desktop first -- and without
    stopping (and losing) an instance that took a minute to boot."""

    def test_hide_hides_the_window_and_touches_nothing_else(self):
        hidden = []
        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.engine._running_window_bar_pid",
                        return_value=None), \
             mock.patch("omnidroid.hostwin.hide_qemu_window",
                        side_effect=lambda *a, **k: hidden.append((a, k))), \
             mock.patch("omnidroid.hostwin.find_window") as find_window, \
             mock.patch("omnidroid.hostwin.apply_chrome") as chrome, \
             mock.patch("omnidroid.hostwin.show_qemu_window") as show, \
             mock.patch("omnidroid.engine._spawn_window_bar") as spawn, \
             mock.patch("omnidroid.engine._persist_window_bar_geometry") \
                as persist, \
             mock.patch("omnidroid.engine._kill_window_bar") as kill, \
             mock.patch("omnidroid.engine._clear_window_bar_pid") as clear:
            engine.cmd_view(_args(name="farm3", hide=True))
        self.assertEqual(len(hidden), 1)
        self.assertEqual(hidden[0][0], ("omni-farm3",))
        self.assertEqual(hidden[0][1].get("pid"), 4242)
        # Not apply_chrome's 20s DEFAULT_TIMEOUT: the case that pays this
        # cost is exactly the one where the window is genuinely gone, and a
        # hide has to fail fast there like every other probe in this branch.
        self.assertEqual(hidden[0][1].get("timeout"), 2)
        # Hiding must not probe for the window, restyle it, show it, or spawn
        # a bar -- it is a distinct, minimal action, not a shortcut through
        # the rest of the window path. And with no bar running, none of the
        # live-bar teardown (persist/kill/clear) has anything to do.
        find_window.assert_not_called()
        chrome.assert_not_called()
        show.assert_not_called()
        spawn.assert_not_called()
        persist.assert_not_called()
        kill.assert_not_called()
        clear.assert_not_called()

    def test_hide_while_a_bar_is_live_persists_geometry_kills_it_then_hides(self):
        """The bar is OWNED by QEMU's window, not a CHILD of it -- Windows
        only cascades DESTROY to an OWNED window when the OWNER is
        destroyed (that is what lets `stop` clean the bar up for free); a
        bare hide of the owner gives no such guarantee, so a live bar is
        left on screen, captioning nothing, unless this branch kills it
        itself. And killing it does NOT run `_run_windowbar`'s `finally` --
        Windows does not run them on termination -- so the geometry that
        `finally` would have persisted has to be captured and written HERE,
        strictly before the kill or the hide, or it is lost for good."""
        calls = []
        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.engine._running_window_bar_pid",
                        return_value=9999), \
             mock.patch("omnidroid.engine._persist_window_bar_geometry",
                        side_effect=lambda *a, **k:
                            calls.append(("geometry", a))), \
             mock.patch("omnidroid.engine._kill_window_bar",
                        side_effect=lambda *a, **k: calls.append(("kill", a))), \
             mock.patch("omnidroid.engine._clear_window_bar_pid",
                        side_effect=lambda *a, **k:
                            calls.append(("clear", a))), \
             mock.patch("omnidroid.hostwin.hide_qemu_window",
                        side_effect=lambda *a, **k:
                            calls.append(("hide", a, k)) or True):
            engine.cmd_view(_args(name="farm3", hide=True))
        # Order is the whole point: geometry captured while the window is
        # still visible, THEN the bar dies, THEN its pid file is cleared,
        # THEN the window is hidden.
        self.assertEqual([c[0] for c in calls], ["geometry", "kill", "clear",
                                                  "hide"])
        self.assertEqual(calls[0][1], ("farm3", "omni-farm3", 4242))
        self.assertEqual(calls[1][1], (9999,))
        self.assertEqual(calls[2][1], ("farm3",))

    def test_hide_reports_json_when_asked(self):
        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.engine._running_window_bar_pid",
                        return_value=None), \
             mock.patch("omnidroid.hostwin.hide_qemu_window",
                        return_value=True), \
             mock.patch("omnidroid.engine.emit_json") as emit:
            engine.cmd_view(_args(name="farm3", hide=True, json=True))
        emit.assert_called_once_with(
            {"name": "farm3", "viewer": "window", "hidden": True, "ok": True})

    def test_hide_reports_the_actual_result_not_always_true(self):
        # The window was genuinely gone: hide_qemu_window's own contract is
        # to report False rather than raise, and that must reach the JSON
        # honestly instead of being papered over with a hardcoded True.
        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.engine._running_window_bar_pid",
                        return_value=None), \
             mock.patch("omnidroid.hostwin.hide_qemu_window",
                        return_value=False), \
             mock.patch("omnidroid.engine.emit_json") as emit:
            engine.cmd_view(_args(name="farm3", hide=True, json=True))
        emit.assert_called_once_with(
            {"name": "farm3", "viewer": "window", "hidden": False,
             "ok": True})

    def test_hide_without_json_prints_nothing_that_crashes(self):
        # No --json: cmd_view must still return cleanly rather than raise.
        with mock.patch("omnidroid.engine.load_config", return_value={}), \
             mock.patch("omnidroid.engine.running_pid", return_value=4242), \
             mock.patch("omnidroid.engine.load_account",
                        return_value={"name": "farm3", "vnc_port": 18001}), \
             mock.patch("omnidroid.engine.boot_has_hidden_window",
                        return_value=True), \
             mock.patch("omnidroid.engine._run_record",
                        return_value={"identity": "omni-farm3", "pid": 4242}), \
             mock.patch("omnidroid.engine._running_window_bar_pid",
                        return_value=None), \
             mock.patch("omnidroid.hostwin.hide_qemu_window",
                        return_value=True):
            result = engine.cmd_view(_args(name="farm3", hide=True))
        self.assertIsNone(result)


class TheBarsPidFileTracksItsLifetime(unittest.TestCase):
    """`_running_window_bar_pid` reads what `_write_window_bar_pid` writes
    and what `_run_windowbar`'s `finally` clears -- the plumbing under
    ASecondViewDoesNotStackASecondBar, tested directly against a real
    temp directory rather than mocked out."""

    def setUp(self):
        import tempfile
        from pathlib import Path
        self._tmp = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            self._tmp, ignore_errors=True))

    def test_no_file_means_no_bar(self):
        with mock.patch.object(engine, "runtime_dir",
                               return_value=self._tmp):
            self.assertIsNone(engine._running_window_bar_pid("u1"))

    def test_a_live_pid_is_reported(self):
        with mock.patch.object(engine, "runtime_dir",
                               return_value=self._tmp), \
             mock.patch.object(engine, "pid_alive", return_value=True):
            engine._write_window_bar_pid("u1", 4242)
            self.assertEqual(engine._running_window_bar_pid("u1"), 4242)

    def test_a_dead_pid_reads_as_no_bar(self):
        with mock.patch.object(engine, "runtime_dir",
                               return_value=self._tmp), \
             mock.patch.object(engine, "pid_alive", return_value=False):
            engine._write_window_bar_pid("u1", 4242)
            self.assertIsNone(engine._running_window_bar_pid("u1"))

    def test_a_corrupt_file_reads_as_no_bar_not_a_crash(self):
        with mock.patch.object(engine, "runtime_dir",
                               return_value=self._tmp):
            (self._tmp / "windowbar.pid").write_text("not-a-pid")
            self.assertIsNone(engine._running_window_bar_pid("u1"))

    def test_clearing_removes_the_file_and_is_safe_when_there_is_none(self):
        with mock.patch.object(engine, "runtime_dir",
                               return_value=self._tmp):
            engine._write_window_bar_pid("u1", 4242)
            engine._clear_window_bar_pid("u1")
            self.assertFalse((self._tmp / "windowbar.pid").exists())
            engine._clear_window_bar_pid("u1")   # idempotent, does not raise

    def test_run_windowbar_clears_the_pid_file_on_the_way_out(self):
        with mock.patch.object(engine, "runtime_dir",
                               return_value=self._tmp), \
             mock.patch("omnidroid.windowbar.run_window_bar",
                        return_value=0):
            engine._write_window_bar_pid("u1", 4242)
            a = type("Args", (), {"name": "u1", "identity": "omni-u1",
                                  "title": None, "pid": None})()
            engine._run_windowbar(a)
        self.assertFalse((self._tmp / "windowbar.pid").exists())

    def test_run_windowbar_persists_geometry_before_it_returns(self):
        """So the NEXT `view` restores the position instead of letting QEMU
        pick its own default -- written in the same `finally` that clears
        the pid file, so it happens on every exit path (hide, stop, or the
        window closed some other way)."""
        (self._tmp / "run.json").write_text(
            '{"identity": "omni-u1", "pid": 4242}')
        with mock.patch.object(engine, "runtime_dir",
                               return_value=self._tmp), \
             mock.patch("omnidroid.windowbar.run_window_bar",
                        return_value=0), \
             mock.patch("omnidroid.hostwin.window_geometry",
                        return_value=(10, 20, 800, 600)) as geom:
            a = type("Args", (), {"name": "u1", "identity": "omni-u1",
                                  "title": None, "pid": 4242})()
            engine._run_windowbar(a)
        geom.assert_called_once_with("omni-u1", pid=4242)
        run = json.loads((self._tmp / "run.json").read_text())
        self.assertEqual(run["geometry"], [10, 20, 800, 600])
        # The rest of the record survives -- this is a merge, not a clobber.
        self.assertEqual(run["pid"], 4242)

    def test_a_window_that_is_already_gone_leaves_the_record_untouched(self):
        """Best-effort: a stopped instance's window has already been
        destroyed by the time this runs, and that must not raise or corrupt
        run.json -- it just means the next view opens at QEMU's own default
        position."""
        (self._tmp / "run.json").write_text(
            '{"identity": "omni-u1", "pid": 4242}')
        with mock.patch.object(engine, "runtime_dir",
                               return_value=self._tmp), \
             mock.patch("omnidroid.windowbar.run_window_bar",
                        return_value=0), \
             mock.patch("omnidroid.hostwin.window_geometry",
                        return_value=None):
            a = type("Args", (), {"name": "u1", "identity": "omni-u1",
                                  "title": None, "pid": 4242})()
            engine._run_windowbar(a)   # must not raise
        run = json.loads((self._tmp / "run.json").read_text())
        self.assertNotIn("geometry", run)
        self.assertEqual(run["pid"], 4242)


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
