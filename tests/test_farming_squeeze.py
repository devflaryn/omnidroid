#!/usr/bin/env python3
"""The farming runtime-squeeze command sequence has the right shape.

    python3 tests/test_farming_squeeze.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import farming  # noqa: E402


class SqueezeSequence(unittest.TestCase):
    def setUp(self):
        self.seq = farming.build_squeeze_sequence()

    def test_returns_nonempty_list_of_argv(self):
        self.assertIsInstance(self.seq, list)
        self.assertGreater(len(self.seq), 0)
        for cmd in self.seq:
            self.assertIsInstance(cmd, list)
            self.assertTrue(all(isinstance(a, str) for a in cmd))
            self.assertEqual(cmd[0], "shell")  # every step is an adb shell cmd

    def test_flat_text_covers_the_four_levers(self):
        flat = " ".join(" ".join(c) for c in self.seq).lower()
        self.assertIn("zram", flat)                  # memory: zram swap
        self.assertIn("lmk", flat)                   # lmkd / lowmemorykiller tune
        self.assertRegex(flat, r"cpu|cgroup|cpuset") # game CPU throttle
        self.assertRegex(flat, r"idle|stop|disable") # quiesce residual work

    def test_deterministic_order(self):
        self.assertEqual(self.seq, farming.build_squeeze_sequence())
