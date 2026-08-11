#!/usr/bin/env python3
"""The never-sleep / never-blank guest sequence.

    python3 tests/test_awake.py

An omnidroid instance is a headless farm worker or a remote-viewed game: there
is no human touching the panel, so Android's ordinary "no input for a while ->
dim -> screen off -> doze" ladder is pure downside. Roblox stops rendering (and
in-place farming stops earning) the moment the display goes off, and the VNC
viewer shows the black screen that made this a bug report.

The ladder has SIX independent rungs, and turning off only the famous one
(screen_off_timeout) leaves the other five to blank the screen anyway:

  * Settings.System screen_off_timeout   - the classic inactivity timer
  * Settings.Global stay_on_while_plugged_in - overrides the timer, but ONLY
    while the battery service says "charging", which a QEMU guest with no
    battery HAL does not say on its own
  * Settings.Secure sleep_timeout        - the separate "user is away" timer
  * Settings.Secure attentive_timeout    - Android 11+ attentive display
  * Settings.Secure adaptive_sleep       - screen-attention (camera) sleep
  * the dream/screensaver manager        - blanks to a daydream on its own

so this module's contract is that build_awake_sequence covers all of them, in
an order where the battery override lands BEFORE the setting that depends on
it.
"""
import os
import shlex
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import awake  # noqa: E402


def _flat(steps):
    return " | ".join(" ".join(s) for s in steps)


class CoversEveryRungOfTheSleepLadder(unittest.TestCase):
    def setUp(self):
        self.steps = awake.build_awake_sequence(su=None)
        self.flat = _flat(self.steps)

    def test_the_inactivity_timer_is_effectively_infinite(self):
        self.assertIn(
            f"settings put system screen_off_timeout "
            f"{awake.SCREEN_OFF_TIMEOUT_MS}", self.flat)
        # A "long" timeout is not the same promise as "never". Anything that
        # still fits in a day would blank a farm instance overnight.
        self.assertGreater(awake.SCREEN_OFF_TIMEOUT_MS, 24 * 60 * 60 * 1000)

    def test_stay_awake_while_plugged_covers_every_plug_type(self):
        # BATTERY_PLUGGED_AC(1)|USB(2)|WIRELESS(4)|DOCK(8). Picking only one
        # loses the guarantee the moment a base reports a different plug type,
        # and the full mask is also what the ROM's own `svc power stayon true`
        # writes — a smaller value here would just be overwritten by the next
        # command in the same script.
        self.assertEqual(awake.STAY_ON_ANY_PLUG, 1 | 2 | 4 | 8)
        self.assertIn("settings put global stay_on_while_plugged_in "
                      f"{awake.STAY_ON_ANY_PLUG}", self.flat)

    def test_the_away_and_attentive_timers_are_disabled(self):
        self.assertIn("settings put secure sleep_timeout -1", self.flat)
        self.assertIn("settings put secure attentive_timeout -1", self.flat)

    def test_screen_attention_sleep_is_off(self):
        self.assertIn("settings put secure adaptive_sleep 0", self.flat)

    def test_the_daydream_screensaver_cannot_blank_the_screen(self):
        self.assertIn("screensaver_enabled 0", self.flat)
        self.assertIn("screensaver_activate_on_sleep 0", self.flat)
        self.assertIn("screensaver_activate_on_dock 0", self.flat)

    def test_the_screen_is_woken_now_not_just_kept_awake(self):
        # A restored/warm-booted instance can arrive with the display already
        # off; settings alone do not turn it back on.
        self.assertIn(f"input keyevent {awake.KEYCODE_WAKEUP}", self.flat)

    def test_it_never_presses_the_power_key(self):
        # KEYCODE_POWER(26) TOGGLES. On an already-awake instance it is exactly
        # the black screen this module exists to prevent.
        self.assertNotIn("keyevent 26", self.flat)
        self.assertNotIn("KEYCODE_POWER", self.flat)


class ForcesTheBatteryStateStayOnDependsOn(unittest.TestCase):
    """stay_on_while_plugged_in is a MASK OF PLUG TYPES, not a boolean.

    PowerManagerService honours it only while BatteryService reports the device
    plugged in. A QEMU guest with no battery HAL reports plugged=0, so the
    setting alone is a silent no-op — set, readable, and doing nothing. The
    `dumpsys battery set` override is what makes it real, which is why it must
    come first.
    """

    def setUp(self):
        self.steps = awake.build_awake_sequence(su=None)
        self.flat = _flat(self.steps)

    def test_the_battery_is_reported_plugged_in_and_charging(self):
        self.assertIn("dumpsys battery set ac 1", self.flat)
        self.assertIn(f"dumpsys battery set status {awake.BATTERY_CHARGING}",
                      self.flat)

    def test_the_battery_is_reported_full_so_saver_never_dims(self):
        self.assertIn("dumpsys battery set level 100", self.flat)

    def test_the_battery_override_lands_before_stay_on(self):
        self.assertLess(self.flat.index("dumpsys battery set ac 1"),
                        self.flat.index("stay_on_while_plugged_in"))

    def test_it_never_resets_the_override_it_just_installed(self):
        self.assertNotIn("battery reset", self.flat)
        self.assertNotIn("battery unplug", self.flat)


class RootOnlyStepsAreOmittedWithoutRoot(unittest.TestCase):
    """Same rule as gaming.root_only_steps: emit a step that cannot work and
    it fails silently behind its trailing `; true`, reporting success having
    changed nothing."""

    def test_the_kernel_wakelock_needs_root(self):
        self.assertNotIn("/sys/power/wake_lock",
                         _flat(awake.build_awake_sequence(su=None)))
        self.assertIn("/sys/power/wake_lock",
                      _flat(awake.build_awake_sequence(su="su")))

    def test_the_wakelock_is_tagged_so_it_can_be_found_and_released(self):
        self.assertIn(awake.WAKE_LOCK_TAG,
                      _flat(awake.build_awake_sequence(su="su")))

    def test_the_rootless_sequence_is_still_useful(self):
        # The user-space levers are the ones that actually stop the blanking;
        # the kernel wakelock only stops suspend. An unrooted base must not
        # lose the screen guarantee.
        flat = _flat(awake.build_awake_sequence(su=None))
        for must in ("screen_off_timeout", "stay_on_while_plugged_in",
                     "dumpsys battery set ac 1"):
            self.assertIn(must, flat)

    def test_root_names_what_it_skips(self):
        self.assertTrue(awake.root_only_steps())


class AdbShellQuoting(unittest.TestCase):
    """adb shell does not forward argv — it joins the arguments with spaces and
    lets the guest's shell re-parse the result. An unquoted `a; b` therefore
    runs fragments of itself and, because every script ends in `; true`,
    reports success. Same trap farming.sh was written for."""

    def test_every_compound_step_is_quoted_as_one_word(self):
        for step in awake.build_awake_sequence(su="su"):
            joined = " ".join(step[1:])
            if ";" not in joined:
                continue
            # Re-parse it exactly as the guest's shell will. A correctly quoted
            # script arrives as ONE token — the last one — so the whole thing
            # reaches `sh -c`. If the quoting is missing, the `;` splits across
            # several tokens and the guest runs fragments of it instead.
            toks = shlex.split(joined)
            carriers = [t for t in toks if ";" in t]
            self.assertEqual(
                len(carriers), 1,
                f"unquoted compound step would fragment: {step}")
            self.assertEqual(
                carriers[0], toks[-1],
                f"script is not the final argument of the shell: {step}")
            self.assertIn("-c", toks, f"compound step is not an sh -c: {step}")

    def test_every_step_is_an_adb_shell_vector(self):
        for step in awake.build_awake_sequence(su="su"):
            self.assertEqual(step[0], "shell", f"not an adb shell step: {step}")


class TheRecheckIsCheapAndSufficient(unittest.TestCase):
    """The watchdog re-asserts periodically because the guest can lose these
    at runtime: Roblox's own settings writes, a `dumpsys battery reset` from a
    devkit session, or a framework restart (`am restart`) all drop them, and a
    farm instance nobody is watching would then blank hours later."""

    def setUp(self):
        self.steps = awake.build_awake_recheck(su=None)
        self.flat = _flat(self.steps)

    def test_it_re_asserts_the_levers_that_actually_expire(self):
        self.assertIn("dumpsys battery set ac 1", self.flat)
        self.assertIn("stay_on_while_plugged_in", self.flat)
        self.assertIn(f"input keyevent {awake.KEYCODE_WAKEUP}", self.flat)

    def test_it_is_strictly_cheaper_than_the_full_sequence(self):
        self.assertLess(len(self.steps),
                        len(awake.build_awake_sequence(su=None)))

    def test_it_holds_to_the_same_quoting_rule(self):
        for step in awake.build_awake_recheck(su="su"):
            self.assertEqual(step[0], "shell")


class ReadingBackTheGuestState(unittest.TestCase):
    """`dumpsys power` is the honest answer to 'is the screen actually on'.
    Reporting success from the fact that the commands ran is the silent-no-op
    shape this repo keeps getting bitten by."""

    AWAKE = """
  mWakefulness=Awake
  mWakefulnessChanging=false
  mIsPowered=true
  mPlugType=1
  mStayOn=true
  Display Power: state=ON
"""
    ASLEEP = """
  mWakefulness=Asleep
  mIsPowered=false
  mPlugType=0
  mStayOn=false
  Display Power: state=OFF
"""

    def test_an_awake_guest_reads_as_awake(self):
        st = awake.parse_power_state(self.AWAKE)
        self.assertEqual(st["wakefulness"], "Awake")
        self.assertEqual(st["display"], "ON")
        self.assertTrue(st["powered"])
        self.assertTrue(awake.is_awake(self.AWAKE))

    def test_a_sleeping_guest_reads_as_asleep(self):
        st = awake.parse_power_state(self.ASLEEP)
        self.assertEqual(st["wakefulness"], "Asleep")
        self.assertEqual(st["display"], "OFF")
        self.assertFalse(st["powered"])
        self.assertFalse(awake.is_awake(self.ASLEEP))

    def test_unreadable_output_is_unknown_not_awake(self):
        # An adb hiccup must never be reported as a satisfied guarantee.
        for junk in ("", None, "error: device offline"):
            st = awake.parse_power_state(junk)
            self.assertIsNone(st["wakefulness"])
            self.assertFalse(awake.is_awake(junk))

    def test_a_dozing_but_screen_on_guest_is_not_called_awake(self):
        # Dozing/Dreaming are distinct wakefulness states; only Awake counts.
        self.assertFalse(awake.is_awake(
            "mWakefulness=Dreaming\nDisplay Power: state=ON"))


class TheEffectiveTimeoutIsTheHonestReading(unittest.TestCase):
    """CAPTURED from the live instance that prompted this work (2026-08-09,
    arm base, LineageOS, an ordinary `omnidroid start`).

    Note what `settings get system screen_off_timeout` said on that same
    instance: -1. Not "never" — PowerManagerService clamps the setting up to
    mMinimumScreenOffTimeoutConfig, so the effective timeout was TEN SECONDS.
    Verifying via the setting would have reported the instance healthy while
    it went black ten seconds after the last input.
    """

    BEFORE = """
  mWakefulness=Awake
  mWakefulnessChanging=false
  mIsPowered=true
  mPlugType=1
  mStayOn=true
  mStayOnWhilePluggedInSetting=1
  mMinimumScreenOffTimeoutConfig=10000
  mScreenOffTimeoutSetting=-1
Screen off timeout: 10000 ms
Screen dim duration: 2000 ms
"""

    AFTER = """
  mWakefulness=Awake
  mIsPowered=true
  mPlugType=1
  mStayOnWhilePluggedInSetting=15
Screen off timeout: 2147483647 ms
"""

    def test_the_ten_second_blank_is_visible_in_the_reading(self):
        st = awake.parse_power_state(self.BEFORE)
        self.assertEqual(st["screen_off_timeout_ms"], 10000)
        self.assertEqual(st["stay_on_setting"], 1)

    def test_awake_is_not_the_same_question_as_never_blanks(self):
        # The instance really was Awake. It was also ten seconds from black.
        # Conflating the two is what let this ship.
        self.assertTrue(awake.is_awake(self.BEFORE))

    def test_the_fixed_instance_reads_as_never_blanking(self):
        st = awake.parse_power_state(self.AFTER)
        self.assertGreaterEqual(st["screen_off_timeout_ms"],
                                awake.SCREEN_OFF_TIMEOUT_MS)
        self.assertEqual(st["stay_on_setting"], awake.STAY_ON_ANY_PLUG)
        self.assertTrue(awake.never_blanks(self.AFTER))

    def test_stay_on_while_plugged_alone_satisfies_the_guarantee(self):
        # The BEFORE instance was not blanking, because it happened to be on
        # AC and stay-on happened to cover AC. That is a coincidence worth
        # crediting honestly — and worth not depending on, which is why the
        # sequence also fixes the timeout.
        self.assertTrue(awake.never_blanks(self.BEFORE))

    def test_a_short_timeout_with_no_plug_does_not_satisfy_it(self):
        self.assertFalse(awake.never_blanks(
            "mIsPowered=false\nmStayOnWhilePluggedInSetting=0\n"
            "Screen off timeout: 30000 ms"))

    def test_stay_on_set_but_unplugged_does_not_satisfy_it(self):
        # The trap this module documents: the setting is a plug-type mask, so
        # with nothing plugged in it is a silent no-op.
        self.assertFalse(awake.never_blanks(
            "mIsPowered=false\nmStayOnWhilePluggedInSetting=15\n"
            "Screen off timeout: 10000 ms"))

    def test_unreadable_output_never_claims_the_guarantee(self):
        for junk in ("", None, "error: device offline"):
            self.assertFalse(awake.never_blanks(junk))


class TheKillSwitch(unittest.TestCase):
    def test_it_is_on_by_default(self):
        self.assertTrue(awake.awake_enabled(env={}))

    def test_the_env_var_turns_it_off(self):
        self.assertFalse(awake.awake_enabled(env={awake.NO_AWAKE_ENV: "1"}))
        self.assertFalse(awake.awake_enabled(env={awake.NO_AWAKE_ENV: "true"}))

    def test_an_empty_or_zero_value_is_not_a_kill(self):
        for v in ("", "0", "no", "off"):
            self.assertTrue(awake.awake_enabled(env={awake.NO_AWAKE_ENV: v}))


if __name__ == "__main__":
    unittest.main(verbosity=2)
