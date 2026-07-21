#!/usr/bin/env python3
"""farming mode: a low mem/smp MODES entry; playable/DEFAULT unchanged.

    python3 tests/test_farming_mode.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


class FarmingMode(unittest.TestCase):
    def test_farming_in_modes_low_footprint(self):
        self.assertIn("farming", omni.MODES)
        self.assertLessEqual(omni.MODES["farming"]["mem"], 1024)
        self.assertLessEqual(omni.MODES["farming"]["smp"], 2)

    def test_resolve_farming(self):
        m = omni.resolve_mode({}, "farming")
        self.assertEqual(m["name"], "farming")
        self.assertEqual(m["mem"], omni.MODES["farming"]["mem"])
        self.assertEqual(m["smp"], omni.MODES["farming"]["smp"])

    def test_playable_and_default_unchanged(self):
        self.assertEqual(omni.DEFAULT_MODE, "playable")
        self.assertEqual(omni.MODES["playable"], {"mem": 4096, "smp": 4})


if __name__ == "__main__":
    unittest.main()
