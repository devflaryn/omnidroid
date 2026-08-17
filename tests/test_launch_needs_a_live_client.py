#!/usr/bin/env python3
"""A launch with no game process in it is not a successful launch.

    python3 -m pytest tests/test_launch_needs_a_live_client.py -q

MEASURED 2026-08-17. Two farming instances on PS99 at `-m 2048`, four client
deaths between them, and `start` reported `ok: true, in_world: true` for every
one of them:

    farm2   14:20:22 start 4600  ->  14:21:31 died  fg  TOP   (69 s)
            14:27:31 start 7023  ->  14:27:43 died  prcp TOP  (12 s)
    farm3   14:28:11 start 4576  ->  14:29:11 died  fg  TOP   (60 s)
            14:35:14 start 6376  ->  14:35:27 died  fg  TOP   (13 s)

Two separate defects made that possible, and this file pins both.

FIRST: `in_world` came out of the client's own LOG FILE. The join markers are
real -- the client genuinely joined -- and they stay in the file after lmkd
kills the process. So the log was telling the truth about something that had
stopped being true six minutes earlier. A log says what HAPPENED; only a pid
says what IS.

SECOND: success was decided before anything watched the client. The
2026-08-17 change correctly stopped blocking the launch for 120 s and handed
the watching to the memory governor -- but `start` still returned `ok: true`
straight away, and the governor recorded `client_died_after_s: 11` into
run.json afterwards, where nothing surfaced it. The caller had already been
told it worked.

This is the same judgement `client_died_after_squeeze` already made, extended
to the case the squeeze cannot be blamed for: a client killed while it was
still LOADING never reached the squeeze at all.
"""
import inspect
import unittest
from unittest import mock

from omnidroid import engine


class _R:
    def __init__(self, stdout=""):
        self.stdout = stdout


ACCT = {"name": "farm2", "adb_port": 16001, "game_package": "com.roblox.client"}
JOINED_LOG = "... Connection accepted ... DataModel::doDataModelSetup ..."


class InWorldNeedsAProcess(unittest.TestCase):

    def _probe(self, body, running):
        with mock.patch.object(engine, "root_shell", return_value=_R(body)), \
             mock.patch.object(engine, "game_is_running",
                               return_value=running):
            return engine.probe_client_join(ACCT)

    def test_a_joined_log_with_no_process_is_not_in_the_world(self):
        """THE BUG, exactly. The log is a truthful record of a join by a client
        that no longer exists."""
        got = self._probe(JOINED_LOG, False)
        self.assertFalse(got["in_world"])
        # The join itself is still reported -- it happened, and losing that
        # would make a client killed after joining indistinguishable from one
        # that never got in.
        self.assertTrue(got["joined_marker"])
        self.assertFalse(got["game_running"])

    def test_a_joined_log_with_a_live_process_is_in_the_world(self):
        got = self._probe(JOINED_LOG, True)
        self.assertTrue(got["in_world"])
        self.assertTrue(got["game_running"])

    def test_could_not_ask_does_not_demote_a_join(self):
        """None is "no information", not "dead". If an unanswered adb probe
        could clear `in_world`, then the mode built for slow guests would
        report its slowest instances as failures -- and the pre-existing
        contract (log says joined -> in_world) would silently change meaning
        on every host where this probe cannot run."""
        got = self._probe(JOINED_LOG, None)
        self.assertTrue(got["in_world"])
        self.assertIsNone(got["game_running"])

    def test_a_live_process_does_not_rescue_a_refused_join(self):
        """`in_world` is an AND of three things. A client sitting on 'Error
        Code: 279' is running and is not in the place -- that was the failure
        probe_client_join was written for, and it must not regress."""
        got = self._probe("... Error Code: 279 ...", True)
        self.assertFalse(got["in_world"])
        self.assertTrue(got["game_running"])


class TheLaunchFailsWhenTheGameIsGone(unittest.TestCase):

    def test_start_refuses_to_call_it_ok(self):
        src = inspect.getsource(engine.cmd_start)
        self.assertIn('result["client"].get("game_running") is False', src)
        self.assertIn('result["error"] = "client_not_running"', src)

    def test_it_re_asks_before_condemning_a_launch(self):
        """`pidof` goes over adb, and a squeezed guest under load is exactly
        where one probe can miss. A single miss must not fail a healthy
        launch -- but two, seconds apart, is a dead client."""
        src = inspect.getsource(engine.cmd_start)
        first = src.index('result["client"].get("game_running") is False')
        self.assertIn("time.sleep(3)", src[first:first + 400])
        self.assertEqual(
            src.count('result["client"].get("game_running") is False'), 2,
            "ask, wait, ask again")


class TheSettleStopsWaitingForADeadGame(unittest.TestCase):

    def _settle(self, running, pss, timeout=999):
        """Drive the loop with scripted liveness/PSS readings."""
        with mock.patch.object(engine, "game_is_running",
                               side_effect=list(running)), \
             mock.patch.object(engine, "game_pss_mb", side_effect=list(pss)), \
             mock.patch.object(engine.time, "sleep"):
            return engine.wait_for_game_settled(ACCT, timeout=timeout,
                                                interval=0)

    def test_a_game_that_disappears_ends_the_wait(self):
        """It burned the FULL 420 s deadline before this: `game_pss_mb` reads
        dumpsys, and a dead package reads the same as a slow one -- nothing.
        That is the whole difference between a 478 s launch and the 126 s
        baseline, measured twice on 2026-08-17."""
        settled, pss = self._settle(running=[True, True, False],
                                    pss=[900, 1200, None])
        self.assertFalse(settled)
        self.assertEqual(pss, 1200, "reports what it saw before it vanished")

    def test_no_pid_yet_is_an_ordinary_boot_not_a_dead_game(self):
        """Before the game has EVER been seen, "not running" means it has not
        started. Treating that as death would end the wait on every launch,
        instantly, and squeeze a guest with nothing loaded in it."""
        settled, _pss = self._settle(
            running=[False, False, True, True, True],
            pss=[None, None, 1000, 1010, 1015])
        self.assertTrue(settled, "waited for the game to appear, then settled")

    def test_an_unanswered_probe_is_not_a_dead_game(self):
        """None is "could not ask". Ending the wait on it would let one slow
        adb round trip -- routine on a squeezed guest -- cut the settle short
        and squeeze a client in the middle of loading, which is the failure
        this whole wait exists to prevent."""
        settled, _pss = self._settle(
            running=[True, None, None, True, True],
            pss=[1000, 1200, 1400, 1450, 1460])
        self.assertTrue(settled, "kept waiting through the unanswered probes")

    def test_a_settled_game_still_settles(self):
        settled, pss = self._settle(running=[True] * 4,
                                    pss=[1000, 1500, 1510, 1515])
        self.assertTrue(settled)
        self.assertGreaterEqual(pss, engine.SETTLE_FLOOR_MB)


if __name__ == "__main__":
    unittest.main()
