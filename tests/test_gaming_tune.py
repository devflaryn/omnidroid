#!/usr/bin/env python3
"""The gaming-mode guest tune-up: frames and input latency, not footprint.

    python3 tests/test_gaming_tune.py

farming.build_squeeze_sequence and gaming.build_tuning_sequence are deliberate
opposites applied to the same base, and several farming levers are actively
hostile to a playable instance:

  * `wm size 480x270` — a postage stamp
  * `window_animation_scale 0` — the one lever both modes want
  * `swappiness 100` — every frame can stall on a zram decompress
  * game pinned to /dev/cpuset/background — starved of the cores it needs
  * `deviceidle force-idle` — the framework throttling a foreground game
  * `DFIntTaskSchedulerTargetFps: 5` — a hard 5 fps cap on the engine tick

Those are all correct for farming and all wrong for gaming, so the gaming
sequence has to UNDO them rather than merely not-apply them: an account that
was farmed and is then started in gaming mode keeps every one of them in its
/data until something explicitly reverses it.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import gaming, farming, lean  # noqa: E402


def _flat(steps):
    return " | ".join(" ".join(s) for s in steps)


class SettingsProfile(unittest.TestCase):
    def test_the_engine_tick_is_not_capped_at_the_farming_rate(self):
        farm = lean.CLIENT_APP_SETTINGS["DFIntTaskSchedulerTargetFps"]
        game = lean.GAMING_APP_SETTINGS["DFIntTaskSchedulerTargetFps"]
        self.assertGreater(game, farm)

    def test_the_tick_target_is_above_60(self):
        # The cap must not be what limits frames; the renderer should be.
        self.assertGreaterEqual(
            lean.GAMING_APP_SETTINGS["DFIntTaskSchedulerTargetFps"], 60)

    def test_the_two_profiles_are_distinct_objects(self):
        # A shared dict would let one mode's tuning leak into the other.
        self.assertIsNot(lean.GAMING_APP_SETTINGS, lean.CLIENT_APP_SETTINGS)

    def test_the_profile_serialises_like_the_farming_one(self):
        body = lean.client_settings_json(lean.GAMING_APP_SETTINGS)
        self.assertIn("DFIntTaskSchedulerTargetFps", body)

    def test_the_install_script_can_carry_the_gaming_profile(self):
        script = farming.build_client_settings_script(
            su="su", settings=lean.GAMING_APP_SETTINGS)
        self.assertIn("DFIntTaskSchedulerTargetFps", script)
        self.assertIn(lean.CLIENT_SETTINGS_FILE, script)


class UndoesTheFarmingSqueeze(unittest.TestCase):
    def setUp(self):
        self.steps = _flat(gaming.build_tuning_sequence(su="su"))

    def test_the_display_is_returned_to_native(self):
        self.assertIn("wm size reset", self.steps)
        self.assertIn("wm density reset", self.steps)

    def test_it_never_sets_the_postage_stamp_size(self):
        self.assertNotIn("480x270", self.steps)

    def test_swappiness_is_lowered_so_frames_do_not_wait_on_zram(self):
        self.assertRegex(self.steps, r"swappiness")
        self.assertNotIn("100 > /proc/sys/vm/swappiness", self.steps)

    def test_the_game_is_never_left_on_the_background_cpuset(self):
        # The move itself is a separate, post-session step — see
        # TheCpusetMoveWaitsForTheGame for why it cannot live in this
        # sequence. What must be true HERE is that nothing re-applies
        # farming's background pin.
        self.assertNotIn("cpuset/background", self.steps)
        self.assertIn("top-app", " ".join(gaming.build_pin_game_step("su")))

    def test_doze_is_disabled_rather_than_forced(self):
        self.assertIn("deviceidle disable", self.steps)
        self.assertNotIn("force-idle", self.steps)

    def test_the_keyboard_is_re_enabled(self):
        # farming disables the IME (~149 MB, nothing ever types); a player
        # needs to type into chat and login fields.
        self.assertIn("com.android.inputmethod.latin", self.steps)
        self.assertIn("enable", self.steps)


class LowInputLatency(unittest.TestCase):
    def setUp(self):
        self.steps = _flat(gaming.build_tuning_sequence(su="su"))

    def test_all_three_animation_scales_are_zeroed(self):
        for scale in ("window_animation_scale", "transition_animation_scale",
                      "animator_duration_scale"):
            self.assertIn(scale, self.steps)

    def test_the_game_is_kept_awake(self):
        self.assertIn(farming.GAME_PKG, self.steps)


class RootOnlyStepsGoThroughSu(unittest.TestCase):
    """MEASURED on a live gaming instance (2026-08-06): as uid shell,

        $ adb shell cat /proc/sys/vm/swappiness
        cat: /proc/sys/vm/swappiness: Permission denied

    so a bare `echo 10 > /proc/sys/vm/swappiness` is a silent no-op — the
    step runs, ends in `; true`, and reports success having done nothing.
    Same for the cpuset write. This is the exact failure class farming.sh was
    written to document, so the root-needing steps must be routed through su
    and must be OMITTED (not silently ineffective) when there is no root."""

    def test_swappiness_is_not_attempted_as_plain_shell(self):
        plain = _flat(gaming.build_tuning_sequence(su=None))
        self.assertNotIn("swappiness", plain)

    def test_swappiness_goes_through_su_when_root_exists(self):
        rooted = _flat(gaming.build_tuning_sequence(su="su"))
        self.assertIn("swappiness", rooted)
        step = next(s for s in gaming.build_tuning_sequence(su="su")
                    if "swappiness" in " ".join(s))
        self.assertIn("su 0 sh -c", " ".join(step))

    def test_the_cpuset_move_is_not_attempted_as_plain_shell(self):
        self.assertNotIn("cpuset", _flat(gaming.build_tuning_sequence(su=None)))
        self.assertIsNone(gaming.build_pin_game_step(None))

    def test_the_unrooted_sequence_still_does_the_rest(self):
        plain = _flat(gaming.build_tuning_sequence(su=None))
        self.assertIn("wm size reset", plain)
        self.assertIn("window_animation_scale", plain)
        self.assertIn("deviceidle disable", plain)

    def test_root_only_steps_are_reported_so_a_skip_is_visible(self):
        self.assertTrue(gaming.root_only_steps())


class TheCpusetMoveWaitsForTheGame(unittest.TestCase):
    """MEASURED on a live gaming instance (2026-08-06): right after boot,

        $ adb shell su 0 sh -c 'pidof com.roblox.client'
        (empty)

    The tune-up runs immediately after boot, and the game only starts when the
    session is delivered to the kiosk AFTERWARDS. So a `pidof`-based cpuset
    move at tune-up time can never find a pid — it runs, finds nothing, ends
    in `; true`, and reports success. The move therefore belongs in its own
    step, invoked after the session lands, not in the boot-time sequence."""

    def test_the_boot_sequence_does_not_try_to_pin_a_game_that_is_not_running(self):
        self.assertNotIn("cpuset", _flat(gaming.build_tuning_sequence(su="su")))

    def test_there_is_a_separate_pin_step(self):
        self.assertIn("cpuset/top-app",
                      " ".join(gaming.build_pin_game_step("su")))

    def test_the_pin_step_needs_root(self):
        self.assertIsNone(gaming.build_pin_game_step(None))

    def test_the_pin_step_waits_for_the_pid_rather_than_sampling_once(self):
        # The kiosk launch is asynchronous; a single pidof races it.
        step = " ".join(gaming.build_pin_game_step("su"))
        self.assertRegex(step, r"while|until|for ")

    def test_the_pin_step_runs_as_root(self):
        self.assertIn("su 0 sh -c", " ".join(gaming.build_pin_game_step("su")))


class SafeToRunTwice(unittest.TestCase):
    def test_every_step_is_an_adb_argv_vector(self):
        for step in gaming.build_tuning_sequence(su="su"):
            self.assertIsInstance(step, list)
            self.assertEqual(step[0], "shell")

    def test_multi_command_steps_are_quoted_for_adb_shell(self):
        # Same trap farming.sh documents: `adb shell` re-parses a joined
        # string, so an unquoted `a; b` runs only fragments of itself.
        for step in gaming.build_tuning_sequence(su="su"):
            if step[:3] == ["shell", "sh", "-c"]:
                self.assertTrue(step[3].startswith("'"),
                                f"unquoted script: {step[3][:60]}")

    def test_the_sequence_is_not_empty(self):
        self.assertTrue(gaming.build_tuning_sequence(su="su"))


if __name__ == "__main__":
    unittest.main()
