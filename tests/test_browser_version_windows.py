# omnidroid/tests/test_browser_version_windows.py
"""Detecting the installed Chrome's version, on Windows.

REGRESSION. `_browser_version` shelled out to `chrome --version` and parsed
stdout. That works on macOS/Linux, but chrome.exe on Windows is a GUI
subsystem binary: it prints NOTHING and exits, so the probe returned None on
every Windows host. `_resolve_chromedriver` then never passed
--browser-version to Selenium Manager, silently disabling the version pinning
that exists to prevent the fatal "session not created: This version of
ChromeDriver only supports Chrome version N".

Chrome installs its build into a version-named directory beside the exe, which
holds for every Chromium build (Edge, Brave, Chromium) and needs no registry.
"""
import sys
import unittest
from unittest import mock

from omnidroid import accounts


class WindowsVersionProbe(unittest.TestCase):
    def _chrome(self, tmp, versions):
        app = tmp / "Application"
        app.mkdir(parents=True)
        for v in versions:
            (app / v).mkdir()
        exe = app / "chrome.exe"
        exe.write_bytes(b"MZ")
        return exe

    def test_it_reads_the_version_named_folder_beside_the_exe(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as td:
            exe = self._chrome(Path(td), ["151.0.7922.110"])
            self.assertEqual(accounts._windows_browser_version(exe),
                             "151.0.7922.110")

    def test_it_picks_the_newest_when_an_old_build_lingers(self):
        # Chrome leaves the previous build behind until the next restart, and
        # a lexical sort would call 9.x newer than 151.x.
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as td:
            exe = self._chrome(Path(td), ["99.0.1.1", "151.0.7922.110",
                                          "151.0.7922.9"])
            self.assertEqual(accounts._windows_browser_version(exe),
                             "151.0.7922.110")

    def test_non_version_folders_are_ignored(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as td:
            exe = self._chrome(Path(td), ["SetupMetrics", "151.0.7922.110"])
            self.assertEqual(accounts._windows_browser_version(exe),
                             "151.0.7922.110")

    def test_a_missing_directory_is_not_fatal(self):
        # Must not raise. It may still answer from the BLBeacon registry
        # fallback on a machine that really has Chrome, so the contract is
        # "a version string or None", never an exception.
        v = accounts._windows_browser_version(r"X:\nope\chrome.exe")
        self.assertTrue(v is None or accounts._VERSION_RE.fullmatch(v), v)

    def test_it_returns_none_when_nothing_can_answer(self):
        import builtins
        real_import = builtins.__import__

        def no_winreg(name, *a, **k):
            if name == "winreg":
                raise ImportError("no winreg")
            return real_import(name, *a, **k)

        with mock.patch.object(builtins, "__import__", side_effect=no_winreg):
            self.assertIsNone(
                accounts._windows_browser_version(r"X:\nope\chrome.exe"))

    @unittest.skipUnless(sys.platform == "win32", "windows-only path")
    def test_browser_version_does_not_depend_on_stdout_on_windows(self):
        # The whole point: even with the CLI probe returning nothing, a
        # version still comes back.
        import subprocess
        with mock.patch.object(accounts, "_windows_browser_version",
                               return_value="151.0.7922.110"), \
             mock.patch.object(subprocess, "run",
                               side_effect=AssertionError("must not shell out")):
            self.assertEqual(accounts._browser_version(r"C:\c\chrome.exe"),
                             "151.0.7922.110")


class PosixIsUnchanged(unittest.TestCase):
    @unittest.skipIf(sys.platform == "win32", "posix path")
    def test_it_still_parses_the_cli_output(self):
        import subprocess
        r = mock.Mock(stdout="Google Chrome 151.0.7922.110 \n")
        with mock.patch.object(subprocess, "run", return_value=r):
            self.assertEqual(accounts._browser_version("/usr/bin/chrome"),
                             "151.0.7922.110")


if __name__ == "__main__":
    unittest.main()
