#!/usr/bin/env python3
"""Phase 0 instrumentation: per-stage wall-clock timings for one launch.

    python3 -m pytest tests/test_timings.py -q

The recorder is a pure function of an injected clock precisely so the whole
shape is testable without a real boot under it.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid.timings import Timings  # noqa: E402


class StageTimings(unittest.TestCase):
    def test_stages_are_deltas_between_consecutive_marks(self):
        ticks = iter([0.0, 1.0, 3.5, 4.0])
        t = Timings(clock=lambda: next(ticks))
        t.mark("boot")
        t.mark("session")
        t.mark("joined")

        d = t.as_dict()

        self.assertEqual(d["stages"], {"boot": 1.0, "session": 2.5,
                                       "joined": 0.5})
        self.assertEqual(d["marks"], {"boot": 1.0, "session": 3.5,
                                      "joined": 4.0})
        self.assertEqual(d["total_s"], 4.0)

    def test_no_marks_is_an_empty_report_not_a_crash(self):
        # A launch that fails before the first mark must still emit JSON.
        t = Timings(clock=lambda: 0.0)
        self.assertEqual(t.as_dict(),
                         {"total_s": 0.0, "stages": {}, "marks": {}})

    def test_mark_returns_self_so_calls_can_chain(self):
        ticks = iter([0.0, 2.0])
        t = Timings(clock=lambda: next(ticks))
        self.assertIs(t.mark("boot"), t)


if __name__ == "__main__":
    unittest.main()
