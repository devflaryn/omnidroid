# omnidroid/tests/test_offsets_x86.py
"""Offsets on an x86 base — the arch-aware half of the naming surface.

An offset must land in the SAME arch subfolder as the pristine /data it
overlays, because a qcow2's backing reference resolves relative to the
overlay's own directory (see offsets.offset_image_name). On an x86 base that
folder is `x86/`, not `arm/` — so a naming function that always answered
`arm/...` would bake the x86 offset into a directory whose backing file is not
there, and the image would simply fail to open.
"""
import unittest
from pathlib import PurePosixPath

import omnidroid.bases as bases
import omnidroid.offsets as offsets


class ArchAwareNaming(unittest.TestCase):
    def test_x86_base_gets_the_x86_offset_path(self):
        x86_base = {"tag": "x86", "type": bases.BASE_TYPE_X86}
        self.assertEqual(
            offsets.offset_image_name("arceusremote", base=x86_base),
            bases.X86_DIR + "base_x86_data_offset_arceusremote.qcow2")

    def test_arm_base_is_unchanged(self):
        arm_base = {"tag": "arm", "type": bases.BASE_TYPE_ARM}
        self.assertEqual(
            offsets.offset_image_name("arceusremote", base=arm_base),
            bases.ARM_DIR + "base_arm_data_offset_arceusremote.qcow2")

    def test_no_base_still_answers_arm(self):
        # Back-compat: every pre-existing caller passes only a name and must
        # keep getting the arm path it has always got.
        self.assertEqual(offsets.offset_image_name("2.731.944"),
                         bases.ARM_DIR + "base_arm_data_offset_2.731.944.qcow2")

    def test_x86_offset_sits_beside_the_template_it_overlays(self):
        # The x86 pristine /data is the shared ext4 template; the offset must
        # be its directory sibling or `qemu-img rebase -u -b <bare name>` (and
        # therefore a relocatable images_dir) breaks.
        x86_base = {"tag": "x86", "type": bases.BASE_TYPE_X86}
        self.assertEqual(
            PurePosixPath(offsets.offset_image_name("x", base=x86_base)).parent,
            PurePosixPath(bases.X86_DATA_TEMPLATE).parent)

    def test_dotted_and_plain_names_are_valid(self):
        self.assertTrue(offsets.OFFSET_NAME_RE.match("2.740.101"))
        self.assertTrue(offsets.OFFSET_NAME_RE.match("arceusremote"))


class ArchAwareResolution(unittest.TestCase):
    def _x86_base(self, **kw):
        b = {"tag": "x86", "type": bases.BASE_TYPE_X86}
        b.update(kw)
        return b

    def test_offset_data_image_falls_back_to_the_x86_convention(self):
        # An entry with no recorded `data` must still resolve to the x86 path
        # when it hangs off an x86 base.
        b = self._x86_base(offsets={"arceusremote": {}})
        self.assertEqual(
            offsets.offset_data_image(b, "arceusremote"),
            bases.X86_DIR + "base_x86_data_offset_arceusremote.qcow2")

    def test_a_recorded_data_name_still_wins(self):
        # Reading the RECORDED name is what lets an offset baked under an
        # older convention keep booting; arch-awareness must not override it.
        b = self._x86_base(offsets={"old": {"data": "x86/hand-built.qcow2"}})
        self.assertEqual(offsets.offset_data_image(b, "old"),
                         "x86/hand-built.qcow2")

    def test_offset_rows_report_the_x86_path(self):
        b = self._x86_base(offsets={"arceusremote": {}})
        row = offsets.offset_rows(b)[0]
        self.assertEqual(
            row["data"],
            bases.X86_DIR + "base_x86_data_offset_arceusremote.qcow2")


if __name__ == "__main__":
    unittest.main()
