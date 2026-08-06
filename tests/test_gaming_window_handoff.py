#!/usr/bin/env python3
"""One instance, one window: the VNC viewer stands down for a native window.

    python3 tests/test_gaming_window_handoff.py

`omni start` opens the built-in Tk/RFB viewer by default for interactive use.
A gaming boot that also opens a QEMU native window would therefore put TWO
windows on screen for one instance — and the VNC one is the laggy one, so it
is the one a user would naturally click on and then judge the mode by.

The instance still RUNS the VNC server in every mode (screenshot, autocap and
the omnidroid-input skill attach to it). What stands down is only the second
viewer, and only when a native window actually opened — a gaming boot that
degraded to headless must still get its viewer, or it becomes unwatchable.
"""
import json
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402

HEADLESS_CMD = ["qemu", "-device", "virtio-gpu-pci", "-display", "none",
                "-vnc", "127.0.0.1:1"]
WINDOW_CMD = ["qemu", "-device", "virtio-gpu-pci", "-display", "cocoa",
              "-vnc", "127.0.0.1:1"]
GL_CMD = ["qemu", "-device", "virtio-gpu-gl-pci", "-display", "cocoa,gl=on",
          "-vnc", "127.0.0.1:1"]


class DetectsAWindowInTheCommand(unittest.TestCase):
    """Read off the command actually handed to QEMU rather than re-deciding.
    The command is the ground truth and cannot drift from what is running."""

    def test_headless_command_has_no_window(self):
        self.assertFalse(qemu_proc.command_opens_a_window(HEADLESS_CMD))

    def test_plain_window_command_has_one(self):
        self.assertTrue(qemu_proc.command_opens_a_window(WINDOW_CMD))

    def test_accelerated_command_has_one(self):
        self.assertTrue(qemu_proc.command_opens_a_window(GL_CMD))

    def test_a_command_with_no_display_flag_has_none(self):
        self.assertFalse(qemu_proc.command_opens_a_window(["qemu", "-m", "1"]))


class RecordsItForTheCaller(unittest.TestCase):
    def _run_json(self, cmd):
        written = {}

        class FakePath:
            def __init__(self, name):
                self.name = name

            def __truediv__(self, other):
                return FakePath(other)

            def mkdir(self, **k):
                pass

            def write_text(self, text):
                written[self.name] = text

        # spawn_qemu does `from omnidroid.runtime import runtime_dir` inside
        # the function body, so the runtime module is what must be patched.
        from omnidroid import runtime as _rt
        with mock.patch.object(_rt, "runtime_dir",
                               side_effect=lambda n: FakePath("d")), \
             mock.patch.object(qemu_proc, "qemu_command", return_value=cmd), \
             mock.patch.object(qemu_proc, "check_accel"), \
             mock.patch("builtins.open", mock.mock_open()), \
             mock.patch.object(qemu_proc.subprocess, "Popen",
                               return_value=mock.Mock(pid=99)):
            qemu_proc.spawn_qemu({"name": "u1", "base": "arm", "adb_port": 1,
                                  "qmp_port": 2, "vnc_port": 3}, {}, False,
                                 mode={"name": "gaming"})
        return json.loads(written["run.json"])

    def test_run_json_records_a_native_window(self):
        self.assertTrue(self._run_json(WINDOW_CMD)["native_window"])

    def test_run_json_records_a_headless_boot(self):
        self.assertFalse(self._run_json(HEADLESS_CMD)["native_window"])


class TheViewerDecision(unittest.TestCase):
    def test_a_native_window_suppresses_the_viewer(self):
        self.assertFalse(omni._want_vnc_viewer(
            native_window=True, explicit_window=False, json_mode=False,
            no_window=False))

    def test_a_degraded_gaming_boot_still_gets_a_viewer(self):
        self.assertTrue(omni._want_vnc_viewer(
            native_window=False, explicit_window=False, json_mode=False,
            no_window=False))

    def test_an_explicit_window_flag_still_wins(self):
        # --window means "I want the viewer"; it must not be second-guessed.
        self.assertTrue(omni._want_vnc_viewer(
            native_window=True, explicit_window=True, json_mode=False,
            no_window=False))

    def test_no_window_always_wins(self):
        for native in (True, False):
            self.assertFalse(omni._want_vnc_viewer(
                native_window=native, explicit_window=True, json_mode=False,
                no_window=True))

    def test_json_mode_stays_headless_unless_asked(self):
        self.assertFalse(omni._want_vnc_viewer(
            native_window=False, explicit_window=False, json_mode=True,
            no_window=False))

    def test_json_mode_with_an_explicit_window_still_opens_one(self):
        self.assertTrue(omni._want_vnc_viewer(
            native_window=False, explicit_window=True, json_mode=True,
            no_window=False))


if __name__ == "__main__":
    unittest.main()
