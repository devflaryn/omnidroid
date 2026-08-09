#!/usr/bin/env python3
"""When may a launch use the warm cache at all?

    python3 -m pytest tests/test_warm_boot_policy.py -q

The policy is the whole safety story, so it is a pure function tested without
QEMU under it:
  * a --debug boot changes device topology (devkit vdc), so it must never
    read OR write the cache;
  * an entry already backing a running instance must not be restored a second
    time -- the second concurrent restore lands `offline` on adb (spec 8b).
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine  # noqa: E402


class WarmPolicy(unittest.TestCase):
    def test_debug_boots_never_touch_the_cache(self):
        self.assertFalse(engine._warm_cache_allowed(debug=True, in_use=set(),
                                                    key="k"))

    def test_a_normal_boot_may_use_the_cache(self):
        self.assertTrue(engine._warm_cache_allowed(debug=False, in_use=set(),
                                                   key="k"))

    def test_an_entry_already_in_use_is_refused(self):
        # Interim rule: siblings cold-boot until the adb blocker is root-caused.
        self.assertFalse(engine._warm_cache_allowed(debug=False,
                                                    in_use={"k"}, key="k"))

    def test_a_different_entry_being_in_use_is_irrelevant(self):
        self.assertTrue(engine._warm_cache_allowed(debug=False,
                                                   in_use={"other"}, key="k"))

    def test_no_key_means_no_cache(self):
        self.assertFalse(engine._warm_cache_allowed(debug=False, in_use=set(),
                                                    key=None))


if __name__ == "__main__":
    unittest.main()
