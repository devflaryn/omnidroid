#!/usr/bin/env python3
"""The kiosk must know what the game is BEFORE it guesses. Every boot.

    python3 tests/test_kiosk_boot_app.py

THE BUG THIS PINS (traced live 2026-08-06 on the rooted arm base):

The kiosk picks what to launch in MainActivity.resolveGamePackage(). When
Settings.Global `omni_game_package` is unset it falls back to "the first
launchable NON-SYSTEM app" — a dev-mode guess. Observed timeline:

    02:15:40.882  OmniKiosk: launching com.topjohnwu.magisk (boot)
    02:15:43.274  settings put global omni_game_package com.roblox.client
    02:15:44.904  E ActivityTaskManager: Attempted Lock Task Mode violation
                     r=...com.roblox.client/.ActivityProtocolLaunch

The manager set the setting 2.4 s AFTER the kiosk had already resolved and
launched. Three things made that fatal rather than cosmetic:

  1. `omni_game_package` was only ever written by deliver_session and by
     provision_settings — and provision_settings never runs on the arm
     bases (their /data is pre-provisioned, first_boot_done is set at create
     time). deliver_session's own comment says it is there so "a LATER REBOOT
     re-joins on its own".
  2. Instances are EPHEMERAL: /data writes are discarded at power-off, so that
     later reboot never inherits the setting. Every boot is a first boot with
     it unset.
  3. Rooting production (the dual-use change) installed the Magisk manager as
     a launchable NON-SYSTEM app. Before that the fallback found nothing to
     pick; now it picks Magisk — which the kiosk then whitelists and PINS
     under Lock Task, so the real game is not on the whitelist when the
     session arrives and its launch is refused.

Roblox is a SYSTEM app here (/product/app/Roblox/Roblox.apk), so it can never
win that fallback scan — the setting is the only thing that selects it.

So the fix is not "stop Magisk"; it is to tell the kiosk what the game is on
every boot, and then make it re-resolve.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


def _acct(**kw):
    a = {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
         "vnc_port": 18001, "base": "arm", "ephemeral": True,
         "first_boot_done": True}
    a.update(kw)
    return a


class ResolvesWhichPackageIsTheGame(unittest.TestCase):
    def test_the_accounts_own_package_wins(self):
        self.assertEqual(
            omni.resolve_game_package(_acct(game_package="com.custom.apk"),
                                      {"base_game": {"arm": "com.roblox.client"}}),
            "com.custom.apk")

    def test_otherwise_the_bases_configured_game(self):
        self.assertEqual(
            omni.resolve_game_package(_acct(),
                                      {"base_game": {"arm": "com.roblox.client"}}),
            "com.roblox.client")

    def test_no_configuration_resolves_to_nothing(self):
        self.assertIsNone(omni.resolve_game_package(_acct(), {}))

    def test_a_broken_config_does_not_raise(self):
        self.assertIsNone(omni.resolve_game_package(_acct(), None))


class EveryBootTellsTheKiosk(unittest.TestCase):
    """Not just the first, and not just a debug boot."""

    def _boot(self, debug=False):
        calls = []
        with mock.patch.object(omni, "running_pid", return_value=None), \
             mock.patch.object(omni, "spawn_qemu", return_value=1), \
             mock.patch.object(omni, "maybe_start_autocap"), \
             mock.patch.object(omni, "wait_for_boot", return_value=True), \
             mock.patch.object(omni, "post_boot"), \
             mock.patch.object(omni, "_enforce_hiding"), \
             mock.patch.object(omni, "_devkit_activate"), \
             mock.patch.object(omni, "apply_roblox_settings"), \
             mock.patch.object(omni, "apply_gaming_tuning"), \
             mock.patch.object(omni, "adb") as adb, \
             mock.patch.object(omni, "_assert_kiosk_foreground",
                               side_effect=lambda *a, **k: calls.append("front")):
            omni._ensure_booted(_acct(), omni.read_config(), "t",
                                mode_name="gaming", debug=debug)
        sent = " ".join(str(c) for c in adb.call_args_list)
        return calls, sent

    def test_a_production_boot_sets_the_game_package(self):
        _, sent = self._boot()
        self.assertIn("omni_game_package", sent)
        self.assertIn("com.roblox.client", sent)

    def test_a_production_boot_refronts_the_kiosk(self):
        calls, _ = self._boot()
        self.assertIn("front", calls)

    def test_a_debug_boot_does_it_too(self):
        calls, sent = self._boot(debug=True)
        self.assertIn("omni_game_package", sent)
        self.assertIn("front", calls)


class ClearsTheWrongLockTaskPin(unittest.TestCase):
    """Observed after the bad boot:

        mLockTaskModeState=LOCKED
        mLockTaskPackages (userId:packages)=      <- empty

    The kiosk had pinned MAGISK, so the game was not on the whitelist and its
    launch was refused. Re-fronting the kiosk has to make it re-run
    launchGame(), which is what re-whitelists and re-pins the right package."""

    def test_the_kiosk_is_restarted_not_merely_focused(self):
        adb_calls = []
        with mock.patch.object(omni, "adb",
                               side_effect=lambda a, *c, **k: adb_calls.append(c)
                               or mock.Mock(stdout="package:/x/kiosk.apk")), \
             mock.patch.object(omni, "_magisk_pkg", return_value="com.topjohnwu.magisk"):
            omni._assert_kiosk_foreground(_acct(), "t")
        flat = " ".join(" ".join(str(x) for x in c) for c in adb_calls)
        self.assertIn("force-stop com.topjohnwu.magisk", flat)
        self.assertIn("com.omni.kiosk/.MainActivity", flat)


if __name__ == "__main__":
    unittest.main()
