#!/usr/bin/env python3
"""A trim registration bumps the version and RETAINS the prior image refs.

    python3 tests/test_trim_registration.py
"""
import copy
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import trimreg  # noqa: E402

CFG = {
    "current_base": "arm",
    "bases": {
        "arm": {"type": "arm-uefi", "base_disk": "base_arm_v2.qcow2",
                "system": "base_arm_system.qcow2", "version": 2,
                "changelog": {"2": "branded"}},
    },
}


class TrimRegistration(unittest.TestCase):
    def test_bumps_version_and_records_new_images(self):
        out = trimreg.register_trim(
            copy.deepcopy(CFG), "arm",
            new_disk="base_arm_v3.qcow2", new_system="base_arm_system_v3.qcow2",
            note="trim: removed stock browser/gallery/telephony")
        self.assertEqual(out["bases"]["arm"]["version"], 3)
        self.assertEqual(out["bases"]["arm"]["base_disk"], "base_arm_v3.qcow2")
        self.assertEqual(out["bases"]["arm"]["system"], "base_arm_system_v3.qcow2")
        self.assertIn("3", out["bases"]["arm"]["changelog"])
        self.assertIn("trim", out["bases"]["arm"]["changelog"]["3"])

    def test_prior_changelog_retained(self):
        out = trimreg.register_trim(
            copy.deepcopy(CFG), "arm", new_disk="d", new_system="s", note="n")
        self.assertEqual(out["bases"]["arm"]["changelog"]["2"], "branded")

    def test_does_not_mutate_input(self):
        cfg = copy.deepcopy(CFG)
        trimreg.register_trim(cfg, "arm", new_disk="d", new_system="s", note="n")
        self.assertEqual(cfg["bases"]["arm"]["version"], 2)  # input untouched

    def test_unknown_base_raises(self):
        with self.assertRaises(KeyError):
            trimreg.register_trim(copy.deepcopy(CFG), "nope",
                                  new_disk="d", new_system="s", note="n")


if __name__ == "__main__":
    unittest.main()
