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


if __name__ == "__main__":
    unittest.main()
