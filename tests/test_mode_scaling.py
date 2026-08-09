#!/usr/bin/env python3
"""Modes: playable/gaming take the host, farming gives it back.

    python3 tests/test_mode_scaling.py

Two opposite requirements share one table, and the tests below are what keeps
them from drifting into each other:

  PLAYABLE (and gaming) is what a human plays in AND what the AI tests in, so
  it should use as much of the host as it safely can — the most RAM and vCPUs
  the machine can spare, no balloon, no squeeze, native resolution, real
  render quality. "As much as it safely can" is the load-bearing half: a guest
  sized past the host's spare RAM makes the HOST swap, and a swapping host
  misses QEMU's vCPU deadlines, which is slower than the smaller guest would
  have been.

  FARMING is the opposite trade — minimum footprint, quality irrelevant — and
  must NOT inherit any of the above.

The scaling policy is a pure function (autoscale_perf) precisely so the whole
matrix is testable without a particular machine under it.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import qemu_proc as qp  # noqa: E402
from omnidroid import lean  # noqa: E402


class TheModeTable(unittest.TestCase):
    def test_every_mode_declares_a_profile(self):
        # The engine branches on `profile`, so a mode without one silently
        # gets the performance path — including, once, farming.
        for name, m in qp.MODES.items():
            self.assertIn("profile", m, name)
            self.assertIn(m["profile"], ("performance", "density"), name)

    def test_playable_is_the_default_and_a_performance_mode(self):
        self.assertEqual(qp.DEFAULT_MODE, "playable")
        self.assertEqual(qp.MODES["playable"]["profile"], "performance")

    def test_only_farming_trades_quality_for_density(self):
        density = [n for n, m in qp.MODES.items()
                   if m["profile"] == "density"]
        self.assertEqual(density, ["farming"])

    def test_no_performance_mode_inflates_a_balloon(self):
        # Reclaiming pages out from under a running game is a stutter source.
        for name, m in qp.MODES.items():
            if m["profile"] == "performance":
                self.assertIsNone(m["balloon"], name)

    def test_farming_keeps_its_measured_balloon_targets(self):
        # These are MEASURED floors (1536 without zram, 896 with lz4 zram);
        # lowering them OOM-killed the game. Pinned so a refactor cannot
        # quietly move them.
        self.assertEqual(qp.MODES["farming"]["balloon"], 1536)
        self.assertEqual(qp.MODES["farming"]["balloon_zram"], 896)
        self.assertEqual(qp.MODES["farming"]["smp"], 1)

    def test_only_playable_and_gaming_grow_to_the_host(self):
        # hard/brutal are explicit "give it LESS" requests; autoscaling them
        # would make the flag mean its opposite.
        grow = sorted(n for n, m in qp.MODES.items() if m.get("autoscale"))
        self.assertEqual(grow, ["gaming", "playable"])

    def test_the_quality_names_all_exist(self):
        for name, m in qp.MODES.items():
            if m.get("quality"):
                self.assertIsNotNone(lean.app_settings_for(m["quality"]), name)


class Autoscaling(unittest.TestCase):
    def _mem(self, host_mb, cpus=8):
        return qp.autoscale_perf(qp.MODES["playable"], host_mb, cpus)["mem"]

    def test_a_big_host_gives_the_guest_the_ceiling(self):
        self.assertEqual(self._mem(64 * 1024), qp.PERF_MEM_CEIL_MB)

    def test_a_typical_host_gives_the_guest_half(self):
        self.assertEqual(self._mem(16 * 1024), 8192)

    def test_the_host_always_keeps_its_reserve(self):
        # 12 GB host: half is 6 GB, but leaving the host only 6 GB is the
        # rule; the binding constraint is whichever is smaller.
        m = self._mem(12 * 1024)
        self.assertLessEqual(m, 12 * 1024 - qp.PERF_HOST_RESERVE_MB)

    def test_a_small_host_still_gets_the_measured_floor(self):
        # Below the floor the game does not fit at all (1024 MB OOM-killed it
        # in the measured runs), so shrinking further would trade a slow
        # instance for a dead one.
        self.assertEqual(self._mem(8 * 1024), qp.PERF_MEM_FLOOR_MB)
        self.assertEqual(self._mem(4 * 1024), qp.PERF_MEM_FLOOR_MB)

    def test_memory_is_rounded_to_a_sane_granularity(self):
        for host in (9000, 13000, 17000, 21000):
            self.assertEqual(self._mem(host) % qp.PERF_MEM_GRANULARITY_MB, 0)

    def test_vcpus_leave_the_host_some_cores(self):
        self.assertEqual(
            qp.autoscale_perf(qp.MODES["playable"], 32768, 8)["smp"], 6)
        self.assertEqual(
            qp.autoscale_perf(qp.MODES["playable"], 32768, 16)["smp"],
            qp.PERF_SMP_CEIL)

    def test_a_tiny_host_never_goes_below_the_vcpu_floor(self):
        self.assertEqual(
            qp.autoscale_perf(qp.MODES["playable"], 32768, 2)["smp"],
            qp.PERF_SMP_FLOOR)

    def test_an_unreadable_host_costs_the_upgrade_not_the_boot(self):
        m = qp.autoscale_perf(qp.MODES["playable"], None, None)
        self.assertEqual(m["mem"], qp.MODES["playable"]["mem"])
        self.assertEqual(m["smp"], qp.MODES["playable"]["smp"])

    def test_a_non_autoscaling_mode_is_returned_untouched(self):
        for name in ("hard", "brutal", "farming"):
            m = qp.autoscale_perf(qp.MODES[name], 64 * 1024, 32)
            self.assertEqual(m["mem"], qp.MODES[name]["mem"], name)
            self.assertEqual(m["smp"], qp.MODES[name]["smp"], name)

    def test_it_never_mutates_the_table(self):
        before = dict(qp.MODES["playable"])
        qp.autoscale_perf(qp.MODES["playable"], 64 * 1024, 32)
        self.assertEqual(qp.MODES["playable"], before)


class ResolveMode(unittest.TestCase):
    def test_an_explicit_mem_wins_over_the_host_derived_size(self):
        m = qp.resolve_mode({}, "playable", mem=2048, host=(65536, 32))
        self.assertEqual(m["mem"], 2048)

    def test_an_explicit_smp_wins_too(self):
        m = qp.resolve_mode({}, "playable", smp=2, host=(65536, 32))
        self.assertEqual(m["smp"], 2)

    def test_the_resolved_name_survives_autoscaling(self):
        # autoscale_perf returns a fresh dict; a lost `name` would send the
        # post-boot branch and run.json's mode field to the wrong place.
        self.assertEqual(qp.resolve_mode({}, "gaming", host=(65536, 32))
                         ["name"], "gaming")
        self.assertEqual(qp.resolve_mode({}, None, host=(65536, 32))["name"],
                         qp.DEFAULT_MODE)

    def test_the_default_mode_is_scaled_like_an_explicit_playable(self):
        self.assertEqual(qp.resolve_mode({}, None, host=(32768, 12)),
                         qp.resolve_mode({}, "playable", host=(32768, 12)))

    def test_farming_is_untouched_by_a_big_host(self):
        m = qp.resolve_mode({}, "farming", host=(262144, 64))
        self.assertEqual((m["mem"], m["smp"]), (2048, 1))


class QualityProfiles(unittest.TestCase):
    def test_the_three_names_resolve(self):
        for n in ("low", "balanced", "high"):
            self.assertIsInstance(lean.app_settings_for(n), dict)

    def test_an_unknown_name_returns_none_rather_than_a_silent_fallback(self):
        # A silent fallback here would install the 5-fps farming profile onto
        # a gaming boot and report success.
        self.assertIsNone(lean.app_settings_for("ultra"))

    def test_high_actually_renders_more_than_balanced(self):
        hi = lean.PLAYABLE_APP_SETTINGS
        bal = lean.GAMING_APP_SETTINGS
        self.assertGreater(hi["DFIntDebugFRMQualityLevelOverride"],
                           bal["DFIntDebugFRMQualityLevelOverride"])
        self.assertFalse(hi["FFlagDisablePostFx"])   # post-FX ON
        self.assertTrue(bal["FFlagDisablePostFx"])   # post-FX off

    def test_high_does_not_uncork_msaa(self):
        # The guest has no 3D acceleration on the primary host, so every MSAA
        # sample is resolved in software on the CPU running the game. It is
        # the one quality key deliberately left off.
        self.assertEqual(lean.PLAYABLE_APP_SETTINGS[
            "FIntDebugForceMSAASamples"], 0)

    def test_neither_performance_profile_caps_the_tick_the_way_farming_does(self):
        self.assertGreater(
            lean.PLAYABLE_APP_SETTINGS["DFIntTaskSchedulerTargetFps"],
            lean.CLIENT_APP_SETTINGS["DFIntTaskSchedulerTargetFps"])


if __name__ == "__main__":
    unittest.main()
