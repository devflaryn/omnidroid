# omnidroid/tests/test_self_argv_prefix.py
"""Re-invoking the frozen binary for detached children.

The engine spawns two children by re-running itself: the VNC viewer
(`_vncview`) and the autocap recorder (`capture ... --auto`). It built the
command as `[sys.executable, "<subcommand>", ...]`, which is correct for the
standalone omnidroid.exe -- its frozen entry point IS the engine CLI.

It is WRONG when the engine is embedded. omni-exec.exe's entry point is the
GUI, which routes to the engine only when argv[1] == "--omnidroid", so
`omni-exec.exe _vncview --host ...` fell through to the GUI: clicking "Open
viewer" opened A SECOND COPY OF OMNI EXECUTOR instead of the viewer, and
every launch silently spawned another GUI in place of the recorder.
"""
import os
import unittest
from unittest import mock

from omnidroid import config


class SelfArgvPrefix(unittest.TestCase):
    def tearDown(self):
        os.environ.pop("OMNIDROID_SELF_ARGV", None)

    def test_unset_means_my_argv_is_the_engines(self):
        # The standalone omnidroid.exe must keep spawning `<exe> _vncview`.
        os.environ.pop("OMNIDROID_SELF_ARGV", None)
        self.assertEqual(config.self_argv_prefix(), [])

    def test_a_host_can_declare_its_marker(self):
        os.environ["OMNIDROID_SELF_ARGV"] = "--omnidroid"
        self.assertEqual(config.self_argv_prefix(), ["--omnidroid"])

    def test_a_multi_token_prefix_is_split(self):
        os.environ["OMNIDROID_SELF_ARGV"] = "--engine run"
        self.assertEqual(config.self_argv_prefix(), ["--engine", "run"])

    def test_it_is_read_live_not_frozen_at_import(self):
        # configure_engine() sets this AFTER omnidroid is imported.
        os.environ.pop("OMNIDROID_SELF_ARGV", None)
        self.assertEqual(config.self_argv_prefix(), [])
        os.environ["OMNIDROID_SELF_ARGV"] = "--omnidroid"
        self.assertEqual(config.self_argv_prefix(), ["--omnidroid"])


class SpawnedViewerCommand(unittest.TestCase):
    """The command actually handed to Popen for the viewer child."""

    def _spawned_cmd(self, frozen, prefix, tmp):
        from omnidroid import engine
        seen = {}

        class FakeProc:
            pid = 4242

        def fake_popen(cmd, **kw):
            seen["cmd"] = cmd
            return FakeProc()

        if prefix is None:
            os.environ.pop("OMNIDROID_SELF_ARGV", None)
        else:
            os.environ["OMNIDROID_SELF_ARGV"] = prefix

        with mock.patch.object(engine.sys, "frozen", frozen, create=True), \
             mock.patch.object(engine.sys, "executable", r"C:\App\omni-exec.exe"), \
             mock.patch.object(engine.subprocess, "Popen", fake_popen), \
             mock.patch.object(engine, "runtime_dir", return_value=tmp):
            engine._spawn_builtin_viewer("u1", "127.0.0.1", 18001, "t")
        return seen["cmd"]

    def setUp(self):
        import tempfile
        from pathlib import Path
        self._tmp = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            self._tmp, ignore_errors=True))

    def tearDown(self):
        os.environ.pop("OMNIDROID_SELF_ARGV", None)

    def test_an_embedding_host_gets_its_marker_first(self):
        cmd = self._spawned_cmd(True, "--omnidroid", self._tmp)
        self.assertEqual(cmd[0], r"C:\App\omni-exec.exe")
        self.assertEqual(cmd[1], "--omnidroid")
        self.assertEqual(cmd[2], "_vncview")

    def test_the_standalone_exe_is_unchanged(self):
        cmd = self._spawned_cmd(True, None, self._tmp)
        self.assertEqual(cmd[1], "_vncview")

    def test_the_port_and_host_still_reach_the_viewer(self):
        cmd = self._spawned_cmd(True, "--omnidroid", self._tmp)
        self.assertIn("--host", cmd)
        self.assertIn("127.0.0.1", cmd)
        self.assertIn("18001", cmd)


class SpawnedWindowBarCommand(unittest.TestCase):
    """The command actually handed to Popen for the window-bar child.

    This is the highest-consequence code in the gaming-window task: invisible
    in every mocked-out test of cmd_view, fatal in every shipped release if
    it ever regresses to the dev-only `[sys.executable, "-m", "omnidroid",
    ...]` shape -- that shape works when this file is run as a script and
    fails silently in the PyInstaller binary, exactly the failure
    SpawnedViewerCommand above exists to pin for the RFB viewer. Mirrors it
    line for line for `_spawn_window_bar`.
    """

    def _spawned_cmd(self, frozen, prefix, tmp):
        from omnidroid import engine
        seen = {}

        class FakeProc:
            pid = 4343

        def fake_popen(cmd, **kw):
            seen["cmd"] = cmd
            return FakeProc()

        if prefix is None:
            os.environ.pop("OMNIDROID_SELF_ARGV", None)
        else:
            os.environ["OMNIDROID_SELF_ARGV"] = prefix

        with mock.patch.object(engine.sys, "frozen", frozen, create=True), \
             mock.patch.object(engine.sys, "executable", r"C:\App\omni-exec.exe"), \
             mock.patch.object(engine.subprocess, "Popen", fake_popen), \
             mock.patch.object(engine, "runtime_dir", return_value=tmp):
            engine._spawn_window_bar("u1", "omni: u1", "omni-u1", 4242)
        return seen["cmd"]

    def setUp(self):
        import tempfile
        from pathlib import Path
        self._tmp = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            self._tmp, ignore_errors=True))

    def tearDown(self):
        os.environ.pop("OMNIDROID_SELF_ARGV", None)

    def test_an_embedding_host_gets_its_marker_first(self):
        cmd = self._spawned_cmd(True, "--omnidroid", self._tmp)
        self.assertEqual(cmd[0], r"C:\App\omni-exec.exe")
        self.assertEqual(cmd[1], "--omnidroid")
        self.assertEqual(cmd[2], "_windowbar")

    def test_the_standalone_exe_is_unchanged(self):
        cmd = self._spawned_cmd(True, None, self._tmp)
        self.assertEqual(cmd[1], "_windowbar")

    def test_the_identity_title_and_pid_still_reach_the_bar(self):
        cmd = self._spawned_cmd(True, "--omnidroid", self._tmp)
        self.assertIn("--identity", cmd)
        self.assertIn("omni-u1", cmd)
        self.assertIn("--pid", cmd)
        self.assertIn("4242", cmd)

    def test_the_non_frozen_dev_shape_reinvokes_this_file_not_dash_m(self):
        # NOT [sys.executable, "-m", "omnidroid", ...] -- that shape has no
        # entry point once frozen. Same non-frozen shape as _vncview/_embed
        # style subcommands: re-run engine.py itself with the subcommand as
        # argv.
        cmd = self._spawned_cmd(False, None, self._tmp)
        self.assertEqual(cmd[0], r"C:\App\omni-exec.exe")
        self.assertNotIn("-m", cmd)
        self.assertTrue(cmd[1].endswith("engine.py"), cmd[1])
        self.assertEqual(cmd[2], "_windowbar")


if __name__ == "__main__":
    unittest.main()
