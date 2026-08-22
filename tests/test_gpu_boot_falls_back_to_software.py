#!/usr/bin/env python3
"""A GPU that cannot carry the guest must cost the GPU, not the boot.

    python3 -m pytest tests/test_gpu_boot_falls_back_to_software.py -q

THE MEASUREMENT THIS COMES FROM (a user's PC, 2026-08-22, fresh install).
Slot `_pool0` recorded a farming boot with:

    "display_kind": "gl-window", "gpu": "gl", "native_window": true,
    "window_hidden": true

...and QEMU was gone 13.7 s later with a ZERO-BYTE qemu.log. The window was
real -- `window_hidden` is only ever true when `find_window` actually returned a
handle -- so QEMU got through display init and died afterwards, which is where
virglrenderer starts doing real work through the host's GL driver.

WHY THE HOST WAS NEVER ASKED. `default_display()` decides `tier: "gl"` from two
things, and NEITHER is a fact about the machine it is running on:

    has_gui = _host_has_gui()          # hardcoded True on Windows
    GL_GPU_DEVICE in qemu_device_help  # what OUR shipped binary was compiled
                                       # with -- identical on every PC

So every Windows host is told it can render on the GPU, because our own binary
can. That is the same mistake `accelprobe.py` was written to correct one layer
down, and this file's own docstring already says the stakes: "a false positive
costs a BOOT". It did.

Worse, this was a FARMING boot -- the density profile, 5 fps tick, floor
quality, nobody watching -- where `gpu_policy`'s own docstring says the GPU
"buys it almost nothing". It took the riskiest display path on unknown hardware
to buy nothing.

THE RULE PINNED HERE: a boot whose QEMU DIED, on a boot that was using the GPU,
is retried once with the GPU off. A stall is not retried (the display is not
implicated -- the process is alive and stuck). A software boot is not retried
(there is nothing left to give up, and retrying forever is its own bug).

This cannot fix a dead boot whose cause is elsewhere -- a full disk will kill
the software retry too. It is not meant to: it is meant to stop ONE known cause
from being fatal, and to make the failure that remains say so honestly.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import bootwait  # noqa: E402
from omnidroid import engine  # noqa: E402
from omnidroid.qemu_proc import GPU_OFF  # noqa: E402


DEAD = bootwait.BootOutcome(False, bootwait.QEMU_EXITED, "qemu.log is EMPTY")
STALLED = bootwait.BootOutcome(False, bootwait.STALLED, "nothing moved")
OK = bootwait.BootOutcome(True, bootwait.BOOTED)


class WhenTheDisplayIsImplicated(unittest.TestCase):
    """The policy, on its own. This is where the judgement lives."""

    def test_a_dead_qemu_on_a_gpu_boot_implicates_the_display(self):
        self.assertTrue(engine._display_is_implicated(DEAD, used_gpu=True))

    def test_a_dead_qemu_on_a_software_boot_does_not(self):
        """Nothing left to give up. Retrying would be a loop, not a fallback."""
        self.assertFalse(engine._display_is_implicated(DEAD, used_gpu=False))

    def test_a_stall_does_not_implicate_the_display(self):
        """A stalled guest is ALIVE and stuck. Killing it to retry in software
        would throw away a boot that might still be going."""
        self.assertFalse(engine._display_is_implicated(STALLED, used_gpu=True))

    def test_a_successful_boot_implicates_nothing(self):
        self.assertFalse(engine._display_is_implicated(OK, used_gpu=True))

    def test_a_bare_false_from_an_unconverted_caller_is_not_retried(self):
        """Without a reason there is no evidence the display did anything."""
        self.assertFalse(engine._display_is_implicated(False, used_gpu=True))


class TheFallbackItself(unittest.TestCase):
    """`_boot_with_display_fallback` takes an `attempt(gpu)` callable so the
    decision can be tested without booting a virtual machine."""

    def test_a_boot_that_works_is_attempted_exactly_once(self):
        asked = []

        def attempt(gpu):
            asked.append(gpu)
            return OK

        out = engine._boot_with_display_fallback(
            "t", attempt, used_gpu=lambda: True)
        self.assertTrue(out)
        self.assertEqual(asked, [None])

    def test_a_dead_gpu_boot_is_retried_with_the_gpu_off(self):
        asked = []

        def attempt(gpu):
            asked.append(gpu)
            return OK if gpu == GPU_OFF else DEAD

        out = engine._boot_with_display_fallback(
            "t", attempt, used_gpu=lambda: True)
        self.assertTrue(out, "the software retry booted but was not reported")
        self.assertEqual(asked, [None, GPU_OFF])

    def test_it_retries_only_once(self):
        """Two deaths is not a display problem. Report the second one."""
        asked = []

        def attempt(gpu):
            asked.append(gpu)
            return DEAD

        out = engine._boot_with_display_fallback(
            "t", attempt, used_gpu=lambda: True)
        self.assertFalse(out)
        self.assertEqual(asked, [None, GPU_OFF])

    def test_the_failure_reported_is_the_retrys_own(self):
        """Not the first attempt's -- the software boot is the one that
        establishes there is something else wrong."""
        second = bootwait.BootOutcome(False, bootwait.QEMU_EXITED,
                                      "No space left on device")

        def attempt(gpu):
            return DEAD if gpu is None else second

        out = engine._boot_with_display_fallback(
            "t", attempt, used_gpu=lambda: True)
        self.assertIn("No space left", out.detail)

    def test_a_software_boot_that_dies_is_never_retried(self):
        asked = []

        def attempt(gpu):
            asked.append(gpu)
            return DEAD

        engine._boot_with_display_fallback("t", attempt, used_gpu=lambda: False)
        self.assertEqual(asked, [None])

    def test_a_stalled_gpu_boot_is_never_retried(self):
        asked = []

        def attempt(gpu):
            asked.append(gpu)
            return STALLED

        engine._boot_with_display_fallback("t", attempt, used_gpu=lambda: True)
        self.assertEqual(asked, [None])


class TheGpuOverrideReachesTheNextSpawn(unittest.TestCase):
    """`gpu_policy` reads OMNI_GPU FIRST -- ahead of the config and the mode --
    so a retry that only edited the mode would be silently ignored on exactly
    the launches the app makes (it sets OMNI_GPU from its own --gpu argument).
    The override has to go where the policy actually looks."""

    def setUp(self):
        self.before = os.environ.get("OMNI_GPU")

    def tearDown(self):
        if self.before is None:
            os.environ.pop("OMNI_GPU", None)
        else:
            os.environ["OMNI_GPU"] = self.before

    def test_the_policy_sees_it(self):
        os.environ["OMNI_GPU"] = "auto"
        with engine._forced_gpu(GPU_OFF):
            from omnidroid.qemu_proc import gpu_policy
            self.assertEqual(gpu_policy({}, {"gpu": "auto"}), GPU_OFF)

    def test_it_is_put_back_afterwards(self):
        os.environ["OMNI_GPU"] = "auto"
        with engine._forced_gpu(GPU_OFF):
            pass
        self.assertEqual(os.environ["OMNI_GPU"], "auto")

    def test_it_is_put_back_even_when_the_boot_raises(self):
        os.environ["OMNI_GPU"] = "auto"
        with self.assertRaises(RuntimeError):
            with engine._forced_gpu(GPU_OFF):
                raise RuntimeError("boom")
        self.assertEqual(os.environ["OMNI_GPU"], "auto")

    def test_an_unset_variable_is_left_unset(self):
        os.environ.pop("OMNI_GPU", None)
        with engine._forced_gpu(GPU_OFF):
            pass
        self.assertNotIn("OMNI_GPU", os.environ)


if __name__ == "__main__":
    unittest.main()
