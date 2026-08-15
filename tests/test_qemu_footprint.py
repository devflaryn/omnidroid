#!/usr/bin/env python3
"""What the QEMU command line costs per instance, on both architectures.

    python3 tests/test_qemu_footprint.py
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402


ACCT = {"name": "u1", "adb_port": 16001, "qmp_port": 17001, "vnc_port": 18001}

ARM_CFG = {
    "images_dir": "/img",
    "qemu": {"mem_mb": 4096, "smp": 4},
    "bases": {"arm": {"type": "arm-uefi", "base_disk": "b.qcow2",
                      "system": "s.qcow2", "data": "d.qcow2"}},
}
X86_CFG = {
    "images_dir": "/img",
    "qemu": {"mem_mb": 4096, "smp": 4},
    "bases": {"x86": {"type": "x86-bliss", "disk": "b.qcow2",
                      "kernel": "k", "initrd": "i", "src": "/src"}},
}


def arm_cmd(mode_name="farming", interactive=False, debug=False):
    acct = dict(ACCT, base="arm")
    with mock.patch.object(qemu_proc, "arm_edk2_code", return_value="/edk2.fd"), \
         mock.patch("omnidroid.engine.runtime_dir", return_value=__import__(
             "pathlib").Path("/rt")), \
         mock.patch("omnidroid.engine.account_dir", return_value=__import__(
             "pathlib").Path("/acct")):
        return qemu_proc.qemu_command_arm(
            acct, ARM_CFG, interactive, mode=omni.resolve_mode({}, mode_name),
            debug=debug)


def x86_cmd(mode_name="farming", interactive=False, debug=False):
    acct = dict(ACCT, base="x86")
    with mock.patch("omnidroid.engine.account_dir", return_value=__import__(
             "pathlib").Path("/acct")):
        return qemu_proc.qemu_command(
            acct, X86_CFG, interactive, mode=omni.resolve_mode({}, mode_name),
            debug=debug)


class Balloon(unittest.TestCase):
    """free-page-reporting is the mechanism that makes host RSS track the
    guest's live set instead of its -m size. It must be on EVERY instance,
    both architectures — a guest without the driver just ignores the device,
    so there is no reason to branch."""

    def test_arm_has_free_page_reporting(self):
        joined = " ".join(arm_cmd())
        self.assertIn("virtio-balloon-pci", joined)
        self.assertIn("free-page-reporting=on", joined)

    def test_x86_has_free_page_reporting(self):
        joined = " ".join(x86_cmd())
        self.assertIn("virtio-balloon-pci", joined)
        self.assertIn("free-page-reporting=on", joined)

    def test_playable_gets_it_too(self):
        self.assertIn("free-page-reporting=on", " ".join(arm_cmd("playable")))


class DroppedHardware(unittest.TestCase):
    def test_farming_KEEPS_usb(self):
        """Reversal, recorded on purpose. Dropping USB from farming looked
        free — nothing taps a farming instance by hand. Then a rebooted guest
        came up with adb unauthorized, putting an "Allow USB debugging?"
        dialog on screen that could not be dismissed from adb (unauthorized)
        or QMP (input-send-event needs a device). The instance was
        permanently unreachable. An unrecoverable instance in a 50-instance
        fleet costs more than the device models it saves."""
        joined = " ".join(arm_cmd("farming"))
        self.assertIn("usb-tablet", joined)
        self.assertIn("usb-kbd", joined)

    def test_playable_keeps_usb(self):
        joined = " ".join(arm_cmd("playable"))
        self.assertIn("usb-tablet", joined)
        self.assertIn("usb-kbd", joined)

    def test_arm_has_exactly_one_usb_controller(self):
        """Two xHCI controllers used to be attached on every arm boot
        (nec-usb-xhci AND qemu-xhci) with the input devices bound only to the
        first, so the second was pure dead weight. THIS removal stands - it
        cost nothing and removed nothing reachable."""
        cmd = arm_cmd("playable")
        controllers = [a for a in cmd if "xhci" in a]
        self.assertEqual(len(controllers), 1, controllers)

    def test_arm_dropped_the_unused_virtio_serial(self):
        self.assertNotIn("virtio-serial", " ".join(arm_cmd("playable")))

    def test_arm_keeps_virtio_rng(self):
        """Not an oversight that this survived: without it the guest's early
        entropy pool fills from nothing and boot stalls."""
        self.assertIn("virtio-rng-pci", " ".join(arm_cmd("farming")))

    def test_usb_can_still_be_dropped_by_a_mode_that_asks(self):
        """The mechanism stays; only farming's use of it was reverted."""
        self.assertEqual(qemu_proc.usb_devices({"usb": False}, arm=True), [])


class ModeSizing(unittest.TestCase):
    def test_farming_is_single_vcpu(self):
        cmd = arm_cmd("farming")
        self.assertEqual(cmd[cmd.index("-smp") + 1], "1")

    def test_farming_boots_with_enough_address_space(self):
        """512 MB was measured twice and never reached adbd."""
        cmd = arm_cmd("farming")
        self.assertGreaterEqual(int(cmd[cmd.index("-m") + 1]), 2048)

    def test_instances_stay_headless_and_localhost_only(self):
        for cmd in (arm_cmd("farming"), x86_cmd("farming")):
            joined = " ".join(cmd)
            self.assertIn("-display none", joined)
            self.assertIn("-vnc 127.0.0.1:", joined)


class BalloonTarget(unittest.TestCase):
    """apply_balloon_target must POLL. Inflation is asynchronous: QEMU
    returns as soon as the request is queued and the guest then walks its
    free lists over some seconds. A single eager read reports the
    pre-inflation size, which is indistinguishable from a missing balloon
    driver — observed live before this was fixed."""

    def _run(self, actuals, target=1024):
        seq = list(actuals)

        def fake_qmp(acct, cmd, args=None, **kw):
            if cmd == "balloon":
                return {"return": {}}
            v = seq.pop(0) if len(seq) > 1 else seq[0]
            return {"return": {"actual": v * 1024 * 1024}}

        with mock.patch.object(omni, "qmp", side_effect=fake_qmp), \
             mock.patch.object(omni.time, "sleep"):
            return omni.apply_balloon_target(
                {"name": "u1", "qmp_port": 1}, {"balloon": target, "mem": 2048})

    def test_waits_for_a_late_inflation(self):
        self.assertEqual(self._run([2046, 2046, 2046, 1020]), 1020)

    def test_reports_the_actual_when_it_never_inflates(self):
        self.assertEqual(self._run([2046]), 2046)

    def test_returns_none_when_the_mode_wants_no_balloon(self):
        with mock.patch.object(omni, "qmp") as q:
            self.assertIsNone(
                omni.apply_balloon_target({"name": "u1"}, {"balloon": None}))
        self.assertFalse(q.called)

    def test_returns_none_when_qmp_is_unreachable(self):
        with mock.patch.object(omni, "qmp", return_value=None):
            self.assertIsNone(omni.apply_balloon_target(
                {"name": "u1", "qmp_port": 1}, {"balloon": 1024, "mem": 2048}))


class MemPlumbing(unittest.TestCase):
    """`omnidroid start --mem N` was accepted by argparse and then dropped on the
    floor: _ensure_booted called resolve_mode() without it, so the flag
    silently booted at the mode's own size. A 512 MB farming boot then
    surfaced as an unexplained boot timeout."""

    def _spawned_mode(self, **kw):
        seen = {}

        def fake_spawn(acct, cfg, interactive, mode=None, accel=None, debug=False):
            seen.update(mode or {})

        with mock.patch.object(omni, "running_pid", return_value=None), \
             mock.patch.object(omni, "spawn_qemu", side_effect=fake_spawn), \
             mock.patch.object(omni, "maybe_start_autocap"), \
             mock.patch.object(omni, "wait_for_boot", return_value=True), \
             mock.patch.object(omni, "post_boot"), \
             mock.patch.object(omni, "_devkit_activate"), \
             mock.patch.object(omni, "_enforce_hiding"), \
             mock.patch.object(omni, "assert_kiosk_game"), \
             mock.patch.object(omni, "apply_consent"), \
             mock.patch.object(omni, "apply_awake"), \
             mock.patch.object(omni, "apply_farming_squeeze"), \
             mock.patch.object(omni, "apply_balloon_target"), \
             mock.patch.object(omni, "apply_roblox_settings"), \
             mock.patch.object(omni, "enable_zram"):
            omni._ensure_booted({"name": "u1", "first_boot_done": True},
                                {}, "t", **kw)
        return seen

    def test_mem_override_reaches_qemu(self):
        self.assertEqual(
            self._spawned_mode(mode_name="farming", mem=3072)["mem"], 3072)

    def test_balloon_override_reaches_the_mode(self):
        self.assertEqual(
            self._spawned_mode(mode_name="farming", balloon=700)["balloon"],
            700)

    def test_default_is_the_modes_own_size(self):
        self.assertEqual(self._spawned_mode(mode_name="farming")["mem"],
                         omni.MODES["farming"]["mem"])


class ZramDependentCap(unittest.TestCase):
    """The safe balloon floor depends on whether the guest actually has zram.

    MEASURED 2026-08-05 (arm64, real Roblox APK): without zram a 1024 MB cap
    KILLS the game (mem-pressure-event); with zram (lz4, 2.97x measured) the
    game survives 1280, 1024 and even 896 with zero kills. So picking the low
    cap when zram is absent is not a missed optimization, it is an OOM."""

    def _target(self, zram_on):
        seen = {}

        def fake_qmp(acct, cmd, args=None, **kw):
            if cmd == "balloon":
                seen["mb"] = args["value"] // (1024 * 1024)
                return {"return": {}}
            return {"return": {"actual": seen.get("mb", 0) * 1024 * 1024}}

        mode = {"balloon": 1536, "balloon_zram": 1024, "mem": 2048}
        with mock.patch.object(omni, "qmp", side_effect=fake_qmp), \
             mock.patch.object(omni, "zram_active", return_value=zram_on), \
             mock.patch.object(omni.time, "sleep"):
            omni.apply_balloon_target({"name": "u1", "qmp_port": 1}, mode)
        return seen.get("mb")

    def test_uses_the_low_cap_only_when_zram_is_really_on(self):
        self.assertEqual(self._target(zram_on=True), 1024)

    def test_holds_the_safe_cap_without_zram(self):
        """Applying 1024 here would OOM the game - measured, not theorised."""
        self.assertEqual(self._target(zram_on=False), 1536)

    def test_zram_cap_is_a_real_reduction(self):
        f = omni.MODES["farming"]
        self.assertLess(f["balloon_zram"], f["balloon"])
        self.assertLess(f["balloon_zram"], f["mem"])

    def test_modes_without_a_zram_cap_are_unaffected(self):
        seen = {}

        def fake_qmp(acct, cmd, args=None, **kw):
            if cmd == "balloon":
                seen["mb"] = args["value"] // (1024 * 1024)
                return {"return": {}}
            return {"return": {"actual": seen.get("mb", 0) * 1024 * 1024}}

        with mock.patch.object(omni, "qmp", side_effect=fake_qmp), \
             mock.patch.object(omni, "zram_active") as za, \
             mock.patch.object(omni.time, "sleep"):
            omni.apply_balloon_target({"name": "u1", "qmp_port": 1},
                                      {"balloon": 1536, "mem": 2048})
        self.assertEqual(seen.get("mb"), 1536)
        self.assertFalse(za.called, "should not probe when there is no "
                                    "zram cap to choose")


class ExplicitBalloonWins(unittest.TestCase):
    """`--balloon N` must not be silently overridden by the zram cap.

    Regression: with zram active, apply_balloon_target substituted the mode's
    balloon_zram figure, so `--balloon 896` ran at 1024 and said so in the
    log. Same family as the `--mem` bug — a flag argparse accepts and the
    engine then ignores."""

    def _applied(self, **kw):
        seen = {}

        def fake_qmp(acct, cmd, args=None, **k):
            if cmd == "balloon":
                seen["mb"] = args["value"] // (1024 * 1024)
                return {"return": {}}
            return {"return": {"actual": seen.get("mb", 0) * 1024 * 1024}}

        mode = omni.resolve_mode({}, "farming", **kw)
        with mock.patch.object(omni, "qmp", side_effect=fake_qmp), \
             mock.patch.object(omni, "zram_active", return_value=True), \
             mock.patch.object(omni.time, "sleep"):
            omni.apply_balloon_target({"name": "u1", "qmp_port": 1}, mode)
        return seen.get("mb")

    def test_explicit_balloon_beats_the_zram_cap(self):
        self.assertEqual(self._applied(balloon=896), 896)

    def test_explicit_balloon_above_the_zram_cap_is_honoured_too(self):
        """Someone asking for MORE headroom must get it, not be squeezed."""
        self.assertEqual(self._applied(balloon=1400), 1400)

    def test_without_the_flag_the_zram_cap_still_applies(self):
        self.assertEqual(self._applied(),
                         omni.MODES["farming"]["balloon_zram"])

    def test_resolve_drops_the_zram_cap_when_overridden(self):
        self.assertNotIn("balloon_zram",
                         omni.resolve_mode({}, "farming", balloon=896))
        self.assertIn("balloon_zram", omni.resolve_mode({}, "farming"))


if __name__ == "__main__":
    unittest.main()
