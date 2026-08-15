#!/usr/bin/env python3
"""farming mode: the memory model, and playable/DEFAULT left alone.

    python3 tests/test_farming_mode.py

These assertions changed on 2026-08-05 and the reason matters. farming used
to be {"mem": 512, "smp": 2}, and this test asserted mem <= 1024 to lock that
in. The number was never booted: a 512 MB arm64 instance was measured twice
(5 min and 7 min) and never reached adbd at all, and 1024 boots but idles with
143 MB available, which the ~533 MB game does not fit into.

The fix was not a bigger number, it was the right variable. `mem` is the
guest's address space and has to be big enough to run; `balloon` is the
post-boot cap on what the HOST actually pays. So this file now asserts the
relationship (a real host cap, well under the address space) instead of an
absolute size that encoded a guess.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import lean  # noqa: E402


class FarmingMode(unittest.TestCase):
    def test_farming_boots_and_caps(self):
        self.assertIn("farming", omni.MODES)
        f = omni.MODES["farming"]
        # Enough address space to actually boot Android + the game.
        self.assertGreaterEqual(f["mem"], 2048)
        # One vCPU: 50+ instances means 50+ vCPU threads.
        self.assertEqual(f["smp"], 1)
        # The host cap is what makes it a farming mode, and it must be a real
        # reduction rather than a restatement of mem.
        self.assertTrue(f["balloon"])
        self.assertLess(f["balloon"], f["mem"])

    def test_farming_shrinks_the_display(self):
        self.assertIsNotNone(omni.MODES["farming"]["display"])

    def test_farming_keeps_input_devices(self):
        """Deliberate: an instance with no input device cannot dismiss an
        on-screen dialog, and a rebooted guest comes up with adb
        unauthorized behind exactly such a dialog — unreachable from adb or
        QMP. See the MODES comment in qemu_proc.py."""
        self.assertTrue(omni.MODES["farming"]["usb"])

    def test_resolve_farming(self):
        m = omni.resolve_mode({}, "farming")
        self.assertEqual(m["name"], "farming")
        for k in ("mem", "smp", "balloon"):
            self.assertEqual(m[k], omni.MODES["farming"][k])

    def test_resolve_honours_mem_and_balloon_overrides(self):
        m = omni.resolve_mode({}, "farming", mem=3072, balloon=768)
        self.assertEqual(m["mem"], 3072)
        self.assertEqual(m["balloon"], 768)

    def test_balloon_zero_disables(self):
        """argparse cannot say 'absent vs zero', so 0 must mean 'no cap'."""
        self.assertIsNone(omni.resolve_mode({}, "farming", balloon=0)["balloon"])

    def test_resolve_does_not_mutate_the_registry(self):
        omni.resolve_mode({}, "farming", mem=9999, balloon=1)
        self.assertNotEqual(omni.MODES["farming"]["mem"], 9999)

    def test_gaming_is_untouched_and_uncapped(self):
        self.assertEqual(omni.DEFAULT_MODE, "gaming")
        p = omni.MODES["gaming"]
        self.assertEqual((p["mem"], p["smp"]), (4096, 4))
        # A gaming instance is one a human is looking at and touching.
        self.assertIsNone(p["balloon"])
        self.assertTrue(p["usb"])
        self.assertIsNone(p["display"])


class TheMemoryFloorIsPerGame(unittest.TestCase):
    """`MODES["farming"]["mem"]` is a DEFAULT, not a constant.

    The mode's 2048 is right for a light place — it boots Android plus a
    Roblox client with room to spare, and every balloon figure in FOOTPRINT.md
    was taken against it. It is wrong for a heavy one, and no single number
    can be right for both, because the variable is the game's world.

    MEASURED 2026-08-16 on Pet Simulator 99 (place 8737899170), x86 base,
    in-world: game PSS climbed 1018 -> 1528 MB, and at 2048 the client was
    OOM-killed three times in a row with no squeeze and no balloon in the way.
    At 3072 the same launch reached the world and stayed there. See
    CHANGELOG 2026-08-16 and FOOTPRINT.md.

    The table lives in lean.py rather than in the mode entry because a mode is
    a HOST sizing policy and this is a property of a GAME; folding one into
    the other is how "farming needs 3072" would end up shipped as a fleet-wide
    default that costs 1 GB per instance on every light place.
    """

    def test_the_mode_default_is_what_the_floor_falls_back_to(self):
        self.assertEqual(lean.GUEST_MEM_FLOOR_DEFAULT_MB,
                         omni.MODES["farming"]["mem"])

    def test_ps99_raises_the_floor(self):
        self.assertEqual(
            lean.guest_mem_floor_mb("8737899170",
                                    omni.MODES["farming"]["mem"]), 3072)

    def test_an_unmeasured_place_gets_the_mode_default_and_may_oom(self):
        """Deliberate. An invented floor for a place nobody has run would make
        a table of guesses read like a table of facts; the symptom to look for
        is `has died: fg TOP` / `mem-pressure-event` while the client loads."""
        self.assertEqual(
            lean.guest_mem_floor_mb("999", omni.MODES["farming"]["mem"]),
            omni.MODES["farming"]["mem"])

    def test_no_place_is_not_a_boot_failure(self):
        """A launch can genuinely not know the place (no `--place`)."""
        self.assertEqual(lean.guest_mem_floor_mb(None, 2048), 2048)

    def test_an_explicit_mem_is_never_lowered(self):
        """`--mem 4096 --place <ps99>` keeps 4096: the floor only ever raises,
        which is what makes it safe to apply unconditionally."""
        self.assertEqual(lean.guest_mem_floor_mb("8737899170", 4096), 4096)


if __name__ == "__main__":
    unittest.main()
