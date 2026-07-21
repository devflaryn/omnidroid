#!/usr/bin/env python3
"""Measurement row shape + guest-meminfo parse.

    python3 tests/test_measure_row.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import measure  # noqa: E402

MEMINFO = "MemTotal:  524288 kB\nMemFree: 100000 kB\nMemAvailable: 424288 kB\n"


class GuestUsed(unittest.TestCase):
    def test_used_is_total_minus_available(self):
        self.assertEqual(measure.parse_guest_used_kb(MEMINFO), 524288 - 424288)

    def test_none_when_unparseable(self):
        self.assertIsNone(measure.parse_guest_used_kb("garbage"))


class Row(unittest.TestCase):
    def test_row_shape_and_units(self):
        row = measure.measurement_row(
            base="base_arm_v3", mode="farming", arch="arm",
            boot_minutes=0.5, host_rss_kb=800_000, guest_used_kb=380_000)
        self.assertEqual(row["base"], "base_arm_v3")
        self.assertEqual(row["mode"], "farming")
        self.assertEqual(row["arch"], "arm")
        self.assertEqual(row["boot_minutes"], 0.5)
        self.assertEqual(row["guest_used_mb"], round(380_000 / 1024, 1))
        self.assertEqual(row["host_rss_mb"], round(800_000 / 1024, 1))
        self.assertFalse(row["suspect"])
        self.assertIn("ts", row)

    def test_suspect_flag_set_on_zero_boot(self):
        row = measure.measurement_row(
            base="b", mode="farming", arch="arm",
            boot_minutes=0.0, host_rss_kb=1, guest_used_kb=1)
        self.assertTrue(row["suspect"])


if __name__ == "__main__":
    unittest.main()
