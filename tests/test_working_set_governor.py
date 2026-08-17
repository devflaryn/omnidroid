#!/usr/bin/env python3
"""Farming gives its memory and its CPU back, and the guest survives it.

    python3 -m pytest tests/test_working_set_governor.py -q

WHY THERE ARE TWO GOVERNORS. The balloon is the mechanism on a host where QEMU
can decommit. Windows is not one: `ram_block_discard_range()` is behind
`CONFIG_MADVISE`, so nothing the guest hands back is ever released and an idle
instance costs the host its whole `-m`. That is the "30 instances drain the
RAM" problem, and none of the in-QEMU answers work --

    balloon inflate      3414 -> 3403 MB of host RSS, at ~0.4 MB/s, while
                         logging one warning per page (it never finishes, and
                         the log spam starved QMP badly enough that liveness
                         checks timed out)
    EmptyWorkingSet      run once: 3192 -> 1 MB, back to ~150 MB in 20 s.
                         Run on a TIMER, which is what the governor did:
                         adb stopped answering within 12 s, the client was
                         killed, and the guest never recovered.

-- so the Windows governor asks the memory manager for a HARD WORKING SET
MAXIMUM instead and lets it choose which pages and when. Measured on a live
PS99 farming instance:

    ceiling   host RSS   client   adb round trip
    (none)        3417   alive    0.05 s
    1000          1000   alive    0.05 s
    650            650   alive    0.04 s
    500            500   alive    0.10 s
    384            384   alive    0.10 s   <- 152% of a guest core, still playing
    300            300   DEAD

and at 384 MB the host read **0.01 MB/s** off disk: the faults are soft, served
from the standby list, which is why the guest stays responsive at 8.9x less
resident memory.

The CPU ceiling is the other half, and it is what decides instance COUNT.
Farming's cost is the game's own arm64 translation -- 148% of a guest core
against SurfaceFlinger's 6.7%, so "render less" is not available, there is
nothing to render. A job-object hard cap gives each instance a fixed slice
instead: measured 160.9% uncapped -> 49.9% at a 50% ceiling, client alive and
adb answering in 0.06 s throughout.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import balloon, engine, qemu_proc, runtime


class TheCeilingSearchFindsTheGamesFloor(unittest.TestCase):
    """The right ceiling is a property of THE GAME, not a constant. PS99 dies
    below ~500 MB; another place will differ, and nobody should have to
    measure each one by hand. So the search walks down while the guest is
    healthy and stops clear of anything that hurt it."""

    def _search(self, hurts_below, floor=384, start=3072, max_mb=3072,
                polls=40):
        ceiling, unsafe = start, None
        for _ in range(polls):
            ceiling, unsafe = balloon.next_ceiling(
                ceiling_mb=ceiling, healthy=(ceiling > hurts_below),
                floor_mb=floor, unsafe_mb=unsafe, max_mb=max_mb)
        return ceiling, unsafe

    def test_it_settles_above_the_level_that_hurt(self):
        ceiling, unsafe = self._search(hurts_below=500)
        self.assertGreater(ceiling, unsafe)
        self.assertLess(ceiling, 1024)

    def test_a_game_with_a_higher_floor_settles_higher(self):
        low, _ = self._search(hurts_below=500)
        high, _ = self._search(hurts_below=1400)
        self.assertGreater(high, low)

    def test_it_never_goes_below_the_modes_floor(self):
        ceiling, _ = self._search(hurts_below=0, floor=768)
        self.assertGreaterEqual(ceiling, 768)

    def test_it_never_goes_below_the_hard_floor_whatever_the_mode_says(self):
        ceiling, _ = self._search(hurts_below=0, floor=16)
        self.assertGreaterEqual(ceiling, balloon.CEILING_HARD_FLOOR_MB)

    def test_it_starts_at_the_guests_own_size(self):
        # The ceiling can only hurt by being too LOW, so the search approaches
        # from the safe side: the first minutes of an instance's life, when
        # the client is loading and needs the most, are spent uncapped.
        self.assertEqual(balloon.starting_ceiling(3072, {}), 3072)
        self.assertEqual(balloon.starting_ceiling(3072, {"ws_ceiling": 900}),
                         900)


class TheBackoffCannotRunAway(unittest.TestCase):
    """MEASURED, and it is why `max_mb` exists: an instance whose client had
    died for reasons of its own read as unhealthy every poll, and the ceiling
    climbed 3072 -> 3200 -> ... -> 4736 MB and kept going. That is meaningless
    (there is nothing above `-m` to hand back) and it hides the real problem
    behind a number that looks like it is still working on it."""

    def test_it_stops_at_the_guests_own_size(self):
        ceiling, unsafe = 3072, None
        for _ in range(20):
            ceiling, unsafe = balloon.next_ceiling(
                ceiling_mb=ceiling, healthy=False, floor_mb=384,
                unsafe_mb=unsafe, max_mb=3072)
        self.assertEqual(ceiling, 3072)

    def test_one_bad_poll_backs_off_by_more_than_one_step(self):
        # The level that hurt was measured while the guest was ALREADY under
        # pressure, so the safe level is meaningfully above it.
        ceiling, _ = balloon.next_ceiling(ceiling_mb=1000, healthy=False,
                                          floor_mb=384, max_mb=3072)
        self.assertGreaterEqual(ceiling - 1000,
                                2 * balloon.CEILING_STEP_MB)

    def test_a_level_that_hurt_is_never_revisited(self):
        ceiling, unsafe = balloon.next_ceiling(
            ceiling_mb=640, healthy=False, floor_mb=384, max_mb=3072)
        self.assertEqual(unsafe, 640)
        # ...and from there, healthy polls may not walk back down onto it.
        for _ in range(30):
            ceiling, unsafe = balloon.next_ceiling(
                ceiling_mb=ceiling, healthy=True, floor_mb=384,
                unsafe_mb=unsafe, max_mb=3072)
        self.assertGreater(ceiling, 640)


class HealthIsAdbLatencyNotJustAPid(unittest.TestCase):
    """The leading indicator. MEASURED on an instance squeezed too hard: adb
    round trips went from 0.05 s to 15 s while the client was still alive, and
    the client died some time after that. A governor that waits for the game
    to die has already lost the instance."""

    def _health(self, out, seconds):
        clock = iter([100.0, 100.0 + seconds])
        with mock.patch.object(engine, "adb_soft",
                               return_value=type("R", (), {"stdout": out})()), \
             mock.patch.object(engine.time, "monotonic",
                               side_effect=lambda: next(clock)):
            return engine.instance_health({"name": "u1"})

    def test_a_fast_answer_with_a_live_client_is_healthy(self):
        healthy, secs, game = self._health("4321\n", 0.05)
        self.assertTrue(healthy)
        self.assertTrue(game)
        self.assertLess(secs, 1)

    def test_a_slow_answer_is_unhealthy_even_though_the_client_lives(self):
        healthy, _secs, game = self._health("4321\n", 12.0)
        self.assertFalse(healthy)
        self.assertTrue(game, "the client is alive; it is the guest that hurts")

    def test_a_dead_client_is_unhealthy(self):
        healthy, _secs, game = self._health("", 0.05)
        self.assertFalse(healthy)
        self.assertFalse(game)


class TheGovernorDoesNotSqueezeABootingGuest(unittest.TestCase):
    """Before the client is up there is no game pid, so `healthy` is False --
    and a search that read a perfectly normal boot as damage would climb away
    from the floor and never come back."""

    def _poll(self, game_out, state):
        with mock.patch.object(engine, "running_pid", return_value=4242), \
             mock.patch.object(engine, "_run_record",
                               return_value={"mem_mb": 3072}), \
             mock.patch.object(engine, "adb_soft",
                               return_value=type("R", (), {"stdout": game_out})()), \
             mock.patch.object(engine, "cap_working_set", return_value=True), \
             mock.patch.object(engine, "host_rss_mb", return_value=900), \
             mock.patch.object(engine, "CpuCeiling") as cpu:
            cpu.return_value.set.return_value = True
            cpu.return_value.cores = 24
            return engine.govern_working_set(
                {"name": "u1"}, qemu_proc.MODES["farming"], state)

    def test_it_waits_for_the_client_before_touching_anything(self):
        state = {}
        result = self._poll("", state)
        self.assertEqual(result.get("reason"), "waiting_for_game")
        self.assertNotIn("ceiling", state)

    def test_once_the_client_has_been_seen_it_starts_the_search(self):
        state = {}
        self._poll("4321\n", state)
        self.assertIn("ceiling", state)
        self.assertLess(state["ceiling"], 3072)

    def test_the_search_starts_from_THIS_boots_mem_not_the_modes_default(self):
        # PS99 raises farming's 2048 to 3072; starting at the mode default
        # would begin the descent already below the guest's real size.
        state = {}
        self._poll("4321\n", state)
        self.assertGreater(state["ceiling"], qemu_proc.MODES["farming"]["mem"])


class WindowsUsesTheCeilingAndEverywhereElseUsesTheBalloon(unittest.TestCase):

    def test_the_farming_mode_carries_both_ceilings(self):
        farming = qemu_proc.MODES["farming"]
        self.assertIn("ws_floor", farming)
        self.assertIn("cpu_ceiling_pct", farming)

    def test_gaming_has_neither_and_no_governor(self):
        # `performance` is the opposite trade by definition: frames,
        # resolution and input latency. Holding it at half a core would be
        # the one thing the mode exists to prevent.
        gaming = qemu_proc.MODES["gaming"]
        self.assertNotIn("cpu_ceiling_pct", gaming)
        self.assertIsNone(balloon.governor_wanted(gaming))

    def test_farming_still_wants_a_governor(self):
        self.assertIsNotNone(balloon.governor_wanted(qemu_proc.MODES["farming"]))


class TheCapsDegradeToNothingOffWindows(unittest.TestCase):
    """Linux and macOS decommit for real through the balloon's own discard
    path. Neither cap is an error there -- it is simply somebody else's job."""

    def test_the_working_set_cap_is_a_no_op(self):
        with mock.patch.object(runtime, "IS_WINDOWS", False):
            self.assertFalse(runtime.cap_working_set(4242, 900))
            self.assertFalse(runtime.uncap_working_set(4242))

    def test_the_cpu_ceiling_attaches_to_nothing(self):
        with mock.patch.object(runtime, "IS_WINDOWS", False):
            ceiling = runtime.CpuCeiling(4242)
            self.assertFalse(ceiling.attached)
            self.assertFalse(ceiling.set(50))
            ceiling.close()          # must not raise

    def test_neither_raises_on_a_pid_that_is_gone(self):
        self.assertFalse(runtime.cap_working_set(999999, 900))
        ceiling = runtime.CpuCeiling(999999)
        self.assertFalse(ceiling.set(50))
        ceiling.close()


if __name__ == "__main__":
    unittest.main()
