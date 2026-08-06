#!/usr/bin/env python3
"""The farming squeeze must survive `adb shell`'s argv mangling.

    python3 tests/test_squeeze_quoting.py

This is a regression test for a bug that made most of the squeeze a no-op
while reporting success. `adb shell` does not forward argv: it joins the
arguments with spaces and lets the guest's shell re-parse the result. So

    ["shell", "sh", "-c", "pm disable-user X; am force-stop X"]

arrives in the guest as `sh -c pm disable-user X; am force-stop X`, which
parses as `sh -c pm` (bare `pm`, rest as positional params) followed by a
separate `am force-stop X`. The first command never runs — and since every
script here ends in `; true`, the step still exits 0.

Measured live on 2026-08-05: with the quoting missing, `pm disable-user`
disabled nothing (`pm list packages -d` unchanged) while its `am force-stop`
half ran fine; with it, the same package showed up disabled immediately.
"""
import os
import shlex
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import farming, lean  # noqa: E402


def _shell_steps(steps):
    return [s for s in steps if s[:3] == ["shell", "sh", "-c"]]


class Quoting(unittest.TestCase):
    def setUp(self):
        self.steps = farming.build_squeeze_sequence()

    def test_every_multi_command_step_is_quoted(self):
        multi = _shell_steps(self.steps)
        self.assertTrue(multi, "expected sh -c steps in the squeeze")
        for step in multi:
            # The payload must survive a join+reparse round trip as ONE word.
            reparsed = shlex.split(" ".join(step[1:]))
            self.assertEqual(len(reparsed), 3,
                             f"step re-parses into {len(reparsed)} words, so "
                             f"the guest shell would split it: {step}")
            self.assertEqual(reparsed[:2], ["sh", "-c"])

    def test_reparsed_payload_is_the_intended_script(self):
        """Not just 'one word' — the word must be the script we wrote."""
        for step in _shell_steps(self.steps):
            reparsed = shlex.split(" ".join(step[1:]))
            self.assertEqual(reparsed[2], shlex.split(step[3])[0])

    def test_sh_helper_round_trips(self):
        script = "pm disable-user --user 0 com.x >/dev/null 2>&1; true"
        step = farming.sh(script)
        self.assertEqual(shlex.split(" ".join(step[1:]))[2], script)

    def test_bare_argv_steps_are_left_unquoted(self):
        """Steps that are already discrete argv (wm size, settings put) need
        no quoting and must not acquire any."""
        bare = [s for s in self.steps if s[:3] != ["shell", "sh", "-c"]]
        self.assertTrue(bare)
        for step in bare:
            for word in step:
                self.assertNotIn("'", word)


class SqueezeShape(unittest.TestCase):
    def test_display_is_shrunk_first(self):
        """Ordering is load-bearing: shrinking after apps have allocated
        tablet-sized buffers leaves the big buffers around."""
        steps = farming.build_squeeze_sequence()
        self.assertEqual(steps[0], ["shell", "wm", "size", "480x270"])

    def test_mode_display_override_is_honoured(self):
        steps = farming.build_squeeze_sequence({"display": (320, 180, 60),
                                                "mem": 2048})
        self.assertEqual(steps[0], ["shell", "wm", "size", "320x180"])

    def test_native_display_mode_skips_the_resize(self):
        steps = farming.build_squeeze_sequence({"display": None, "mem": 2048})
        self.assertNotIn("wm", [s[1] for s in steps])

    def test_every_trimmed_package_gets_a_step(self):
        steps = farming.build_squeeze_sequence()
        joined = " ".join(" ".join(s) for s in steps)
        for pkg in lean.trim_packages():
            self.assertIn(f"pm disable-user --user 0 {pkg}", joined)

    def test_game_is_never_disabled_by_the_squeeze(self):
        joined = " ".join(" ".join(s)
                          for s in farming.build_squeeze_sequence())
        self.assertNotIn(f"disable-user --user 0 {farming.GAME_PKG}", joined)

    def test_doze_whitelists_the_game_before_forcing_idle(self):
        """An un-whitelisted game loses its network the moment doze engages,
        which is the opposite of 'the instances must be on'."""
        joined = " ".join(" ".join(s)
                          for s in farming.build_squeeze_sequence())
        wl = joined.index(f"deviceidle whitelist +{farming.GAME_PKG}")
        idle = joined.index("deviceidle force-idle")
        self.assertLess(wl, idle)

    def test_every_step_fails_open(self):
        """A squeeze is an optimization, never a precondition: a step that
        cannot run must not leave the instance unstarted."""
        for step in _shell_steps(farming.build_squeeze_sequence()):
            self.assertTrue(step[3].rstrip("'").rstrip().endswith("true"),
                            f"step does not fail open: {step}")


class ZramSizing(unittest.TestCase):
    def test_scales_with_guest_memory(self):
        self.assertEqual(farming.zram_size_mb(2048), 1024)
        self.assertEqual(farming.zram_size_mb(1024), 512)

    def test_clamped_at_both_ends(self):
        self.assertEqual(farming.zram_size_mb(64), 128)
        self.assertEqual(farming.zram_size_mb(65536), 1024)


class ClientSettings(unittest.TestCase):
    """Roblox's own settings: a CPU lever, not a memory one.

    Measured 2026-08-05 on the dev base with the real APK: memory 680 -> 677
    MB (no change), host CPU 36% -> 18.8% (halved). Filed honestly as CPU,
    because for 50 instances CPU binds as hard as RAM."""

    def test_returns_none_without_root(self):
        """No root, no write - and the caller must be able to SEE that
        rather than run a step that quietly does nothing."""
        self.assertIsNone(farming.build_client_settings_script(None))

    def test_script_writes_the_settings_file(self):
        sc = farming.build_client_settings_script("/debug_ramdisk/su")
        self.assertIn(lean.CLIENT_SETTINGS_FILE, sc)
        self.assertIn("DFIntTaskSchedulerTargetFps", sc)

    def test_script_restores_ownership_and_label(self):
        """A file the app cannot read is the same as no file, except that it
        looks like it worked."""
        sc = farming.build_client_settings_script("/debug_ramdisk/su")
        self.assertIn("chown", sc)
        self.assertIn("restorecon", sc)

    def test_fps_is_actually_capped(self):
        self.assertLessEqual(lean.CLIENT_APP_SETTINGS[
            "DFIntTaskSchedulerTargetFps"], 10)

    def test_json_is_valid_and_stable(self):
        import json
        a = lean.client_settings_json()
        self.assertEqual(json.loads(a), lean.CLIENT_APP_SETTINGS)
        self.assertEqual(a, lean.client_settings_json())

    def test_heredoc_body_is_not_shell_quoted_away(self):
        """The JSON goes in via a quoted heredoc, so braces and quotes must
        survive verbatim."""
        sc = farming.build_client_settings_script("/debug_ramdisk/su")
        self.assertIn("<<'OMNI_EOF'", sc)
        self.assertIn('"DFIntDebugFRMQualityLevelOverride": 1', sc)


if __name__ == "__main__":
    unittest.main()
