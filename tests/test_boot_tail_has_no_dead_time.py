#!/usr/bin/env python3
"""Waiting a fixed number of seconds for something you can just look at.

    python3 -m pytest tests/test_boot_tail_has_no_dead_time.py -q

Three places in the boot tail ran `adb root` and then slept a flat 2-3 seconds
for adbd to come back as root. The number is a guess in both directions:

  * on a fast host adbd is back in a few hundred milliseconds, and the rest of
    that sleep is pure dead time on EVERY launch, and
  * on a slow host -- a weak CPU, or thirty instances sharing a box, or a guest
    being emulated because the PC has no hypervisor -- adbd is NOT back in 3 s,
    and the code carried on regardless against an endpoint that was still down.
    That is the same "measured on one machine, enforced on every machine"
    mistake as the boot timeout, in miniature, and it produced the same class of
    failure: a step that silently did nothing on exactly the hosts that could
    least afford to lose it.

So: poll for the condition instead of guessing at it. Fast hosts stop paying for
slow ones, and slow hosts stop being lied to.

These tests pin that the wait is a poll with a bound, not a sleep.
"""
import os
import subprocess
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402

ACCT = {"name": "u1", "adb_port": 16001, "qmp_port": 17001, "vnc_port": 18001}


class AdbReadiness(unittest.TestCase):
    def test_it_returns_as_soon_as_the_endpoint_answers(self):
        states = ["", "offline", "device"]

        with mock.patch.object(omni, "adb_state", side_effect=states), \
                mock.patch.object(omni, "adb_connect"), \
                mock.patch.object(omni.time, "sleep") as slept:
            ok = omni.wait_adb_ready(ACCT, timeout=30)
        self.assertTrue(ok)
        # Three polls, so at most two waits -- nothing like the 3 s it replaced.
        self.assertLessEqual(slept.call_count, 3)
        for call in slept.call_args_list:
            self.assertLessEqual(call.args[0], 1.0)

    def test_an_endpoint_that_is_already_up_costs_nothing(self):
        with mock.patch.object(omni, "adb_state", return_value="device"), \
                mock.patch.object(omni, "adb_connect"), \
                mock.patch.object(omni.time, "sleep") as slept:
            self.assertTrue(omni.wait_adb_ready(ACCT, timeout=30))
        slept.assert_not_called()

    def test_it_gives_up_rather_than_hanging(self):
        clock = iter([0.0, 1.0, 2.0, 99.0, 99.0, 99.0])

        with mock.patch.object(omni, "adb_state", return_value=""), \
                mock.patch.object(omni, "adb_connect"), \
                mock.patch.object(omni.time, "sleep"), \
                mock.patch.object(omni.time, "monotonic",
                                  side_effect=lambda: next(clock)):
            self.assertFalse(omni.wait_adb_ready(ACCT, timeout=10))

    def test_a_probe_that_raises_is_not_ready_rather_than_a_traceback(self):
        with mock.patch.object(omni, "adb_state",
                               side_effect=subprocess.TimeoutExpired("adb", 5)), \
                mock.patch.object(omni, "adb_connect"), \
                mock.patch.object(omni.time, "sleep"):
            # Bounded by the timeout, and it must come back False, not explode.
            self.assertFalse(omni.wait_adb_ready(ACCT, timeout=0.01))

    def test_the_default_budget_survives_a_host_under_load(self):
        """3 s was measured to be too short. Whatever the new bound is, it must
        be generous -- it costs nothing on a host that is ready."""
        import inspect
        sig = inspect.signature(omni.wait_adb_ready)
        self.assertGreaterEqual(sig.parameters["timeout"].default, 30)


class TheBootTailUsesIt(unittest.TestCase):
    """Wiring: the two boot-path callers must poll, not sleep."""

    def test_post_boot_does_not_sleep_a_fixed_three_seconds(self):
        import inspect
        src = inspect.getsource(omni.post_boot)
        self.assertNotIn("time.sleep(3)", src)
        self.assertIn("wait_adb_ready", src)

    def test_provision_settings_does_not_sleep_a_fixed_two_seconds(self):
        import inspect
        src = inspect.getsource(omni.provision_settings)
        self.assertNotIn("time.sleep(2)", src)
        self.assertIn("wait_adb_ready", src)


if __name__ == "__main__":
    unittest.main()
