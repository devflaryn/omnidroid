#!/usr/bin/env python3
"""`start --json` must report where the wall clock went.

    python3 -m pytest tests/test_start_timings.py -q

Without this, "make boot fast" is unfalsifiable: Android boot and Roblox's own
cold start and join are both inside one number.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine  # noqa: E402


class StartEmitsTimings(unittest.TestCase):
    def test_result_carries_named_stage_timings(self):
        # The contract the GUI/executor reads: a `timings` block with the
        # stage names the boot path marks.
        self.assertTrue(hasattr(engine, "_start_timings_stages"))
        self.assertEqual(
            engine._start_timings_stages(has_apk=False),
            ["boot", "session_delivered", "game_foreground"])

    def test_apk_install_is_its_own_stage_only_when_an_apk_was_given(self):
        self.assertEqual(
            engine._start_timings_stages(has_apk=True),
            ["boot", "apk_install", "session_delivered", "game_foreground"])


if __name__ == "__main__":
    unittest.main()
