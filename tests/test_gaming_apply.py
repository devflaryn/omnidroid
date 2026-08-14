#!/usr/bin/env python3
"""Post-boot tuning follows the mode's PROFILE, in both directions.

    python3 tests/test_gaming_apply.py

Every mode declares a `profile`: "performance" (spend the host on one
instance) or "density" (spend quality on instance count). The engine branches
on that, and it has to be right in BOTH directions — a performance boot must
not inherit farming's squeeze, balloon or 5-fps settings, and a farming boot
must not start paying for the performance tune-up. Getting either wrong is
silent: the instance still boots, it is just tuned for the wrong job.

THE BUG THIS FILE NOW PINS. The engine used to compare `mode_name`, the raw
--mode argument, against the literals "gaming" and "farming". A bare
`omnidroid start` passes mode_name=None, which resolves to `playable` — the
DEFAULT mode — and matched neither literal, so the most-used mode was the only
one that got NO post-boot tuning at all. `PlayableBoot` below is the
regression test: playable is a performance mode and must be tuned like one.
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

    # assert_kiosk_game and adb are mocked for a reason that costs real time:
    # _acct() claims adb_port 16001, and on a developer machine that port is
    # very often a REAL running instance. Unmocked, this harness talked to it —
    # the module hung for minutes against a live guest and, worse, mutated it.
    # Nothing here wants a device; every assertion is about which collaborator
    # the engine chose.
    with mock.patch.object(omni, "running_pid", return_value=None), \
         mock.patch.object(omni, "spawn_qemu", return_value=1234), \
         mock.patch.object(omni, "maybe_start_autocap"), \
         mock.patch.object(omni, "wait_for_boot", return_value=True), \
         mock.patch.object(omni, "post_boot"), \
         mock.patch.object(omni, "_enforce_hiding"), \
         mock.patch.object(omni, "assert_kiosk_game"), \
         mock.patch.object(omni, "apply_consent"), \
         mock.patch.object(omni, "adb"), \
         mock.patch.object(omni, "apply_awake", rec("awake")), \
         mock.patch.object(omni, "apply_roblox_settings", rec("roblox_settings")), \
         mock.patch.object(omni, "enable_zram", rec("zram")), \
         mock.patch.object(omni, "apply_farming_squeeze", rec("farming_squeeze")), \
         mock.patch.object(omni, "apply_balloon_target", rec("balloon")), \
         mock.patch.object(omni, "apply_gaming_tuning", rec("gaming_tuning")):
        # no_warm: this harness passes the REAL config, so without it the boot
        # walks into the warm-cache lookup/bake — QMP sockets and qemu-img
        # against the host's actual images dir, for a test whose every
        # assertion is about which tuning collaborator the mode chose. (It
        # never showed up before because this whole file failed at the
        # apply_awake patch, which the engine did not have.)
        ok, _ = omni._ensure_booted(_acct(), omni.read_config(), "t",
                                    mode_name=mode_name, no_warm=True)
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
    """The DEFAULT mode is a performance mode and must be tuned like one.

    This is the regression test for the mode_name-vs-profile bug: `playable`
    is what a bare `omnidroid start` resolves to, and it used to fall through
    every gate untouched."""

    def setUp(self):
        self.calls = _boot("playable")

    def test_applies_the_performance_tuning(self):
        self.assertIn("gaming_tuning", self.calls)

    def test_installs_a_roblox_profile(self):
        self.assertIn("roblox_settings", self.calls)

    def test_does_not_run_the_farming_squeeze(self):
        self.assertNotIn("farming_squeeze", self.calls)

    def test_does_not_inflate_a_balloon(self):
        self.assertNotIn("balloon", self.calls)

    def test_the_untyped_default_is_tuned_too(self):
        # mode_name=None is what cmd_start passes when nobody typed --mode.
        # It must behave exactly like an explicit `playable`.
        self.assertEqual(_boot(None), self.calls)


class BothProfilesGetTheNeverBlankGuarantee(unittest.TestCase):
    """The one post-boot step that is NOT a profile decision.

    Every other entry in `calls` is deliberately asymmetric — the whole point
    of this file is that a density boot and a performance boot get different
    treatment. This one must be symmetric: a blanked instance is broken in
    both directions, so it sits above the profile branch rather than in either
    arm of it. If a future refactor moves it inside one, these two tests are
    what notice.
    """

    def test_a_performance_boot_is_kept_awake(self):
        self.assertIn("awake", _boot("gaming"))

    def test_a_density_boot_is_kept_awake(self):
        # Farming needs it MOST: nobody is watching, so a blanked instance
        # stops rendering (and stops earning) unnoticed for hours.
        self.assertIn("awake", _boot("farming"))

    def test_the_untyped_default_is_kept_awake(self):
        self.assertIn("awake", _boot(None))


class TheProfileThatGetsInstalled(unittest.TestCase):
    @staticmethod
    def _settings_for(mode_name, quality=None):
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
                                    mode_name=mode_name, quality=quality)
        return seen["settings"]

    def test_a_gaming_boot_installs_the_high_quality_settings(self):
        # Quality, not just frame rate: `playable`/`gaming` are what the AI
        # SCREENSHOTS, and a screenshot of a deliberately ugly render is a
        # screenshot of a different program.
        self.assertIs(self._settings_for("gaming"),
                      lean.PLAYABLE_APP_SETTINGS)

    def test_a_playable_boot_installs_the_same_high_quality_settings(self):
        self.assertIs(self._settings_for("playable"),
                      lean.PLAYABLE_APP_SETTINGS)

    def test_a_farming_boot_installs_the_low_profile(self):
        self.assertIs(self._settings_for("farming"),
                      lean.CLIENT_APP_SETTINGS)

    def test_an_explicit_quality_flag_wins_over_the_mode(self):
        self.assertIs(self._settings_for("playable", quality="balanced"),
                      lean.GAMING_APP_SETTINGS)
        self.assertIs(self._settings_for("farming", quality="high"),
                      lean.PLAYABLE_APP_SETTINGS)

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
