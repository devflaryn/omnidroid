#!/usr/bin/env python3
"""Unattended consent: the instance never waits for a human tap.

    python3 tests/test_consent.py

The requirements these tests pin:

  1. the guest script is QUOTED as one argument — adb re-parses a joined argv,
     and an unquoted `a; b` runs a fragment of itself while still reporting
     success (the exact failure farming.sh was written to stop);
  2. the GAME package is covered even though it is an updated system app and
     therefore absent from `pm list packages -3`;
  3. nothing touches `wm size`/`wm density` — the flag is read live, and that
     kick would silently undo a farming boot's 480x270;
  4. state is REPORTED from a read-back, so "applied" can never be printed on
     the strength of having sent the commands;
  5. the kill switch works, because reproducing a crash-loop by eye needs the
     dialogs back.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import consent  # noqa: E402


GAME = "com.roblox.client"


def _script(step):
    """The guest script out of an adb argv vector ['shell', '<script>']."""
    return step[-1]


class TheSequenceIsSafeOverAdb(unittest.TestCase):
    def test_every_step_is_sh_c_with_the_script_as_one_quoted_argument(self):
        # farming.sh's shape: ["shell", "sh", "-c", "<quoted script>"]. adb
        # joins argv and re-parses it in the guest, so the whole multi-command
        # script has to arrive as a SINGLE pre-quoted word — otherwise the
        # guest runs `sh -c <first word>` and the rest as separate commands,
        # and the step still reports success.
        for step in consent.build_consent_sequence(GAME):
            self.assertEqual(step[:3], ["shell", "sh", "-c"], step)
            self.assertEqual(len(step), 4, step)
            self.assertTrue(_script(step).startswith(("'", '"')),
                            f"not quoted: {_script(step)[:40]}")

    def test_the_dialogs_are_silenced_before_the_long_loop(self):
        # Order is load-bearing: a crash DURING the permission loop must not
        # be the one that parks a modal on screen.
        steps = consent.build_consent_sequence(GAME)
        self.assertIn("hide_error_dialogs", _script(steps[0]))
        self.assertIn("appops", _script(steps[1]))

    def test_it_never_touches_the_display_levers(self):
        # `wm size`/`wm density` would undo a farming boot's 480x270. The flag
        # is read live (A/B'd on a live instance), so no kick is needed.
        for step in consent.build_consent_sequence(GAME):
            self.assertNotIn("wm size", _script(step))
            self.assertNotIn("wm density", _script(step))


class TheGameIsCovered(unittest.TestCase):
    def test_the_game_package_is_added_to_the_third_party_list(self):
        # It is installed as an UPDATED SYSTEM APP, so `pm list packages -3`
        # does not list it — the one package the policy exists for.
        script = _script(consent.build_consent_sequence(GAME)[1])
        self.assertIn("pm list packages -3", script)
        self.assertIn(GAME, script)
        # …and deduped, since a dev/adb-installed build IS third-party and
        # would otherwise be processed twice.
        self.assertIn("printf", consent._package_list_expr(GAME))
        self.assertIn("sort -u", consent._package_list_expr(GAME))

    def test_no_game_package_still_covers_third_party_apps(self):
        expr = consent._package_list_expr(None)
        self.assertIn("pm list packages -3", expr)
        self.assertNotIn("printf", expr)   # nothing to append, nothing to dedupe
        self.assertIn("pm list packages -3",
                      _script(consent.build_consent_sequence(None)[1]))

    def test_full_disk_access_is_in_the_op_list(self):
        self.assertIn("MANAGE_EXTERNAL_STORAGE", consent.APP_OPS)
        self.assertIn("REQUEST_INSTALL_PACKAGES", consent.APP_OPS)
        script = _script(consent.build_consent_sequence(GAME)[1])
        for op in consent.APP_OPS:
            self.assertIn(op, script)


class PersistingIsNotApplying(unittest.TestCase):
    """A bake writes an IMAGE, so it has to force the RAM-held state to disk
    and then prove it is there. The first version of the bake did neither and
    committed images whose app-ops read back as "No operations."."""

    def test_the_persist_step_flushes_app_ops_and_syncs(self):
        script = _script(consent.build_persist_script())
        self.assertIn("appops write-settings", script)
        self.assertIn("sync", script)

    def test_the_reload_step_discards_ram_state(self):
        # `read-settings` replaces the in-RAM state from disk — an op that
        # still reads `allow` afterwards is genuinely persisted.
        self.assertIn("appops read-settings",
                      _script(consent.build_reload_script()))

    def test_persisting_is_not_part_of_the_per_boot_sequence(self):
        # A launch is ephemeral: flushing to a disk whose writes are discarded
        # at power-off would be pure cost.
        for step in consent.build_consent_sequence(GAME):
            self.assertNotIn("write-settings", _script(step))
            self.assertNotIn("read-settings", _script(step))


class StateIsReadBackNotAssumed(unittest.TestCase):
    APPLIED = ("hide_error_dialogs=1\n"
               "full_disk=MANAGE_EXTERNAL_STORAGE: allow\n"
               "install_unknown=REQUEST_INSTALL_PACKAGES: allow\n")

    def test_a_fully_applied_guest_reads_back_as_applied(self):
        st = consent.parse_consent_state(self.APPLIED)
        self.assertEqual(st, {"dialogs_hidden": True, "full_disk": True,
                              "install_unknown": True})

    def test_the_pre_policy_shape_reads_back_as_not_applied(self):
        # The exact strings a live instance produced before the policy ran.
        st = consent.parse_consent_state(
            "hide_error_dialogs=null\n"
            "full_disk=MANAGE_EXTERNAL_STORAGE: default; rejectTime=+1h28m ago\n"
            "install_unknown=No operations.\n")
        self.assertEqual(st, {"dialogs_hidden": False, "full_disk": False,
                              "install_unknown": False})

    def test_unreadable_output_is_not_applied_rather_than_a_crash(self):
        for junk in ("", None, "error: device offline"):
            self.assertEqual(consent.parse_consent_state(junk),
                             {"dialogs_hidden": False, "full_disk": False,
                              "install_unknown": False})

    def test_the_marker_distinguishes_finished_from_died_halfway(self):
        self.assertTrue(consent.applied_ok(f"x\n{consent.CONSENT_OK}\n"))
        self.assertFalse(consent.applied_ok("appops: command not found"))
        self.assertFalse(consent.applied_ok(""))

    def test_counts_are_parsed_or_none_never_guessed(self):
        self.assertEqual(
            consent.parse_counts(f"{consent.COUNTS_PREFIX} 2 12 6 18\n"
                                 f"{consent.CONSENT_OK}"),
            (2, 12, 6, 18))
        self.assertIsNone(consent.parse_counts("nothing here"))

    def test_the_summary_says_nothing_landed_when_nothing_did(self):
        line = consent.summary_line(None, consent.parse_consent_state(""))
        self.assertIn("NOTHING landed", line)

    def test_the_summary_quotes_the_read_back_not_the_intent(self):
        line = consent.summary_line((2, 12, 6, 18),
                                    consent.parse_consent_state(self.APPLIED))
        self.assertIn("full disk access", line)
        self.assertIn("error dialogs off", line)
        self.assertIn("6 runtime permission", line)

    def test_a_bake_never_claims_the_halves_that_do_not_persist(self):
        # MEASURED twice, on both images: with the boot-time step disabled, a
        # committed image reads hide_error_dialogs=1 but its app-ops are back
        # to `default`. A bake that reported "full disk access" as baked would
        # be describing state the image does not carry.
        line = consent.baked_summary((2, 12, 6, 18),
                                     consent.parse_consent_state(self.APPLIED))
        self.assertIn("error dialogs off (image-resident)", line)
        self.assertIn("NOT image state", line)
        self.assertIn("re-applies them on every launch", line)

    def test_a_bake_with_nothing_persistable_says_so(self):
        line = consent.baked_summary(None, consent.parse_consent_state(""))
        self.assertIn("nothing worth committing", line)


class TheKillSwitch(unittest.TestCase):
    def test_absent_or_empty_means_enabled(self):
        self.assertTrue(consent.consent_enabled({}))
        self.assertTrue(consent.consent_enabled({consent.NO_CONSENT_ENV: ""}))

    def test_truthy_values_disable_it(self):
        for v in ("1", "true", "TRUE", "yes", "on"):
            self.assertFalse(
                consent.consent_enabled({consent.NO_CONSENT_ENV: v}), v)

    def test_a_falsey_value_leaves_it_on(self):
        for v in ("0", "false", "no"):
            self.assertTrue(
                consent.consent_enabled({consent.NO_CONSENT_ENV: v}), v)


if __name__ == "__main__":
    unittest.main(verbosity=2)
