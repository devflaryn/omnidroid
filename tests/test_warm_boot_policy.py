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
    """Every call names an ACCELERATOR, because one of the refusal reasons is
    now the accelerator itself and these assertions must not depend on
    whichever host runs them. `kvm` stands for "can migrate"."""

    def test_debug_boots_never_touch_the_cache(self):
        self.assertFalse(engine._warm_cache_allowed(debug=True, in_use=set(),
                                                    key="k", accel="kvm"))

    def test_a_normal_boot_may_use_the_cache(self):
        self.assertTrue(engine._warm_cache_allowed(debug=False, in_use=set(),
                                                   key="k", accel="kvm"))

    def test_an_entry_already_in_use_is_refused(self):
        # Interim rule: siblings cold-boot until the adb blocker is root-caused.
        self.assertFalse(engine._warm_cache_allowed(debug=False,
                                                    in_use={"k"}, key="k",
                                                    accel="kvm"))

    def test_a_different_entry_being_in_use_is_irrelevant(self):
        self.assertTrue(engine._warm_cache_allowed(debug=False,
                                                   in_use={"other"}, key="k",
                                                   accel="kvm"))

    def test_no_key_means_no_cache(self):
        self.assertFalse(engine._warm_cache_allowed(debug=False, in_use=set(),
                                                    key=None, accel="kvm"))

    def test_an_accelerator_that_cannot_migrate_is_refused(self):
        """The one that matters on Windows.

        QEMU/WHPX registers a migration blocker at CPU realize time, so a bake
        cannot succeed there however much disk, however good the transport.
        MEASURED against a real booted instance:

            warm bake failed (migration State blocked due to non-migratable
            CPUID feature support,dirty memory tracking support, and
            XSAVE/XRSTOR support)

        Without this gate every launch pays a guest stop, two staged qcow2
        overlays and a refused migration, forever, for a cache that can never
        hold anything."""
        self.assertFalse(engine._warm_cache_allowed(
            debug=False, in_use=set(), key="k",
            accel="whpx,kernel-irqchip=off"))
        for good in ("kvm", "hvf", "tcg"):
            self.assertTrue(engine._warm_cache_allowed(
                debug=False, in_use=set(), key="k", accel=good), good)


def _acct(base="arm", first_boot_done=True, debug=False):
    return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
            "vnc_port": 18001, "base": base, "ephemeral": True,
            "first_boot_done": first_boot_done, "debug": debug}


def _cfg():
    return {"images_dir": "/tmp/omni-warm-boot-policy-images",
            "bases": {"arm": {"type": "arm-uefi", "version": 3,
                              "system": "sys.qcow2", "data": "data.qcow2",
                              "efivars": "efivars.fd"}}}


def _acct_x86(first_boot_done=True, debug=False):
    return {"name": "u2", "adb_port": 16002, "qmp_port": 17002,
            "vnc_port": 18002, "base": "x86", "ephemeral": True,
            "first_boot_done": first_boot_done, "debug": debug}


def _cfg_x86():
    return {"images_dir": "/tmp/omni-warm-boot-policy-images-x86",
            "bases": {"x86": {"type": "x86-bliss", "version": 1,
                              "kernel": "k.img", "initrd": "i.img",
                              "src": "/android-x86"}}}


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
            (engine, "apply_awake", "apply_awake",
             dict(return_value=True)),
            (engine, "_enforce_hiding", "_enforce_hiding", {}),
            (engine, "assert_kiosk_game", "assert_kiosk_game", {}),
            (engine, "apply_consent", "apply_consent",
             dict(return_value=True)),
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
            # ...and the RESOLVED one, which is what the cache gate and the
            # cache key ask now. `default_accel` is the platform's preference;
            # `effective_accel` is what this machine can actually run, and the
            # two differ on any host that has fallen back to emulation. See
            # omnidroid/accelprobe.py.
            (engine, "effective_accel", "effective_accel",
             dict(return_value="tcg")),
            (engine, "_stage_bake_overlays", "_stage_bake_overlays", {}),
            (runtime_mod, "warm_keys_in_use", "warm_keys_in_use",
             dict(return_value=set())),
            (warmcache, "lookup", "lookup", dict(return_value=None)),
            (warmcache, "has_room", "has_room", dict(return_value=True)),
            # _ensure_booted asks room_report(), not has_room(): a skipped
            # bake has to be able to PRINT why, and a bare bool cannot. The
            # tuple is (has_room, free_bytes, needed_bytes).
            (warmcache, "room_report", "room_report",
             dict(return_value=(True, 1 << 40, 1 << 20))),
            (warmcache, "free_reserve_bytes", "free_reserve_bytes",
             dict(return_value=0)),
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

    def run(self, acct=None, cfg=None, mode_name="hard", **kw):
        acct = acct or _acct()
        cfg = cfg if cfg is not None else _cfg()
        return engine._ensure_booted(acct, cfg, "t", mode_name=mode_name,
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

    def test_a_restore_whose_android_never_finishes_booting_keeps_the_entry_and_cold_boots(self):
        # I4(a): a mere wait_for_boot timeout does NOT implicate the state
        # file (adb hiccup, port conflict, ...) -- only a rejected/failed
        # migrate does. The entry must be KEPT here, not destroyed.
        entry = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            entry, ignore_errors=True))
        wait = mock.MagicMock(side_effect=[False, True])  # restore, then cold
        # has_room=False keeps the fallback a PURE cold boot for this
        # assertion -- whether that fallback itself then also opts to bake
        # is a separate concern, covered by WarmBakeHandoff below.
        with _StubbedBoot(lookup=mock.MagicMock(return_value=entry),
                          wait_for_boot=wait,
                          room_report=mock.MagicMock(return_value=(False, 1, 1 << 40))) as b:
            ok, first = b.run()
        self.assertTrue(ok)
        self.assertFalse(first)
        # The entry is NOT implicated by a boot timeout alone -- kept.
        self.assertTrue(entry.exists())
        # ...and the fallback actually cold-booted (a second spawn_qemu, this
        # one WITHOUT warm= or bake=).
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 2)
        _, second_kwargs = b.mocks["spawn_qemu"].call_args_list[1]
        self.assertNotIn("warm", second_kwargs)
        self.assertNotIn("bake", second_kwargs)
        b.mocks["bake_entry"].assert_not_called()

    def test_a_rejected_restore_discards_the_poisoned_entry_when_unused(self):
        # I4(a): restore_into() itself failing (migrate rejected/failed) DOES
        # implicate the state file -- that entry must be discarded.
        entry = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            entry, ignore_errors=True))
        with _StubbedBoot(lookup=mock.MagicMock(return_value=entry),
                          restore_into=mock.MagicMock(return_value=False),
                          room_report=mock.MagicMock(return_value=(False, 1, 1 << 40))) as b:
            ok, first = b.run()
        self.assertTrue(ok)
        self.assertFalse(first)
        self.assertFalse(entry.exists())
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 2)
        _, second_kwargs = b.mocks["spawn_qemu"].call_args_list[1]
        self.assertNotIn("warm", second_kwargs)
        self.assertNotIn("bake", second_kwargs)

    def test_a_rejected_restore_does_not_delete_an_entry_in_use_by_a_sibling(self):
        # I4(b): `in_use` is sampled before THIS launch's own spawn_qemu made
        # its run.json visible, so a concurrent sibling launch could have
        # started restoring from the SAME entry in the meantime. The
        # fresh, immediately-before-the-rmtree check must catch that.
        entry = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            entry, ignore_errors=True))
        in_use_calls = mock.MagicMock(side_effect=[set(), {"FIXEDKEY"}])
        with mock.patch.object(warmcache, "cache_key", return_value="FIXEDKEY"):
            with _StubbedBoot(lookup=mock.MagicMock(return_value=entry),
                              restore_into=mock.MagicMock(return_value=False),
                              room_report=mock.MagicMock(return_value=(False, 1, 1 << 40)),
                              warm_keys_in_use=in_use_calls) as b:
                ok, first = b.run()
        self.assertTrue(ok)
        self.assertFalse(first)
        # A sibling is running off this entry right now -- must not delete.
        self.assertTrue(entry.exists())
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 2)

    def test_a_cache_miss_on_a_non_interactive_boot_attempts_a_bake(self):
        with _StubbedBoot(room_report=mock.MagicMock(return_value=(True, 1 << 40, 1 << 20))) as b:
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


class WarmRestoreGetsTuned(unittest.TestCase):
    """I2: a warm-restored instance must reach the SAME post-boot mode
    tuning a cold-booted one does -- the density chain (zram + squeeze +
    balloon) IS farming's whole density mechanism, so a restored farming
    instance that skips it silently runs at full memory.
    """

    def _restored(self, **kw):
        entry = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            entry, ignore_errors=True))
        return _StubbedBoot(lookup=mock.MagicMock(return_value=entry), **kw)

    def test_a_successful_restore_applies_performance_tuning(self):
        with self._restored() as b:
            ok, first = b.run(mode_name="hard")
        self.assertTrue(ok)
        self.assertFalse(first)
        b.mocks["apply_roblox_settings"].assert_called_once()
        b.mocks["apply_gaming_tuning"].assert_called_once()
        b.mocks["enable_zram"].assert_not_called()
        b.mocks["apply_farming_squeeze"].assert_not_called()
        b.mocks["apply_balloon_target"].assert_not_called()

    def test_a_successful_restore_gets_the_settings_and_not_the_squeeze(self):
        # The density chain -- zram, the squeeze, the balloon -- no longer
        # runs in the boot tail AT ALL, restored or cold: every lever in it
        # exists to make a JOINED, IDLE instance cheap, and at this point the
        # session has not been delivered, so the client has not been told
        # which place to load. Applying it to a client that is still loading
        # is what stopped farming ever reaching the PS99 world (measured
        # 2026-08-16; see settle_density_instance, which cmd_start calls once
        # the client has finished loading). What a restore must STILL get is
        # the ClientAppSettings file, because Roblox reads it at client start
        # and there is no second chance.
        with self._restored() as b:
            ok, first = b.run(mode_name="farming")
        self.assertTrue(ok)
        self.assertFalse(first)
        b.mocks["apply_roblox_settings"].assert_called_once()
        b.mocks["enable_zram"].assert_not_called()
        b.mocks["apply_farming_squeeze"].assert_not_called()
        b.mocks["apply_balloon_target"].assert_not_called()
        b.mocks["apply_gaming_tuning"].assert_not_called()

    def test_a_successful_restore_is_kept_awake_too(self):
        # The never-sleep guarantee is NOT mode tuning -- it sits above the
        # profile branch precisely so both modes get it -- but it lives in the
        # same shared tail, so an early return on the restore path would drop
        # it exactly the way it once dropped the tuning. A warm-restored
        # instance that blanks is the same product bug as a cold-booted one
        # that blanks.
        with self._restored() as b:
            ok, _ = b.run(mode_name="hard")
        self.assertTrue(ok)
        b.mocks["apply_awake"].assert_called_once()

    def test_a_farming_restore_is_kept_awake_too(self):
        # Farming is the mode where this matters MOST: nobody is watching, so
        # a blanked instance stops rendering (and stops earning) unnoticed.
        with self._restored() as b:
            b.run(mode_name="farming")
        b.mocks["apply_awake"].assert_called_once()

    def test_a_cold_boot_is_kept_awake(self):
        with _StubbedBoot() as b:
            ok, _ = b.run()
        self.assertTrue(ok)
        b.mocks["apply_awake"].assert_called_once()

    def test_the_screen_is_kept_awake_before_the_mode_gets_its_say(self):
        # Ordering, not decoration: gaming's tune-up and farming's squeeze both
        # write settings and move the game around. Applying the awake levers
        # after them would let a mode step be the last writer on a shared
        # surface; applying them first leaves the mode owning everything it
        # legitimately does own.
        order = []
        with _StubbedBoot(
                apply_awake=mock.MagicMock(
                    side_effect=lambda *a, **k: order.append("awake")),
                apply_gaming_tuning=mock.MagicMock(
                    side_effect=lambda *a, **k: order.append("tuning"))) as b:
            b.run(mode_name="hard")
        self.assertEqual(order, ["awake", "tuning"])

    def test_a_successful_restore_does_not_cold_boot_a_second_time(self):
        with self._restored() as b:
            b.run(mode_name="hard")
        self.assertEqual(b.mocks["spawn_qemu"].call_count, 1)
        b.mocks["wait_for_boot"].assert_called_once()


class WarmCacheCoversX86(unittest.TestCase):
    """I6 (REVISED): the warm cache is NOT arm-only.

    It used to refuse x86 outright, because the only thing that was actually
    arm-specific -- staging/moving efivars.fd, a UEFI pflash artifact x86 has
    no concept of -- was baked into the cache's required-file contract. So a
    Windows/x86 host cold-booted every single launch and waited minutes for
    an instance, which is the whole cost the cache exists to remove.

    efivars is now optional and recorded per entry (warmcache.required_files
    keys off meta["arch"]), and the x86 restore path in qemu_command already
    existed, so an x86 base takes exactly the same lookup/bake route as arm.
    """

    def test_x86_looks_up_the_cache_like_arm(self):
        with _StubbedBoot(room_report=mock.MagicMock(return_value=(True, 1 << 40, 1 << 20))) as b:
            ok, first = b.run(acct=_acct_x86(), cfg=_cfg_x86())
        self.assertTrue(ok)
        b.mocks["lookup"].assert_called()

    def test_x86_bakes_an_entry_after_a_cold_boot(self):
        with _StubbedBoot(room_report=mock.MagicMock(return_value=(True, 1 << 40, 1 << 20))) as b:
            ok, first = b.run(acct=_acct_x86(), cfg=_cfg_x86())
        self.assertTrue(ok)
        b.mocks["_stage_bake_overlays"].assert_called()
        b.mocks["bake_entry"].assert_called()

    def test_the_entry_records_its_arch(self):
        # required_files() keys off this: without it an x86 entry would be
        # judged incomplete for lacking efivars.fd and never restore.
        with _StubbedBoot(room_report=mock.MagicMock(return_value=(True, 1 << 40, 1 << 20))) as b:
            b.run(acct=_acct_x86(), cfg=_cfg_x86())
        args, _ = b.mocks["bake_entry"].call_args
        meta = args[3]
        self.assertEqual(meta.get("arch"), "x86")


class WarmEntryFileContract(unittest.TestCase):
    def test_an_x86_entry_does_not_need_efivars(self):
        from omnidroid import warmcache
        self.assertNotIn(warmcache.EFIVARS_NAME,
                         warmcache.required_files("x86"))

    def test_an_arm_entry_still_needs_efivars(self):
        from omnidroid import warmcache
        self.assertIn(warmcache.EFIVARS_NAME, warmcache.required_files("arm"))

    def test_an_entry_with_no_recorded_arch_is_treated_as_arm(self):
        # Every entry written before x86 support was an arm one; they must
        # keep validating exactly as before.
        from omnidroid import warmcache
        self.assertEqual(warmcache.required_files(None),
                         warmcache.REQUIRED_FILES)


class WarmCacheKillSwitch(unittest.TestCase):
    """I8: --no-warm / OMNI_NO_WARM=1 must disable BOTH restore and bake for
    a launch, routed through the single _warm_cache_allowed() decision
    point."""

    def test_no_warm_flag_disables_restore(self):
        entry = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            entry, ignore_errors=True))
        with _StubbedBoot(lookup=mock.MagicMock(return_value=entry)) as b:
            ok, first = b.run(no_warm=True)
        self.assertTrue(ok)
        b.mocks["lookup"].assert_not_called()
        b.mocks["restore_into"].assert_not_called()
        _, kwargs = b.mocks["spawn_qemu"].call_args
        self.assertNotIn("warm", kwargs)

    def test_no_warm_flag_disables_bake(self):
        with _StubbedBoot(room_report=mock.MagicMock(return_value=(True, 1 << 40, 1 << 20))) as b:
            ok, first = b.run(no_warm=True)
        self.assertTrue(ok)
        b.mocks["_stage_bake_overlays"].assert_not_called()
        b.mocks["bake_entry"].assert_not_called()
        _, kwargs = b.mocks["spawn_qemu"].call_args
        self.assertNotIn("bake", kwargs)

    def test_omni_no_warm_env_var_disables_the_cache(self):
        entry = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(
            entry, ignore_errors=True))
        with mock.patch.dict(os.environ, {"OMNI_NO_WARM": "1"}):
            with _StubbedBoot(lookup=mock.MagicMock(return_value=entry)) as b:
                ok, first = b.run()
        self.assertTrue(ok)
        b.mocks["lookup"].assert_not_called()

    def test_warm_cache_allowed_refuses_when_no_warm(self):
        self.assertFalse(engine._warm_cache_allowed(
            debug=False, in_use=set(), key="k", no_warm=True, accel="kvm"))
        self.assertTrue(engine._warm_cache_allowed(
            debug=False, in_use=set(), key="k", no_warm=False, accel="kvm"))


if __name__ == "__main__":
    unittest.main()
