#!/usr/bin/env python3
"""The render floor: `--quality minimal`, the smaller panel, and STEP_RENDER.

    python3 tests/test_render_floor.py

Farming is unattended — no fps requirement, no view quality requirement — and
it boots `-display none` with a VNC server that encodes nothing while nobody
is attached, so there is no HOST-side render cost left to attack. What remains
is the guest rasterising frames nobody looks at, and the render floor is the
explicitly-tunable tier that attacks it.

WHAT THESE TESTS DO AND DO NOT PROVE. They pin the SHAPE of the tier: that it
is lower than `low` on the only axis that had room, that it is bisectable by
name like every other squeeze step, that it lands in the ordering slot its
rationale claims, and that nothing it emits can be silently guillotined by
`adb shell`'s argv re-parse. None of that is a measurement. 3 fps and the
320x180/60 panel have NOT been run against a place — see the UNVERIFIED notes
in lean.py — and no assertion here should be read as evidence that they were.
"""
import os
import shlex
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import farming, lean  # noqa: E402
from omnidroid.qemu_proc import MODES  # noqa: E402

FPS = "DFIntTaskSchedulerTargetFps"


def _flat(steps):
    return " ".join(" ".join(s) for s in steps)


def _first(steps, needle):
    """Index of the first step whose text contains `needle`, or -1."""
    for i, step in enumerate(steps):
        if needle in " ".join(step):
            return i
    return -1


class TheMinimalProfile(unittest.TestCase):
    def test_minimal_is_a_quality_name(self):
        self.assertIn("minimal", lean.QUALITY_PROFILES)
        self.assertIs(lean.app_settings_for("minimal"),
                      lean.MINIMAL_APP_SETTINGS)

    def test_it_ticks_slower_than_low(self):
        """The tick target caps the WHOLE engine loop, which is why it was
        worth 36% -> 18.8% host CPU per instance on its own (2026-08-05). It
        is the only key in the farming profile that still had room in it."""
        self.assertLess(lean.MINIMAL_APP_SETTINGS[FPS],
                        lean.CLIENT_APP_SETTINGS[FPS])

    def test_it_is_low_plus_a_lower_tick_and_nothing_else(self):
        """`minimal` must be 'everything low does, and less'.

        Asserted as a DERIVATION rather than a value list: a `minimal` that
        drifted heavier than `low` is an inversion nothing downstream would
        notice, because the engine just installs whichever dict it is handed.
        """
        differs = {k for k in set(lean.MINIMAL_APP_SETTINGS)
                   | set(lean.CLIENT_APP_SETTINGS)
                   if lean.MINIMAL_APP_SETTINGS.get(k)
                   != lean.CLIENT_APP_SETTINGS.get(k)}
        self.assertEqual(differs, {FPS})

    def test_it_is_a_separate_dict(self):
        # Derived, not aliased: mutating one must not edit the other.
        self.assertIsNot(lean.MINIMAL_APP_SETTINGS, lean.CLIENT_APP_SETTINGS)

    def test_it_invents_no_new_settings_keys(self):
        """Stay inside the vocabulary this file already justifies. An FFlag
        name nobody can check is indistinguishable from a typo: Roblox ignores
        both in silence."""
        self.assertTrue(set(lean.MINIMAL_APP_SETTINGS)
                        <= set(lean.CLIENT_APP_SETTINGS))

    def test_it_serialises(self):
        import json
        body = lean.client_settings_json(lean.MINIMAL_APP_SETTINGS)
        self.assertEqual(json.loads(body), lean.MINIMAL_APP_SETTINGS)


class TheMinimalPanel(unittest.TestCase):
    def test_display_for_quality_shrinks_only_minimal(self):
        self.assertEqual(lean.display_for_quality("minimal"),
                         lean.MINIMAL_DISPLAY)
        self.assertEqual(lean.display_for_quality("low"), lean.FARMING_DISPLAY)

    def test_an_unknown_quality_keeps_the_default_panel(self):
        """A bad name costs the boot its render floor and nothing else — this
        runs from the POST-BOOT squeeze, where raising is not an option."""
        self.assertEqual(lean.display_for_quality("ultra"),
                         lean.FARMING_DISPLAY)
        self.assertEqual(lean.display_for_quality(None), lean.FARMING_DISPLAY)

    def test_the_caller_s_own_display_is_the_default(self):
        mine = (640, 360, 100)
        self.assertEqual(lean.display_for_quality("low", mine), mine)

    def test_native_wins_over_minimal(self):
        """None is a MEANINGFUL value ('leave the base's resolution alone'),
        the same trap resolve_mode's `guest_display` sentinel documents."""
        self.assertIsNone(lean.display_for_quality("minimal",
                                                   lean.NATIVE_DISPLAY))

    def test_it_is_actually_smaller(self):
        w, h, dpi = lean.MINIMAL_DISPLAY
        fw, fh, fdpi = lean.FARMING_DISPLAY
        self.assertLess(w * h, fw * fh)
        self.assertLess(dpi, fdpi)

    def test_the_density_floor_is_respected(self):
        """`wm density` below ~60 has not been verified, and a collapsed UI is
        indistinguishable from a hung client from outside the guest: adb up,
        process alive, PSS flat, squeeze reporting success."""
        self.assertGreaterEqual(lean.MINIMAL_DISPLAY[2], 60)


class TheRenderStep(unittest.TestCase):
    def test_it_is_a_named_step(self):
        self.assertIn(farming.STEP_RENDER, farming.STEP_NAMES)

    def test_it_is_skippable_by_name(self):
        """Bisectability is not a convenience: this sequence has twice been
        what stopped Roblox running on the x86 base, and editing farming.py to
        find out which lever did it makes every attempt a different build."""
        self.assertIn(farming.STEP_RENDER, farming.parse_skip("render"))
        self.assertIn(farming.STEP_RENDER,
                      farming.parse_skip("doze, RENDER ,zram"))
        self.assertNotIn(farming.STEP_RENDER, farming.parse_skip("renderer"))

    def test_it_sits_with_the_display_step_not_after_the_trim(self):
        """Ordering rationale (farming.build_squeeze_sequence): the floor is a
        SECOND, smaller panel, so it belongs in the same slot as the first
        one. Everything between the two would otherwise size its buffers for a
        panel this boot is about to abandon."""
        steps = self._minimal()
        render = _first(steps, "transition_animation_scale")
        self.assertGreater(render, _first(steps, "wm size"))
        self.assertLess(render, _first(steps, "pm disable-user"))

    def test_it_applies_the_smaller_panel_on_minimal(self):
        steps = self._minimal()
        self.assertEqual(steps[0], ["shell", "wm", "size", "480x270"])
        self.assertIn(["shell", "wm", "size", "320x180"], steps)
        self.assertIn(["shell", "wm", "density", "60"], steps)

    def test_a_normal_farming_boot_gets_no_second_resize(self):
        """`low` already IS the mode's panel, so the floor must emit nothing
        rather than re-sending the same size."""
        steps = farming.build_squeeze_sequence(dict(MODES["farming"]))
        sizes = [s for s in steps if s[:3] == ["shell", "wm", "size"]]
        self.assertEqual(sizes, [["shell", "wm", "size", "480x270"]])

    def test_it_zeroes_the_two_scales_quiesce_does_not(self):
        flat = _flat(self._minimal())
        self.assertIn("transition_animation_scale 0", flat)
        self.assertIn("animator_duration_scale 0", flat)

    def test_it_does_not_restate_window_animation_scale(self):
        """The quiesce step already sets it. Two steps writing one setting
        would make a bisect of either one lie about what it changed."""
        flat = _flat(self._minimal())
        self.assertEqual(flat.count("window_animation_scale"), 1)

    def test_skipping_it_leaves_the_mode_s_own_panel(self):
        steps = self._minimal(skip=("render",))
        self.assertEqual(steps[0], ["shell", "wm", "size", "480x270"])
        flat = _flat(steps)
        self.assertNotIn("320x180", flat)
        self.assertNotIn("animator_duration_scale", flat)

    def test_skipping_display_means_no_panel_change_at_all(self):
        """`OMNI_FARM_SKIP=display` means 'do not touch the panel'. A render
        floor that resized anyway would make that bisect prove nothing."""
        flat = _flat(self._minimal(skip=("display",)))
        self.assertNotIn("wm size", flat)
        # ...but the rest of the floor is a different lever and still runs.
        self.assertIn("animator_duration_scale", flat)

    def test_it_sets_no_property(self):
        """Every compositing property in lean.py is `ro.*` and init freezes
        those once set, so a setprop here is a SILENT no-op that reports
        success. `ro.config.low_ram` is worse than inert — baked, it bricks
        the guest (lean.py:120-127)."""
        flat = _flat(self._minimal())
        self.assertNotIn("ro.config.low_ram", flat)
        for prop in lean.LOW_RAM_PROPS:
            if prop.startswith("ro."):
                self.assertNotIn(prop, flat)

    def test_it_never_blanks_the_screen(self):
        """A blanked farming instance stops rendering and stops EARNING,
        unnoticed. awake.py exists to prevent exactly this."""
        flat = _flat(self._minimal())
        for forbidden in ("svc power", "KEYCODE_POWER", "input keyevent 26"):
            self.assertNotIn(forbidden, flat)

    def test_the_game_and_systemui_survive_the_floor(self):
        flat = _flat(self._minimal())
        self.assertNotIn("disable-user --user 0 com.android.systemui", flat)
        self.assertNotIn(f"disable-user --user 0 {farming.GAME_PKG}", flat)

    def test_quality_falls_back_to_the_mode_s_own(self):
        """A caller that does not care still gets the right thing: farming's
        mode entry carries `quality`, so the sequence can resolve it."""
        mode = dict(MODES["farming"], quality="minimal")
        self.assertIn("320x180", _flat(farming.build_squeeze_sequence(mode)))

    def test_it_is_deterministic(self):
        self.assertEqual(self._minimal(), self._minimal())

    @staticmethod
    def _minimal(skip=()):
        return farming.build_squeeze_sequence(dict(MODES["farming"]),
                                              skip=skip, quality="minimal")


class QuotingSurvivesTheRenderFloor(unittest.TestCase):
    """`adb shell` joins argv and lets the GUEST shell re-parse it, so an
    unquoted `a; b` runs `sh -c a` and then `b` — and every script here ends
    in `; true`, so the step still reports success. That failure has bitten
    this project twice. test_squeeze_quoting.py pins the default sequence;
    this pins the sequences the render floor produces."""

    def _sequences(self):
        mode = dict(MODES["farming"])
        return {
            "minimal": farming.build_squeeze_sequence(mode, quality="minimal"),
            "low": farming.build_squeeze_sequence(mode, quality="low"),
            "skip-display": farming.build_squeeze_sequence(
                mode, skip=("display",), quality="minimal"),
        }

    def test_every_multi_command_script_is_quoted(self):
        for name, steps in self._sequences().items():
            multi = [s for s in steps if s[:3] == ["shell", "sh", "-c"]]
            self.assertTrue(multi, name)
            for step in multi:
                self.assertEqual(len(step), 4, f"{name}: {step}")
                # It must survive a join + re-parse as exactly ONE word.
                reparsed = shlex.split(" ".join(step[1:]))
                self.assertEqual(reparsed[:2], ["sh", "-c"], f"{name}: {step}")
                self.assertEqual(len(reparsed), 3,
                                 f"{name}: the guest shell would split "
                                 f"this into {len(reparsed)} words: {step}")
                self.assertEqual(reparsed[2], shlex.split(step[3])[0],
                                 f"{name}: {step}")

    def test_the_render_script_goes_through_sh(self):
        steps = self._sequences()["minimal"]
        render = [s for s in steps
                  if "transition_animation_scale" in " ".join(s)]
        self.assertEqual(len(render), 1)
        self.assertEqual(render[0][:3], ["shell", "sh", "-c"])
        self.assertEqual(render[0], farming.sh(shlex.split(render[0][3])[0]))

    def test_every_step_still_fails_open(self):
        """A squeeze is an optimization, never a precondition."""
        for name, steps in self._sequences().items():
            for step in steps:
                if step[:3] == ["shell", "sh", "-c"]:
                    self.assertTrue(step[3].rstrip("'").rstrip()
                                    .endswith("true"), f"{name}: {step}")

    def test_bare_argv_steps_acquire_no_quotes(self):
        for name, steps in self._sequences().items():
            for step in steps:
                if step[:3] == ["shell", "sh", "-c"]:
                    continue
                for word in step:
                    self.assertNotIn("'", word, f"{name}: {step}")


class ThePerGameMemoryFloor(unittest.TestCase):
    """`mem` is a property of the GAME, not a tuning constant.

    MEASURED 2026-08-16 on PS99 (place 8737899170): at the shipped 2048 the
    client was OOM-killed three times in a row with no squeeze and no balloon
    in the way; at 3072 the same launch reached the world and stayed there.
    """

    def test_ps99_needs_3072(self):
        self.assertEqual(lean.guest_mem_floor_mb("8737899170", 2048), 3072)

    def test_an_unmeasured_place_gets_the_default(self):
        """Deliberate: this table records measurements. Inventing a floor for
        a place nobody has run would make it read like a table of facts."""
        self.assertEqual(lean.guest_mem_floor_mb("1234567890", 2048), 2048)
        self.assertEqual(lean.guest_mem_floor_mb("1234567890", 4096), 4096)

    def test_no_place_at_all_is_not_an_error(self):
        """The launch path can genuinely not know the place (no `--place`),
        and that must mean 'no measurement applies', not a crash on boot."""
        for missing in (None, "", "   ", 0):
            self.assertEqual(lean.guest_mem_floor_mb(missing, 2048), 2048)

    def test_an_int_place_id_works_too(self):
        """Place ids arrive as argv strings, run.json numbers and app
        settings; normalising on the way in beats hoping callers agree."""
        self.assertEqual(lean.guest_mem_floor_mb(8737899170, 2048), 3072)

    def test_it_only_ever_raises(self):
        """A measured place started with a bigger `--mem` keeps it. That max()
        is what makes this safe to wire in unconditionally."""
        self.assertEqual(lean.guest_mem_floor_mb("8737899170", 4096), 4096)

    def test_the_default_matches_the_shipped_mode(self):
        self.assertEqual(lean.GUEST_MEM_FLOOR_DEFAULT_MB,
                         MODES["farming"]["mem"])

    def test_the_table_is_keyed_by_string(self):
        for key, mb in lean.GUEST_MEM_FLOOR_MB.items():
            self.assertIsInstance(key, str)
            self.assertTrue(key.isdigit(), key)
            self.assertGreater(mb, 0)


if __name__ == "__main__":
    unittest.main()
