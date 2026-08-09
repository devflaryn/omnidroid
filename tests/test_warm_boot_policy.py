#!/usr/bin/env python3
"""When may a launch use the warm cache at all?

    python3 -m pytest tests/test_warm_boot_policy.py -q

The policy is the whole safety story, so it is a pure function tested without
QEMU under it:
  * a --debug boot changes device topology (devkit vdc), so it must never
    read OR write the cache;
  * an entry already backing a running instance must not be restored a second
    time -- the second concurrent restore lands `offline` on adb (spec 8b).
"""
import contextlib
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402
from omnidroid import runtime as runtime_mod  # noqa: E402
from omnidroid import warmboot, warmcache  # noqa: E402


class WarmPolicy(unittest.TestCase):
    def test_debug_boots_never_touch_the_cache(self):
        self.assertFalse(engine._warm_cache_allowed(debug=True, in_use=set(),
                                                    key="k"))

    def test_a_normal_boot_may_use_the_cache(self):
        self.assertTrue(engine._warm_cache_allowed(debug=False, in_use=set(),
                                                   key="k"))

    def test_an_entry_already_in_use_is_refused(self):
        # Interim rule: siblings cold-boot until the adb blocker is root-caused.
        self.assertFalse(engine._warm_cache_allowed(debug=False,
                                                    in_use={"k"}, key="k"))

    def test_a_different_entry_being_in_use_is_irrelevant(self):
        self.assertTrue(engine._warm_cache_allowed(debug=False,
                                                   in_use={"other"}, key="k"))

    def test_no_key_means_no_cache(self):
        self.assertFalse(engine._warm_cache_allowed(debug=False, in_use=set(),
                                                    key=None))


def _acct(base="arm", first_boot_done=True, debug=False):
    return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
            "vnc_port": 18001, "base": base, "ephemeral": True,
            "first_boot_done": first_boot_done, "debug": debug}


def _cfg():
    return {"images_dir": "/tmp/omni-warm-boot-policy-images",
            "bases": {"arm": {"type": "arm-uefi", "version": 3,
                              "system": "sys.qcow2", "data": "data.qcow2",
                              "efivars": "efivars.fd"}}}


class _StubbedBoot:
    """Runs _ensure_booted with every warm-cache collaborator stubbed out --
    no real QEMU process, QMP socket, adb, or disk I/O. Each test overrides
    only the specific behaviors (`overrides`) it cares about; everything
    else defaults to "a boring, successful, non-caching boot" so a test that
    doesn't care about e.g. the bake still gets one that doesn't explode.

    `qmp` is patched to the SAME mock on both `engine.qmp` (the name the
    post-bake-failure resume path calls directly) and `qemu_proc.qmp` (what
    `_halt_qemu`'s own lazy `from omnidroid.qemu_proc import qmp` resolves
    to at call time) so a single mock sees every QMP call regardless of
    which of the two import paths made it.
    """

    def __init__(self, **overrides):
        self.stack = contextlib.ExitStack()
        self.mocks = {}
        self.overrides = overrides

    def _mock(self, name, **kw):
        m = self.overrides.get(name, mock.MagicMock(**kw))
        self.mocks[name] = m
        return m

    def __enter__(self):
        qmp_mock = self._mock("qmp")
        specs = [
            (engine, "running_pid", "running_pid", dict(return_value=None)),
            (engine, "pid_alive", "pid_alive", dict(return_value=False)),
            (engine, "spawn_qemu", "spawn_qemu", dict(return_value=4321)),
            (engine, "maybe_start_autocap", "maybe_start_autocap", {}),
            (engine, "wait_for_boot", "wait_for_boot", dict(return_value=True)),
            (engine, "post_boot", "post_boot", {}),
            (engine, "_enforce_hiding", "_enforce_hiding", {}),
            (engine, "assert_kiosk_game", "assert_kiosk_game", {}),
            (engine, "_devkit_activate", "_devkit_activate", {}),
            (engine, "apply_roblox_settings", "apply_roblox_settings", {}),
            (engine, "enable_zram", "enable_zram", {}),
            (engine, "apply_farming_squeeze", "apply_farming_squeeze", {}),
            (engine, "apply_balloon_target", "apply_balloon_target", {}),
            (engine, "apply_gaming_tuning", "apply_gaming_tuning", {}),
            (engine, "_qemu_version", "_qemu_version",
             dict(return_value="9.9.9")),
            (engine, "default_accel", "default_accel",
             dict(return_value="tcg")),
            (engine, "_stage_bake_overlays", "_stage_bake_overlays", {}),
            (runtime_mod, "warm_keys_in_use", "warm_keys_in_use",
             dict(return_value=set())),
            (warmcache, "lookup", "lookup", dict(return_value=None)),
            (warmcache, "has_room", "has_room", dict(return_value=True)),
            (warmcache, "touch", "touch", {}),
            (warmcache, "evict_lru", "evict_lru", {}),
            (warmboot, "restore_into", "restore_into", dict(return_value=True)),
            (warmboot, "resync_guest_clock", "resync_guest_clock", {}),
            (warmboot, "bake_entry", "bake_entry", dict(return_value=False)),
        ]
        for obj, attr, name, kw in specs:
            m = self._mock(name, **kw)
            self.stack.enter_context(mock.patch.object(obj, attr, m))
        # Same mock object under both names qmp resolves through.
        self.stack.enter_context(mock.patch.object(engine, "qmp", qmp_mock))
        self.stack.enter_context(mock.patch.object(qemu_proc, "qmp", qmp_mock))
        # _halt_qemu() sleeps 1s for real between "quit" and a SIGKILL check;
        # skip that delay in every test that goes through it (poisoned-entry
        # fallback, post-bake handoff).
        self.stack.enter_context(mock.patch.object(engine.time, "sleep"))
        return self

    def __exit__(self, *exc):
        self.stack.close()

    def run(self, acct=None, mode_name="hard", **kw):
        acct = acct or _acct()
        return engine._ensure_booted(acct, _cfg(), "t", mode_name=mode_name,
                                     **kw)


class WarmRestoreBranchSelection(unittest.TestCase):
    """The four ways a launch can leave the cache-resolution block: a hit
    that restores, a hit that's poisoned and falls back, a miss that bakes,
    and a debug boot that touches neither. Each collaborator is stubbed so
    these exercise ONLY _ensure_booted's branch selection -- if the restore
    or bake wiring silently broke while every collaborator still returned a
    plausible value, the pre-existing tuning/kiosk tests would not catch it
    (they never touch warmcache/warmboot), but these will.
    """

    def test_a_cache_hit_restores_instead_of_cold_booting(self):
        entry = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            entry, ignore_errors=True))
        with _StubbedBoot(lookup=mock.MagicMock(return_value=entry)) as b:
            ok, first = b.run()
        self.assertTrue(ok)
        self.assertFalse(first)
        b.mocks["restore_into"].assert_called_once()
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 1)
        _, kwargs = b.mocks["spawn_qemu"].call_args
        self.assertEqual(kwargs.get("warm"), entry)
        self.assertNotIn("bake", kwargs)
        b.mocks["bake_entry"].assert_not_called()

    def test_a_restore_that_never_comes_up_discards_the_entry_and_cold_boots(self):
        entry = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            entry, ignore_errors=True))
        wait = mock.MagicMock(side_effect=[False, True])  # restore, then cold
        # has_room=False keeps the fallback a PURE cold boot for this
        # assertion -- whether that fallback itself then also opts to bake
        # is a separate concern, covered by WarmBakeHandoff below.
        with _StubbedBoot(lookup=mock.MagicMock(return_value=entry),
                          wait_for_boot=wait,
                          has_room=mock.MagicMock(return_value=False)) as b:
            ok, first = b.run()
        self.assertTrue(ok)
        self.assertFalse(first)
        # The poisoned entry is gone...
        self.assertFalse(entry.exists())
        # ...and the fallback actually cold-booted (a second spawn_qemu, this
        # one WITHOUT warm= or bake=).
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 2)
        _, second_kwargs = b.mocks["spawn_qemu"].call_args_list[1]
        self.assertNotIn("warm", second_kwargs)
        self.assertNotIn("bake", second_kwargs)
        b.mocks["bake_entry"].assert_not_called()

    def test_a_cache_miss_on_a_non_interactive_boot_attempts_a_bake(self):
        with _StubbedBoot(has_room=mock.MagicMock(return_value=True)) as b:
            ok, first = b.run()
        self.assertTrue(ok)
        self.assertFalse(first)
        b.mocks["_stage_bake_overlays"].assert_called_once()
        b.mocks["bake_entry"].assert_called_once()
        _, kwargs = b.mocks["spawn_qemu"].call_args
        self.assertTrue(kwargs.get("bake"))
        self.assertIsNotNone(kwargs.get("warm_key"))

    def test_a_debug_boot_neither_restores_nor_bakes(self):
        with _StubbedBoot() as b:
            ok, first = b.run(acct=_acct(debug=True), debug=True)
        self.assertTrue(ok)
        self.assertFalse(first)
        b.mocks["lookup"].assert_not_called()
        b.mocks["_stage_bake_overlays"].assert_not_called()
        b.mocks["bake_entry"].assert_not_called()
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 1)
        _, kwargs = b.mocks["spawn_qemu"].call_args
        self.assertNotIn("warm", kwargs)
        self.assertNotIn("bake", kwargs)
        b.mocks["_devkit_activate"].assert_called_once()


class WarmBakeHandoff(unittest.TestCase):
    """What happens after a bake attempt finishes -- success or failure --
    since the brief's own snippet gets the failure side wrong (see the
    task-11 report's deviation (c))."""

    def test_a_successful_bake_reenters_with_no_rebake_and_cannot_bake_again(self):
        with _StubbedBoot(bake_entry=mock.MagicMock(return_value=True)) as b:
            ok, first = b.run()
        self.assertTrue(ok)
        self.assertFalse(first)
        # Exactly one bake across the whole run, including the recursive
        # re-entry -- if _no_rebake didn't stick, this would be >= 2 and the
        # bake -> restore -> discard -> bake loop the brief warns about
        # would be live.
        self.assertEqual(b.mocks["bake_entry"].call_count, 1)
        # _halt_qemu ran between the bake and the recursive restore attempt.
        self.assertIn(mock.call(mock.ANY, "quit"),
                      b.mocks["qmp"].call_args_list)

    def test_a_failed_bake_resumes_the_guest_and_still_runs_post_boot(self):
        with _StubbedBoot(bake_entry=mock.MagicMock(return_value=False)) as b:
            ok, first = b.run()
        self.assertTrue(ok)
        self.assertFalse(first)
        # Deviation (c): a failed bake must resume the paused guest...
        self.assertIn(mock.call(mock.ANY, "cont"),
                      b.mocks["qmp"].call_args_list)
        # ...and fall through to the SAME pipeline an unbaked boot gets, not
        # return early and skip it.
        b.mocks["post_boot"].assert_called_once()
        b.mocks["_enforce_hiding"].assert_called_once()
        b.mocks["assert_kiosk_game"].assert_called_once()


if __name__ == "__main__":
    unittest.main()
