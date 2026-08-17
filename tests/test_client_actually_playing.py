#!/usr/bin/env python3
"""Running is not playing, and nothing in this engine could tell them apart.

    python3 -m pytest tests/test_client_actually_playing.py -q

MEASURED 2026-08-17, six farming instances, verified against screenshots:

    farm8  utime 301  ~109%   in PS99, pets and currency on screen
    farm9  utime 388  ~143%   in PS99
    farm7  utime  29   ~35%   "restart the game" prompt over the world
    farm2  utime  29   ~30%   "Connection Failed (Error Code: 279)"
    farm3  utime  21   ~30%   same
    farm6  utime  13   ~31%   same

FOUR OF SIX WERE FARMING NOTHING, and every reading this project takes said
they were fine: `pidof` finds a process, the working-set governor held them at
a perfect 384 MB, the CPU ceiling held them at 50%, and `list` called them
running. `probe_client_join` could not help either -- it reads a 400-line tail
of the client log, and join markers scroll out of it within minutes, so it
reported `in_world: False` for the two that WERE playing and `error: None` for
the three with the dialog on screen.

USER time is the discriminator, not total. System time is ~60-80 jiffies
either way (the client keeps a render loop going for the dialog too), so a
total-CPU threshold sees 30% vs 143% collapse into something much closer.
"""
import unittest

from omnidroid import engine


# Straight from the measurement above: (utime, stime) jiffies over 3 s.
PLAYING = ((0, 0), (388, 41), 3)
DIALOG = ((0, 0), (29, 61), 3)


class TheDiscriminatorMatchesWhatWasMeasured(unittest.TestCase):

    def test_a_client_in_the_world_reads_as_playing(self):
        self.assertIs(engine.client_is_playing(*PLAYING), True)

    def test_a_client_on_a_connection_failed_dialog_does_not(self):
        self.assertIs(engine.client_is_playing(*DIALOG), False)

    def test_the_restart_prompt_case_does_not_either(self):
        # farm7: game content visible behind a prompt, 29 jiffies of user time.
        # The screenshot is the trap here -- it LOOKS like a running game.
        self.assertIs(engine.client_is_playing((0, 0), (29, 75), 3), False)

    def test_system_time_alone_cannot_rescue_a_dead_client(self):
        """The threshold is on USER time on purpose. A dialog's stime (61) is
        comparable to a playing client's (41), so counting both would put the
        two cases 3x apart instead of 10x and invite a threshold that splits
        the difference wrongly."""
        busy_kernel = ((0, 0), (20, 400), 3)
        self.assertIs(engine.client_is_playing(*busy_kernel), False)


class CouldNotTellIsNotTheSameAsNotPlaying(unittest.TestCase):
    """Same three-state discipline as `game_is_running`. An instance this
    probe cannot read must never be recorded as idle -- that would put "not
    farming" on the app's row for a guest whose only crime was answering adb
    slowly, which is routine on the mode built for slow guests."""

    def test_no_previous_reading_is_none(self):
        self.assertIsNone(engine.client_is_playing(None, (388, 41), 3))

    def test_no_current_reading_is_none(self):
        self.assertIsNone(engine.client_is_playing((0, 0), None, 3))

    def test_a_recycled_pid_is_none_not_a_verdict(self):
        """Counters that went BACKWARDS mean the process was replaced between
        polls -- the kiosk relaunching the client after a kill. Dividing that
        negative delta would report a healthy new client as idle."""
        self.assertIsNone(engine.client_is_playing((500, 500), (29, 61), 3))

    def test_a_zero_length_window_is_none_not_a_division_error(self):
        self.assertIsNone(engine.client_is_playing((0, 0), (388, 41), 0))


class TheGovernorIsWhereThisLives(unittest.TestCase):

    def test_it_records_idle_into_run_json(self):
        """`list` and the app read run.json. A governor that noticed and kept
        it to itself would leave the UI showing the same green row for an
        instance farming nothing."""
        import inspect
        src = inspect.getsource(engine.govern_working_set)
        self.assertIn("client_is_playing", src)
        self.assertIn("_record_client_idle", src)

    def test_it_costs_no_extra_sampling_window(self):
        """The poll loop already runs every 5 s, so it has the two readings a
        CPU delta needs. Blocking the governor for a sampling window would add
        a sleep to every poll for the life of every instance."""
        import inspect
        # `time.sleep(` -- the CALL. Matching the bare word "sleep" also hits
        # the docstring that promises there isn't one, which is a test that
        # fails for saying so.
        for fn in (engine.client_cpu_jiffies, engine.client_is_playing):
            self.assertNotIn("time.sleep(", inspect.getsource(fn))


if __name__ == "__main__":
    unittest.main()
