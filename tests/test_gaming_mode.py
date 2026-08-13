#!/usr/bin/env python3
"""`gaming` mode: a real window when the host can open one, headless when not.

    python3 tests/test_gaming_mode.py

The two use cases this engine serves pull in opposite directions:

  farming  — headless, many instances, RAM/CPU is the only thing that matters
  gaming   — one or two instances, frames and INPUT LATENCY are all that matter

`gaming` is a separate mode rather than a change to `playable` on purpose:
`playable` is DEFAULT_MODE, so teaching it to open a window would put a QEMU
window on every existing `omnidroid start`. Adding a mode is additive; every other
mode's command has to stay byte-for-byte what it is today, and that is what
most of these tests assert.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402

GL_CAP = {"available": True, "tier": "gl",
          "gpu_args": ["-device", "virtio-gpu-gl-pci"],
          "display_args": ["-display", "cocoa,gl=on"], "reason": "gl"}
WINDOW_CAP = {"available": True, "tier": "window",
              "gpu_args": ["-device", "virtio-gpu-pci"],
              "display_args": ["-display", "cocoa"], "reason": "window"}
NO_CAP = {"available": False, "tier": "none", "gpu_args": [],
          "display_args": [], "reason": "headless host"}


def _acct(base="arm"):
    return {"name": "u1", "adb_port": 16001, "qmp_port": 17001,
            "vnc_port": 18001, "base": base, "ephemeral": True}


def _cfg():
    # `data_template` is required by the x86 command builder (an x86 boot with
    # no offset falls back to the shared empty /data). Without it the x86 cases
    # below died with KeyError before reaching a single assertion.
    return {"images_dir": "/imgs", "current_base": "arm",
            "data_template": "data-template-8g.qcow2",
            "bases": {"arm": {"type": "arm-uefi",
                              "system": "base_arm_system.qcow2",
                              "data": "base_arm_data.qcow2",
                              "base_disk": "base_arm_v2.qcow2",
                              "efivars": "base_arm_efivars.fd"},
                      "x86": {"type": "x86-bliss", "disk": "base_x86.qcow2",
                              "kernel": "base_x86.kernel",
                              "initrd": "base_x86.initrd.img",
                              "src": "/android"}},
            "qemu": {"mem_mb": 4096, "smp": 4}}


def _cmd(mode_name, cap=NO_CAP, base="arm", interactive=False, env=None):
    """The QEMU command for one boot, with the host capability faked."""
    from pathlib import Path
    environ = {"OMNI_GL_WINDOW": ""} if env is None else env
    with mock.patch.dict(os.environ, environ, clear=False), \
         mock.patch.object(qemu_proc, "default_display", return_value=cap), \
         mock.patch.object(qemu_proc, "_qemu_help_texts", return_value=("", "")), \
         mock.patch.object(qemu_proc, "_host_has_gui", return_value=True), \
         mock.patch.object(qemu_proc, "arm_edk2_code", return_value="/fw/code.fd"), \
         mock.patch.object(qemu_proc, "qemu_bin", side_effect=lambda x: x), \
         mock.patch.object(qemu_proc, "default_accel", return_value="hvf"), \
         mock.patch.object(omni, "runtime_dir", side_effect=lambda n: Path(f"/RT/{n}")), \
         mock.patch.object(omni, "account_dir", side_effect=lambda n: Path(f"/AC/{n}")):
        if env is None:
            os.environ.pop("OMNI_GL_WINDOW", None)
        mode = qemu_proc.resolve_mode(_cfg(), mode_name)
        return " ".join(qemu_proc.qemu_command(_acct(base), _cfg(),
                                               interactive, mode=mode))


class TheModeExists(unittest.TestCase):
    def test_gaming_is_a_registered_mode(self):
        self.assertIn("gaming", qemu_proc.MODES)

    def test_gaming_asks_for_a_window_and_no_other_mode_does(self):
        wanting = [n for n, m in qemu_proc.MODES.items() if m.get("window")]
        self.assertEqual(wanting, ["gaming"])

    def test_gaming_keeps_the_bases_native_resolution(self):
        # A shrunken display is a farming lever; a game needs its real one.
        self.assertIsNone(qemu_proc.MODES["gaming"]["display"])

    def test_gaming_keeps_input_devices(self):
        self.assertTrue(qemu_proc.MODES["gaming"]["usb"])

    def test_gaming_does_not_balloon(self):
        # Reclaiming pages under a running game is a stutter source.
        self.assertIsNone(qemu_proc.MODES["gaming"]["balloon"])

    def test_playable_is_still_the_default_and_still_headless(self):
        self.assertEqual(qemu_proc.DEFAULT_MODE, "playable")
        self.assertFalse(qemu_proc.MODES["playable"].get("window"))


class ArmBootUsesTheHostCapability(unittest.TestCase):
    def test_gaming_on_a_gl_host_is_accelerated(self):
        cmd = _cmd("gaming", GL_CAP)
        self.assertIn("-device virtio-gpu-gl-pci", cmd)
        self.assertIn("-display cocoa,gl=on", cmd)
        self.assertNotIn("-display none", cmd)

    def test_gaming_on_a_window_only_host_opens_a_plain_window(self):
        cmd = _cmd("gaming", WINDOW_CAP)
        self.assertIn("-display cocoa", cmd)
        self.assertNotIn("-display none", cmd)

    def test_gaming_on_a_headless_host_degrades_without_failing(self):
        cmd = _cmd("gaming", NO_CAP)
        self.assertIn("-device virtio-gpu-pci", cmd)
        self.assertIn("-display none", cmd)

    def test_farming_is_headless_even_on_a_gl_host(self):
        cmd = _cmd("farming", GL_CAP)
        self.assertIn("-device virtio-gpu-pci", cmd)
        self.assertIn("-display none", cmd)
        self.assertNotIn("gl=on", cmd)

    def test_playable_the_default_is_untouched_on_a_gl_host(self):
        cmd = _cmd("playable", GL_CAP)
        self.assertIn("-device virtio-gpu-pci", cmd)
        self.assertIn("-display none", cmd)

    def test_an_interactive_builder_boot_never_opens_a_window(self):
        cmd = _cmd("gaming", GL_CAP, interactive=True)
        self.assertIn("-display none", cmd)


class VncSurvivesTheWindow(unittest.TestCase):
    """`omnidroid screenshot`, the auto-capture recorder and the omnidroid-input
    skill all attach to the instance's VNC framebuffer (see capture.py). A
    gaming boot that dropped `-vnc` would silently blind every one of them,
    so the window is ADDITIVE to VNC, never a replacement."""

    def test_vnc_is_present_on_a_gaming_boot(self):
        self.assertIn("-vnc 127.0.0.1:", _cmd("gaming", GL_CAP))

    def test_vnc_is_present_on_a_farming_boot(self):
        self.assertIn("-vnc 127.0.0.1:", _cmd("farming"))


class X86GetsTheSameTreatment(unittest.TestCase):
    def test_gaming_opens_a_window_on_x86(self):
        cmd = _cmd("gaming", WINDOW_CAP, base="x86")
        self.assertIn("-display cocoa", cmd)
        self.assertNotIn("-display none", cmd)

    def test_farming_stays_headless_on_x86(self):
        cmd = _cmd("farming", GL_CAP, base="x86")
        self.assertIn("-display none", cmd)
        self.assertNotIn("gl=on", cmd)


class TheSpikeEnvStillWorks(unittest.TestCase):
    """OMNI_GL_WINDOW predates the mode and is kept as an alias, so the B2
    runbook's one-liner still means something — but it now goes through the
    same capability gate instead of hardcoding args QEMU may not accept."""

    def test_env_requests_a_window_from_any_mode(self):
        cmd = _cmd("playable", WINDOW_CAP, env={"OMNI_GL_WINDOW": "1"})
        self.assertIn("-display cocoa", cmd)

    def test_env_still_degrades_when_the_host_cannot(self):
        cmd = _cmd("playable", NO_CAP, env={"OMNI_GL_WINDOW": "1"})
        self.assertIn("-display none", cmd)


class TheHostIsProbedAtMostOnce(unittest.TestCase):
    """Probing the host costs two QEMU subprocess launches.

    This used to assert a farming boot never probes at all, because only
    `gaming` cared about the answer. Headless boots now care too — that is how
    they get `egl-headless` and the host GPU instead of software rendering —
    so "never" is no longer the right property. The COST concern behind it is
    unchanged and still load-bearing: a host bringing up 50 instances must not
    pay 100 subprocess launches for an answer that cannot change. So the
    invariant is now "asked once per process", enforced by the memo in
    _qemu_help_texts.
    """

    def test_fifty_boots_probe_the_host_once(self):
        from pathlib import Path
        qemu_proc._HELP_CACHE.clear()
        with mock.patch.dict(os.environ, {}, clear=False), \
             mock.patch.object(qemu_proc, "subprocess") as sp, \
             mock.patch.object(qemu_proc, "arm_edk2_code", return_value="/fw/c.fd"), \
             mock.patch.object(qemu_proc, "qemu_bin", side_effect=lambda x: x), \
             mock.patch.object(qemu_proc, "default_accel", return_value="hvf"), \
             mock.patch.object(omni, "runtime_dir", side_effect=lambda n: Path(f"/RT/{n}")), \
             mock.patch.object(omni, "account_dir", side_effect=lambda n: Path(f"/AC/{n}")):
            sp.run.return_value = mock.Mock(stdout="")
            os.environ.pop("OMNI_GL_WINDOW", None)
            mode = qemu_proc.resolve_mode(_cfg(), "farming")
            for _ in range(50):
                qemu_proc.qemu_command(_acct(), _cfg(), False, mode=mode)
            # two calls: `-display help` and `-device help`, once between them
            self.assertLessEqual(sp.run.call_count, 2)
        qemu_proc._HELP_CACHE.clear()


if __name__ == "__main__":
    unittest.main()
