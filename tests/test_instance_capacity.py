#!/usr/bin/env python3
"""How many instances fit, which wall is in the way, and what would move it.

    python3 -m pytest tests/test_instance_capacity.py -q

"Why can't I run thirty" has four possible answers and only one of them is the
RAM everybody plans for. The governors made an instance's RAM and CPU cheap
(3417 MB -> 384 MB, 161% -> 50% of a core) and that moved the wall somewhere
else entirely -- on Windows, to COMMIT, which is charged at the full `-m` the
moment QEMU maps guest memory whether or not a byte is touched.

MEASURED on a paused QEMU, which is the floor:

    -m 1024 whpx, -display none      1065 MB commit   (+41)
    -m 2048 whpx, -display none      2092 MB          (+44)
    -m 3072 whpx, -display none      3117 MB          (+45)
    -m 3072 whpx + gtk,gl=on         3258 MB          (+186)

⚠ Measure that with `-accel whpx`. The same probe without it reads +1070 MB,
because TCG reserves a ~1 GB translation buffer by default -- an artifact of
the probe, not a cost of an instance. That mistake is why this file exists
with numbers in it rather than a rule of thumb.

So commit tracks `-m` at 1:1, which makes `-m` the one lever entirely in the
launcher's hands -- everything else needs disk or a different machine.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import qemu_proc


def _host(free_ram_mb=16000, commit_free_mb=37000, disk_free_mb=30000,
          cores=24):
    """Pin every host probe, so the arithmetic is testable without a host."""
    return (
        mock.patch.object(qemu_proc, "_commit_status_mb",
                          return_value=(65207, 65207 - commit_free_mb,
                                        commit_free_mb)),
        mock.patch.object(qemu_proc, "scratch_free_mb",
                          return_value=disk_free_mb),
        mock.patch.object(qemu_proc, "host_capacity",
                          return_value=(32000, cores)),
        mock.patch("omnidroid.runtime.host_mem_available_mb",
                   return_value=free_ram_mb),
    )


def _with_host(fn, **kw):
    patches = _host(**kw)
    for p in patches:
        p.start()
    try:
        return fn()
    finally:
        for p in patches:
            p.stop()


FARMING = qemu_proc.MODES["farming"]


class TheReportNamesTheWallNotJustTheNumber(unittest.TestCase):

    def test_commit_is_the_wall_on_a_box_with_a_same_size_pagefile(self):
        r = _with_host(lambda: qemu_proc.instance_capacity(FARMING,
                                                           mem_mb=3072))
        self.assertEqual(r["binding"], "commit")
        # 37 GB free / (3072 + the measured overhead) MB each. Derived from
        # the constant rather than spelled out, so re-measuring the overhead
        # does not silently turn this into a test of a stale number.
        self.assertEqual(
            r["fits"], 37000 // (3072 + qemu_proc.COMMIT_OVERHEAD_MB))

    def test_ram_is_no_longer_the_wall_because_of_the_ceiling(self):
        # The governor holds a farming instance at its ws_floor, not at `-m`.
        # Sizing this against `-m` is what made RAM look like the constraint.
        r = _with_host(lambda: qemu_proc.instance_capacity(FARMING,
                                                           mem_mb=3072))
        self.assertEqual(r["walls"]["ram"]["each_mb"], FARMING["ws_floor"])
        self.assertGreater(r["walls"]["ram"]["fits"],
                           r["walls"]["commit"]["fits"])

    def test_cpu_is_sized_from_the_ceiling_the_governor_holds(self):
        r = _with_host(lambda: qemu_proc.instance_capacity(FARMING,
                                                           mem_mb=3072))
        self.assertEqual(r["walls"]["cpu"]["each_pct"],
                         FARMING["cpu_ceiling_pct"])
        self.assertEqual(r["walls"]["cpu"]["fits"],
                         (24 * 100) // FARMING["cpu_ceiling_pct"])

    def test_a_wall_this_host_cannot_measure_is_left_out_not_guessed(self):
        # An estimate that silently drops a constraint is how a capacity plan
        # turns into an out-of-memory host.
        with mock.patch.object(qemu_proc, "_commit_status_mb",
                               return_value=None), \
             mock.patch.object(qemu_proc, "scratch_free_mb",
                               return_value=None), \
             mock.patch.object(qemu_proc, "host_capacity",
                               return_value=(None, None)), \
             mock.patch("omnidroid.runtime.host_mem_available_mb",
                        return_value=None):
            r = qemu_proc.instance_capacity(FARMING, mem_mb=3072)
        self.assertEqual(r["walls"], {})
        self.assertIsNone(r["fits"])
        self.assertIsNone(r["binding"])


class TheAdviceNamesTheLeverNotTheNumber(unittest.TestCase):
    """"You are short on commit" is a fact nobody can act on."""

    def test_commit_advice_says_pagefile(self):
        r = _with_host(lambda: qemu_proc.instance_capacity(FARMING,
                                                           mem_mb=3072))
        self.assertIn("pagefile", qemu_proc.capacity_advice(r).lower())

    def test_disk_advice_says_where_the_scratch_lives(self):
        r = _with_host(lambda: qemu_proc.instance_capacity(FARMING,
                                                           mem_mb=1024),
                       disk_free_mb=6000)
        advice = qemu_proc.capacity_advice(r)
        self.assertIn("DISK", advice)

    def test_nothing_binding_gives_no_advice(self):
        self.assertEqual(qemu_proc.capacity_advice({}), "")


class TheLadderSaysHowToGetThere(unittest.TestCase):
    """`-m` is the one lever entirely in the launcher's hands, so the answer
    to "how do I fit more" starts with what each guest size buys."""

    def test_a_smaller_guest_fits_more_while_commit_is_the_wall(self):
        ladder = _with_host(
            lambda: qemu_proc.capacity_ladder(FARMING, want=30))
        by_mem = {r["mem_mb"]: r["fits"] for r in ladder["rungs"]}
        self.assertGreater(by_mem[2048], by_mem[3072])

    def test_it_stops_helping_once_another_wall_takes_over(self):
        # Below a point, shrinking the guest buys nothing because DISK is the
        # constraint -- which is the difference between "use less memory" and
        # "free some disk", and only one of them is the right advice.
        ladder = _with_host(
            lambda: qemu_proc.capacity_ladder(FARMING, want=30),
            disk_free_mb=30000)
        walls = {r["mem_mb"]: r["binding"] for r in ladder["rungs"]}
        self.assertEqual(walls[1024], "disk")

    def test_it_names_the_size_that_reaches_the_target(self):
        ladder = _with_host(
            lambda: qemu_proc.capacity_ladder(FARMING, want=8),
            commit_free_mb=37000, disk_free_mb=200000)
        self.assertIsNotNone(ladder["reaches_want_at_mem_mb"])


class TheShortfallIsAShoppingList(unittest.TestCase):

    def test_it_says_how_much_more_of_each_thing_is_needed(self):
        gap = _with_host(
            lambda: qemu_proc.capacity_shortfall(30, FARMING, mem_mb=2048))
        self.assertEqual(gap["want"], 30)
        self.assertGreater(gap["gaps"]["commit"]["short_mb"], 0)
        self.assertTrue(any("commit" in line for line in gap["lines"]))

    def test_a_host_that_already_fits_them_reports_no_gap(self):
        gap = _with_host(
            lambda: qemu_proc.capacity_shortfall(2, FARMING, mem_mb=2048),
            commit_free_mb=200000, disk_free_mb=200000,
            free_ram_mb=200000)
        self.assertEqual(gap["lines"], [])

    def test_the_biggest_gap_is_reported_first(self):
        gap = _with_host(
            lambda: qemu_proc.capacity_shortfall(30, FARMING, mem_mb=3072))
        shorts = [gap["gaps"][line.split(":")[0]]["short_mb"]
                  for line in gap["lines"]]
        self.assertEqual(shorts, sorted(shorts, reverse=True))


class TheOverheadIsTheMeasuredOne(unittest.TestCase):
    """⚠ THIS CLASS USED TO ASSERT `< 512`, AND THAT BOUND HID A 4x ERROR.

    The reasoning was: the probe that once read +1070 MB was running TCG,
    whose default translation buffer is ~1 GB, so a large value means somebody
    re-measured without `-accel whpx`. Sound as far as it went -- but it
    silently also pinned the constant to a PAUSED QEMU with no guest in it,
    which is where 192 came from, and a paused QEMU is a floor rather than a
    cost. Six live in-world instances measured 993 MB of marginal commit each.

    So magnitude alone can no longer tell a real overhead (993) from the TCG
    artifact (1070) -- they are 77 MB apart. The defence against that mistake
    is not a number in this file; it is measuring against a LIVE guest with
    `-accel whpx`, which is now written next to the constant. What these
    bounds still do is catch a return to the paused-QEMU floor, and catch a
    value so large it could only be a different bug.
    """

    def test_it_covers_a_live_guest_and_not_just_a_paused_one(self):
        # The smallest per-QEMU overhead measured against a RUNNING game was
        # +722 MB (at -m 2048). Anything under that is the paused-QEMU floor
        # coming back, and with it capacity answers that are ~2x optimistic.
        self.assertGreaterEqual(qemu_proc.COMMIT_OVERHEAD_MB, 722)

    def test_it_is_not_absurd(self):
        # An overhead larger than a small guest itself would mean the probe
        # measured something other than one instance.
        self.assertLess(qemu_proc.COMMIT_OVERHEAD_MB, 2048)


if __name__ == "__main__":
    unittest.main()
