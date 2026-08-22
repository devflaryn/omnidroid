#!/usr/bin/env python3
"""The display fallback, through _ensure_booted itself.

    python3 -m pytest tests/test_display_fallback_end_to_end.py -q

test_gpu_boot_falls_back_to_software.py pins the POLICY in isolation. This
pins the WIRING: that a launch whose QEMU died on a GPU boot really does spawn
a second one, that an ordinary boot still spawns exactly one, and -- the
regression this file exists for as much as the feature -- that an ordinary
SOFTWARE boot still bakes its warm-cache entry.

That last one is not hypothetical. The first version of the wiring decided
"the fallback ran" by asking "is this boot NOT on the GPU", which is equally
true of every ordinary software boot, and so switched baking off for all of
them. test_warm_boot_policy caught it. The guard keys on the retry actually
having happened now, and this pins that directly rather than by side effect.

Reuses test_warm_boot_policy's `_StubbedBoot`: every collaborator (QEMU, adb,
QMP, the warm cache, the disk) is already stubbed there, and duplicating it
would be a second copy to keep in step with _ensure_booted.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from omnidroid import bootwait  # noqa: E402
from omnidroid import engine  # noqa: E402
from omnidroid.qemu_proc import GPU_OFF  # noqa: E402
from test_warm_boot_policy import _StubbedBoot, _acct_x86, _cfg_x86  # noqa: E402


DEAD = bootwait.BootOutcome(False, bootwait.QEMU_EXITED, "qemu.log is EMPTY")
STALLED = bootwait.BootOutcome(False, bootwait.STALLED, "nothing moved")
OK = bootwait.BootOutcome(True, bootwait.BOOTED)


def _boot(waits, used_gpu=True):
    """Run _ensure_booted with wait_for_boot yielding `waits` in order."""
    with mock.patch.object(engine, "_boot_used_gpu", return_value=used_gpu):
        with _StubbedBoot(
                wait_for_boot=mock.MagicMock(side_effect=list(waits))) as b:
            ok, _first = b.run(acct=_acct_x86(), cfg=_cfg_x86())
    return ok, b


class TheRetryHappensForReal(unittest.TestCase):
    def test_a_dead_gpu_boot_spawns_a_second_qemu_and_comes_up(self):
        ok, b = _boot([DEAD, OK])
        self.assertTrue(ok, "the software retry booted but was reported failed")
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 2,
                         "the fallback did not respawn QEMU")

    def test_the_retry_runs_with_the_gpu_switched_off(self):
        """What each spawn saw in OMNI_GPU, which is where gpu_policy looks
        first. The first spawn must see the launch's own setting and the
        second must see `off`, or the retry is a second identical boot."""
        seen = []

        def _spy(*_a, **_kw):
            seen.append(os.environ.get("OMNI_GPU"))
            return 4321

        with mock.patch.object(engine, "_boot_used_gpu", return_value=True):
            with _StubbedBoot(
                    wait_for_boot=mock.MagicMock(side_effect=[DEAD, OK]),
                    spawn_qemu=mock.MagicMock(side_effect=_spy)) as b:
                b.run(acct=_acct_x86(), cfg=_cfg_x86())
        self.assertEqual(len(seen), 2, "expected a first boot and a retry")
        self.assertNotEqual(seen[0], GPU_OFF, "the FIRST boot gave up the GPU")
        self.assertEqual(seen[1], GPU_OFF, "the retry did not force the GPU off")

    def test_the_dead_qemu_is_halted_before_the_retry_takes_its_ports(self):
        """The dead process's run.json and its adb/QMP/VNC ports have to be
        released, or the retry collides with the corpse of the first attempt."""
        order = []
        with mock.patch.object(engine, "_boot_used_gpu", return_value=True), \
             mock.patch.object(engine, "_halt_qemu",
                               side_effect=lambda *a, **k: order.append("halt")):
            with _StubbedBoot(
                    wait_for_boot=mock.MagicMock(side_effect=[DEAD, OK]),
                    spawn_qemu=mock.MagicMock(
                        side_effect=lambda *a, **k: order.append("spawn"))) as b:
                b.run(acct=_acct_x86(), cfg=_cfg_x86())
        self.assertEqual(order, ["spawn", "halt", "spawn"])

    def test_two_deaths_are_reported_rather_than_retried_forever(self):
        ok, b = _boot([DEAD, DEAD])
        self.assertFalse(ok)
        self.assertEqual(ok.reason, bootwait.QEMU_EXITED)
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 2)


class OrdinaryBootsAreUntouched(unittest.TestCase):
    def test_a_boot_that_works_spawns_exactly_one_qemu(self):
        ok, b = _boot([OK])
        self.assertTrue(ok)
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 1)

    def test_a_stalled_boot_is_not_respawned(self):
        ok, b = _boot([STALLED])
        self.assertFalse(ok)
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 1)

    def test_a_software_boot_that_dies_is_not_respawned(self):
        ok, b = _boot([DEAD], used_gpu=False)
        self.assertFalse(ok)
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 1)

    def test_an_ordinary_software_boot_still_bakes_its_warm_entry(self):
        """THE REGRESSION. A software boot is not a fallback boot, and must
        keep the warm cache it has always had."""
        _ok, b = _boot([OK], used_gpu=False)
        b.mocks["bake_entry"].assert_called()

    def test_a_fallback_boot_does_not_bake(self):
        """A degraded boot must not be cached: the warm key does not record the
        display, so restoring it later would render in software silently."""
        _ok, b = _boot([DEAD, OK])
        b.mocks["bake_entry"].assert_not_called()


if __name__ == "__main__":
    unittest.main()
