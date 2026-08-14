#!/usr/bin/env python3
"""Auto-registration of the DUAL-USE bases: an arm/x86 base prefers its ROOTED
matched pair when those images exist, and falls back to the unrooted images
(with a "root pending" note) when they don't. There is no separate dev base.

    python3 tests/test_dev_base_registration.py     (or: pytest tests/)
"""
import json
import os
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import bases as b  # noqa: E402


class ArmRootedRegistration(unittest.TestCase):
    """autoregister_bases picks the rooted matched pair over the unrooted one
    whenever base_arm_system_rooted.qcow2 + base_arm_data_rooted.qcow2 exist."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="omni-reg-"))
        self.images = self.tmp / "images"
        self.images.mkdir()
        self.cfg_path = self.tmp / "paths.json"
        self.cfg_path.write_text(json.dumps({"images_dir": str(self.images),
                                             "current_base": None, "bases": {}}))
        self._patches = [
            unittest.mock.patch.object(omni, "CONFIG_PATH", self.cfg_path),
            unittest.mock.patch.object(b, "CONFIG_PATH", self.cfg_path),
            unittest.mock.patch.dict(os.environ,
                                     {"OMNI_IMAGES_DIR": str(self.images)}),
        ]
        for p in self._patches:
            p.start()
            self.addCleanup(p.stop)

    def _touch(self, *names):
        for n in names:
            # Recorded names carry their arch subfolder (arm/…, x86/…), so
            # the folder has to exist before the file can be written.
            p = self.images / n
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(b"x")

    def _arm_files(self):
        self._touch(b.ARM_BASE_DISK, b.ARM_BASE_SYSTEM, b.ARM_BASE_DATA,
                    b.ARM_BASE_EFIVARS)

    def test_unrooted_arm_registers_root_pending(self):
        self._arm_files()
        raw, new = b.autoregister_bases()
        self.assertIn("arm", raw["bases"])
        entry = raw["bases"]["arm"]
        self.assertFalse(entry.get("rooted"))
        self.assertEqual(entry["system"], b.ARM_BASE_SYSTEM)
        self.assertIn("root pending", entry["notes"])

    def test_rooted_pair_is_preferred_when_present(self):
        self._arm_files()
        self._touch(b.ARM_ROOTED_SYSTEM, b.ARM_ROOTED_DATA)
        raw, new = b.autoregister_bases()
        entry = raw["bases"]["arm"]
        self.assertTrue(entry["rooted"])
        self.assertEqual(entry["system"], b.ARM_ROOTED_SYSTEM)
        self.assertEqual(entry["data"], b.ARM_ROOTED_DATA)
        self.assertIn("[rooted]", entry["notes"])

    def test_no_dev_base_is_ever_registered(self):
        self._arm_files()
        self._touch(b.ARM_DEVKIT_DISK)   # devkit disk present…
        raw, new = b.autoregister_bases()
        # …but it is NOT a base; only the arm base is registered.
        self.assertEqual(set(raw["bases"]), {"arm"})


class DevkitIsArchGeneric(unittest.TestCase):
    def test_devkit_name_per_arch(self):
        # Each devkit disk lives in ITS OWN arch subfolder of images_dir.
        self.assertEqual(b.devkit_disk_name("arm"),
                         "arm/base_arm_devkit.qcow2")
        self.assertEqual(b.devkit_disk_name("x86"),
                         "x86/base_x86_devkit.qcow2")

    def test_base_is_rooted_reads_the_flag(self):
        self.assertTrue(b.base_is_rooted({"rooted": True}))
        self.assertFalse(b.base_is_rooted({}))


if __name__ == "__main__":
    import unittest.mock  # noqa: F401
    unittest.main(verbosity=2)
