#!/usr/bin/env python3
"""Writing ClientAppSettings.json must not take the game's files/ dir away.

    python3 tests/test_client_settings_perms.py

MEASURED on a live instance (2026-08-06), after a boot that installed the
settings file. Every directory in the game's private data belongs to the app
(uid 10138) EXCEPT the one this script created:

    drwx------ 12 10138 10138  /data/data/com.roblox.client/
    drwxrwx--x  5 10138 10138  ./app_assets
    drwxrwx--x  2 10138 10138  ./databases
    drwxr-xr-x  3 0     0      ./files              <-- root:root, mode 755
    drwxr-xr-x  2 10138 10138  ./files/ClientSettings

`mkdir -p .../files/ClientSettings` running as root CREATES THE INTERMEDIATE
`files/` as root:root, and the chown that follows only covered the leaf. The
app then cannot create anything under its own files/ dir, and the logcat fills
with the consequences:

    E SplitCompat:      Unable to create directory: .../files/splitcompat
    E CrossProcessLock: .../files/generatefid.lock: EACCES (Permission denied)
    E FA:               .../files/google_app_measurement.db: EACCES
    E rbx.xapkmanager:  .../files/exe/ssl/cacert.pem: ENOENT

— after which the game never finishes initialising and drops out of the
foreground. This is the "Roblox black-screens" symptom, and it is caused by
the settings installer rather than by the APK.

The rule: every directory this script creates inside the app's sandbox must
end up owned and labelled as the APP, not as root.
"""
import os
import re
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import farming, lean  # noqa: E402

APP_DIR = "/data/data/com.roblox.client"
FILES_DIR = APP_DIR + "/files"


class OwnershipIsRestoredForEveryCreatedDir(unittest.TestCase):
    def setUp(self):
        self.script = farming.build_client_settings_script("su")

    def test_the_files_dir_is_chowned_not_just_the_leaf(self):
        # The bug: chown -R covered only .../files/ClientSettings. The path
        # must END at files/ — matching `.../files/ClientSettings` here is
        # precisely the false pass that let the bug ship.
        self.assertRegex(self.script,
                         rf"chown -R \$U:\$U {re.escape(FILES_DIR)}(?![/\w])")

    def test_the_files_dir_is_relabelled_for_selinux(self):
        # A root-created dir also carries the wrong SELinux context; without
        # restorecon the app is denied even once the uid is right.
        self.assertRegex(self.script,
                         rf"restorecon -R {re.escape(FILES_DIR)}(?![/\w])")

    def test_it_still_fixes_the_settings_dir(self):
        self.assertIn(lean.CLIENT_SETTINGS_DIR, self.script)

    def test_it_never_chowns_the_whole_app_sandbox(self):
        # Over-correcting is its own bug: cache/ and code_cache/ are owned
        # 10138:20138 (a different GROUP), so a blanket `chown -R` over the
        # package dir would break them.
        self.assertNotRegex(self.script,
                            rf"chown -R \$U:\$U {re.escape(APP_DIR)}\s*(;|$)")

    def test_the_uid_still_comes_from_the_package_dir(self):
        self.assertIn(f"stat -c %u /data/data/{farming.GAME_PKG}", self.script)


class StillWorksAtAll(unittest.TestCase):
    def test_no_root_means_no_script(self):
        self.assertIsNone(farming.build_client_settings_script(None))

    def test_the_settings_body_is_still_written(self):
        s = farming.build_client_settings_script("su")
        self.assertIn("DFIntTaskSchedulerTargetFps", s)
        self.assertIn(lean.CLIENT_SETTINGS_FILE, s)

    def test_the_gaming_profile_is_carried_through(self):
        s = farming.build_client_settings_script(
            "su", settings=lean.GAMING_APP_SETTINGS)
        self.assertIn("240", s)


if __name__ == "__main__":
    unittest.main()
