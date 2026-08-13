# omnidroid/tests/test_adb_offline_recovery.py
"""Recovering an adb endpoint the host has stuck in `offline`.

OBSERVED on the Windows box: a guest booted all the way to the Android
launcher (sys.boot_completed=1, VNC screenshot showed the home screen) while
`adb devices` kept reporting `127.0.0.1:16001  offline`. `adb connect` on an
endpoint already in the server's table just answers "already connected" and
changes nothing, so wait_for_boot polled a dead entry for its whole timeout:
`start` hung for 15 minutes with no output, and the UI showed no sign of a
boot that had in fact already finished.

Plain disconnect+connect was observed NOT to clear it; `adb kill-server` was.
So recovery escalates, and the heavy step is deliberately late because it
drops every other endpoint on the host.
"""
import unittest
from unittest import mock

from omnidroid import adb as adbmod


ACCT = {"name": "u1", "adb_port": 16001}


class Recovery(unittest.TestCase):
    def _calls(self, hard):
        seen = []

        def fake_run(cmd, timeout):
            seen.append(cmd)
            return mock.Mock(stdout="", stderr="", returncode=0)

        with mock.patch.object(adbmod, "_run_adb", fake_run):
            adbmod.adb_recover(ACCT, hard=hard)
        return [c[1:] for c in seen]      # drop the leading "adb"

    def test_soft_recovery_does_not_kill_the_server(self):
        calls = self._calls(hard=False)
        flat = [" ".join(c) for c in calls]
        self.assertNotIn("kill-server", " ".join(flat))
        self.assertIn("reconnect offline", flat)
        self.assertIn("connect 127.0.0.1:16001", flat)

    def test_soft_recovery_clears_the_stale_entry_first(self):
        flat = [" ".join(c) for c in self._calls(hard=False)]
        self.assertLess(flat.index("disconnect 127.0.0.1:16001"),
                        flat.index("connect 127.0.0.1:16001"))

    def test_hard_recovery_restarts_the_server_then_reconnects(self):
        flat = [" ".join(c) for c in self._calls(hard=True)]
        self.assertIn("kill-server", flat)
        self.assertIn("start-server", flat)
        # Reconnecting AFTER the restart is the whole point.
        self.assertLess(flat.index("kill-server"),
                        flat.index("connect 127.0.0.1:16001"))

    def test_recovery_never_raises(self):
        # It runs inside a poll loop; a failure here must not kill the boot.
        with mock.patch.object(adbmod, "_run_adb",
                               side_effect=OSError("adb gone")):
            adbmod.adb_recover(ACCT)          # must not raise
            adbmod.adb_recover(ACCT, hard=True)

    def test_state_reports_offline_and_never_raises(self):
        with mock.patch.object(adbmod, "_run_adb",
                               return_value=mock.Mock(stdout="offline\n",
                                                      stderr="")):
            self.assertEqual(adbmod.adb_state(ACCT), "offline")
        with mock.patch.object(adbmod, "_run_adb",
                               side_effect=OSError("boom")):
            self.assertEqual(adbmod.adb_state(ACCT), "")


class Escalation(unittest.TestCase):
    def test_the_hard_step_is_much_later_than_the_soft_one(self):
        from omnidroid import engine
        # A few offline reads are normal early in a boot, so the soft attempt
        # must not fire immediately, and the server restart must be rarer
        # still -- it drops every other endpoint on the host.
        self.assertGreater(engine._ADB_SOFT_RECOVER, 3)
        self.assertGreater(engine._ADB_HARD_RECOVER,
                           engine._ADB_SOFT_RECOVER * 2)




class StateReadsBothStreams(unittest.TestCase):
    """REGRESSION: `adb get-state` reports an OFFLINE endpoint on stderr and
    leaves stdout EMPTY. Reading stdout alone returned "" for exactly the
    condition wait_for_boot's recovery keys on, so the recovery never fired
    and a launch sat at its full timeout against a guest that was already up
    -- which is what "booting takes 15 minutes" actually was."""

    def _state(self, stdout="", stderr="", rc=0):
        with mock.patch.object(adbmod, "_run_adb",
                               return_value=mock.Mock(stdout=stdout,
                                                      stderr=stderr,
                                                      returncode=rc)):
            return adbmod.adb_state(ACCT)

    def test_a_healthy_device_still_reads_from_stdout(self):
        self.assertEqual(self._state(stdout="device\n"), "device")

    def test_offline_is_detected_on_stderr(self):
        self.assertEqual(
            self._state(stderr="error: device offline\n", rc=1), "offline")

    def test_a_missing_endpoint_is_unknown_not_offline(self):
        # Must NOT trigger the recovery escalation: nothing is stuck.
        self.assertEqual(
            self._state(stderr="error: device '127.0.0.1:1' not found\n",
                        rc=1), "unknown")

    def test_an_unrecognised_error_is_empty(self):
        self.assertEqual(self._state(stderr="error: something else\n", rc=1), "")

class NoPipeForTheAdbServer(unittest.TestCase):
    """REGRESSION: `adb` forks a long-lived SERVER that inherits our stdout and
    stderr. Handed PIPES (capture_output=True), that daemon holds them open for
    its whole life, so subprocess.run's timeout path -- kill the child, then
    communicate() again to reap it -- blocks in that second communicate()
    FOREVER. Measured: adb_connect sat for 12+ minutes against a QEMU that had
    already exited, with a 15 s timeout set.

    So the transport must never hand adb a pipe. This pins that."""

    def test_adb_is_never_given_a_pipe(self):
        captured = {}

        class FakeProc:
            pid = 1234
            def wait(self, timeout=None):
                return 0

        def fake_popen(cmd, **kw):
            captured.update(kw)
            return FakeProc()

        with mock.patch.object(adbmod.subprocess, "Popen", fake_popen):
            r = adbmod._run_adb(["adb", "devices"], timeout=5)
        self.assertEqual(r.returncode, 0)
        self.assertIsNot(captured.get("stdout"), adbmod.subprocess.PIPE)
        self.assertIsNot(captured.get("stderr"), adbmod.subprocess.PIPE)
        # stdin, too: an inherited console stdin is its own hang.
        self.assertIs(captured.get("stdin"), adbmod.subprocess.DEVNULL)

    def test_a_timeout_kills_the_child_and_raises(self):
        killed = []

        class HangingProc:
            def wait(self, timeout=None):
                if timeout is not None:
                    raise adbmod.subprocess.TimeoutExpired("adb", timeout)
                return 0
            def kill(self):
                killed.append(True)

        with mock.patch.object(adbmod.subprocess, "Popen",
                               lambda cmd, **kw: HangingProc()):
            with self.assertRaises(adbmod.subprocess.TimeoutExpired):
                adbmod._run_adb(["adb", "devices"], timeout=1)
        self.assertTrue(killed, "a timed-out adb must be killed, not left running")


if __name__ == "__main__":
    unittest.main()
