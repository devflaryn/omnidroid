"""Ephemeral (fully-shared, no-persistence) arm boot: qemu_command_arm points the
system/data drives at the SHARED base templates with snapshot=on (throwaway
per-process overlay) instead of per-account overlays, so many instances run
concurrently and nothing persists. Non-ephemeral accounts are unchanged.
"""
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


def _cfg():
    return {
        "images_dir": "/imgs",
        "qemu": {"smp": 4, "mem_mb": 4096},
        "bases": {
            "arm": {"type": "arm-uefi", "base_disk": "base_arm.qcow2",
                    "system": "base_arm_system.qcow2", "data": "base_arm_data.qcow2",
                    "efivars": "base_arm_efivars.fd"},
        },
    }


def _acct(**over):
    a = {"name": "u1", "base": "arm", "adb_port": 16001, "qmp_port": 17001,
         "vnc_port": 18001}
    a.update(over)
    return a


class EphemeralBoot(unittest.TestCase):
    def _cmd(self, acct, interactive=False):
        with mock.patch.object(omni, "arm_edk2_code", return_value="/fw/code.fd"), \
             mock.patch.object(omni, "qemu_bin", side_effect=lambda x: x), \
             mock.patch.object(omni, "default_accel", return_value="tcg"), \
             mock.patch.object(omni, "resolve_mode",
                               return_value={"smp": 4, "mem": 4096, "name": "playable"}), \
             mock.patch.object(omni, "_assert_port_triple", return_value=1), \
             mock.patch.object(omni, "runtime_dir",
                               side_effect=lambda n: Path(f"/RT/{n}")), \
             mock.patch.object(omni, "account_dir",
                               side_effect=lambda n: Path(f"/AC/{n}")):
            return " ".join(omni.qemu_command_arm(acct, _cfg(), interactive))

    def test_ephemeral_uses_shared_templates_with_snapshot(self):
        cmd = self._cmd(_acct(ephemeral=True))
        self.assertIn("snapshot=on", cmd)
        self.assertIn("/imgs/base_arm_system.qcow2", cmd)
        self.assertIn("/imgs/base_arm_data.qcow2", cmd)
        # It must NOT boot a per-account overlay for vda/vdb.
        self.assertNotIn("u1/system.qcow2", cmd)
        self.assertNotIn("u1/data.qcow2", cmd)

    def test_non_ephemeral_uses_per_account_overlays_no_snapshot(self):
        cmd = self._cmd(_acct())
        self.assertNotIn("snapshot=on", cmd)
        self.assertIn("u1/system.qcow2", cmd)
        self.assertIn("u1/data.qcow2", cmd)
        self.assertNotIn("/imgs/base_arm_system.qcow2", cmd)

    def test_efivars_and_serial_split_by_ephemeral(self):
        # Ephemeral: efivars + serial.log both under the throwaway runtime dir.
        # interactive=True so qemu_command_arm actually emits the -serial flag (arm-only
        # boots write serial.log solely on interactive boots; see qemu_command_arm).
        eph = self._cmd(_acct(ephemeral=True), interactive=True)
        self.assertIn("/RT/u1/efivars.fd", eph)
        self.assertIn("/RT/u1/serial.log", eph)
        self.assertNotIn("/AC/u1/efivars.fd", eph)
        # Non-ephemeral: efivars stays in the per-account dir (Task 4 territory),
        # but serial.log still goes to the runtime dir.
        non = self._cmd(_acct(), interactive=True)
        self.assertIn("/AC/u1/efivars.fd", non)
        self.assertIn("/RT/u1/serial.log", non)
        self.assertNotIn("/RT/u1/efivars.fd", non)


if __name__ == "__main__":
    unittest.main()
