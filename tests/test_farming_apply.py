#!/usr/bin/env python3
"""apply_farming_squeeze runs each built step over adb; only in farming mode.

    python3 tests/test_farming_apply.py
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import farming  # noqa: E402


class ApplySqueeze(unittest.TestCase):
    def test_runs_every_step_over_adb(self):
        acct = {"name": "u1"}
        with mock.patch.object(omni, "adb") as adb:
            omni.apply_farming_squeeze(acct)
        expected = len(farming.build_squeeze_sequence())
        self.assertEqual(adb.call_count, expected)
        # first positional arg of each call is the account
        for call in adb.call_args_list:
            self.assertIs(call.args[0], acct)


class FarmingGate(unittest.TestCase):
    """_ensure_booted only squeezes on a farming-named, non-dev boot.

    Exercises the NOT-running path (running_pid -> falsy) since the
    already-running branch returns early (before the squeeze gate) when
    boot_completed is already "1". first_boot_done=True makes `first` False
    (dev=False in the product path), matching how cmd_start's build_acct()
    handles always carry first_boot_done=True.
    """

    def _boot(self, mode_name):
        acct = {"name": "u1", "first_boot_done": True}
        with mock.patch.object(omni, "running_pid", return_value=None), \
             mock.patch.object(omni, "spawn_qemu"), \
             mock.patch.object(omni, "maybe_start_autocap"), \
             mock.patch.object(omni, "wait_for_boot", return_value=True), \
             mock.patch.object(omni, "post_boot"), \
             mock.patch.object(omni, "_devkit_activate"), \
             mock.patch.object(omni, "acct_is_dev", return_value=False), \
             mock.patch.object(omni, "resolve_mode",
                 side_effect=lambda cfg, name=None: {
                     "name": name or "playable", "mem": 1, "smp": 1}), \
             mock.patch.object(omni, "apply_farming_squeeze") as sq:
            omni._ensure_booted(acct, {}, "t", mode_name=mode_name)
        return sq

    def test_farming_applies_squeeze(self):
        self.assertTrue(self._boot("farming").called)

    def test_playable_does_not_squeeze(self):
        self.assertFalse(self._boot("playable").called)

    def test_dev_account_never_squeezes(self):
        """Even mode_name='farming', a dev account must never squeeze."""
        acct = {"name": "u1", "first_boot_done": True, "dev": True}
        with mock.patch.object(omni, "running_pid", return_value=None), \
             mock.patch.object(omni, "spawn_qemu"), \
             mock.patch.object(omni, "maybe_start_autocap"), \
             mock.patch.object(omni, "wait_for_boot", return_value=True), \
             mock.patch.object(omni, "post_boot"), \
             mock.patch.object(omni, "_devkit_activate"), \
             mock.patch.object(omni, "acct_is_dev", return_value=True), \
             mock.patch.object(omni, "resolve_mode",
                 side_effect=lambda cfg, name=None: {
                     "name": name or "playable", "mem": 1, "smp": 1}), \
             mock.patch.object(omni, "apply_farming_squeeze") as sq:
            omni._ensure_booted(acct, {}, "t", mode_name="farming")
        self.assertFalse(sq.called)


if __name__ == "__main__":
    unittest.main()
