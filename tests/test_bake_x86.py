# omnidroid/tests/test_bake_x86.py
"""What an x86 offset bake overlays.

An arm base ships its own provisioned /data, so `data_bake_source` could just
read base["data"]. An x86-bliss base has no /data key at all -- it ships a
system disk + kernel + initrd, and a fresh instance's /data is seeded from the
shared empty ext4 template. Before this, an x86 bake resolved its source to
None and died on `pristine /data not found: <images>/None`.
"""
import unittest

import omnidroid.bases as bases


class X86BakeSource(unittest.TestCase):
    def test_an_x86_base_overlays_the_shared_data_template(self):
        b = {"type": bases.BASE_TYPE_X86, "disk": "x86/base_x86.qcow2"}
        self.assertEqual(bases.data_bake_source(b), bases.X86_DATA_TEMPLATE)

    def test_the_template_sits_in_the_x86_subfolder(self):
        # The offset is rebased onto this file by BARE name, so the two must
        # be directory siblings.
        self.assertTrue(bases.X86_DATA_TEMPLATE.startswith(bases.X86_DIR))


class ArmIsUnchanged(unittest.TestCase):
    def test_a_rooted_arm_base_still_reports_its_pristine_data(self):
        b = {"type": bases.BASE_TYPE_ARM,
             "data": "arm/base_arm_data_offset_x.qcow2",
             "root_manifest": {"rooted_data": "arm/base_arm_data_rooted.qcow2"}}
        self.assertEqual(bases.data_bake_source(b),
                         "arm/base_arm_data_rooted.qcow2")

    def test_an_unrooted_arm_base_falls_back_to_its_own_data(self):
        b = {"type": bases.BASE_TYPE_ARM, "data": "arm/base_arm_data.qcow2"}
        self.assertEqual(bases.data_bake_source(b), "arm/base_arm_data.qcow2")

    def test_an_arm_base_with_no_data_reports_nothing_rather_than_x86(self):
        # The x86 fallback must not leak onto an arm entry.
        b = {"type": bases.BASE_TYPE_ARM}
        self.assertIsNone(bases.data_bake_source(b))


if __name__ == "__main__":
    unittest.main()
