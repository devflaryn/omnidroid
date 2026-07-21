#!/usr/bin/env python3
"""OMNI_GL_WINDOW swaps the arm GPU/display to accelerated — only when set.

    python3 tests/test_gl_spike.py
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


def _acct():
    return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
            "vnc_port": 18001, "base": "arm"}


def _cfg():
    return {"images_dir": "/imgs", "current_base": "arm",
            "bases": {"arm": {"type": "arm-uefi", "system": "base_arm_system.qcow2",
                              "data": "base_arm_data.qcow2",
                              "base_disk": "base_arm_v2.qcow2",
                              "efivars": "base_arm_efivars.fd"}},
            "qemu": {"mem_mb": 4096, "smp": 4}}


def _cmd(gl_env, is_mac=True):
    env = {"OMNI_GL_WINDOW": "1"} if gl_env else {}
    with mock.patch.dict(os.environ, env, clear=False), \
         mock.patch.object(omni, "IS_MACOS", is_mac), \
         mock.patch.object(omni, "arm_edk2_code", return_value="/fw/code.fd"), \
         mock.patch.object(omni, "qemu_bin", side_effect=lambda x: x), \
         mock.patch.object(omni, "default_accel", return_value="hvf"), \
         mock.patch.object(omni, "resolve_mode",
                           return_value={"smp": 4, "mem": 4096, "name": "playable"}), \
         mock.patch.object(omni, "_assert_port_triple", return_value=1), \
         mock.patch.object(omni, "runtime_dir", side_effect=lambda n: __import__("pathlib").Path(f"/RT/{n}")), \
         mock.patch.object(omni, "account_dir", side_effect=lambda n: __import__("pathlib").Path(f"/AC/{n}")):
        if not gl_env:
            os.environ.pop("OMNI_GL_WINDOW", None)
        return " ".join(omni.qemu_command_arm(_acct(), _cfg(), dev=False))


class GlSpikeSwap(unittest.TestCase):
    def test_gl_env_swaps_to_accelerated(self):
        cmd = _cmd(gl_env=True)
        self.assertIn("virtio-gpu-gl", cmd)
        self.assertIn("cocoa,gl=on", cmd)
        self.assertNotIn("virtio-gpu-pci", cmd)

    def test_no_env_is_unchanged_headless(self):
        cmd = _cmd(gl_env=False)
        self.assertIn("virtio-gpu-pci", cmd)
        self.assertIn("-display none", cmd)
        self.assertNotIn("virtio-gpu-gl", cmd)

    def test_helper_reads_env(self):
        with mock.patch.dict(os.environ, {"OMNI_GL_WINDOW": "1"}):
            self.assertTrue(omni._gl_window_requested())
        os.environ.pop("OMNI_GL_WINDOW", None)
        self.assertFalse(omni._gl_window_requested())


if __name__ == "__main__":
    unittest.main()
