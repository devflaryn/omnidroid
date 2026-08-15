#!/usr/bin/env python3
"""A slow guest is an ordinary condition, not an unhandled error.

    python3 -m pytest tests/test_slow_guest_tolerance.py -q

Farming exists to run guests that are deliberately starved: 1-2 vCPU, ballooned
to ~896 MB, running Roblox's arm64 build through libndk_translation. Such a
guest answers adb LATE. The launch path is full of probes with 8-45 s timeouts,
and `subprocess.TimeoutExpired` from any one of them used to come out of
`omnidroid start` as a traceback.

It happened twice on the same mode, in the same afternoon, in two different
probes:

  * the ordered `am broadcast` that hands the session to the kiosk (45 s), and
  * the `pm path` that checks the kiosk is installed (15 s)

-- the second one on a boot that had ALREADY joined a place successfully. Both
are now results rather than exceptions, and `deliver_session` enforces the
promise its docstring always made by wrapping the whole delivery.

These tests pin the tolerance, not the timeouts: the numbers will move again,
the "never raise into a launch" property must not.
"""
import os
import subprocess
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import adb as adb_mod  # noqa: E402
from omnidroid import engine as omni  # noqa: E402

ACCT = {"name": "u1", "adb_port": 16001, "qmp_port": 17001, "vnc_port": 18001}


class AdbSoft(unittest.TestCase):
    def test_a_timeout_is_a_result(self):
        with mock.patch.object(adb_mod, "adb", side_effect=subprocess.TimeoutExpired(
                cmd="adb shell pm path", timeout=15)):
            r = adb_mod.adb_soft(ACCT, "shell", "pm", "path", "x", timeout=15)
        self.assertEqual(r.returncode, -1)
        self.assertEqual(r.stdout, "")

    def test_a_missing_adb_binary_is_a_result(self):
        with mock.patch.object(adb_mod, "adb", side_effect=OSError("no adb")):
            r = adb_mod.adb_soft(ACCT, "shell", "true")
        self.assertEqual(r.returncode, -1)

    def test_a_normal_answer_passes_through_untouched(self):
        want = subprocess.CompletedProcess(["adb"], 0, "package:/x.apk", "")
        with mock.patch.object(adb_mod, "adb", return_value=want):
            self.assertIs(adb_mod.adb_soft(ACCT, "shell", "pm", "path", "x"),
                          want)


class KioskProbe(unittest.TestCase):
    """`pm path` queues behind whatever else a 1-vCPU guest is doing."""

    def _probe(self, results):
        calls = []

        def fake(acct, *args, **kw):
            calls.append(kw.get("timeout"))
            return results[min(len(calls) - 1, len(results) - 1)]

        with mock.patch.object(omni, "adb_soft", side_effect=fake):
            return omni.kiosk_installed(ACCT), calls

    def test_a_late_answer_still_counts(self):
        no_answer = subprocess.CompletedProcess(["adb"], -1, "", "")
        found = subprocess.CompletedProcess(["adb"], 0, "package:/k.apk", "")
        ok, calls = self._probe([no_answer, found])
        self.assertTrue(ok)
        self.assertEqual(len(calls), 2)          # it retried

    def test_never_answering_is_false_not_an_exception(self):
        no_answer = subprocess.CompletedProcess(["adb"], -1, "", "")
        ok, calls = self._probe([no_answer])
        self.assertFalse(ok)
        self.assertEqual(len(calls), omni.KIOSK_PROBE_TRIES)

    def test_a_real_answer_of_not_installed_does_not_retry(self):
        # Answered, and the kiosk is genuinely absent: retrying would just
        # spend another 45 s to be told the same thing.
        absent = subprocess.CompletedProcess(["adb"], 1, "", "")
        ok, calls = self._probe([absent])
        self.assertFalse(ok)
        self.assertEqual(len(calls), 1)

    def test_the_budget_is_generous_enough_to_be_worth_having(self):
        # 15 s was measured to be too short on a farming guest.
        self.assertGreaterEqual(omni.KIOSK_PROBE_TIMEOUT, 30)


class DeliverySwallowsWhatItPromisedTo(unittest.TestCase):
    """The docstring always said "never raises into a boot path". It does now."""

    def test_an_exception_becomes_a_failure_dict(self):
        with mock.patch.object(omni, "_deliver_session",
                               side_effect=subprocess.TimeoutExpired(
                                   cmd="adb shell am broadcast", timeout=45)):
            out = omni.deliver_session(ACCT, "t", {"token": "x"})
        self.assertFalse(out["delivered"])
        self.assertEqual(out["reason"], "delivery_error")
        self.assertIn("TimeoutExpired", out["detail"])

    def test_a_normal_result_passes_through(self):
        want = {"delivered": True, "played": True}
        with mock.patch.object(omni, "_deliver_session", return_value=want):
            self.assertIs(omni.deliver_session(ACCT, "t", {"token": "x"}),
                          want)


if __name__ == "__main__":
    unittest.main()
