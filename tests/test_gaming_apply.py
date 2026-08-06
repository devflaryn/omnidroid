#!/usr/bin/env python3
"""A gaming boot applies the gaming tune-up — and only a gaming boot does.

    python3 tests/test_gaming_apply.py

The engine's post-boot block is a list of `if mode_name == "farming"` gates.
Adding a second use case means every one of them has to be right in BOTH
directions: a gaming boot must not inherit farming's squeeze, balloon or
5-fps settings, and a farming boot must not start paying for gaming's tune-up.
Getting either wrong is silent — the instance still boots, it is just tuned
for the wrong job.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import lean  # noqa: E402


def _acct():
    return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
            "vnc_port": 18001, "base": "arm", "ephemeral": True,
            "first_boot_done": True}


def _boot(mode_name):
    """Run _ensure_booted for one mode; return the recorded post-boot calls."""
    calls = []

    def rec(name):
        def f(*a, **k):
            calls.append(name)
            return True
        return f

    with mock.patch.object(omni, "running_pid", return_value=None), \
         mock.patch.object(omni, "spawn_qemu", return_value=1234), \
         mock.patch.object(omni, "maybe_start_autocap"), \
         mock.patch.object(omni, "wait_for_boot", return_value=True), \
         mock.patch.object(omni, "post_boot"), \
         mock.patch.object(omni, "_enforce_hiding"), \
         mock.patch.object(omni, "apply_roblox_settings", rec("roblox_settings")), \
         mock.patch.object(omni, "enable_zram", rec("zram")), \
         mock.patch.object(omni, "apply_farming_squeeze", rec("farming_squeeze")), \
         mock.patch.object(omni, "apply_balloon_target", rec("balloon")), \
         mock.patch.object(omni, "apply_gaming_tuning", rec("gaming_tuning")):
        ok, _ = omni._ensure_booted(_acct(), omni.read_config(), "t",
                                    mode_name=mode_name)
    assert ok
    return calls


class GamingBoot(unittest.TestCase):
    def setUp(self):
        self.calls = _boot("gaming")

    def test_applies_the_gaming_tuning(self):
        self.assertIn("gaming_tuning", self.calls)

    def test_applies_roblox_settings(self):
        # Same install path as farming, different profile — asserted below.
        self.assertIn("roblox_settings", self.calls)

    def test_does_not_run_the_farming_squeeze(self):
        self.assertNotIn("farming_squeeze", self.calls)

    def test_does_not_inflate_a_balloon(self):
        self.assertNotIn("balloon", self.calls)


class FarmingBoot(unittest.TestCase):
    def setUp(self):
        self.calls = _boot("farming")

    def test_still_does_everything_it_did(self):
        for step in ("roblox_settings", "zram", "farming_squeeze", "balloon"):
            self.assertIn(step, self.calls)

    def test_does_not_run_the_gaming_tuning(self):
        self.assertNotIn("gaming_tuning", self.calls)


class PlayableBoot(unittest.TestCase):
    """The default mode keeps doing nothing extra, as today."""

    def test_applies_neither_profile(self):
        calls = _boot("playable")
        self.assertNotIn("gaming_tuning", calls)
        self.assertNotIn("farming_squeeze", calls)


class TheProfileThatGetsInstalled(unittest.TestCase):
    def test_a_gaming_boot_installs_the_gaming_settings(self):
        seen = {}

        def capture(acct, label=None, settings=None):
            seen["settings"] = settings
            return True

        with mock.patch.object(omni, "resolve_su", return_value="su"), \
             mock.patch.object(omni, "adb"):
            with mock.patch.object(omni, "apply_roblox_settings", capture), \
                 mock.patch.object(omni, "running_pid", return_value=None), \
                 mock.patch.object(omni, "spawn_qemu", return_value=1), \
                 mock.patch.object(omni, "maybe_start_autocap"), \
                 mock.patch.object(omni, "wait_for_boot", return_value=True), \
                 mock.patch.object(omni, "post_boot"), \
                 mock.patch.object(omni, "_enforce_hiding"), \
                 mock.patch.object(omni, "apply_gaming_tuning"):
                omni._ensure_booted(_acct(), omni.read_config(), "t",
                                    mode_name="gaming")
        self.assertIs(seen["settings"], lean.GAMING_APP_SETTINGS)

    def test_apply_roblox_settings_defaults_to_the_farming_profile(self):
        # Existing callers pass no `settings` and must keep the 5-fps profile.
        sent = {}

        def fake_script(su, settings=None):
            sent["settings"] = settings
            return "echo hi"

        with mock.patch.object(omni.farming, "build_client_settings_script",
                               fake_script), \
             mock.patch.object(omni, "resolve_su", return_value="su"), \
             mock.patch.object(omni, "adb",
                               return_value=mock.Mock(stdout="x")):
            omni.apply_roblox_settings(_acct())
        self.assertIsNone(sent["settings"])


if __name__ == "__main__":
    unittest.main()
