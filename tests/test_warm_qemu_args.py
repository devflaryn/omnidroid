#!/usr/bin/env python3
"""QEMU args for the warm-restore paths.

    python3 -m pytest tests/test_warm_qemu_args.py -q

Two properties are load-bearing and easy to break silently:
  * a RESTORE must open the golden disks snapshot=on, or the first restore
    poisons the entry for every later one;
  * a restore must use `-incoming defer`, never `-incoming file:` -- the
    latter dies with "Capability mapped-ram is off, but received capability
    is on" because caps can only be set over QMP.
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import qemu_proc as qp  # noqa: E402
from omnidroid import warmcache  # noqa: E402


def _cfg(images):
    return {"images_dir": str(images),
            "qemu": {"mem_mb": 4096, "smp": 4, "adb_port_start": 16001,
                     "qmp_port_start": 17001, "vnc_port_start": 18001},
            "bases": {"arm": {"type": "arm-uefi",
                              "system": "base_arm_system_rooted.qcow2",
                              "data": "base_arm_data_rooted.qcow2",
                              "efivars": "base_arm_efivars.fd"}}}


def _acct():
    return {"name": "t", "base": "arm", "ephemeral": True, "adb_port": 16001,
            "qmp_port": 17001, "vnc_port": 18001, "arch": "arm64"}


class WarmRestoreArgs(unittest.TestCase):
    def setUp(self):
        self.images = Path(tempfile.mkdtemp())
        self.entry = warmcache.entry_path(self.images, "abc123")
        self.entry.mkdir(parents=True, exist_ok=True)
        for name in warmcache.REQUIRED_FILES:
            (self.entry / name).write_bytes(b"x")

    def test_restore_opens_the_golden_disks_snapshot_on(self):
        cmd = qp.qemu_command_arm(_acct(), _cfg(self.images), interactive=False,
                                  warm=self.entry)
        drives = [a for a in cmd if a.startswith("file=")]
        golden = [d for d in drives if warmcache.SYSTEM_NAME in d
                  or warmcache.DATA_NAME in d]
        self.assertEqual(len(golden), 2, drives)
        for d in golden:
            self.assertIn("snapshot=on", d)

    def test_restore_defers_incoming_and_never_uses_incoming_file(self):
        cmd = qp.qemu_command_arm(_acct(), _cfg(self.images), interactive=False,
                                  warm=self.entry)
        self.assertIn("-incoming", cmd)
        self.assertEqual(cmd[cmd.index("-incoming") + 1], "defer")
        self.assertFalse(any(str(a).startswith("file:") for a in cmd))

    def test_restore_uses_the_entrys_own_efivars(self):
        cmd = qp.qemu_command_arm(_acct(), _cfg(self.images), interactive=False,
                                  warm=self.entry)
        pflash = [a for a in cmd if "if=pflash,unit=1" in str(a)][0]
        self.assertIn(str(self.entry / warmcache.EFIVARS_NAME), pflash)

    def test_bake_uses_writable_overlays_not_snapshot_on(self):
        # The freeze point must be persistable; snapshot=on would discard it.
        cmd = qp.qemu_command_arm(_acct(), _cfg(self.images), interactive=False,
                                  bake=True)
        drives = [a for a in cmd if a.startswith("file=")]
        self.assertTrue(drives)
        for d in drives:
            self.assertNotIn("snapshot=on", d)
        self.assertNotIn("-incoming", cmd)

    def test_a_normal_boot_is_byte_for_byte_unchanged(self):
        # The cache is an optimization layer: with no warm/bake it must emit
        # exactly what it emitted before this feature existed.
        before = qp.qemu_command_arm(_acct(), _cfg(self.images),
                                     interactive=False)
        after = qp.qemu_command_arm(_acct(), _cfg(self.images),
                                    interactive=False, warm=None, bake=False)
        self.assertEqual(before, after)
        self.assertNotIn("-incoming", before)


if __name__ == "__main__":
    unittest.main()
