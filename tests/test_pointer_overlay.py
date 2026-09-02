"""The pointer overlay and its bake: pure parts.

The overlay's effect (no guest pointer) is only provable by booting a base
that carries it; what is pinned here is that the artefact is well-formed and
lands where zygote reads static overlays from.
"""
import unittest
import zlib

from omnidroid import engine
from tools.build_pointer_overlay import (POINTER_BITMAPS, GUEST_APK,
                                         GUEST_DIR, PACKAGE, png_size,
                                         resource_plan, transparent_png)


class Png(unittest.TestCase):
    def test_header_and_size_round_trip(self):
        data = transparent_png(22, 28)
        self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
        self.assertEqual(png_size(data), (22, 28))

    def test_every_pixel_is_transparent(self):
        data = transparent_png(3, 2)
        idat = data.index(b"IDAT")
        length = int.from_bytes(data[idat - 4:idat], "big")
        raw = zlib.decompress(data[idat + 4:idat + 4 + length])
        self.assertEqual(raw, bytes(2 * (1 + 3 * 4)))

    def test_rejects_empty(self):
        with self.assertRaises(ValueError):
            transparent_png(0, 4)


class Plan(unittest.TestCase):
    def test_one_mdpi_png_per_framework_bitmap(self):
        plan = resource_plan()
        self.assertEqual(len(plan), len(POINTER_BITMAPS))
        self.assertIn("res/drawable-mdpi/pointer_arrow.png", plan)
        self.assertEqual(png_size(plan["res/drawable-mdpi/pointer_arrow.png"]),
                         (22, 28))

    def test_table_is_the_bliss_16_9_7_set(self):
        names = {n for n, _, _ in POINTER_BITMAPS}
        self.assertEqual(len(names), 79)
        self.assertTrue(all(n.startswith("pointer_") for n in names))
        self.assertIn("pointer_wait_35", names)

    def test_refuses_a_non_pointer_name(self):
        with self.assertRaises(ValueError):
            resource_plan((("ic_launcher", 4, 4),))


class Bake(unittest.TestCase):
    def test_lands_where_zygote_reads_static_overlays(self):
        d, apk = engine.overlay_guest_paths("x/OmniPointerOverlay.apk")
        self.assertEqual(d, GUEST_DIR)
        self.assertEqual(apk, GUEST_APK)
        self.assertTrue(apk.startswith("/system/product/overlay/"))

    def test_script_is_all_or_nothing(self):
        s = engine.overlay_bake_script("OmniPointerOverlay.apk")
        self.assertTrue(s.endswith("echo OVERLAY_OK"))
        self.assertIn("chcon u:object_r:system_file:s0", s)
        self.assertNotIn("; ", s.split("echo")[0])   # && chain, no ;

    def test_refuses_an_odd_file_name(self):
        with self.assertRaises(ValueError):
            engine.overlay_guest_paths("x/bad name.apk")
        self.assertEqual(PACKAGE, "com.omni.pointeroverlay")


if __name__ == "__main__":
    unittest.main()
