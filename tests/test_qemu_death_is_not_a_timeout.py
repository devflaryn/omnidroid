#!/usr/bin/env python3
"""A QEMU that died is not a boot that timed out. Say which one happened.

    python3 -m pytest tests/test_qemu_death_is_not_a_timeout.py -q

MEASURED ON A USER'S MACHINE, 2026-08-22. A fresh install, first ever launch.
The warm pool's `_pool0` slot recorded:

    started 1787410457.4657   run.json pid 19044 at 1787410457.8005
    updated 1787410471.4719   state "failed"   error "boot_timeout"

QEMU was gone **13.7 seconds** after it was spawned, and `qemu.log` was zero
bytes. Nothing timed out. What actually fired is `wait_for_boot`'s dead-process
check -- `pid is None and elapsed > 10` -- whose own message is the useful one:

    QEMU exited before the guest booted -- see .../qemu.log

But `wait_for_boot` returns a bare `False` for that, exactly as it does for a
genuine stall, and both `cmd_start` and `pool_boot_slot` then write the literal
string "boot_timeout". So the one fact that would have identified the failure --
that the VM process DIED, rather than booted slowly -- was thrown away at the
return statement, and the label that replaced it sent everyone reading it to
the boot-wait code, which was not involved.

That is what these tests pin. The reason a boot ended must survive out of
`wait_for_boot` and reach whatever reports the failure, so that a user's report
of "it said boot timeout" means the boot actually timed out.

`BootOutcome` is falsy when it failed, so every `if not wait_for_boot(...)`
call site keeps working unchanged -- there are thirteen of them and they are
all asking the same yes/no question they always were.
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import bootwait  # noqa: E402
from omnidroid import engine  # noqa: E402


class FakeTime:
    """Stands in for `engine.time`, so a wait that takes minutes of wall clock
    takes none. `sleep` advances the clock instead of spending it."""

    def __init__(self, start=1000.0):
        self.t = start

    def time(self):
        return self.t

    def sleep(self, dt):
        self.t += dt

    def monotonic(self):
        return self.t


class QemuDyingIsNamed(unittest.TestCase):
    """The dead-process path must report itself as a death, not a timeout."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.d = Path(self.tmp.name)
        self.acct = {"name": "_pool0", "base": "x86"}
        self.clock = FakeTime()

    def tearDown(self):
        self.tmp.cleanup()

    def _wait(self, qemu_log=""):
        """Run wait_for_boot against a QEMU that is already gone."""
        (self.d / "qemu.log").write_text(qemu_log)
        with mock.patch.object(engine, "time", self.clock), \
             mock.patch.object(engine, "running_pid", return_value=None), \
             mock.patch.object(engine, "acct_base_is_arm", return_value=False), \
             mock.patch.object(engine, "account_dir", return_value=self.d), \
             mock.patch.object(engine, "runtime_dir", return_value=self.d), \
             mock.patch.object(engine, "adb_connect"), \
             mock.patch.object(engine, "adb_getprop", return_value=""), \
             mock.patch.object(engine, "adb_state", return_value=""):
            return engine.wait_for_boot(self.acct, None, "pool _pool0")

    def test_it_is_falsy_so_every_existing_caller_still_works(self):
        """Thirteen call sites say `if not wait_for_boot(...)`. They must not
        have to change to keep asking the same question."""
        self.assertFalse(self._wait())

    def test_the_reason_is_qemu_exited_not_a_timeout(self):
        outcome = self._wait()
        self.assertEqual(outcome.reason, "qemu_exited")
        self.assertNotEqual(outcome.reason, "boot_timeout")

    def test_qemus_own_last_words_are_carried_out_with_it(self):
        """The reason QEMU gives is in qemu.log and nowhere else -- the process
        is detached, so nothing else ever sees its stderr. Carrying it in the
        outcome is what puts it in front of the user instead of in a file on a
        machine we cannot reach."""
        outcome = self._wait(qemu_log="qemu-system-x86_64: Could not open "
                                      "'x86/data-template-8g.qcow2': No such "
                                      "file or directory\n")
        self.assertIn("data-template-8g.qcow2", outcome.detail)

    def test_an_empty_qemu_log_is_reported_as_the_fact_it_is(self):
        """A zero-byte log is not "no information" -- it is the signature of a
        guest killed without a chance to speak (a full disk, or a hard crash in
        a host driver). The user in the report above had exactly this, and a
        blank detail would have hidden it."""
        outcome = self._wait(qemu_log="")
        self.assertTrue(outcome.detail,
                        "an empty qemu.log produced an empty explanation")
        self.assertIn("empty", outcome.detail.lower())


class TheOutcomeType(unittest.TestCase):
    """`BootOutcome` lives in bootwait, which is the module that already owns
    the question 'why did this boot end'."""

    def test_a_successful_boot_is_truthy_and_says_so(self):
        ok = bootwait.BootOutcome(True, "booted")
        self.assertTrue(ok)
        self.assertEqual(ok.reason, "booted")

    def test_a_failure_is_falsy(self):
        self.assertFalse(bootwait.BootOutcome(False, "stalled"))

    def test_the_reason_is_never_empty_on_a_failure(self):
        """An unnamed failure is the bug this file exists about."""
        with self.assertRaises(ValueError):
            bootwait.BootOutcome(False, "")


if __name__ == "__main__":
    unittest.main()
