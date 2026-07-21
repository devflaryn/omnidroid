#!/usr/bin/env python3
"""Measurement guards: stale-QEMU detection + boot-time parse.

    python3 tests/test_measure_guard.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import measure  # noqa: E402


class BootParse(unittest.TestCase):
    def test_parses_minutes(self):
        self.assertEqual(
            measure.parse_boot_minutes("... boot completed after 0.5 min"), 0.5)

    def test_none_when_absent(self):
        self.assertIsNone(measure.parse_boot_minutes("no such line here"))

    def test_zero_boot_is_suspect(self):
        self.assertTrue(measure.is_suspect_boot(0.0))
        self.assertTrue(measure.is_suspect_boot(0.02))

    def test_real_boot_not_suspect(self):
        self.assertFalse(measure.is_suspect_boot(0.5))


class StrayQemu(unittest.TestCase):
    PS = (
        "USER  PID  COMMAND\n"
        "berat 100 qemu-system-aarch64 -name omni-alice ...\n"
        "berat 200 qemu-system-x86_64 -name omni-bob ...\n"
        "berat 300 python engine.py\n"
    )

    def test_flags_unknown_qemu(self):
        self.assertEqual(measure.stray_qemu_pids(self.PS, {100}), [200])

    def test_none_when_all_known(self):
        self.assertEqual(measure.stray_qemu_pids(self.PS, {100, 200}), [])

    def test_ignores_nonqemu(self):
        self.assertNotIn(300, measure.stray_qemu_pids(self.PS, set()))


if __name__ == "__main__":
    unittest.main()
