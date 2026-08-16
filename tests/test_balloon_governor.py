#!/usr/bin/env python3
"""The memory governor's policy: what cap does the next poll ask for?

    python3 tests/test_balloon_governor.py

Pure arithmetic, no QEMU and no adb — the whole point of putting the decision
in its own function is that the dangerous cases (a client that is loading, a
guest that spikes, a floor that would starve the game) can be exercised
without booting anything.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import balloon  # noqa: E402


BASE = {"mem_mb": 3072, "floor_mb": 1024, "headroom_mb": 512}


def step(used, cap, **kw):
    """One poll, with the fixture's defaults."""
    return balloon.next_cap(used_mb=used, cap_mb=cap, **{**BASE, **kw})


class Grow(unittest.TestCase):
    """Growth is the pressure valve and it may never be delayed."""

    def test_grows_on_the_very_first_poll(self):
        # No confirmation counting on the way up: a guest that is running out
        # of headroom is seconds from an lmkd kill, and this project has
        # already measured what that costs (Roblox "has died: fg TOP").
        cap, slack, why = step(used=1800, cap=2048)
        self.assertEqual(cap, 2312)          # 1800 + 512 headroom
        self.assertEqual(slack, 0)
        self.assertIn("grow", why)

    def test_growth_stops_at_mem(self):
        # The balloon can never hand back more than -m: asking for it is a
        # QMP error, and a governor that errors is a governor that stops.
        cap, _, _ = step(used=2900, cap=3072)
        self.assertEqual(cap, 3072)

    def test_growth_from_a_tight_cap_is_one_jump_not_a_ramp(self):
        # The boot cap is deliberately small; the first thing the game does is
        # need more than it. That must be a single deflate, not 8 polls of
        # 256 MB steps while the client is loading.
        cap, _, why = step(used=1400, cap=1024)
        self.assertEqual(cap, 1912)
        self.assertIn("grow", why)


class Stability(unittest.TestCase):
    """The band. These are regression tests for a bug that shipped to a live
    instance on 2026-08-16: with the grow rule written as `want > cap` alone,
    a perfectly healthy guest moved its own cap every poll (observed
    1536 -> 1543 -> 1585 MB) and then fought the shrink rule forever.

    On Windows that is not cosmetic. Every grow lets the guest touch pages it
    had given back, and a touched page is never released there, so an
    oscillating governor ratchets the host's cost up one cycle at a time --
    the exact opposite of the thing it exists to do. It also wrote 266 KB of
    qemu.log every 30 s, one line per 4 KB page moved.
    """

    def test_a_healthy_guest_well_inside_the_band_does_not_move(self):
        # The exact sample that oscillated in production.
        cap, slack, why = step(used=1031, cap=1536)
        self.assertEqual(cap, 1536)
        self.assertEqual(slack, 0)
        self.assertIn("band", why)

    def test_usage_drifting_inside_the_band_is_stable_for_many_polls(self):
        # Drive the policy the way the loop does, with usage wandering the way
        # a running game's does. Nothing may move.
        cap, slack = 1536, 0
        for used in (1000, 1031, 1073, 1050, 1100, 1042, 1090, 1010, 1120):
            cap, slack, _ = step(used=used, cap=cap, slack_polls=slack)
            self.assertEqual(cap, 1536, f"moved at used={used}")

    def test_grow_waits_until_slack_actually_runs_out(self):
        # 400 MB of slack is not an emergency, even though used + headroom is
        # above the cap. This is the condition the first cut got wrong.
        cap, _, why = step(used=1136, cap=1536)      # slack 400
        self.assertEqual(cap, 1536)
        self.assertIn("band", why)
        # ...but 200 MB of slack is.
        cap, _, why = step(used=1336, cap=1536)      # slack 200
        self.assertEqual(cap, 1848)
        self.assertIn("grow", why)

    def test_a_grow_lands_outside_the_shrink_trigger(self):
        # Otherwise the very next poll would undo it, which IS the
        # oscillation. After a grow the new slack must sit inside the band.
        cap, slack, _ = step(used=1336, cap=1536)
        new_slack = cap - 1336
        self.assertGreaterEqual(new_slack, balloon.GROW_TRIGGER_MB)
        self.assertLessEqual(new_slack,
                             BASE["headroom_mb"] + balloon.SHRINK_SLACK_MB)
        cap2, _, why = step(used=1336, cap=cap, slack_polls=slack)
        self.assertEqual(cap2, cap, f"undone next poll: {why}")

    def test_a_shrink_lands_outside_the_grow_trigger(self):
        # The mirror of the above: a step down must not immediately trip the
        # pressure valve back up.
        cap, _, _ = step(used=1200, cap=3072, slack_polls=2)
        self.assertGreaterEqual(cap - 1200, balloon.GROW_TRIGGER_MB)

    def test_a_full_descent_converges_and_stops(self):
        # An idle guest at a big cap should walk down in steps and then STOP,
        # rather than ending in a limit cycle.
        cap, slack = 3072, 0
        moves = 0
        for _ in range(60):
            new, slack, _ = step(used=900, cap=cap, slack_polls=slack)
            if new != cap:
                moves += 1
            cap = new
        self.assertGreaterEqual(cap, BASE["floor_mb"])
        # Converged: the last 20 polls moved nothing.
        tail_cap, tail_slack, tail_moves = cap, slack, 0
        for _ in range(20):
            new, tail_slack, _ = step(used=900, cap=tail_cap,
                                      slack_polls=tail_slack)
            if new != tail_cap:
                tail_moves += 1
            tail_cap = new
        self.assertEqual(tail_moves, 0, f"still moving at {tail_cap} MB")
        self.assertLess(moves, 12)


class Shrink(unittest.TestCase):
    """Shrinking is the optimisation, so it is the half that must be timid."""

    def test_does_not_shrink_until_settled(self):
        # may_shrink=False is the loading client. Every lever that makes an
        # idle instance cheap starves a loading one — measured 2026-08-16,
        # PSS flat at ~400 MB with the guest 200% idle for six minutes.
        cap, slack, why = step(used=1200, cap=3072, may_shrink=False)
        self.assertEqual(cap, 3072)
        self.assertEqual(slack, 0)
        self.assertIn("not settled", why)

    def test_needs_consecutive_confirmations(self):
        # One quiet poll is not idleness; it is the gap between two
        # allocations.
        cap, slack, _ = step(used=1200, cap=3072, slack_polls=0)
        self.assertEqual(cap, 3072)
        self.assertEqual(slack, 1)
        cap, slack, _ = step(used=1200, cap=3072, slack_polls=1)
        self.assertEqual(cap, 3072)
        self.assertEqual(slack, 2)
        cap, slack, why = step(used=1200, cap=3072, slack_polls=2)
        self.assertLess(cap, 3072)
        self.assertEqual(slack, 0)
        self.assertIn("shrink", why)

    def test_shrinks_by_a_step_not_to_the_target(self):
        # A cliff from 3072 to 1712 in one command is a 1.3 GB inflate, and
        # the guest walks its free lists to satisfy it (measured ~25 MB/s on
        # this host). Stepping keeps every individual move cheap and lets a
        # sudden allocation interrupt the descent.
        cap, _, _ = step(used=1200, cap=3072, slack_polls=2)
        self.assertEqual(cap, 3072 - balloon.SHRINK_STEP_MB)

    def test_a_spike_resets_the_confirmation_count(self):
        _, slack, _ = step(used=2600, cap=3072, slack_polls=2)
        self.assertEqual(slack, 0)

    def test_never_shrinks_below_the_floor(self):
        # The floor is the mode's known-survivable cap. Below it the game
        # dies, and a dead game is not a memory saving. The final step down
        # is the one that would overshoot it.
        cap, _, why = step(used=64, cap=1200, slack_polls=2)
        self.assertEqual(cap, 1024)          # not 1200 - 256 = 944
        self.assertIn("shrink", why)

    def test_the_floor_holds_across_the_whole_input_space(self):
        # The clamp is the one invariant a bad poll must never break, so it is
        # asserted as a property rather than at one hand-picked point.
        for used in (None, 0, 1, 64, 900, 1500, 2900, 3072, 99999):
            for cap in (1024, 1200, 2048, 3072):
                for slack in range(balloon.SHRINK_CONFIRMATIONS + 1):
                    for settled in (True, False):
                        new, _, _ = step(used=used, cap=cap, slack_polls=slack,
                                         may_shrink=settled)
                        self.assertGreaterEqual(new, BASE["floor_mb"])
                        self.assertLessEqual(new, BASE["mem_mb"])

    def test_settles_and_then_holds(self):
        # Once the cap is want + nothing to give, further polls must be
        # no-ops rather than oscillating by a step each time.
        cap, slack, why = step(used=1200, cap=1712, slack_polls=2)
        self.assertEqual(cap, 1712)
        self.assertIn("hold", why)
        self.assertEqual(slack, 0)


class Clamps(unittest.TestCase):
    """Misconfiguration must degrade to 'fat but alive', never to a crash."""

    def test_floor_above_mem_yields_mem(self):
        cap, _, _ = step(used=100, cap=2048, floor_mb=8192, slack_polls=2)
        self.assertEqual(cap, 3072)

    def test_unknown_usage_holds(self):
        # adb hiccup -> used_mb is None. Never advance on missing information;
        # that is the same rule cmd_watch already applies to the game pid.
        cap, slack, why = step(used=None, cap=2048, slack_polls=2)
        self.assertEqual(cap, 2048)
        self.assertEqual(slack, 2)
        self.assertIn("unknown", why)


class Plateau(unittest.TestCase):
    """When is the client done loading? Only then may memory be given back."""

    def test_a_short_history_is_never_settled(self):
        self.assertFalse(balloon.plateaued([]))
        self.assertFalse(balloon.plateaued([900, 900, 900]))

    def test_a_climbing_history_is_not_settled(self):
        # The PS99 load profile: PSS climbing steadily. Shrinking into this is
        # how you meet the cap on the way up.
        self.assertFalse(balloon.plateaued([400, 700, 1100, 1500]))

    def test_a_flat_history_is_settled(self):
        self.assertTrue(balloon.plateaued([1500, 1510, 1495, 1502]))

    def test_noise_within_tolerance_still_counts_as_flat(self):
        self.assertTrue(balloon.plateaued([1500, 1540, 1480, 1520]))

    def test_a_live_games_streaming_churn_counts_as_settled(self):
        # REGRESSION. The first cut asked for max - min <= tolerance, i.e. for
        # the guest to hold still. MEASURED 2026-08-16, PS99 in-world moved
        # 2137..2217 MB over four polls purely from asset streaming, so that
        # test never passed and the governor did nothing for the life of the
        # instance. Only sustained GROWTH may block the shrink.
        self.assertTrue(balloon.plateaued([2217, 2182, 2177, 2171]))
        self.assertTrue(balloon.plateaued([2137, 2260, 2100, 2150]))

    def test_a_falling_history_is_settled(self):
        # Freeing memory is not a reason to refuse to reclaim it.
        self.assertTrue(balloon.plateaued([2000, 1800, 1600, 1400]))

    def test_a_gap_in_the_history_is_not_settled(self):
        # An adb hiccup leaves a None. Missing information never advances the
        # decision to start giving memory away.
        self.assertFalse(balloon.plateaued([1500, None, 1500, 1500]))


class MemInfo(unittest.TestCase):
    """Reading the guest's own view of what it is using."""

    SAMPLE = ("MemTotal:        1572864 kB\n"
              "MemFree:          120000 kB\n"
              "MemAvailable:     540672 kB\n"
              "Buffers:            8000 kB\n")

    def test_uses_memavailable_not_memfree(self):
        # MemFree counts the page cache as used, and the page cache is the
        # exact thing the governor is trying to stop the guest hoarding.
        # Sizing against it would read every capped guest as still full.
        self.assertEqual(balloon.used_mb_from_meminfo(self.SAMPLE), 1008)

    def test_missing_fields_are_unknown_not_zero(self):
        self.assertIsNone(balloon.used_mb_from_meminfo("MemTotal: 100 kB\n"))
        self.assertIsNone(balloon.used_mb_from_meminfo(""))
        self.assertIsNone(balloon.used_mb_from_meminfo(None))

    def test_garbage_does_not_raise(self):
        self.assertIsNone(balloon.used_mb_from_meminfo("error: device offline"))


class BootCap(unittest.TestCase):
    """What the guest is capped at before adb — and so before any poll."""

    def test_governor_uses_the_modes_own_floor(self):
        self.assertEqual(balloon.governor_wanted(
            {"mem": 3072, "balloon_floor": 1536, "profile": "density"}), 1536)

    def test_the_performance_profile_gets_no_governor(self):
        # A latency decision, not a memory one: an evicted page comes back
        # from the pagefile, so a trimmed instance can hitch for as long as
        # that read takes. Farming does not care; `performance` exists for
        # "frames, resolution, input latency" and the frame-time cost has
        # never been measured. Gaming keeps the behaviour it has always had.
        self.assertIsNone(balloon.governor_wanted(
            {"mem": 4096, "balloon_floor": 1024, "profile": "performance"}))

    def test_a_mode_with_no_profile_gets_no_governor(self):
        self.assertIsNone(balloon.governor_wanted(
            {"mem": 4096, "balloon_floor": 1024}))

    def test_the_shipped_modes_land_on_the_right_side(self):
        from omnidroid.qemu_proc import MODES
        self.assertIsNone(balloon.governor_wanted(MODES["gaming"]))
        self.assertEqual(balloon.governor_wanted(MODES["farming"]), 896)

    def test_a_host_that_cannot_reclaim_gets_no_governor(self):
        # MEASURED 2026-08-16 on Windows: capping at spawn saved ~60 MB of the
        # 3.4 GB an uncapped instance costs, and spent a 0.3 -> 1.4 min boot
        # and 31 MB of qemu.log to do it. The balloon descends at ~25 MB/s
        # there, so it is still descending while Android boots, and the guest
        # touches nearly all of `-m` on different pages instead of the same
        # ones. See balloon.governor_wanted's docstring.
        self.assertIsNone(balloon.governor_wanted(
            {"mem": 3072, "balloon_floor": 1536}, host_can_reclaim=False))

    def test_no_governor_when_mem_is_below_the_floor(self):
        # `--mem 1024` with a 1536 boot cap is not an error, it is just no cap.
        self.assertIsNone(balloon.governor_wanted({"mem": 1024, "balloon_floor": 1536, "profile": "density"}))

    def test_no_governor_when_the_mode_does_not_ask(self):
        self.assertIsNone(balloon.governor_wanted({"mem": 3072, "profile": "density"}))

    def test_explicit_balloon_wins_over_the_governor(self):
        # `--balloon N` is a hard user instruction and resolve_mode already
        # marks it; the governor must not talk over it.
        self.assertIsNone(balloon.governor_wanted(
            {"mem": 3072, "balloon_floor": 1536, "profile": "density",
             "balloon_explicit": True}))


class GrowableMemory(unittest.TestCase):
    """`-m` as a floor with hotplug headroom, instead of a fixed wall.

    Inert today: no mode sets `mem_boot`, because the Bliss kernel is built
    without CONFIG_MEMORY_HOTPLUG and never onlines a plugged DIMM (measured
    2026-08-16: QEMU plugged 512 MB, guest MemTotal did not move). Tested so
    the host half is ready and correct for a base that ships the config.
    """

    def test_no_mem_boot_is_the_plain_form(self):
        from omnidroid.qemu_proc import mem_args
        self.assertEqual(mem_args({"mem": 4096}, 4096), ["-m", "4096"])
        self.assertEqual(mem_args(None, 4096), ["-m", "4096"])

    def test_mem_boot_declares_slots_and_a_maximum(self):
        from omnidroid.qemu_proc import mem_args, MEM_SLOTS
        self.assertEqual(
            mem_args({"mem_boot": 1536}, 4096),
            ["-m", f"size=1536,slots={MEM_SLOTS},maxmem=4096M"])

    def test_maxmem_always_carries_a_unit(self):
        # Without one QEMU reads it as BYTES and refuses to start with
        # "maximum memory size (0x1000) must be at least the initial memory
        # size" -- which reads like a sizing mistake, not a missing suffix.
        from omnidroid.qemu_proc import mem_args
        self.assertTrue(mem_args({"mem_boot": 1024}, 2048)[1].endswith("M"))

    def test_a_mem_below_the_boot_size_falls_back_to_plain(self):
        # `--mem 1024` against a 1536 boot size is not an error to raise; it
        # is a guest already smaller than the floor.
        from omnidroid.qemu_proc import mem_args
        self.assertEqual(mem_args({"mem_boot": 1536}, 1024), ["-m", "1024"])

    def test_no_shipped_mode_asks_for_it_yet(self):
        from omnidroid.qemu_proc import MODES
        for name, mode in MODES.items():
            self.assertIsNone(mode.get("mem_boot"),
                              f"{name} would boot small with no way to grow")


if __name__ == "__main__":
    unittest.main(verbosity=2)
