#!/usr/bin/env python3
"""apply_farming_squeeze runs each built step over adb; only in farming mode.

    python3 tests/test_farming_apply.py
"""
import os
import shlex
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import farming  # noqa: E402
from omnidroid.qemu_proc import MODES  # noqa: E402


def _fake_resolve_mode(cfg, name=None, **kw):
    """A tiny resolved mode, but carrying the REAL `profile` for this name.

    The engine branches on the profile, not on the mode name, so a stub that
    dropped it would make every mode look like a performance mode and this
    file would assert nothing. Sizes stay at 1 — they are irrelevant here and
    keeping them tiny makes it obvious the stub is not the real table."""
    real = MODES.get(name or "playable", {})
    return {"name": name or "playable", "mem": 1, "smp": 1, "balloon": None,
            "profile": real.get("profile", "performance"),
            "quality": real.get("quality")}


class ApplySqueeze(unittest.TestCase):
    def test_runs_every_step_over_adb(self):
        acct = {"name": "u1"}
        with mock.patch.object(omni, "adb") as adb:
            omni.apply_farming_squeeze(acct)
        expected = len(farming.build_squeeze_sequence())
        self.assertEqual(adb.call_count, expected)
        # first positional arg of each call is the account
        for call in adb.call_args_list:
            self.assertIs(call.args[0], acct)


class TheRenderFloorReachesTheDevice(unittest.TestCase):
    """STEP_RENDER is a real step: it goes over adb, and it bisects.

    The squeeze is the prime suspect whenever Roblox will not run on the x86
    base, so every lever in it has to be removable BY NAME from the
    environment. A step that could only be disabled by editing farming.py
    would make every bisect attempt a different build of the product."""

    def _run(self, mode=None, env=None):
        """The argv vectors apply_farming_squeeze actually hands to adb.

        OMNI_FARM_SKIP is cleared unless the case sets it: a developer running
        the suite mid-bisect must not silently change what these assert."""
        acct = {"name": "u1"}
        env = dict(env or {})
        with mock.patch.dict(os.environ, env, clear=False), \
             mock.patch.object(omni, "adb") as adb:
            if "OMNI_FARM_SKIP" not in env:
                os.environ.pop("OMNI_FARM_SKIP", None)
            omni.apply_farming_squeeze(acct, mode)
        return [list(c.args[1:]) for c in adb.call_args_list]

    def test_the_render_step_is_sent(self):
        flat = " ".join(" ".join(c) for c in self._run())
        self.assertIn("animator_duration_scale", flat)

    def test_a_minimal_quality_mode_no_longer_sends_a_smaller_panel(self):
        """MEASURED 2026-08-15, PS99, in-world: the 320x180 panel KILLS the
        client — process gone, `screencap` solid black, guest MemAvailable
        jumping ~591 MB -> ~2227 MB as the game's 1.6 GB was released. It did
        so both as a second `wm size` and, after the sequence was folded to
        resize once, as the only one. The panel is fatal, not the repetition.

        Asserted at the ADB layer rather than only on the builder, because the
        thing that must never reach a live guest again is this argv."""
        mode = dict(MODES["farming"], quality="minimal")
        flat = " ".join(" ".join(c) for c in self._run(mode))
        self.assertNotIn("320x180", flat)
        self.assertIn("wm size 480x270", flat)

    def test_OMNI_FARM_SKIP_render_removes_exactly_that_step(self):
        full = self._run()
        skipped = self._run(env={"OMNI_FARM_SKIP": "render"})
        self.assertEqual(len(full) - len(skipped), 1)
        flat = " ".join(" ".join(c) for c in skipped)
        self.assertNotIn("animator_duration_scale", flat)
        # ...and the display step, which shares its slot, is untouched.
        self.assertEqual(skipped[0], ["shell", "wm", "size", "480x270"])

    def test_every_step_sent_is_a_quoted_argv_vector(self):
        """`adb shell` joins argv and lets the guest re-parse it; an unquoted
        multi-command script silently runs fragments of itself and still
        reports success. Asserted at the CALL SITE, not just in the builder."""
        for cmd in self._run(dict(MODES["farming"], quality="minimal")):
            self.assertEqual(cmd[0], "shell")
            if cmd[:3] == ["shell", "sh", "-c"]:
                self.assertEqual(len(cmd), 4)
                self.assertEqual(cmd[3], shlex.quote(shlex.split(cmd[3])[0]))


class FarmingGate(unittest.TestCase):
    """_ensure_booted only squeezes on a DENSITY-profile, non-debug boot.

    Exercises the NOT-running path (running_pid -> falsy) since the
    already-running branch returns early (before the squeeze gate) when
    boot_completed is already "1". first_boot_done=True makes `first` False
    (dev=False in the product path), matching how cmd_start's build_acct()
    handles always carry first_boot_done=True.
    """

    def _boot(self, mode_name):
        acct = {"name": "u1", "first_boot_done": True}
        with mock.patch.object(omni, "running_pid", return_value=None), \
             mock.patch.object(omni, "spawn_qemu"), \
             mock.patch.object(omni, "maybe_start_autocap"), \
             mock.patch.object(omni, "wait_for_boot", return_value=True), \
             mock.patch.object(omni, "post_boot"), \
             mock.patch.object(omni, "_devkit_activate"), \
             mock.patch.object(omni, "_enforce_hiding"), \
             mock.patch.object(omni, "assert_kiosk_game"), \
             mock.patch.object(omni, "apply_consent"), \
             mock.patch.object(omni, "apply_awake"), \
             mock.patch.object(omni, "resolve_mode",
                 side_effect=_fake_resolve_mode), \
             mock.patch.object(omni, "apply_balloon_target"), \
             mock.patch.object(omni, "apply_roblox_settings"), \
             mock.patch.object(omni, "apply_gaming_tuning"), \
             mock.patch.object(omni, "enable_zram"), \
             mock.patch.object(omni, "apply_farming_squeeze") as sq:
            omni._ensure_booted(acct, {}, "t", mode_name=mode_name)
        return sq

    def test_the_boot_never_squeezes_any_more(self):
        """The squeeze must NOT happen during the boot, in EITHER profile.

        This assertion is inverted from what it was, and the inversion is the
        bug fix. `_ensure_booted` runs before `cmd_start` delivers the session,
        so at this point the client has not been told which place to load.
        Squeezing there starved the load: MEASURED 2026-08-16 on PS99 at
        3072 MB with no balloon (so memory was not the variable), the client
        stayed alive at a flat ~400 MB with its engine parked in `futex_wait`
        and the guest 200% IDLE, for six minutes. With every squeeze step
        skipped, the same launch reached 1173 MB in 111 s and loaded the
        place. It is `settle_density_instance` that squeezes now, once the
        client has finished loading.
        """
        self.assertFalse(self._boot("farming").called)
        self.assertFalse(self._boot("gaming").called)

    def test_debug_boot_never_squeezes(self):
        """Even mode_name='farming', a DEBUG boot must never squeeze (its extra
        devkit/frida footprint isn't the production baseline)."""
        acct = {"name": "u1", "first_boot_done": True, "debug": True}
        with mock.patch.object(omni, "running_pid", return_value=None), \
             mock.patch.object(omni, "spawn_qemu"), \
             mock.patch.object(omni, "maybe_start_autocap"), \
             mock.patch.object(omni, "wait_for_boot", return_value=True), \
             mock.patch.object(omni, "post_boot"), \
             mock.patch.object(omni, "_devkit_activate"), \
             mock.patch.object(omni, "_enforce_hiding"), \
             mock.patch.object(omni, "assert_kiosk_game"), \
             mock.patch.object(omni, "apply_consent"), \
             mock.patch.object(omni, "apply_awake"), \
             mock.patch.object(omni, "resolve_mode",
                 side_effect=_fake_resolve_mode), \
             mock.patch.object(omni, "apply_balloon_target"), \
             mock.patch.object(omni, "apply_roblox_settings"), \
             mock.patch.object(omni, "apply_gaming_tuning"), \
             mock.patch.object(omni, "enable_zram"), \
             mock.patch.object(omni, "apply_farming_squeeze") as sq:
            omni._ensure_booted(acct, {}, "t", mode_name="farming", debug=True)
        self.assertFalse(sq.called)


class SqueezeHappensAfterTheGameHasLoaded(unittest.TestCase):
    """settle_density_instance is where the squeeze lives now."""

    def _settle(self, debug=False):
        acct = {"name": "u1"}
        mode = MODES["farming"]
        with mock.patch.object(omni, "wait_for_game_settled",
                               return_value=(True, 1200.0)) as wait,              mock.patch.object(omni, "enable_zram") as zram,              mock.patch.object(omni, "apply_balloon_target") as balloon,              mock.patch.object(omni, "apply_farming_squeeze") as sq:
            omni.settle_density_instance(acct, mode, "t", debug=debug)
        return wait, zram, sq, balloon

    def test_it_waits_then_squeezes_then_balloons(self):
        wait, zram, sq, balloon = self._settle()
        self.assertTrue(wait.called)
        self.assertTrue(zram.called)
        self.assertTrue(sq.called)
        # The balloon is LAST on purpose: it can only claim memory the guest
        # has already given up, so it has to follow the squeeze.
        self.assertTrue(balloon.called)

    def test_a_debug_boot_still_never_squeezes(self):
        _wait, zram, sq, balloon = self._settle(debug=True)
        self.assertFalse(sq.called)
        # ...but zram and the cap are not devkit-sensitive and still apply.
        self.assertTrue(zram.called)
        self.assertTrue(balloon.called)


class WaitingForTheGameToLoad(unittest.TestCase):
    """The settle probe: PSS, not a log marker.

    Roblox's log vocabulary moves between client builds; a marker that quietly
    stopped matching would put the squeeze back on top of a loading client
    with nothing to show for it.
    """

    def test_it_settles_when_growth_stops_above_the_floor(self):
        samples = [120.0, 800.0, 1180.0, 1200.0, 1205.0]
        with mock.patch.object(omni, "game_pss_mb", side_effect=samples),              mock.patch.object(omni.time, "sleep"):
            settled, pss = omni.wait_for_game_settled({"name": "u1"},
                                                      interval=0)
        self.assertTrue(settled)
        self.assertEqual(pss, 1205.0)

    def test_a_splash_screen_is_not_settled(self):
        # Flat AND small is a client sitting on a splash, which is the state
        # this whole change exists to stop squeezing.
        with mock.patch.object(omni, "game_pss_mb", return_value=110.0),              mock.patch.object(omni.time, "sleep"):
            settled, pss = omni.wait_for_game_settled(
                {"name": "u1"}, timeout=0.05, interval=0)
        self.assertFalse(settled)
        self.assertEqual(pss, 110.0)

    def test_still_growing_at_the_timeout_squeezes_anyway(self):
        # A farming instance that never squeezes is not a farming instance.
        # It has to say so rather than pretend it waited for the right moment.
        # PROPORTIONAL growth, not a fixed step: the probe's test is
        # "grew by less than 4% of the last reading", so a fixed +200 MB
        # eventually looks settled all by itself (which is the intended
        # behaviour -- growth really is slowing -- but makes for a useless
        # never-settles fixture).
        state = {"mb": 800.0}

        def _growing(*_a, **_k):
            state["mb"] *= 1.15
            return state["mb"]

        with mock.patch.object(omni, "game_pss_mb", side_effect=_growing),              mock.patch.object(omni.time, "sleep"):
            settled, _pss = omni.wait_for_game_settled(
                {"name": "u1"}, timeout=0.05, interval=0)
        self.assertFalse(settled)


class TheTranslatorCannotBeSwappedOut(unittest.TestCase):
    """x86 farming drops zram and swaps gently, and that is a crash fix.

    Roblox ships arm64 only, so the x86 base runs it through
    libndk_translation. MEASURED 2026-08-15 on PS99: a farming instance joins
    and then Roblox ABORTS with memory free and no OOM kill --

        F libc  : Fatal signal 6 (SIGABRT) ... pid (Main), tid (Thread-19)
        F DEBUG : Abort message: 'Cannot process signal 11'
        F DEBUG : #04 libndk_translation.so (HandleHostSignal(...))

    -- i.e. translated code took a SIGSEGV the translator's host-signal
    handler could not process. Farming is the only mode that swaps hard
    (swappiness 100, page-cluster 0, zram on), and evicting translated code
    pages is how that fault is manufactured. Gaming runs at swappiness 10 with
    no zram and has never crashed this way.

    The arm base runs Roblox NATIVELY, has no translator to upset, and keeps
    both levers -- which is why this is an ARCH override and not a retreat.
    """

    def _steps(self, arch):
        mode = omni.resolve_mode({}, "farming", arch=arch)
        return mode, " ".join(str(s) for s in
                              farming.build_squeeze_sequence(mode))

    def test_x86_KEEPS_zram(self):
        """Turning zram off as well was measured to be worse, not safer.

        Two runs, everything else identical:

            swappiness 10, zram ON   aborts 0, client reached the loading screen
            swappiness 10, zram OFF  aborts 0, and Roblox was OOM-KILLED three
                                     times over with `mem-pressure-event`

        zram is not what breaks the translator -- swapping HARD is -- and with
        lz4 compressing ~3x it is the only reason a 2 GB guest holds this game
        at all. Removing it traded one crash for another."""
        mode, steps = self._steps("x86")
        self.assertTrue(mode["zram"])
        self.assertNotIn("swapoff", steps)

    def test_x86_swaps_gently(self):
        mode, steps = self._steps("x86")
        self.assertEqual(mode["swappiness"], 10)
        self.assertIn("echo 10 > /proc/sys/vm/swappiness", steps)

    def test_arm_keeps_both_because_it_has_no_translator(self):
        mode, steps = self._steps("arm")
        self.assertTrue(mode["zram"])
        self.assertEqual(mode["swappiness"], 100)
        self.assertIn("swapon", steps)
        self.assertIn("echo 100 > /proc/sys/vm/swappiness", steps)

    def test_the_arch_override_leaves_no_residue(self):
        for arch in ("x86", "arm"):
            mode = omni.resolve_mode({}, "farming", arch=arch)
            leftovers = [k for k in mode if k.endswith(("_x86", "_arm"))]
            self.assertEqual(leftovers, [], arch)

    def test_gaming_never_swapped_hard_in_the_first_place(self):
        # The mode that was always fine keeps being fine: it has no swappiness
        # or zram key at all, and the density path is the only one that swaps.
        gaming = omni.resolve_mode({}, "gaming", arch="x86", host=(32768, 16))
        self.assertIsNone(gaming.get("zram"))

if __name__ == "__main__":
    unittest.main()


class WhatTheClientSaysAboutTheJoin(unittest.TestCase):
    """`ok: true` means the session was delivered, not that Roblox got in.

    A client sitting on "Connection Failed (Error Code: 279)" satisfied every
    check this engine had -- and it also stops growing, so the settle probe
    reads it as loaded. Measured on PS99, twice. So the launch reads the
    client's own log and REPORTS what it finds; it does not gate on it,
    because the wording moves between client builds and a miss must mean
    "could not tell" rather than "it failed".
    """

    def _probe(self, body):
        class R:
            stdout = body
            stderr = ""
        with mock.patch.object(omni, "root_shell", return_value=R()):
            return omni.probe_client_join({"name": "u1"})

    def test_a_join_is_recognised(self):
        r = self._probe("[FLog::Network] Connection accepted from "
                        "1.2.3.4|54321\n")
        self.assertTrue(r["in_world"])
        self.assertIsNone(r["error"])

    def test_a_refusal_wins_over_a_join_marker(self):
        # The client logs the attempt and THEN the failure, so both markers
        # are present on a failed join. The failure is the later truth.
        r = self._probe("Connection accepted from 1.2.3.4|1\n"
                        "Failed to connect to the experience (Error Code: 279)")
        self.assertFalse(r["in_world"])
        self.assertIn("279", r["error"])

    def test_silence_is_not_a_verdict(self):
        self.assertIsNone(self._probe("")["in_world"])

    def test_an_unreadable_log_is_not_a_verdict_either(self):
        with mock.patch.object(omni, "root_shell", side_effect=OSError("x")):
            self.assertIsNone(
                omni.probe_client_join({"name": "u1"})["in_world"])
