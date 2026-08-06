#!/usr/bin/env python3
"""A window request must never emit args THIS host's QEMU would reject.

    python3 tests/test_gl_spike.py

This file began as the B2 spike test and asserted that OMNI_GL_WINDOW=1
produced `-device virtio-gpu-gl -display cocoa,gl=on`, unconditionally, on
macOS. That assertion was the bug rather than the guard. Measured on the dev
Mac (2026-08-06, Homebrew QEMU 11.0.2):

    $ qemu-system-aarch64 -display cocoa,gl=on
    qemu-system-aarch64: OpenGL support was not enabled in this build of QEMU
    $ qemu-system-aarch64 -device help | grep gpu
    name "virtio-gpu-pci", bus PCI, alias "virtio-gpu"      # no -gl variant

`virtio-gpu-gl` is not a device model on that build, so the spike command
could not start QEMU at all — while the test went green, because it only ever
compared strings to strings.

So the test now runs against the REAL local QEMU: whatever the capability
detector decides, every `-device` model and `-display` backend in the
resulting command must be one the binary on this machine actually advertises.
That is the property the old test was standing in for, and it is the one that
would have caught the failure.

Skips (does not fail) when no QEMU is installed — the unit-level matrix lives
in test_gpu_display.py and test_gaming_mode.py and needs no binary.
"""
import os
import re
import subprocess
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402

ARM_TOOL = "qemu-system-aarch64"


def _acct():
    return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
            "vnc_port": 18001, "base": "arm"}


def _cfg():
    return {"images_dir": "/imgs", "current_base": "arm",
            "bases": {"arm": {"type": "arm-uefi",
                              "system": "base_arm_system.qcow2",
                              "data": "base_arm_data.qcow2",
                              "base_disk": "base_arm_v2.qcow2",
                              "efivars": "base_arm_efivars.fd"}},
            "qemu": {"mem_mb": 4096, "smp": 4}}


def _real_qemu_help():
    """(display_help, device_help) from the installed QEMU, or None."""
    try:
        binary = qemu_proc.qemu_bin(ARM_TOOL)
        out = [subprocess.run([binary, *a], capture_output=True, text=True,
                              timeout=20).stdout for a in
               (("-display", "help"), ("-device", "help"))]
    except Exception:
        return None
    return tuple(out) if all(out) else None


def _command(mode_name, env):
    with mock.patch.dict(os.environ, env, clear=False), \
         mock.patch.object(qemu_proc, "arm_edk2_code", return_value="/fw/c.fd"), \
         mock.patch.object(qemu_proc, "default_accel", return_value="hvf"), \
         mock.patch.object(omni, "runtime_dir", side_effect=lambda n: Path(f"/RT/{n}")), \
         mock.patch.object(omni, "account_dir", side_effect=lambda n: Path(f"/AC/{n}")):
        if not env:
            os.environ.pop("OMNI_GL_WINDOW", None)
        mode = qemu_proc.resolve_mode(_cfg(), mode_name)
        return qemu_proc.qemu_command_arm(_acct(), _cfg(), False, mode=mode)


def _devices(cmd):
    return [cmd[i + 1].split(",")[0] for i, a in enumerate(cmd)
            if a == "-device" and i + 1 < len(cmd)]


def _display_backend(cmd):
    for i, a in enumerate(cmd):
        if a == "-display" and i + 1 < len(cmd):
            return cmd[i + 1].split(",")[0]
    return None


class HostQemuAcceptsWhatWeEmit(unittest.TestCase):
    def setUp(self):
        self.help = _real_qemu_help()
        if not self.help:
            self.skipTest(f"no usable {ARM_TOOL} on this host")

    def _assert_command_is_supported(self, cmd):
        display_help, device_help = self.help
        advertised = set(re.findall(r'name "([^"]+)"', device_help))
        for dev in _devices(cmd):
            self.assertIn(dev, advertised,
                          f"{dev} is not a device model this QEMU has")
        backend = _display_backend(cmd)
        self.assertIn(backend, display_help.split(),
                      f"{backend} is not a display backend this QEMU has")

    def test_a_window_request_stays_within_this_qemus_features(self):
        self._assert_command_is_supported(
            _command("gaming", {"OMNI_GL_WINDOW": "1"}))

    def test_a_gaming_boot_stays_within_this_qemus_features(self):
        self._assert_command_is_supported(_command("gaming", {}))

    def test_a_farming_boot_stays_within_this_qemus_features(self):
        self._assert_command_is_supported(_command("farming", {}))


class TheRequestSwitch(unittest.TestCase):
    def test_env_set_is_a_request(self):
        with mock.patch.dict(os.environ, {"OMNI_GL_WINDOW": "1"}):
            self.assertTrue(omni._gl_window_requested())

    def test_env_unset_is_not(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("OMNI_GL_WINDOW", None)
            self.assertFalse(omni._gl_window_requested())

    def test_explicitly_off_is_not(self):
        for off in ("0", "false", "False", "", "   "):
            with mock.patch.dict(os.environ, {"OMNI_GL_WINDOW": off}):
                self.assertFalse(omni._gl_window_requested(),
                                 f"{off!r} must not request a window")


if __name__ == "__main__":
    unittest.main()
