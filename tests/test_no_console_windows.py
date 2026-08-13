# omnidroid/tests/test_no_console_windows.py
"""Child processes must not flash console windows inside the GUI.

A single launch runs dozens of short-lived console tools (adb is polled once
a second by wait_for_boot, plus qemu-img, e2fsprogs...). When the parent has
no console of its own -- exactly the case inside omni-exec.exe, which is
built windowed -- Windows hands each child a BRAND NEW console window, and
the user watches terminals strobe across the screen for the whole boot.

The default is scoped to the no-console case on purpose: a CLI run in a
terminal must behave byte-for-byte as before.
"""
import subprocess
import sys
import unittest
from unittest import mock

from omnidroid import config


class NoConsoleDefault(unittest.TestCase):
    def setUp(self):
        # Always restore the real Popen: this patches a global.
        self._orig_init = subprocess.Popen.__init__
        self._had_flag = getattr(subprocess.Popen, "_omni_no_window", False)

    def tearDown(self):
        subprocess.Popen.__init__ = self._orig_init
        if not self._had_flag and hasattr(subprocess.Popen, "_omni_no_window"):
            del subprocess.Popen._omni_no_window

    def _install(self, *, windows, has_console):
        if hasattr(subprocess.Popen, "_omni_no_window"):
            del subprocess.Popen._omni_no_window
        with mock.patch.object(config, "IS_WINDOWS", windows), \
             mock.patch.object(config, "_has_own_console",
                               return_value=has_console):
            return config.install_no_console_default()

    def _spawn_and_capture_kwargs(self, **popen_kwargs):
        """Install the default over a stub Popen and report what it injected."""
        seen = {}
        subprocess.Popen.__init__ = lambda s, *a, **k: seen.update(k)
        self._install(windows=True, has_console=False)
        subprocess.Popen(["cmd"], **popen_kwargs)   # tearDown restores
        return seen

    def test_a_windowed_parent_gets_the_no_window_default(self):
        seen = self._spawn_and_capture_kwargs()
        self.assertEqual(seen.get("creationflags"), config.CREATE_NO_WINDOW)

    def test_a_console_parent_is_left_alone(self):
        # In a terminal the child inherits that console: no window is created
        # and nothing should change.
        self.assertFalse(self._install(windows=True, has_console=True))

    def test_non_windows_is_a_no_op(self):
        self.assertFalse(self._install(windows=False, has_console=False))

    def test_it_is_idempotent(self):
        self.assertTrue(self._install(windows=True, has_console=False))
        with mock.patch.object(config, "IS_WINDOWS", True), \
             mock.patch.object(config, "_has_own_console", return_value=False):
            self.assertFalse(config.install_no_console_default())

    def test_explicit_creationflags_win(self):
        # The QEMU spawns pass DETACHED_PROCESS, which CREATE_NO_WINDOW would
        # conflict with -- Windows ignores it beside DETACHED/NEW_CONSOLE.
        seen = self._spawn_and_capture_kwargs(creationflags=0x00000008)
        self.assertEqual(seen.get("creationflags"), 0x00000008)


class ConsoleProbe(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "win32", "windows-only")
    def test_the_probe_answers_without_raising(self):
        self.assertIn(config._has_own_console(), (True, False))

    def test_a_broken_probe_assumes_console(self):
        # Never let a ctypes failure change spawn behaviour: assuming "we have
        # a console" is the direction that changes nothing.
        with mock.patch("ctypes.windll", side_effect=AttributeError,
                        create=True):
            self.assertTrue(config._has_own_console())


if __name__ == "__main__":
    unittest.main()
