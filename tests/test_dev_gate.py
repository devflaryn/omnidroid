#!/usr/bin/env python3
"""The dev base (frida + Magisk root) must be invisible and unselectable to a
customer build, and available to omni-agent.

This is a product-safety boundary, not a preference: omni-executor renders its
base picker from `bases --json` and switches with `use-base`, so anything these
functions expose is one click away for a customer.

    python3 tests/test_dev_gate.py     (or: pytest tests/)
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import engine as omni  # noqa: E402

CFG = {
    "current_base": "arm",
    "bases": {
        "x86": {"type": "x86-bliss", "disk": "base_x86.qcow2"},
        "arm": {"type": "arm-uefi", "base_disk": "base_arm.qcow2"},
        "dev": {"type": "arm-uefi", "base_disk": "base_arm.qcow2",
                "devkit": "base_arm_devkit.qcow2"},
    },
}


class DevGate(unittest.TestCase):

    def setUp(self):
        self._saved = os.environ.pop(omni.DEV_MODE_ENV, None)

    def tearDown(self):
        os.environ.pop(omni.DEV_MODE_ENV, None)
        if self._saved is not None:
            os.environ[omni.DEV_MODE_ENV] = self._saved

    def dev_on(self):
        os.environ[omni.DEV_MODE_ENV] = "1"

    # ---- what identifies a dev base ----

    def test_devkit_field_is_what_marks_a_base_dev(self):
        self.assertTrue(omni.base_is_dev(CFG["bases"]["dev"]))
        self.assertFalse(omni.base_is_dev(CFG["bases"]["arm"]))
        self.assertFalse(omni.base_is_dev(CFG["bases"]["x86"]))

    # ---- visibility ----

    def test_customer_cannot_see_the_dev_base(self):
        self.assertEqual(sorted(omni.visible_bases(CFG)), ["arm", "x86"])

    def test_agent_can_see_the_dev_base(self):
        self.dev_on()
        self.assertEqual(sorted(omni.visible_bases(CFG)), ["arm", "dev", "x86"])

    def test_only_explicit_truthy_values_unlock(self):
        for val, expect in (("1", True), ("true", True), ("TRUE", True),
                            ("yes", True), ("on", True),
                            ("0", False), ("", False), ("false", False),
                            ("maybe", False)):
            os.environ[omni.DEV_MODE_ENV] = val
            self.assertEqual(omni.dev_mode_enabled(), expect,
                             f"{omni.DEV_MODE_ENV}={val!r}")

    # ---- selection ----

    def test_customer_cannot_select_the_dev_base(self):
        with self.assertRaises(SystemExit):
            omni._select_base_tag(CFG, base_tag="dev")

    def test_agent_can_select_the_dev_base(self):
        self.dev_on()
        self.assertEqual(omni._select_base_tag(CFG, base_tag="dev"), "dev")

    def test_production_bases_are_unaffected(self):
        self.assertEqual(omni._select_base_tag(CFG, base_tag="arm"), "arm")
        self.assertEqual(omni._select_base_tag(CFG, arch="x86"), "x86")

    def test_arch_autoselect_never_lands_on_dev(self):
        """--arch arm must resolve to the production arm base even though the
        dev base is also arm."""
        cfg = {"current_base": "dev", "bases": CFG["bases"]}
        self.assertEqual(omni._select_base_tag(cfg, arch="arm"), "arm")

    def test_dev_only_arm_deployment_does_not_leak_dev_to_arch_select(self):
        """If the ONLY arm base registered is the dev one, a customer asking for
        arm gets a clean no_base — not a silent rooted boot."""
        cfg = {"current_base": "x86",
               "bases": {"x86": CFG["bases"]["x86"], "dev": CFG["bases"]["dev"]}}
        with self.assertRaises(SystemExit):
            omni._select_base_tag(cfg, arch="arm")

    def test_default_pointing_at_dev_refuses_rather_than_boots_it(self):
        """current_base=dev in a customer build is a misconfiguration; creating
        an account with no flags must fail loudly, not quietly use dev."""
        cfg = {"current_base": "dev", "bases": CFG["bases"]}
        with self.assertRaises(SystemExit):
            omni._select_base_tag(cfg)
        self.dev_on()
        self.assertEqual(omni._select_base_tag(cfg), "dev")


if __name__ == "__main__":
    unittest.main(verbosity=2)
