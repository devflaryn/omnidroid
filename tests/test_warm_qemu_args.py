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
                              "efivars": "base_arm_efivars.fd"},
                      # Modeled on the real `bases.x86` entry in
                      # configs/paths.json: x86-bliss boots by direct
                      # kernel/initrd, not UEFI pflash.
                      "x86": {"type": "x86-bliss",
                              "disk": "base_x86.qcow2",
                              "kernel": "base_x86.kernel",
                              "initrd": "base_x86.initrd.img",
                              "src": "/android-2024-10-11"}}}


def _acct():
    return {"name": "t", "base": "arm", "ephemeral": True, "adb_port": 16001,
            "qmp_port": 17001, "vnc_port": 18001, "arch": "arm64"}


def _acct_x86():
    return {"name": "t86", "base": "x86", "ephemeral": False,
            "adb_port": 16002, "qmp_port": 17002, "vnc_port": 18002,
            "arch": "x86_64"}


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

    def test_restore_uses_a_private_runtime_dir_copy_not_the_entrys_own_file(self):
        # pflash needs a real writable file, unlike the disks (snapshot=on).
        # Pointing it straight at the entry's own efivars.fd would let a
        # restore write into a file every other restore of the same entry
        # shares -- it must be a private per-instance copy instead.
        from omnidroid.runtime import runtime_dir
        acct = _acct()
        cmd = qp.qemu_command_arm(acct, _cfg(self.images), interactive=False,
                                  warm=self.entry)
        pflash = [a for a in cmd if "if=pflash,unit=1" in str(a)][0]
        rd = runtime_dir(acct["name"])
        self.assertIn(str(rd / "efivars.fd"), pflash)
        self.assertNotIn(str(self.entry), pflash)

    def test_spawn_stages_a_private_efivars_copy_and_leaves_the_entry_untouched(self):
        import os
        import tempfile
        import shutil as shutil_mod
        from omnidroid.runtime import runtime_dir

        data_dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil_mod.rmtree, data_dir, ignore_errors=True)
        old = os.environ.get("OMNI_DATA_DIR")
        os.environ["OMNI_DATA_DIR"] = str(data_dir)
        self.addCleanup(lambda: (os.environ.pop("OMNI_DATA_DIR", None)
                                 if old is None
                                 else os.environ.__setitem__("OMNI_DATA_DIR", old)))

        acct = _acct()
        entry_efivars = self.entry / warmcache.EFIVARS_NAME
        before = entry_efivars.read_bytes()
        before_mtime = entry_efivars.stat().st_mtime_ns

        qp._stage_warm_efivars(acct, _cfg(self.images), self.entry)

        rd = runtime_dir(acct["name"])
        copy_path = rd / "efivars.fd"
        self.assertTrue(copy_path.exists())
        self.assertEqual(copy_path.read_bytes(), before)
        # Mutate the private copy -- the entry's own file must be unaffected.
        copy_path.write_bytes(b"mutated")
        self.assertEqual(entry_efivars.read_bytes(), before)
        self.assertEqual(entry_efivars.stat().st_mtime_ns, before_mtime)

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

    def test_warm_and_bake_together_raises_on_arm(self):
        with self.assertRaises(ValueError):
            qp.qemu_command_arm(_acct(), _cfg(self.images), interactive=False,
                                warm=self.entry, bake=True)

    # ------------------------------------------------------------- x86 ----
    # The design spec is explicit that qemu_command() (x86) is NOT arm-only
    # and must gain the same warm parameter -- cross-platform is a hard
    # requirement, not an arm nicety.

    def test_x86_restore_opens_the_golden_disks_snapshot_on(self):
        cmd = qp.qemu_command(_acct_x86(), _cfg(self.images), interactive=False,
                              warm=self.entry)
        drives = [a for a in cmd if a.startswith("file=")]
        golden = [d for d in drives if warmcache.SYSTEM_NAME in d
                  or warmcache.DATA_NAME in d]
        self.assertEqual(len(golden), 2, drives)
        for d in golden:
            self.assertIn("snapshot=on", d)

    def test_x86_restore_defers_incoming_and_never_uses_incoming_file(self):
        cmd = qp.qemu_command(_acct_x86(), _cfg(self.images), interactive=False,
                              warm=self.entry)
        self.assertIn("-incoming", cmd)
        self.assertEqual(cmd[cmd.index("-incoming") + 1], "defer")
        self.assertFalse(any(str(a).startswith("file:") for a in cmd))

    def test_x86_bake_uses_writable_overlays_not_snapshot_on(self):
        cmd = qp.qemu_command(_acct_x86(), _cfg(self.images), interactive=False,
                              bake=True)
        drives = [a for a in cmd if a.startswith("file=")]
        self.assertTrue(drives)
        for d in drives:
            self.assertNotIn("snapshot=on", d)
        self.assertNotIn("-incoming", cmd)

    def test_x86_normal_boot_is_byte_for_byte_unchanged(self):
        before = qp.qemu_command(_acct_x86(), _cfg(self.images),
                                 interactive=False)
        after = qp.qemu_command(_acct_x86(), _cfg(self.images),
                                interactive=False, warm=None, bake=False)
        self.assertEqual(before, after)
        self.assertNotIn("-incoming", before)

    def test_warm_and_bake_together_raises_on_x86(self):
        with self.assertRaises(ValueError):
            qp.qemu_command(_acct_x86(), _cfg(self.images), interactive=False,
                            warm=self.entry, bake=True)


if __name__ == "__main__":
    unittest.main()
