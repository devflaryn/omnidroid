#!/usr/bin/env python3
"""Hiding the QEMU window on three platforms, and admitting it on none.

    python3 -m pytest tests/test_hostwin_backends.py -q

The product promises a view the user can toggle, on every host. Hiding the REAL
window is always better than a copy of it -- same pixels, drawn once, no encode
-- so `hostwin` tries that first everywhere and the VNC viewer is the fallback.
Each platform gives a different amount of rope:

  * Windows: ShowWindow, and the window keeps rendering while hidden (measured,
    303 frames / 30 s). This is the load-bearing path and it is untouched here.
  * Linux/X11: unmap the window, found by its _NET_WM_NAME. xdotool does it
    itself, wmctrl only asks the window manager to, python-xlib is used if the
    host happens to have it.
  * macOS: there is no public API to hide ONE window of another process, so the
    whole application is hidden by its unix id through System Events -- and
    that needs a TCC permission the user may never have granted.

Two failure modes matter more than the successes, and most of this file is
about them:

  1. NOTHING MAY RAISE. Every one of these runs inside a boot; a visible window
     is cosmetic, an exception is a launch that died for a cosmetic problem.
  2. NOTHING MAY PRETEND. On Wayland no client can unmap another's window --
     protocol design, not a missing feature -- so the answer there is "none"
     plus a reason, never a cheerful True over a window still on screen.

The Linux and macOS backends cannot be EXECUTED on a Windows box, so what is
pinned here is the argv each one builds. That is the part that silently rots.
"""
import contextlib
import os
import sys
import threading
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import embedview, hostwin  # noqa: E402


class Recorder:
    """Stands in for `hostwin._run`: records argv, answers from a table.

    Keyed by the whole argv tuple or just the program name; a callable value
    gets the argv and can answer differently per call, which is how the macOS
    read-back and xdotool's pid fallback are exercised."""

    def __init__(self, replies=None):
        self.calls = []
        self._replies = replies or {}

    def __call__(self, argv, timeout=None):
        argv = list(argv)
        self.calls.append(argv)
        reply = self._replies.get(tuple(argv))
        if reply is None:
            reply = self._replies.get(argv[0], (0, "", ""))
        return reply(argv) if callable(reply) else reply


class _Host:
    def __init__(self, which, run):
        self.which = which
        self.run = run

    @property
    def argvs(self):
        return self.run.calls if self.run is not None else []


@contextlib.contextmanager
def host(win=False, linux=False, mac=False, tools=(), env=None, xlib=False,
         run=None):
    """Pretend to be a platform holding a given set of helper tools.

    The backend probe is MEMOISED (it must be: `keep_hidden` would otherwise
    pay a PATH scan every poll for two and a half minutes), so any test that
    changes the answer has to start from a cold cache -- the same rule
    qemu_proc's _HELP_CACHE has in test_gpu_display.py.
    """
    hostwin._BACKEND_CACHE.clear()
    hostwin._DENIED.clear()
    which = mock.MagicMock(
        side_effect=lambda t: f"/usr/bin/{t}" if t in tools else None)
    with contextlib.ExitStack() as stack:
        stack.enter_context(mock.patch.object(hostwin, "IS_WINDOWS", win))
        stack.enter_context(mock.patch.object(hostwin, "IS_LINUX", linux))
        stack.enter_context(mock.patch.object(hostwin, "IS_MACOS", mac))
        stack.enter_context(mock.patch.object(hostwin, "_which", which))
        stack.enter_context(mock.patch.object(
            hostwin, "_import_xlib",
            return_value=mock.MagicMock() if xlib else None))
        stack.enter_context(mock.patch.dict(
            os.environ, {"DISPLAY": ":0"} if env is None else env, clear=True))
        if run is not None:
            stack.enter_context(mock.patch.object(hostwin, "_run", run))
        try:
            yield _Host(which, run)
        finally:
            hostwin._BACKEND_CACHE.clear()
            hostwin._DENIED.clear()


class BackendSelection(unittest.TestCase):
    """One name per mechanism, chosen by a probe, cached for the process."""

    def test_windows_keeps_the_measured_win32_path(self):
        with host(win=True):
            self.assertEqual(hostwin.backend(), "win32")
            self.assertEqual(hostwin.backend_reason(), "")
            self.assertTrue(hostwin.can_hide())

    def test_macos_hides_the_application_through_system_events(self):
        with host(mac=True, tools=("osascript",)):
            self.assertEqual(hostwin.backend(), "macos-systemevents")
            self.assertTrue(hostwin.can_hide())

    def test_macos_without_osascript_has_nothing_to_ask(self):
        with host(mac=True, tools=()):
            self.assertEqual(hostwin.backend(), "none")
            self.assertIn("osascript", hostwin.backend_reason())

    def test_linux_prefers_xdotool_because_it_unmaps_for_real(self):
        with host(linux=True, tools=("xdotool", "wmctrl"), xlib=True):
            self.assertEqual(hostwin.backend(), "x11-xdotool")

    def test_an_absent_tool_falls_through_to_wmctrl(self):
        with host(linux=True, tools=("wmctrl",), xlib=True):
            self.assertEqual(hostwin.backend(), "x11-wmctrl")

    def test_xlib_is_the_last_resort_and_only_if_importable(self):
        with host(linux=True, tools=(), xlib=True):
            self.assertEqual(hostwin.backend(), "x11-xlib")

    def test_a_linux_box_with_none_of_them_says_what_to_install(self):
        with host(linux=True, tools=(), xlib=False):
            self.assertEqual(hostwin.backend(), "none")
            self.assertIn("xdotool", hostwin.backend_reason())

    def test_wayland_refuses_instead_of_pretending(self):
        # xdotool IS installed here and would still be useless: a Wayland
        # client cannot touch another client's surface, and QEMU's GTK display
        # on that session has no X11 window to find in the first place.
        with host(linux=True, tools=("xdotool", "wmctrl"), xlib=True,
                  env={"DISPLAY": ":0", "WAYLAND_DISPLAY": "wayland-0"}):
            self.assertEqual(hostwin.backend(), "none")
            self.assertIn("Wayland", hostwin.backend_reason())
            self.assertFalse(hostwin.can_hide())

    def test_a_wayland_session_type_counts_even_without_the_socket(self):
        with host(linux=True, tools=("xdotool",),
                  env={"DISPLAY": ":0", "XDG_SESSION_TYPE": "wayland"}):
            self.assertEqual(hostwin.backend(), "none")

    def test_no_display_at_all_is_an_ssh_session_not_a_bug(self):
        with host(linux=True, tools=("xdotool",), env={}):
            self.assertEqual(hostwin.backend(), "none")
            self.assertIn("DISPLAY", hostwin.backend_reason())

    def test_the_probe_is_paid_for_once_per_process(self):
        with host(linux=True, tools=("xdotool",)) as h:
            hostwin.backend()
            hostwin.backend()
            hostwin.can_hide()
            hostwin.backend_reason()
        self.assertEqual(h.which.call_count, 1)

    def test_this_host_either_has_a_backend_or_explains_itself(self):
        # Unpatched, on whatever is actually running the suite.
        self.assertTrue(hostwin.backend() != "none"
                        or hostwin.backend_reason())


class NothingHappensAndNothingRaises(unittest.TestCase):
    """`backend() == "none"` is a normal state, not an error state."""

    def test_every_entry_point_is_falsy_without_a_backend(self):
        with host(linux=True, tools=(), xlib=False):
            self.assertEqual(hostwin.backend(), "none")
            self.assertFalse(hostwin.can_hide())
            self.assertIsNone(hostwin.find_window("omni-u1"))
            self.assertIsNone(hostwin.find_window("omni-u1", pid=4242))
            self.assertFalse(hostwin.hide_qemu_window("omni-u1"))
            self.assertFalse(hostwin.show_qemu_window("omni-u1"))
            self.assertFalse(hostwin.window_is_visible("omni-u1"))
            self.assertFalse(hostwin.window_is_embedded("omni-u1"))
            self.assertIsNone(hostwin.keep_hidden("omni-u1"))

    def test_the_full_timeout_is_not_waited_out_for_a_known_no(self):
        # find_window's 20 s bound exists for a window that is coming. When no
        # mechanism exists nothing is coming, and a boot must not stop for it.
        with host(linux=True, tools=(), xlib=False):
            start = __import__("time").monotonic()
            hostwin.find_window("omni-u1")
            self.assertLess(__import__("time").monotonic() - start, 1.0)

    def test_a_probe_that_raises_is_still_only_a_no(self):
        with host(linux=True, tools=("xdotool",)):
            with mock.patch.object(hostwin, "_detect_backend",
                                   side_effect=OSError("PATH went away")):
                hostwin._BACKEND_CACHE.clear()
                self.assertEqual(hostwin.backend(), "none")
                self.assertFalse(hostwin.can_hide())
                self.assertIn("probe", hostwin.backend_reason())

    def test_nothing_is_attempted_without_an_identity_or_a_pid(self):
        with host(linux=True, tools=("xdotool",), run=Recorder()) as h:
            self.assertIsNone(hostwin.find_window(""))
            self.assertIsNone(hostwin.keep_hidden(""))
        self.assertEqual(h.argvs, [])


class MacOsHidesTheApplication(unittest.TestCase):
    """There is no per-window API for another process on macOS, so the unit is
    the application and the handle is the pid. These pin the AppleScript."""

    READ = ('tell application "System Events" to get visible of '
            '(first process whose unix id is 4242)')
    HIDE = ('tell application "System Events" to set visible of '
            '(first process whose unix id is 4242) to false')
    SHOW = ('tell application "System Events" to set visible of '
            '(first process whose unix id is 4242) to true')

    @staticmethod
    def _app(visible=True):
        """An osascript that behaves like System Events driving one app."""
        state = {"visible": visible}

        def reply(argv):
            script = argv[2]
            if "set visible" in script:
                state["visible"] = script.endswith("to true")
                return 0, "", ""
            return 0, "true\n" if state["visible"] else "false\n", ""

        return {"osascript": reply}, state

    def test_hiding_builds_exactly_this_argv_and_verifies_it(self):
        replies, state = self._app(visible=True)
        with host(mac=True, tools=("osascript",),
                  run=Recorder(replies)) as h:
            self.assertTrue(
                hostwin.hide_qemu_window("omni-u1", timeout=0, pid=4242))
        self.assertFalse(state["visible"])
        # Find (which is a visibility read), set, and read BACK: the set
        # succeeds against an app with no windows and one that declines to
        # deactivate, so the exit code alone never answers the question asked.
        self.assertEqual(h.argvs, [["osascript", "-e", self.READ],
                                   ["osascript", "-e", self.HIDE],
                                   ["osascript", "-e", self.READ]])

    def test_showing_builds_the_mirror_of_it(self):
        replies, state = self._app(visible=False)
        with host(mac=True, tools=("osascript",),
                  run=Recorder(replies)) as h:
            self.assertTrue(
                hostwin.show_qemu_window("omni-u1", timeout=0, pid=4242))
        self.assertTrue(state["visible"])
        self.assertEqual(h.argvs, [["osascript", "-e", self.READ],
                                   ["osascript", "-e", self.SHOW],
                                   ["osascript", "-e", self.READ]])

    def test_a_hide_that_did_not_take_is_reported_as_false(self):
        # System Events said yes and the app is still on screen. That is a
        # False, not a True: the whole point is what the user can see.
        with host(mac=True, tools=("osascript",),
                  run=Recorder({"osascript": (0, "true\n", "")})):
            self.assertFalse(
                hostwin.hide_qemu_window("omni-u1", timeout=0, pid=4242))

    def test_an_identity_alone_resolves_through_pgrep(self):
        replies, _state = self._app()
        replies["pgrep"] = (0, "4242\n", "")
        with host(mac=True, tools=("osascript",),
                  run=Recorder(replies)) as h:
            self.assertEqual(hostwin.find_window("omni-u1", timeout=0), 4242)
        self.assertEqual(h.argvs[0], ["pgrep", "-f", "omni-u1"])

    def test_we_never_ask_system_events_to_hide_ourselves(self):
        # `pgrep -f` matches whole command lines, and an engine invoked with
        # the identity on its own is a match for itself.
        replies, _state = self._app()
        replies["pgrep"] = (0, f"{os.getpid()}\n", "")
        with host(mac=True, tools=("osascript",), run=Recorder(replies)):
            self.assertIsNone(hostwin.find_window("omni-u1", timeout=0))

    def test_a_permission_refusal_is_reported_and_not_retried(self):
        err = ("34:132: execution error: System Events got an error: "
               "osascript is not allowed assistive access. (-1719)")
        with host(mac=True, tools=("osascript",),
                  run=Recorder({"osascript": (1, "", err)})):
            self.assertFalse(
                hostwin.hide_qemu_window("omni-u1", timeout=0, pid=4242))
            self.assertFalse(hostwin.can_hide())
            self.assertIn("Accessibility", hostwin.backend_reason())
            # ...and the watcher does not start, so a denied host does not
            # spend two and a half minutes re-asking a settled question.
            self.assertIsNone(hostwin.keep_hidden("omni-u1"))

    def test_automation_permission_counts_as_a_refusal_too(self):
        err = ("execution error: Not authorized to send Apple events to "
               "System Events. (-1743)")
        with host(mac=True, tools=("osascript",),
                  run=Recorder({"osascript": (1, "", err)})):
            self.assertFalse(
                hostwin.hide_qemu_window("omni-u1", timeout=0, pid=4242))
            self.assertFalse(hostwin.can_hide())

    def test_a_stopped_instance_is_not_a_permissions_problem(self):
        # -1719 doubles as "Can't get <object>", which is exactly what a pid
        # that has exited returns. Sending the user to System Settings over a
        # stopped instance would be a worse bug than the one being detected,
        # so the match is on the TEXT and never on the error number.
        err = ('execution error: System Events got an error: Can’t get '
               'process 1 whose unix id = 4242. (-1719)')
        with host(mac=True, tools=("osascript",),
                  run=Recorder({"osascript": (1, "", err)})):
            self.assertFalse(
                hostwin.hide_qemu_window("omni-u1", timeout=0, pid=4242))
            self.assertTrue(hostwin.can_hide())
            self.assertEqual(hostwin.backend_reason(), "")


class LinuxUnmapsTheWindow(unittest.TestCase):
    """xdotool first: it unmaps the window itself rather than asking the window
    manager to honour a state it is free to ignore."""

    def _x(self, replies=None):
        return host(linux=True, tools=("xdotool",),
                    run=Recorder(replies or {"xdotool": (0, "12345\n", "")}))

    def test_find_matches_the_name_qemu_was_given(self):
        with self._x() as h:
            self.assertEqual(hostwin.find_window("omni-u1", timeout=0),
                             "12345")
        self.assertEqual(h.argvs, [["xdotool", "search", "--name", "omni-u1"]])

    def test_a_pid_is_anded_with_the_name_never_ored(self):
        # Without --all, xdotool ORs its criteria: `--pid X --name Y` would
        # match every window of that process OR every window with that name,
        # which on a host running several instances is the wrong window.
        with self._x() as h:
            hostwin.find_window("omni-u1", timeout=0, pid=4242)
        self.assertEqual(h.argvs[0], ["xdotool", "search", "--all", "--pid",
                                      "4242", "--name", "omni-u1"])

    def test_a_window_without_net_wm_pid_is_still_found_by_name(self):
        # GTK sets _NET_WM_PID, but a QEMU behind a wrapper may have none, and
        # "no pid property" must not read as "the window is gone".
        def reply(argv):
            return (0, "", "") if "--pid" in argv else (0, "12345\n", "")

        with self._x({"xdotool": reply}) as h:
            self.assertEqual(
                hostwin.find_window("omni-u1", timeout=0, pid=4242), "12345")
        self.assertEqual(len(h.argvs), 2)
        self.assertEqual(h.argvs[1], ["xdotool", "search", "--name",
                                      "omni-u1"])

    def test_hide_unmaps_and_show_maps(self):
        with self._x() as h:
            self.assertTrue(hostwin.hide_qemu_window("omni-u1", timeout=0))
            self.assertTrue(hostwin.show_qemu_window("omni-u1", timeout=0))
        self.assertIn(["xdotool", "windowunmap", "12345"], h.argvs)
        self.assertIn(["xdotool", "windowmap", "12345"], h.argvs)

    def test_visibility_asks_the_search_because_there_is_no_other_query(self):
        with self._x() as h:
            self.assertTrue(hostwin.window_is_visible("omni-u1"))
        self.assertIn(["xdotool", "search", "--onlyvisible", "--name",
                       "omni-u1"], h.argvs)

    def test_an_unmapped_window_is_not_visible(self):
        def reply(argv):
            return (0, "", "") if "--onlyvisible" in argv else (0, "12345", "")

        with self._x({"xdotool": reply}):
            self.assertFalse(hostwin.window_is_visible("omni-u1"))

    def test_a_regex_metacharacter_in_the_account_name_is_escaped(self):
        # --name is a POSIX extended regex, so an unescaped `.` matches the
        # wrong instance's window.
        self.assertEqual(hostwin._x11_name_pattern("omni-u.1+x"),
                         "omni-u\\.1\\+x")

    def test_a_hyphen_is_left_alone(self):
        # re.escape() would write `\-`, which POSIX ERE does not define -- and
        # every identity this project builds contains one.
        self.assertEqual(hostwin._x11_name_pattern("omni-u1"), "omni-u1")


class LinuxThroughWmctrl(unittest.TestCase):
    LIST = (0, "0x0400000a  0 4242   thishost omni-u1\n"
               "0x0400000b  0 99     thishost some other window\n", "")

    def _w(self, replies=None, tools=("wmctrl",)):
        base = {"wmctrl": self.LIST}
        base.update(replies or {})
        return host(linux=True, tools=tools, run=Recorder(base))

    def test_find_reads_the_pid_column_wmctrl_only_prints_with_p(self):
        with self._w() as h:
            self.assertEqual(
                hostwin.find_window("omni-u1", timeout=0, pid=4242),
                "0x0400000a")
        self.assertEqual(h.argvs[0], ["wmctrl", "-l", "-p"])

    def test_another_window_of_another_process_is_not_it(self):
        with self._w():
            self.assertIsNone(hostwin.find_window("omni-u1", timeout=0,
                                                  pid=99))

    def test_hide_and_show_toggle_the_hidden_state_by_id(self):
        # `-i` is not optional: without it `-r` treats its argument as a title
        # to match, and the window id would be searched for as literal text.
        with self._w() as h:
            self.assertTrue(hostwin.hide_qemu_window("omni-u1", timeout=0))
            self.assertTrue(hostwin.show_qemu_window("omni-u1", timeout=0))
        self.assertIn(["wmctrl", "-i", "-r", "0x0400000a", "-b",
                       "add,hidden"], h.argvs)
        self.assertIn(["wmctrl", "-i", "-r", "0x0400000a", "-b",
                       "remove,hidden"], h.argvs)

    def test_visibility_comes_from_xprop_because_wmctrl_cannot_say(self):
        state = {"xprop": (0, "WM_STATE(WM_STATE):\n\t\twindow state: "
                              "Normal\n\t\ticon window: 0x0\n", "")}
        with self._w(state, tools=("wmctrl", "xprop")) as h:
            self.assertTrue(hostwin.window_is_visible("omni-u1"))
        self.assertIn(["xprop", "-id", "0x0400000a", "WM_STATE"], h.argvs)

    def test_an_iconified_window_is_not_visible(self):
        state = {"xprop": (0, "WM_STATE(WM_STATE):\n\t\twindow state: "
                              "Iconic\n", "")}
        with self._w(state, tools=("wmctrl", "xprop")):
            self.assertFalse(hostwin.window_is_visible("omni-u1"))

    def test_without_xprop_it_admits_it_cannot_tell(self):
        # False, not a guess: a wrong True costs a wmctrl fork every poll for
        # two and a half minutes, a wrong False costs one missed re-hide.
        with self._w() as h:
            self.assertFalse(hostwin.window_is_visible("omni-u1"))
        self.assertNotIn("xprop", [a[0] for a in h.argvs])


class EmbeddingIsStillWindowsOnly(unittest.TestCase):
    """Hiding went cross-platform; reparenting cannot, and says why."""

    def test_available_is_unchanged(self):
        with host(win=True):
            pass
        self.assertEqual(embedview.available(), embedview.IS_WINDOWS)

    def test_windows_needs_no_reason_because_it_works(self):
        with mock.patch.object(embedview, "IS_WINDOWS", True):
            self.assertEqual(embedview.available_reason(), "")

    def test_macos_names_the_private_api_it_refuses_to_use(self):
        with mock.patch.object(embedview, "IS_WINDOWS", False), \
             mock.patch.object(embedview, "IS_MACOS", True), \
             mock.patch.object(embedview, "IS_LINUX", False):
            self.assertIn("CGSSetWindowParent", embedview.available_reason())

    def test_linux_says_it_does_not_need_it(self):
        with mock.patch.object(embedview, "IS_WINDOWS", False), \
             mock.patch.object(embedview, "IS_MACOS", False), \
             mock.patch.object(embedview, "IS_LINUX", True):
            self.assertIn("egl-headless", embedview.available_reason())

    def test_nothing_is_ever_embedded_off_windows(self):
        # window_is_embedded is the "a viewer already has it" state, and it
        # cannot arise where nothing reparents.
        with host(linux=True, tools=("xdotool",),
                  run=Recorder({"xdotool": (0, "12345\n", "")})) as h:
            self.assertFalse(hostwin.window_is_embedded("omni-u1"))
        self.assertEqual(h.argvs, [])


class KeepHiddenKeepsItsContract(unittest.TestCase):
    """A bounded daemon thread with a stop(), or None. Unchanged since the
    Windows-only version -- the backends underneath it moved, not this."""

    def test_it_returns_none_when_hiding_is_impossible(self):
        with host(linux=True, tools=(), xlib=False):
            self.assertIsNone(hostwin.keep_hidden("omni-u1"))
        with host(linux=True, tools=("xdotool",),
                  env={"WAYLAND_DISPLAY": "wayland-0"}):
            self.assertIsNone(hostwin.keep_hidden("omni-u1"))

    def test_it_returns_none_without_an_identity(self):
        with host(win=True):
            self.assertIsNone(hostwin.keep_hidden(""))

    def test_it_watches_on_a_daemon_thread_that_stops(self):
        with host(linux=True, tools=("xdotool",),
                  run=Recorder({"xdotool": (0, "", "")})):
            stop = hostwin.keep_hidden("omni-u1", seconds=30)
            self.assertTrue(callable(stop))
            watcher = [t for t in threading.enumerate()
                       if t.name == "hide-omni-u1"]
            self.assertEqual(len(watcher), 1)
            self.assertTrue(watcher[0].daemon)
            stop()
            watcher[0].join(timeout=5)
            self.assertFalse(watcher[0].is_alive())


if __name__ == "__main__":
    unittest.main()
