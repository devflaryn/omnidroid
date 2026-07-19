#!/usr/bin/env python3
"""Offline tests for the reused-instance install-recovery matcher.

Pure functions — no VM, no adb. Pins WHICH adb-install failures trigger the
auto force-stop + unpin + uninstall + reinstall path in `omni install`, so a
refactor can't silently stop matching the real adb error strings and let the
agent dead-end again on a reused instance.

    python3 tests/test_install_recovery.py     (or: pytest tests/)
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "manager"))
import omni  # noqa: E402


# Real adb-install stderr seen on a reused instance (verified live).
SIG_MISMATCH = ("Performing Streamed Install\nadb: failed to install x.apk: Failure "
                "[INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package com.roblox.client "
                "signatures do not match newer version; ignoring!]")
DOWNGRADE = "Failure [INSTALL_FAILED_VERSION_DOWNGRADE]"
SUCCESS = "Performing Streamed Install\nSuccess"
OTHER = "Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]"


class InstallRecoveryMatcher(unittest.TestCase):
    def test_signature_mismatch_triggers_recovery(self):
        self.assertTrue(omni._install_needs_clean_replace(SIG_MISMATCH))
        self.assertEqual(omni._install_block_reason(SIG_MISMATCH), "signature mismatch")

    def test_version_downgrade_triggers_recovery(self):
        self.assertTrue(omni._install_needs_clean_replace(DOWNGRADE))
        self.assertEqual(omni._install_block_reason(DOWNGRADE), "version downgrade")

    def test_success_and_unrelated_failures_do_not_trigger(self):
        # Recovery is a signature/downgrade-only escape hatch — it must NOT fire
        # on success (no need) or on an unrelated failure (uninstalling wouldn't
        # help and would needlessly wipe the app).
        self.assertFalse(omni._install_needs_clean_replace(SUCCESS))
        self.assertFalse(omni._install_needs_clean_replace(OTHER))
        self.assertFalse(omni._install_needs_clean_replace(""))
        self.assertFalse(omni._install_needs_clean_replace(None))


if __name__ == "__main__":
    unittest.main(verbosity=2)
