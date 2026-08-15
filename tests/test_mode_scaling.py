#!/usr/bin/env python3
"""Modes: gaming takes the host, farming gives it back.

    python3 tests/test_mode_scaling.py

TWO modes, two opposite requirements, one table, and the tests below are what
keeps them from drifting into each other:

  GAMING is what a human plays in AND what the AI tests in, so it should use as
  much of the host as it safely can — the most RAM and vCPUs the machine can
  spare, no balloon, no squeeze, native resolution, real render quality. "As
  much as it safely can" is the load-bearing half: a guest sized past the
  host's spare RAM makes the HOST swap, and a swapping host misses QEMU's vCPU
  deadlines, which is slower than the smaller guest would have been.

  FARMING is the opposite trade — minimum footprint, quality irrelevant — and
  must NOT inherit any of the above.

`playable`, `hard` and `brutal` are gone; the retired names still RESOLVE (an
installed app persists its chosen mode and would otherwise break on update),
and that aliasing is pinned below.

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

    def test_there_are_exactly_two_modes(self):
        self.assertEqual(sorted(qp.MODES), ["farming", "gaming"])

    def test_gaming_is_the_default_and_a_performance_mode(self):
        self.assertEqual(qp.DEFAULT_MODE, "gaming")
        self.assertEqual(qp.MODES["gaming"]["profile"], "performance")

    def test_no_mode_asks_for_a_native_window(self):
        # The product is headless everywhere now; a window is only reachable
        # through the OMNI_GL_WINDOW debugging hatch. A mode that asked for one
        # would put a QEMU window on every launch again.
        for name, m in qp.MODES.items():
            self.assertFalse(m.get("window"), name)

    def test_every_mode_declares_a_panel(self):
        for name, m in qp.MODES.items():
            self.assertIsNotNone(qp.parse_panel(m.get("panel")) or
                                 tuple(m.get("panel") or ()), name)

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
        # x86 runs Roblox's arm64 build through libndk_translation, and one
        # vCPU could not get through the session handover (measured: the
        # ordered `am broadcast` did not return in 45 s). The arch override is
        # a requirement, not a preference.
        self.assertEqual(qp.MODES["farming"]["smp_x86"], 2)

    def test_only_gaming_grows_to_the_host(self):
        # Farming's whole point is a fixed small footprint; autoscaling it
        # would make the mode mean its opposite.
        grow = sorted(n for n, m in qp.MODES.items() if m.get("autoscale"))
        self.assertEqual(grow, ["gaming"])

    def test_the_quality_names_all_exist(self):
        for name, m in qp.MODES.items():
            if m.get("quality"):
                self.assertIsNotNone(lean.app_settings_for(m["quality"]), name)


class Autoscaling(unittest.TestCase):
    """The sizing POLICY, independent of the host running the tests.

    autoscale_perf() applies the WHPX ceilings (4096 MB / 4 vCPU) at the end
    on Windows, because measurement there showed the autoscaled 8192/8 booting
    6.5x SLOWER than 4096/4 on the same image. That is correct behaviour and it
    made this whole class fail on Windows -- the assertions were written on a
    Mac and encode the un-capped policy. Cap the expectations the same way the
    function does rather than skipping: the policy still has to be right
    everywhere, it is only the ceiling that differs.
    """

    @staticmethod
    def _cap_mem(v):
        return min(v, qp.WHPX_MEM_CEIL_MB) if qp.IS_WINDOWS else v

    @staticmethod
    def _cap_smp(v):
        return min(v, qp.WHPX_SMP_CEIL) if qp.IS_WINDOWS else v

    def _mem(self, host_mb, cpus=8):
        return qp.autoscale_perf(qp.MODES["gaming"], host_mb, cpus)["mem"]

    def test_a_big_host_gives_the_guest_the_ceiling(self):
        self.assertEqual(self._mem(64 * 1024),
                         self._cap_mem(qp.PERF_MEM_CEIL_MB))

    def test_a_typical_host_gives_the_guest_half(self):
        self.assertEqual(self._mem(16 * 1024), self._cap_mem(8192))

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
            qp.autoscale_perf(qp.MODES["gaming"], 32768, 8)["smp"],
            self._cap_smp(6))
        self.assertEqual(
            qp.autoscale_perf(qp.MODES["gaming"], 32768, 16)["smp"],
            self._cap_smp(qp.PERF_SMP_CEIL))

    def test_a_tiny_host_never_goes_below_the_vcpu_floor(self):
        self.assertEqual(
            qp.autoscale_perf(qp.MODES["gaming"], 32768, 2)["smp"],
            qp.PERF_SMP_FLOOR)

    def test_an_unreadable_host_costs_the_upgrade_not_the_boot(self):
        m = qp.autoscale_perf(qp.MODES["gaming"], None, None)
        self.assertEqual(m["mem"], qp.MODES["gaming"]["mem"])
        self.assertEqual(m["smp"], qp.MODES["gaming"]["smp"])

    def test_a_non_autoscaling_mode_is_returned_untouched(self):
        for name in ("farming",):
            m = qp.autoscale_perf(qp.MODES[name], 64 * 1024, 32)
            self.assertEqual(m["mem"], qp.MODES[name]["mem"], name)
            self.assertEqual(m["smp"], qp.MODES[name]["smp"], name)

    def test_it_never_mutates_the_table(self):
        before = dict(qp.MODES["gaming"])
        qp.autoscale_perf(qp.MODES["gaming"], 64 * 1024, 32)
        self.assertEqual(qp.MODES["gaming"], before)


class ResolveMode(unittest.TestCase):
    def test_an_explicit_mem_wins_over_the_host_derived_size(self):
        m = qp.resolve_mode({}, "gaming", mem=2048, host=(65536, 32))
        self.assertEqual(m["mem"], 2048)

    def test_an_explicit_smp_wins_too(self):
        m = qp.resolve_mode({}, "gaming", smp=2, host=(65536, 32))
        self.assertEqual(m["smp"], 2)

    def test_the_resolved_name_survives_autoscaling(self):
        # autoscale_perf returns a fresh dict; a lost `name` would send the
        # post-boot branch and run.json's mode field to the wrong place.
        self.assertEqual(qp.resolve_mode({}, "gaming", host=(65536, 32))
                         ["name"], "gaming")
        self.assertEqual(qp.resolve_mode({}, None, host=(65536, 32))["name"],
                         qp.DEFAULT_MODE)

    def test_the_default_mode_is_scaled_like_an_explicit_gaming(self):
        self.assertEqual(qp.resolve_mode({}, None, host=(32768, 12)),
                         qp.resolve_mode({}, "gaming", host=(32768, 12)))

    def test_farming_is_untouched_by_a_big_host(self):
        m = qp.resolve_mode({}, "farming", host=(262144, 64))
        self.assertEqual((m["mem"], m["smp"]), (2048, 1))

    def test_guest_display_is_overridable_and_none_is_a_real_value(self):
        """`--guest-display native` must be able to say "leave it alone".

        None is a MEANINGFUL value for `display` (native resolution), so
        "not given" cannot be None -- the same trap `--balloon 0` documents.
        The sentinel is the string "unset"."""
        default = qp.resolve_mode({}, "farming", arch="x86")["display"]
        self.assertEqual(default, (480, 270, 80))
        native = qp.resolve_mode({}, "farming", arch="x86",
                                 guest_display=None)["display"]
        self.assertIsNone(native)
        custom = qp.resolve_mode({}, "farming", arch="x86",
                                 guest_display=(720, 1280, 320))["display"]
        self.assertEqual(custom, (720, 1280, 320))
        # ...and the default path is untouched.
        self.assertEqual(qp.resolve_mode({}, "farming", arch="x86")["display"],
                         (480, 270, 80))

    def test_guest_display_parsing(self):
        self.assertIsNone(qp.parse_guest_display("native"))
        self.assertIsNone(qp.parse_guest_display("off"))
        self.assertEqual(qp.parse_guest_display("480x270"), (480, 270, 80))
        self.assertEqual(qp.parse_guest_display("720x1280x320"),
                         (720, 1280, 320))
        self.assertEqual(qp.parse_guest_display("nonsense"), "unset")
        self.assertEqual(qp.parse_guest_display(None), "unset")

    def test_the_retired_names_still_resolve_to_gaming(self):
        # An installed app persists the mode it was configured with, so
        # rejecting these would break every launch from a client that has not
        # been updated yet. Nothing downstream ever sees the old name.
        for old in ("playable", "hard", "brutal"):
            m = qp.resolve_mode({}, old, host=(32768, 12))
            self.assertEqual(m["name"], "gaming", old)

    def test_an_unknown_mode_names_the_real_list(self):
        with self.assertRaises(KeyError) as cm:
            qp.resolve_mode({}, "turbo")
        self.assertIn("gaming", str(cm.exception))
        self.assertIn("farming", str(cm.exception))

    def test_an_arch_override_applies_and_leaves_no_residue(self):
        x86 = qp.resolve_mode({}, "farming", arch="x86")
        arm = qp.resolve_mode({}, "farming", arch="arm")
        self.assertEqual(x86["smp"], 2)
        self.assertEqual(arm["smp"], 1)
        for m in (x86, arm):
            self.assertNotIn("smp_x86", m)


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
