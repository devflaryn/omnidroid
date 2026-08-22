#!/usr/bin/env python3
"""A slow PC is not a broken PC. Wait on PROGRESS, not on the clock.

    python3 -m pytest tests/test_boot_waits_on_progress.py -q

`wait_for_boot` used to run against a fixed wall-clock budget --
`NORMAL_BOOT_TIMEOUT = 360`, `FIRST_BOOT_TIMEOUT = 1500` -- and those numbers
were measured on an i7-13700F with a working hypervisor. Every host slower than
that one had its boots killed while they were still going: a weak CPU, a
spinning disk, a laptop on battery with the governor down, thirty instances
sharing a box, or (the new one) a PC with no hardware virtualization at all,
where the whole guest is emulated and everything takes several times longer.
The user saw `boot_timeout` and a launch that failed for no reason they could
act on, while QEMU carried on booting perfectly well in the background.

The budget was never the right question. "Is this guest still moving?" is. A
boot that is moving is a boot worth waiting for however long it takes; a boot
that has stopped moving is dead within a couple of minutes whatever the budget
says, and waiting out another twenty is just a longer way to fail.

These tests pin the POLICY:

  * progress resets the clock, so a slow boot is never abandoned,
  * silence, and only silence, ends the wait,
  * an explicit `--timeout` still caps absolutely for scripts that need a bound,
  * a signal source that breaks is a missing reading, never an exception, and
  * the poll tightens once adbd is up, because the answer is imminent then.

The stall window itself is a number and will move. The properties must not.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import bootwait  # noqa: E402


class FakeClock:
    def __init__(self):
        self.t = 1000.0

    def __call__(self):
        return self.t

    def advance(self, dt):
        self.t += dt


class StallNotDeadline(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()

    def _watch(self, stall=300, cap=None):
        return bootwait.BootWatch(stall_limit=stall, cap=cap,
                                  clock=self.clock)

    def test_a_boot_that_keeps_moving_is_never_abandoned(self):
        """The whole point. Ten times the old 360 s budget, still going."""
        w = self._watch(stall=300)
        for step in range(120):                      # 120 * 30 s = 1 hour
            w.note(("serial", step))                 # something changed
            self.clock.advance(30)
            self.assertFalse(w.stalled(),
                             f"gave up at {w.elapsed():.0f}s while moving")
        self.assertGreater(w.elapsed(), 3000)

    def test_silence_ends_it(self):
        w = self._watch(stall=300)
        w.note(("serial", 1))
        self.clock.advance(299)
        self.assertFalse(w.stalled())
        w.note(("serial", 1))                        # same fingerprint: nothing moved
        self.clock.advance(2)
        w.note(("serial", 1))
        self.assertTrue(w.stalled())

    def test_progress_resets_the_stall_clock(self):
        w = self._watch(stall=300)
        w.note(("serial", 1))
        self.clock.advance(290)
        w.note(("serial", 2))                        # moved, just in time
        self.clock.advance(290)
        w.note(("serial", 2))
        self.assertFalse(w.stalled())

    def test_any_single_signal_moving_counts_as_progress(self):
        """The fingerprint is a tuple of independent readings. A guest deep in
        dexopt writes nothing to the serial log for minutes at a stretch, but
        its dalvik-cache is growing the whole time."""
        w = self._watch(stall=300)
        w.note(("serial", 10, "adb", "device", "dexopt", 100))
        self.clock.advance(290)
        w.note(("serial", 10, "adb", "device", "dexopt", 141))
        self.clock.advance(290)
        w.note(("serial", 10, "adb", "device", "dexopt", 141))
        self.assertFalse(w.stalled())

    def test_an_unreadable_signal_does_not_read_as_progress(self):
        """None is 'could not sample', and two Nones in a row are not motion.
        Otherwise a signal source that is permanently broken would look like a
        guest that is permanently busy and the wait would never end."""
        w = self._watch(stall=300)
        w.note(("serial", None))
        self.clock.advance(301)
        w.note(("serial", None))
        self.assertTrue(w.stalled())

    def test_a_fingerprint_of_nothing_but_Nones_is_never_motion(self):
        """Not even the FIRST time, and in the shape wait_for_boot actually
        sends -- `dict.items()`, i.e. a tuple of (name, reading) PAIRS.

        This was a real bug: the blankness check looked at the pairs instead of
        the readings, found `("cpu", None)` was not None, and concluded the
        guest had moved. A boot on which every signal was unreadable would then
        have reset its own stall clock forever and never given up."""
        w = self._watch(stall=300)
        blank = tuple(sorted({"cpu": None, "serial": None,
                              "qemu_log": None}.items()))
        self.assertFalse(w.note(blank))
        self.clock.advance(301)
        self.assertFalse(w.note(blank))
        self.assertTrue(w.stalled())

    def test_one_readable_signal_among_blanks_is_still_motion(self):
        w = self._watch(stall=300)
        w.note(tuple(sorted({"cpu": 1.0, "serial": None}.items())))
        self.clock.advance(290)
        moved = w.note(tuple(sorted({"cpu": 2.0, "serial": None}.items())))
        self.assertTrue(moved)
        self.clock.advance(290)
        w.note(tuple(sorted({"cpu": 2.0, "serial": None}.items())))
        self.assertFalse(w.stalled())

    def test_a_mapping_works_too(self):
        w = self._watch(stall=300)
        self.assertFalse(w.note({"cpu": None}))
        self.assertTrue(w.note({"cpu": 12.0}))


class TheCapIsOptional(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()

    def test_no_cap_by_default(self):
        w = bootwait.BootWatch(stall_limit=300, clock=self.clock)
        self.clock.advance(86400)
        self.assertFalse(w.expired())

    def test_an_explicit_cap_is_honoured(self):
        """`--timeout` still means what it says: scripts and CI need a bound."""
        w = bootwait.BootWatch(stall_limit=300, cap=600, clock=self.clock)
        self.clock.advance(599)
        self.assertFalse(w.expired())
        self.clock.advance(2)
        self.assertTrue(w.expired())

    def test_a_capped_wait_still_ends_early_on_a_stall(self):
        w = bootwait.BootWatch(stall_limit=60, cap=6000, clock=self.clock)
        w.note(("x", 1))
        self.clock.advance(61)
        w.note(("x", 1))
        self.assertTrue(w.stalled())
        self.assertFalse(w.expired())


class ThePollTightensWhenTheAnswerIsClose(unittest.TestCase):
    """Every boot used to pay up to 5 s of pure latency after Android was
    already up, because the poll slept a flat 5 s. Once adbd answers,
    boot_completed is seconds away and the poll should be watching for it."""

    def test_the_early_poll_is_cheap(self):
        self.assertGreaterEqual(bootwait.poll_interval(adbd_seen=False), 2.0)

    def test_the_late_poll_is_quick(self):
        self.assertLessEqual(bootwait.poll_interval(adbd_seen=True), 1.5)

    def test_late_is_strictly_quicker_than_early(self):
        self.assertLess(bootwait.poll_interval(adbd_seen=True),
                        bootwait.poll_interval(adbd_seen=False))


class StallWindows(unittest.TestCase):
    """Different phases deserve different patience, and the reason is physical:
    a guest that has not reached adbd yet is showing us a log and a CPU meter,
    while one grinding through dexopt can be genuinely silent for a long time."""

    def test_first_boot_is_the_most_patient(self):
        self.assertGreater(bootwait.stall_limit(first_boot=True, adbd_seen=True),
                           bootwait.stall_limit(first_boot=False, adbd_seen=True))

    def test_every_window_is_generous_enough_to_survive_a_busy_host(self):
        for first in (True, False):
            for seen in (True, False):
                self.assertGreaterEqual(
                    bootwait.stall_limit(first_boot=first, adbd_seen=seen), 120)


class CpuTimeIsTheUniversalSignal(unittest.TestCase):
    """The one signal that works before adbd, before the log says anything
    useful, and on every platform: a guest that is executing burns host CPU.
    A guest that has wedged does not."""

    def test_this_process_has_burnt_some_cpu(self):
        me = bootwait.cpu_seconds(os.getpid())
        self.assertIsNotNone(me, "no CPU-time source on this platform")
        self.assertGreater(me, 0.0)

    def test_it_goes_up(self):
        """Burn a measurable amount, not an arbitrary number of iterations.

        This was `for i in range(400000)` and it FLAKED: Windows' process CPU
        accounting has ~15.6 ms granularity, and that loop finishes inside one
        tick on an idle fast machine, so `after == before` and the signal looked
        broken when it was merely being asked a question below its resolution.
        Burn against the clock instead, well past the tick."""
        import time as _t
        before = bootwait.cpu_seconds(os.getpid())
        end = _t.monotonic() + 0.30          # ~19 ticks at 15.6 ms
        x = 0
        i = 0
        while _t.monotonic() < end:
            i += 1
            x += i * i
        after = bootwait.cpu_seconds(os.getpid())
        self.assertGreater(after, before)

    def test_a_dead_pid_is_none_not_an_exception(self):
        self.assertIsNone(bootwait.cpu_seconds(0x7FFFFFFE))

    def test_no_pid_is_none(self):
        self.assertIsNone(bootwait.cpu_seconds(None))


class NothingHereMayRaise(unittest.TestCase):
    """A boot wait is the least appropriate place in the product for an
    exception: the instance is already running and costing money."""

    def test_a_broken_log_reader_is_a_missing_reading(self):
        self.assertIsNone(bootwait.file_size("/no/such/file/anywhere"))

    def test_a_directory_is_a_missing_reading_not_a_crash(self):
        self.assertIsNone(bootwait.file_size(os.path.dirname(__file__)))

    def test_a_signal_that_explodes_is_swallowed_by_the_sampler(self):
        def boom():
            raise RuntimeError("nope")

        got = bootwait.sample({"ok": lambda: 3, "bad": boom})
        self.assertEqual(got["ok"], 3)
        self.assertIsNone(got["bad"])


class TheBackstopsThatMakeNoCapSafe(unittest.TestCase):
    """Removing the deadline removes a safety net, and it has to be replaced
    rather than simply dropped.

    Two failures used to be caught by the 360 s budget purely by accident:

      * a guest in a REBOOT LOOP -- panic, reset, panic. Every signal keeps
        moving (the log grows, CPU burns, adbd comes and goes), so a pure
        stall watch would wait for it forever.
      * anything else pathological that keeps one signal twitching.

    A boot with no bound at all would hold ~3 GB and block `omnidroid start`
    indefinitely, and the app's own watchdog would not fire either, because the
    engine is still printing progress lines. So: detect the loop directly, and
    keep one very generous absolute backstop underneath everything.
    """

    def test_a_reboot_loop_is_detected_rather_than_waited_out(self):
        w = bootwait.BootWatch(stall_limit=300, clock=FakeClock())
        for _ in range(bootwait.REBOOT_LOOP_LIMIT):
            w.note_adb_state("device")
            w.note_adb_state("")
        self.assertTrue(w.looping())

    def test_one_flap_is_not_a_loop(self):
        """adbd bouncing once is ordinary -- `adb root` alone does it."""
        w = bootwait.BootWatch(stall_limit=300, clock=FakeClock())
        w.note_adb_state("device")
        w.note_adb_state("")
        w.note_adb_state("device")
        self.assertFalse(w.looping())

    def test_a_normal_boot_never_looks_like_a_loop(self):
        w = bootwait.BootWatch(stall_limit=300, clock=FakeClock())
        for state in ["", "", "offline", "offline", "device", "device"]:
            w.note_adb_state(state)
        self.assertFalse(w.looping())

    def test_there_is_a_backstop_and_it_is_generous(self):
        """A backstop, not a budget: it must be far beyond any real boot on any
        real hardware, so that hitting it means something is broken."""
        self.assertGreaterEqual(bootwait.SANITY_CEILING_S, 3600)

    def test_the_backstop_ends_a_wait_that_never_would(self):
        clock = FakeClock()
        w = bootwait.BootWatch(stall_limit=300, clock=clock)
        for _ in range(10):
            clock.advance(bootwait.SANITY_CEILING_S / 5)
            w.note(("moving", clock.t))          # progress, forever
        self.assertFalse(w.stalled())
        self.assertTrue(w.past_sanity_ceiling())

    def test_the_backstop_does_not_fire_on_a_long_but_real_boot(self):
        clock = FakeClock()
        w = bootwait.BootWatch(stall_limit=300, clock=clock)
        clock.advance(1800)                       # half an hour: slow, not sick
        self.assertFalse(w.past_sanity_ceiling())


class TheProgressLineDoesNotFlood(unittest.TestCase):
    """The poll is a second long once adbd is up, and the line is printed
    whenever the PHASE changes. So nothing that varies per poll -- an elapsed
    time, a byte count -- may be part of what "the phase" is compared on, or
    the launch log turns into one line a second. (It did: the stall warning
    carries a live duration and was being folded into the phase string.)"""

    def test_the_phase_string_carries_no_live_numbers(self):
        import inspect
        from omnidroid import engine
        src = inspect.getsource(engine.wait_for_boot)
        # The comparison drives the print; the warning must not reach it.
        self.assertIn("warning = ", src)
        self.assertNotIn("new_phase += ", src)

    def test_the_warning_is_still_printed(self):
        import inspect
        from omnidroid import engine
        src = inspect.getsource(engine.wait_for_boot)
        self.assertIn("{phase}{warning}", src)


class TheEngineUsesIt(unittest.TestCase):
    """Wiring: `wait_for_boot` must be stall-driven, and must not invent a
    wall-clock budget of its own when the caller did not ask for one."""

    def test_the_default_boot_budget_is_no_longer_a_deadline(self):
        from omnidroid import engine
        # The old constants are kept as CAPS a caller may opt into, but the
        # default path must not pass one.
        self.assertIsNone(engine.default_boot_cap(first_boot=False))
        self.assertIsNone(engine.default_boot_cap(first_boot=True))

    def test_an_explicit_timeout_still_becomes_a_cap(self):
        from omnidroid import engine
        self.assertEqual(engine.default_boot_cap(first_boot=False,
                                                 requested=900), 900)


if __name__ == "__main__":
    unittest.main()
