#!/usr/bin/env python3
"""Whatever reports a failed boot must report the REASON the boot gave.

    python3 -m pytest tests/test_failure_reason_reaches_the_user.py -q

Companion to test_qemu_death_is_not_a_timeout.py, which pins the other half:
there, `wait_for_boot` learns to say WHY it gave up. Here, the two things that
turn that into something a user sees stop overwriting it.

Both of them used to hard-code the same guess:

    engine.py cmd_start        result.update({... "error": "boot_timeout"})
    engine.py pool_boot_slot   pool.write_slot_meta(slot, state="failed",
                                                    error="boot_timeout")

So the string a user read had nothing to do with what happened. On the machine
that prompted this, `_pool0/pool.json` said `"error": "boot_timeout"` for a
QEMU that had died 13.7 seconds after being spawned.

The slot meta matters more than it looks: it is written to disk and it SURVIVES.
When the failure is on somebody else's PC, that file is the entire forensic
record, and it was spending its one field on a guess.
"""
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import bootwait  # noqa: E402
from omnidroid import engine  # noqa: E402
from omnidroid import pool  # noqa: E402


DEAD_QEMU = bootwait.BootOutcome(
    False, bootwait.QEMU_EXITED,
    "qemu.log is EMPTY. QEMU was stopped without being able to write a reason.")


class ThePoolSlotRecordsWhatHappened(unittest.TestCase):
    """`pool_boot_slot` writes the only durable record of a background
    failure. It has to be true."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.written = []

        def _record(slot, **kw):
            self.written.append(kw)

        self.patches = [
            mock.patch.object(engine, "_apply_spec_env"),
            mock.patch.object(engine, "build_acct",
                              return_value={"name": "_pool0", "base": "x86"}),
            mock.patch.object(engine, "_ensure_booted",
                              return_value=(DEAD_QEMU, False)),
            mock.patch.object(pool, "write_slot_meta", _record),
            mock.patch.object(pool, "free_slot_name", return_value="_pool0"),
        ]
        for p in self.patches:
            p.start()

    def tearDown(self):
        for p in self.patches:
            p.stop()
        self.tmp.cleanup()

    def _boot(self):
        ok, slot = engine.pool_boot_slot({}, {"mode": "farming"}, "key0")
        self.assertFalse(ok)
        return [w for w in self.written if w.get("state") == "failed"][-1]

    def test_a_dead_qemu_is_not_recorded_as_a_timeout(self):
        meta = self._boot()
        self.assertEqual(meta["error"], "qemu_exited")

    def test_qemus_own_words_are_kept_where_they_can_be_read_later(self):
        """The slot meta is the whole forensic record when the machine belongs
        to somebody else. Losing the detail costs a round trip we may not get:
        the user in the report could not go back for more logs."""
        meta = self._boot()
        self.assertIn("EMPTY", meta.get("detail", ""))


class TheLaunchReportsWhatHappened(unittest.TestCase):
    """`cmd_start`'s JSON is what the app turns into a message on screen."""

    def test_a_dead_qemu_is_not_reported_as_a_timeout(self):
        result = {}
        engine._apply_boot_failure(result, DEAD_QEMU)
        self.assertEqual(result["error"], "qemu_exited")
        self.assertFalse(result["ok"])
        self.assertFalse(result["booted"])

    def test_the_message_carries_the_evidence(self):
        result = {}
        engine._apply_boot_failure(result, DEAD_QEMU)
        self.assertIn("EMPTY", result["message"])

    def test_a_real_timeout_is_still_called_a_timeout(self):
        """The label is not being retired -- it is being made true."""
        result = {}
        engine._apply_boot_failure(
            result, bootwait.BootOutcome(False, bootwait.BOOT_TIMEOUT,
                                         "still making progress at 600s"))
        self.assertEqual(result["error"], "boot_timeout")

    def test_a_bare_false_still_produces_a_usable_failure(self):
        """Thirteen call sites and several of them are dev commands. A caller
        that has not been converted must not produce a result with no error at
        all -- that would trade a wrong label for a missing one."""
        result = {}
        engine._apply_boot_failure(result, False)
        self.assertTrue(result.get("error"))
        self.assertFalse(result["ok"])


if __name__ == "__main__":
    unittest.main()
