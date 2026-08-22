#!/usr/bin/env python3
"""A client that is still loading must not be squeezed because a clock ran out.

    python3 -m pytest tests/test_settle_waits_for_growth.py -q

`wait_for_game_settled` blocked for `SETTLE_TIMEOUT_S = 420` and then squeezed
regardless. 420 s was measured on this dev box against PS99 with a working
hypervisor. On a slower host the client is still loading at 420 s, and the
squeeze is not a cosmetic thing to get wrong: this project has already measured
that squeezing a client mid-load STARVES IT and stops the instance ever reaching
the world. That is the same defect as the boot timeout -- one machine's number
enforced on every machine -- with a worse consequence, because it does not
report a failure, it produces an instance that farms nothing while claiming to
be fine.

The signal to wait on was already being sampled. PSS growth IS load progress:
the loop reads it every 15 s, and already knows how to tell "growing" from
"stable" and how to detect a client that has died. So growth extends the
patience, and the budget becomes what it should always have been -- a bound on
how long the client may sit NOT growing, plus a generous ceiling.

These tests pin the property. The numbers will move.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402

ACCT = {"name": "u1", "adb_port": 16001}


class _Clock:
    def __init__(self):
        self.t = 1000.0

    def time(self):
        return self.t

    def sleep(self, dt):
        self.t += dt


class GrowthBuysTime(unittest.TestCase):
    def _run(self, samples, timeout=420, running=True):
        """Feed `samples` (MB, or None) one per poll and return the verdict."""
        clock = _Clock()
        it = iter(samples)

        def next_pss(_acct):
            try:
                return next(it)
            except StopIteration:
                return samples[-1] if samples else None

        with mock.patch.object(omni, "game_pss_mb", side_effect=next_pss), \
                mock.patch.object(omni, "game_is_running",
                                  return_value=running), \
                mock.patch.object(omni.time, "time", clock.time), \
                mock.patch.object(omni.time, "sleep", clock.sleep):
            settled, pss = omni.wait_for_game_settled(
                ACCT, timeout=timeout, interval=15)
        return settled, pss, clock.t - 1000.0

    def test_a_client_still_growing_past_the_budget_is_not_squeezed(self):
        """The whole point: a slow host loads the same game, just later.

        Growth here is 6 % a sample -- above SETTLE_TOLERANCE, so the existing
        "two samples within 4 % = it has stopped" rule does not fire and this is
        genuinely a client still pulling its place in. 40 polls at 15 s is
        600 s, well past the old 420 s budget."""
        samples = [800 * (1.06 ** i) for i in range(40)] + [8000] * 4
        settled, pss, elapsed = self._run(samples)
        self.assertTrue(settled,
                        "squeezed a client that was still loading")
        self.assertGreater(elapsed, 420)

    def test_a_client_that_stops_growing_settles_as_before(self):
        samples = [800, 1200, 1210, 1215]
        settled, pss, elapsed = self._run(samples)
        self.assertTrue(settled)
        self.assertLess(elapsed, 200)

    def test_a_client_that_never_grows_and_never_settles_still_gives_up(self):
        """Below the floor forever -- stuck on a splash. It must not wait out
        the ceiling, or a broken launch takes an hour to be reported."""
        settled, pss, elapsed = self._run([120] * 400)
        self.assertFalse(settled)
        self.assertLess(elapsed, 900)

    def test_a_dead_client_is_still_detected_immediately(self):
        """The existing fast path must survive: a PSS that stops being
        readable while the process is gone means stop waiting NOW."""
        samples = [900, 1400, None, None]
        settled, pss, elapsed = self._run(samples, running=False)
        self.assertFalse(settled)
        self.assertLess(elapsed, 120)

    def test_a_game_that_never_appears_is_not_waited_out(self):
        """MEASURED on this box: a `--place home` farming launch spent 421 s --
        the whole budget -- waiting for a client that was never going to start,
        and then reported "never settled (? MB)". The existing "the game is
        GONE" fast path could not fire, because it is guarded on having seen a
        reading at least once."""
        settled, pss, elapsed = self._run([None] * 400, running=False)
        self.assertFalse(settled)
        self.assertLess(elapsed, omni.NO_GAME_GRACE_S + 60)

    def test_the_grace_is_not_shorter_than_a_slow_client_needs(self):
        """The guard it relaxes exists because "no pid yet" is ordinary early
        in a launch, and a slow PC is late at everything."""
        self.assertGreaterEqual(omni.NO_GAME_GRACE_S, 90)

    def test_an_unaskable_probe_is_not_read_as_no_game(self):
        """`game_is_running` returns None when it could not ask -- a squeezed
        guest under load is exactly where a probe misses -- and None must never
        become a death sentence."""
        clock = _Clock()
        with mock.patch.object(omni, "game_pss_mb", return_value=None), \
                mock.patch.object(omni, "game_is_running", return_value=None), \
                mock.patch.object(omni.time, "time", clock.time), \
                mock.patch.object(omni.time, "sleep", clock.sleep):
            omni.wait_for_game_settled(ACCT, timeout=420, interval=15)
        self.assertGreaterEqual(clock.t - 1000.0, 420)

    def test_there_is_still_a_ceiling(self):
        """Growth cannot buy unlimited time -- a client leaking memory forever
        would otherwise never be squeezed."""
        settled, pss, elapsed = self._run([100 + 40 * i for i in range(400)])
        self.assertLess(elapsed, omni.SETTLE_CEILING_S + 60)

    def test_the_ceiling_is_far_beyond_a_real_load(self):
        # PS99 measured ~111 s to 1173 MB on this box. Whatever the ceiling is,
        # a genuinely slow host has to fit comfortably inside it.
        self.assertGreaterEqual(omni.SETTLE_CEILING_S, 1800)

    def test_the_budget_still_means_something_when_nothing_moves(self):
        import inspect
        sig = inspect.signature(omni.wait_for_game_settled)
        self.assertEqual(sig.parameters["timeout"].default,
                         omni.SETTLE_TIMEOUT_S)


class TheEscapeHatchStillWorks(unittest.TestCase):
    """`OMNI_SETTLE_TIMEOUT=0` is a fleet launcher saying "give me the handle
    back, I accept the squeeze lands on a loading client". Growth must not
    override an explicit zero."""

    def test_zero_returns_immediately(self):
        clock = _Clock()
        with mock.patch.object(omni, "game_pss_mb", return_value=500), \
                mock.patch.object(omni, "game_is_running", return_value=True), \
                mock.patch.object(omni.time, "time", clock.time), \
                mock.patch.object(omni.time, "sleep", clock.sleep):
            omni.wait_for_game_settled(ACCT, timeout=0, interval=15)
        self.assertEqual(clock.t - 1000.0, 0.0)


if __name__ == "__main__":
    unittest.main()
