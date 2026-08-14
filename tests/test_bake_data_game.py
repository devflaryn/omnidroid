#!/usr/bin/env python3
"""Baking the game into /data — and being able to REPLACE it on every update.

    python3 tests/test_bake_data_game.py

Two requirements, and the second is what shapes the design:

  1. `omni_game_package` must already be in /data when the guest boots, so the
     kiosk's resolveGamePackage() never falls back to its dev-mode guess (it
     guessed the Magisk manager on a rooted base — see test_kiosk_boot_app.py).
     A host-side `settings put` after boot is always too late: the kiosk has
     resolved, launched and PINNED its choice under Lock Task by then.

  2. Roblox updates often. Baking it into the SYSTEM image (`omnidroid bake-game`,
     /product/app/Roblox) means rebuilding a 2.3 GB base with ~6 GiB of scratch
     for every update. Installing it into /DATA instead makes an update one
     ~2-minute command, because an updated system app lives in /data/app and
     the package name never changes across versions.

The anti-chaining rule is the subtle part. The bake writes a THIN COW overlay
of the PRISTINE rooted /data, and every re-bake must start from that same
pristine image again. If it instead overlaid whatever `data` currently points
at, each Roblox update would stack a new overlay on the previous one and the
chain would grow without bound — carrying every superseded APK forever.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import bases  # noqa: E402


ROOTED = {"type": "arm-uefi", "system": "base_arm_system_rooted.qcow2",
          "data": "base_arm_data_rooted.qcow2", "rooted": True,
          "root_manifest": {"rooted_data": "base_arm_data_rooted.qcow2"}}
BAKED = {"type": "arm-uefi", "system": "base_arm_system_rooted.qcow2",
         "data": "base_arm_data_game.qcow2", "rooted": True,
         "root_manifest": {"rooted_data": "base_arm_data_rooted.qcow2"}}
UNROOTED = {"type": "arm-uefi", "system": "base_arm_system.qcow2",
            "data": "base_arm_data.qcow2"}


class NeverChainsOverlays(unittest.TestCase):
    def test_a_fresh_rooted_base_bakes_from_its_own_data(self):
        self.assertEqual(bases.data_bake_source(ROOTED),
                         "base_arm_data_rooted.qcow2")

    def test_re_baking_goes_back_to_the_pristine_data_not_the_last_bake(self):
        # THE anti-chaining assertion: `data` already points at the baked
        # output, and the next bake must still start from the rooted image.
        self.assertEqual(bases.data_bake_source(BAKED),
                         "base_arm_data_rooted.qcow2")

    def test_re_baking_is_idempotent_in_its_source(self):
        self.assertEqual(bases.data_bake_source(BAKED),
                         bases.data_bake_source(ROOTED))

    def test_an_unrooted_base_falls_back_to_its_plain_data(self):
        self.assertEqual(bases.data_bake_source(UNROOTED),
                         "base_arm_data.qcow2")

    def test_the_baked_output_is_never_its_own_source(self):
        self.assertNotEqual(bases.data_bake_source(BAKED), bases.ARM_GAME_DATA)


class TheGuestScript(unittest.TestCase):
    PKG = "com.roblox.client"

    def test_it_writes_the_setting_that_fixes_the_magisk_guess(self):
        s = bases.build_game_bake_script(self.PKG, None)
        self.assertIn("settings put global omni_game_package "
                      "com.roblox.client", s)

    def test_setting_only_bake_installs_nothing(self):
        s = bases.build_game_bake_script(self.PKG, None)
        self.assertNotIn("pm install", s)

    def test_an_apk_is_installed_as_an_update(self):
        s = bases.build_game_bake_script(self.PKG, "/data/local/tmp/game.apk")
        # -r so it REPLACES the pre-installed system app rather than failing
        # with INSTALL_FAILED_ALREADY_EXISTS; -d so a downgrade is allowed
        # (rolling back a bad Roblox build must not need a base rebuild).
        self.assertRegex(s, r"pm install[^\n]*-r")
        self.assertRegex(s, r"pm install[^\n]*-d")
        self.assertIn("/data/local/tmp/game.apk", s)

    def test_it_reports_a_verifiable_marker(self):
        # The bake must be able to FAIL rather than capture a broken /data.
        s = bases.build_game_bake_script(self.PKG, None)
        self.assertIn(bases.GAME_BAKE_OK, s)

    def test_the_marker_is_only_echoed_after_the_setting_reads_back(self):
        s = bases.build_game_bake_script(self.PKG, None)
        self.assertIn("settings get global omni_game_package", s)

    def test_the_staged_apk_is_cleaned_up(self):
        s = bases.build_game_bake_script(self.PKG, "/data/local/tmp/game.apk")
        self.assertIn("rm -f /data/local/tmp/game.apk", s)


class AFailedInstallMustNotReportSuccess(unittest.TestCase):
    """MEASURED 2026-08-06, and this bug was in THIS file's first version:

        guest: Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package
               com.roblox.client signatures do not match newer version;
               ignoring!]
        OMNI_GAME_BAKE_OK
        captured -> base_arm_data_game.qcow2 (221 MB thin overlay)

    `pm install` failed, the script checked only that the SETTING read back,
    printed its success marker anyway, and a 221 MB /data that did not contain
    the new APK was captured and made the shipping image. A bake that cannot
    fail is not a bake, it is a coin flip."""

    PKG = "com.roblox.client"
    APK = "/data/local/tmp/game.apk"

    def test_the_install_result_is_checked(self):
        s = bases.build_game_bake_script(self.PKG, self.APK)
        self.assertIn("Success", s)

    def test_the_script_stops_before_the_ok_marker_on_a_failed_install(self):
        s = bases.build_game_bake_script(self.PKG, self.APK)
        install_at = s.index("pm install")
        ok_at = s.index(bases.GAME_BAKE_OK)
        between = s[install_at:ok_at]
        self.assertRegex(between, r"exit 1|\|\| \{")

    def test_a_distinct_marker_names_the_install_failure(self):
        s = bases.build_game_bake_script(self.PKG, self.APK)
        self.assertIn(bases.GAME_BAKE_INSTALL_FAILED, s)

    def test_the_two_markers_are_not_substrings_of_each_other(self):
        # `if OK in output` must not match the failure line.
        self.assertNotIn(bases.GAME_BAKE_OK, bases.GAME_BAKE_INSTALL_FAILED)
        self.assertNotIn(bases.GAME_BAKE_INSTALL_FAILED, bases.GAME_BAKE_OK)

    def test_a_setting_only_bake_has_nothing_to_check(self):
        s = bases.build_game_bake_script(self.PKG, None)
        self.assertNotIn(bases.GAME_BAKE_INSTALL_FAILED, s)


class PackageNameResolution(unittest.TestCase):
    """Roblox's package name never changes across updates, so the bake must
    NOT depend on the Android SDK's aapt2 being installed just to read it."""

    def test_an_explicit_package_wins(self):
        self.assertEqual(
            bases.resolve_bake_package("com.example.game", "arm", {}),
            "com.example.game")

    def test_otherwise_the_bases_configured_game(self):
        cfg = {"base_game": {"arm": "com.roblox.client"}}
        self.assertEqual(bases.resolve_bake_package(None, "arm", cfg),
                         "com.roblox.client")

    def test_a_missing_config_is_not_a_crash(self):
        self.assertIsNone(bases.resolve_bake_package(None, "arm", {}))
        self.assertIsNone(bases.resolve_bake_package(None, "arm", None))


if __name__ == "__main__":
    unittest.main()
