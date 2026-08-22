#!/usr/bin/env python3
"""The marker that tells the in-game executor menu it is inside Omnidroid.

    python3 tests/test_execmark.py

WHAT IS ACTUALLY BEING PROTECTED. The menu served from /gist branches on one
question — `isfile("omni_host.data")` — and gets a different product depending
on the answer: a card that dismisses itself and leaves the screen clean, or a
floating button parked over the game forever. Nothing in the guest, in adb, or
in this repo's logs reports which branch a live instance took; the only symptom
of a marker that never landed is a farming capture with a button in it, weeks
later.

So the three properties below are the ones with no other witness:

  * EVERY candidate root is written, because the executor's workspace is not
    established and a single-path write is a coin flip;
  * the script survives `adb shell`'s argv-joining — the quoting trap
    farming.sh documents, which silently guts a multi-command script while
    still reporting success;
  * a missing root is CREATED and a failing root is SKIPPED rather than
    aborting the rest, because this runs on every boot and must never be the
    reason one fails.
"""
import os
import shlex
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import execmark  # noqa: E402


class TheMarkerName(unittest.TestCase):
    def test_is_the_name_the_payload_asks_for(self):
        # The other half of this contract is asserted in
        # omni-backend/backend/tests/omniExecUi.test.js, against the assembled
        # payload. Both halves have to be edited together; neither can see the
        # other at runtime.
        self.assertEqual(execmark.MARKER_NAME, "omni_host.data")

    def test_body_names_the_host_and_carries_the_mode(self):
        self.assertEqual(execmark.marker_body(None), "omnidroid")
        self.assertEqual(execmark.marker_body("farming"), "omnidroid:farming")


class TheCandidateRoots(unittest.TestCase):
    def test_cover_private_storage_and_the_sdcard(self):
        roots = execmark.candidate_roots("com.roblox.client")
        self.assertTrue(any(r.startswith("/data/data/") for r in roots),
                        "the app's private files dir is the likeliest home")
        self.assertTrue(any(r.startswith("/storage/emulated/0/Android/data/")
                            for r in roots),
                        "a scoped-storage build keeps its workspace in the "
                        "EXTERNAL files dir; leaving it out is the easy miss")
        self.assertTrue(any("Arceus X" in r for r in roots),
                        "Arceus names its own workspace directory")

    def test_follow_the_game_package(self):
        roots = execmark.candidate_roots("com.example.game")
        self.assertTrue(any("com.example.game" in r for r in roots))
        self.assertFalse(any("com.roblox.client" in r for r in roots))

    def test_default_to_roblox(self):
        self.assertTrue(any("com.roblox.client" in r
                            for r in execmark.candidate_roots(None)))

    def test_are_unique(self):
        roots = execmark.candidate_roots()
        self.assertEqual(len(roots), len(set(roots)),
                         "a duplicated root writes twice and miscounts")


class TheWriteScript(unittest.TestCase):
    def setUp(self):
        self.steps = execmark.build_marker_script("farming", "com.roblox.client")
        # What the GUEST shell finally sees: adb joins the argv and re-parses
        # it, so the quoting farming.sh applies has to be undone to read the
        # script the way the guest will.
        self.script = shlex.split(" ".join(self.steps))[3]

    def test_is_one_adb_shell_step(self):
        self.assertEqual(self.steps[:3], ["shell", "sh", "-c"])
        self.assertEqual(len(self.steps), 4)

    def test_survives_adb_argv_joining(self):
        # adb joins argv with spaces and re-parses the result in the guest, so
        # an unquoted `a; b` runs a FRAGMENT of itself and still exits 0. The
        # payload must therefore arrive as one shell word.
        rejoined = shlex.split(" ".join(self.steps))
        self.assertEqual(len(rejoined), 4,
                         "the script must survive as a single argument")
        self.assertIn(execmark.MARKER_NAME, rejoined[3])

    def test_writes_every_candidate_root(self):
        for root in execmark.candidate_roots("com.roblox.client"):
            self.assertIn(root, self.steps[3],
                          f"{root} is never written, so the executor may "
                          f"resolve isfile against a directory we missed")

    def test_creates_a_root_that_is_absent(self):
        self.assertIn("mkdir -p", self.script)

    def test_carries_the_mode_into_the_marker(self):
        self.assertIn("omnidroid:farming", self.script)

    def test_hands_the_marker_to_the_app_uid(self):
        # Created by root (adb is uid 0 on the x86 base), read by the app. A
        # 0600 root-owned file inside a directory the app does not own answers
        # isfile() with false while sitting exactly where it belongs.
        self.assertIn("chown", self.script)
        self.assertIn("chmod 0644", self.script)
        self.assertIn("restorecon", self.script)

    def test_never_fails_the_boot(self):
        self.assertTrue(self.script.rstrip().endswith("true"),
                        "the step runs on every boot; its exit code must not "
                        "be able to fail one")
        self.assertIn("continue", self.script,
                      "one unusable root must not skip the others")

    def test_reports_a_count_the_engine_can_read(self):
        self.assertIn(execmark.MARK_PREFIX, self.script)
        self.assertEqual(execmark.parse_counts("OMNI_EXECMARK 5 8"), (5, 8))
        self.assertEqual(execmark.parse_counts("nothing here"), (0, 0))


class TheProbe(unittest.TestCase):
    def test_reads_without_writing(self):
        script = execmark.build_marker_probe("com.roblox.client")[3]
        self.assertNotIn("mkdir", script)
        self.assertNotIn("echo omnidroid", script)
        self.assertIn("-r ", script)

    def test_parses_the_roots_it_found(self):
        out = ("PRESENT /data/data/com.roblox.client/files\n"
               "PRESENT /storage/emulated/0/Arceus X/Workspace\n")
        self.assertEqual(execmark.present_roots(out), [
            "/data/data/com.roblox.client/files",
            "/storage/emulated/0/Arceus X/Workspace",
        ])
        self.assertEqual(execmark.present_roots(""), [])


class TheKillSwitch(unittest.TestCase):
    def test_defaults_to_enabled(self):
        self.assertTrue(execmark.execmark_enabled({}))
        self.assertTrue(execmark.execmark_enabled(None))

    def test_disables_on_a_truthy_value(self):
        for value in ("1", "true", "YES", "on"):
            self.assertFalse(
                execmark.execmark_enabled({execmark.NO_EXECMARK_ENV: value}),
                value)

    def test_stays_enabled_on_a_falsy_value(self):
        for value in ("", "0", "no"):
            self.assertTrue(
                execmark.execmark_enabled({execmark.NO_EXECMARK_ENV: value}),
                value)


class TheSummaryLine(unittest.TestCase):
    def test_says_generic_device_when_nothing_landed(self):
        self.assertIn("generic device", execmark.summary_line(0, 8))
        self.assertIn("generic device", execmark.summary_line(0, 0))

    def test_names_the_file_when_it_did(self):
        line = execmark.summary_line(6, 8)
        self.assertIn(execmark.MARKER_NAME, line)
        self.assertIn("6/8", line)


if __name__ == "__main__":
    unittest.main()
