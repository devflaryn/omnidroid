#!/usr/bin/env python3
"""The display policy: headless always, GPU wherever the host can present.

    python3 tests/test_gaming_mode.py

The two use cases this engine serves pull in opposite directions:

  farming  — headless, many instances, RAM/CPU is the only thing that matters
  gaming   — one or two instances, frames and INPUT LATENCY are all that matter

They now share one display policy, and these tests pin it:

  * NO MODE opens a native QEMU window. A window is reachable only through the
    OMNI_GL_WINDOW debugging hatch, and it is the one boot that gives up `-vnc`
    (QEMU refuses the pair — see below).
  * A headless boot still reaches the GPU where `-display egl-headless` can
    actually present, and keeps its VNC server while doing it.
  * A host that can do neither still BOOTS, on software rendering.

The measurement behind the GPU half, at 1280x800 on one account and place,
frame counts off `dumpsys SurfaceFlinger --timestats`:

    software (llvmpipe)               95 frames / 30.1 s  →  3.2 fps
    GPU (virgl, RTX 4060)            728 frames / 30.1 s  → 24.2 fps

and the measurement behind the "wherever the host can present" hedge is in
tests/test_headless_gpu.py: on Windows an egl-headless guest renders on the GPU
and never scans out (totalFrames = 0, SET_SCANOUT rejected), so the default
there is software and the trade is the user's to make explicitly.
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


def _cmd(mode_name, cap=NO_CAP, base="arm", interactive=False, env=None,
         headless_gl=False):
    """The QEMU command for one boot, with the host capability faked.

    `cap` is what default_display() would answer, i.e. the WINDOWED tier, and
    it is only consulted when something asks for a window. `headless_gl` fakes
    a host whose egl-headless can present."""
    from pathlib import Path
    environ = {"OMNI_GL_WINDOW": ""} if env is None else env
    hl_cap = {"available": True,
              "gpu_args": ["-device", "virtio-gpu-gl-pci,xres=1280,yres=800"],
              "display_args": ["-display", "egl-headless"], "reason": "egl"}
    with mock.patch.dict(os.environ, environ, clear=False), \
         mock.patch.object(qemu_proc, "default_display", return_value=cap), \
         mock.patch.object(qemu_proc, "_headless_gl_wanted",
                           return_value=headless_gl), \
         mock.patch.object(qemu_proc, "headless_gl_capability",
                           return_value=hl_cap), \
         mock.patch.object(qemu_proc, "_qemu_help_texts", return_value=("", "")), \
         mock.patch.object(qemu_proc, "_host_has_gui", return_value=True), \
         mock.patch.object(qemu_proc, "arm_edk2_code", return_value="/fw/code.fd"), \
         mock.patch.object(qemu_proc, "qemu_bin", side_effect=lambda x: x), \
         mock.patch.object(qemu_proc, "default_accel", return_value="hvf"), \
         mock.patch.object(omni, "runtime_dir", side_effect=lambda n: Path(f"/RT/{n}")), \
         mock.patch.object(omni, "account_dir", side_effect=lambda n: Path(f"/AC/{n}")):
        # Only clear what THIS call did not ask for: popping unconditionally
        # would silently discard the env the caller is testing.
        for stale in ("OMNI_GL_WINDOW", "OMNI_PANEL", "OMNI_GPU_OPTS",
                      "OMNI_FORCE_VIDEO_MODE", "OMNI_NO_WINDOW"):
            if stale not in (env or {}):
                os.environ.pop(stale, None)
        mode = qemu_proc.resolve_mode(_cfg(), mode_name)
        return " ".join(qemu_proc.qemu_command(_acct(base), _cfg(),
                                               interactive, mode=mode))


class TheModeExists(unittest.TestCase):
    def test_gaming_is_a_registered_mode(self):
        self.assertIn("gaming", qemu_proc.MODES)

    def test_no_mode_asks_for_a_window(self):
        # The product is headless everywhere. A mode that asked for a window
        # would put one on screen for every automated caller, and would take
        # the VNC server away from them at the same time.
        for name, m in qemu_proc.MODES.items():
            self.assertFalse(m.get("window"), name)

    def test_gaming_keeps_the_bases_native_resolution(self):
        # A shrunken display is a farming lever; a game needs its real one.
        self.assertIsNone(qemu_proc.MODES["gaming"]["display"])

    def test_gaming_keeps_input_devices(self):
        self.assertTrue(qemu_proc.MODES["gaming"]["usb"])

    def test_gaming_does_not_balloon(self):
        # Reclaiming pages under a running game is a stutter source.
        self.assertIsNone(qemu_proc.MODES["gaming"]["balloon"])

    def test_gaming_is_the_default(self):
        self.assertEqual(qemu_proc.DEFAULT_MODE, "gaming")

    def test_no_window_overrides_even_the_debugging_hatch(self):
        # --no-window / OMNI_NO_WINDOW means "nothing on my screen", and it has
        # to beat OMNI_GL_WINDOW, which is the only thing that can open one.
        cmd = _cmd("gaming", GL_CAP,
                   env={"OMNI_NO_WINDOW": "1", "OMNI_GL_WINDOW": "1"})
        self.assertIn("-display none", cmd)
        self.assertNotIn("gl=on", cmd)
        # ...and the VNC server is there, because nothing holds a GL context.
        self.assertIn("-vnc 127.0.0.1:", cmd)


class ArmBootUsesTheHostCapability(unittest.TestCase):
    def test_gaming_prefers_a_windowless_gpu_over_a_windowed_one(self):
        # Both are on offer. The windowless one wins, because it keeps the VNC
        # viewer as well as the GPU.
        cmd = _cmd("gaming", GL_CAP, headless_gl=True)
        self.assertIn("-display egl-headless", cmd)
        self.assertIn("-vnc 127.0.0.1:", cmd)

    def test_gaming_falls_back_to_a_window_when_that_is_the_only_gl(self):
        # `auto` means "get to the GPU whatever it takes". On a host whose
        # egl-headless cannot present (Windows), the window is the only GL
        # context QEMU will give, and 3.2 fps is not a product.
        cmd = _cmd("gaming", GL_CAP, headless_gl=False)
        self.assertIn("-display cocoa,gl=on", cmd)
        self.assertNotIn("-vnc", cmd)       # QEMU refuses the pair

    def test_gpu_headless_keeps_the_viewer_and_gives_up_the_gpu(self):
        cmd = _cmd("gaming", GL_CAP, headless_gl=False,
                   env={"OMNI_GPU": "headless"})
        self.assertIn("-display none", cmd)
        self.assertIn("-vnc 127.0.0.1:", cmd)

    def test_gpu_off_is_software_everywhere(self):
        cmd = _cmd("gaming", GL_CAP, headless_gl=True, env={"OMNI_GPU": "off"})
        self.assertIn("-device virtio-gpu-pci", cmd)
        self.assertIn("-display none", cmd)

    def test_farming_stays_headless_even_when_a_window_is_on_offer(self):
        # Fifty QEMU windows is not a product, and a 5-fps-capped instance
        # nobody watches gains almost nothing from the GPU.
        cmd = _cmd("farming", GL_CAP, headless_gl=False)
        self.assertIn("-display none", cmd)
        self.assertIn("-vnc 127.0.0.1:", cmd)

    def test_a_host_whose_egl_headless_presents_gets_the_gpu_and_keeps_vnc(self):
        cmd = _cmd("gaming", NO_CAP, headless_gl=True)
        self.assertIn("-device virtio-gpu-gl-pci", cmd)
        self.assertIn("-display egl-headless", cmd)
        self.assertIn("-vnc 127.0.0.1:", cmd)

    def test_a_host_that_cannot_present_headless_gl_degrades_to_software(self):
        cmd = _cmd("gaming", NO_CAP, headless_gl=False)
        self.assertIn("-device virtio-gpu-pci", cmd)
        self.assertIn("-display none", cmd)
        self.assertIn("-vnc 127.0.0.1:", cmd)

    def test_farming_takes_the_same_gpu_path_as_gaming(self):
        # Farming used to be hardcoded to software. Rendering on the host GPU
        # takes work OFF the guest CPU, which is the scarce thing in a density
        # boot too — there is no reason for the two modes to differ here.
        cmd = _cmd("farming", NO_CAP, headless_gl=True)
        self.assertIn("-device virtio-gpu-gl-pci", cmd)
        self.assertIn("-display egl-headless", cmd)

    def test_farming_renders_at_its_own_smaller_panel(self):
        cmd = _cmd("farming", NO_CAP, headless_gl=False)
        self.assertIn("-display none", cmd)

    def test_an_interactive_builder_boot_stays_on_the_plain_path(self):
        cmd = _cmd("gaming", GL_CAP, interactive=True, headless_gl=True)
        self.assertIn("-display none", cmd)


class OnlyAWindowedGlBootGivesUpVnc(unittest.TestCase):
    """QEMU refuses a VNC server beside a WINDOWED display holding a GL
    context, and says so:

        qemu: -vnc 127.0.0.1:12101: Display vnc is incompatible with the GL context

    (Re-verified on QEMU 11.0.50 for gtk/sdl × gl=on/es/core — all four refuse.)

    It does NOT refuse egl-headless, which QEMU documents as the display to
    pair with VNC. Scoping the drop to the windowed case is the whole fix: this
    file used to drop `-vnc` for any GL context at all, so every
    GPU-accelerated boot came up with no VNC server, and the missing server was
    then reported as "the viewer is black".
    """

    def test_the_debug_window_gives_up_vnc(self):
        cmd = _cmd("gaming", GL_CAP, env={"OMNI_GL_WINDOW": "1"})
        self.assertIn("gl=on", cmd)
        self.assertNotIn("-vnc", cmd)

    def test_a_windowed_boot_without_gl_keeps_vnc(self):
        # tier "window" is a native window with SOFTWARE rendering — no GL
        # context, so nothing stops VNC being served alongside it.
        cmd = _cmd("gaming", WINDOW_CAP, env={"OMNI_GL_WINDOW": "1"})
        self.assertIn("-display cocoa", cmd)
        self.assertIn("-vnc 127.0.0.1:", cmd)

    def test_vnc_is_present_on_a_farming_boot(self):
        self.assertIn("-vnc 127.0.0.1:", _cmd("farming"))

    def test_headless_gl_KEEPS_vnc(self):
        self.assertEqual(qemu_proc.vnc_args(["-display", "egl-headless"], 1),
                         ["-vnc", "127.0.0.1:1"])
        self.assertEqual(qemu_proc.vnc_args(["-display", "none"], 1),
                         ["-vnc", "127.0.0.1:1"])


class X86GetsTheSameTreatment(unittest.TestCase):
    def test_gaming_takes_the_windowless_gpu_on_x86_too(self):
        cmd = _cmd("gaming", WINDOW_CAP, base="x86", headless_gl=True)
        self.assertIn("-display egl-headless", cmd)
        self.assertIn("-vnc 127.0.0.1:", cmd)

    def test_farming_stays_headless_on_x86(self):
        cmd = _cmd("farming", GL_CAP, base="x86")
        self.assertIn("-display none", cmd)
        self.assertNotIn("gl=on", cmd)

    def test_the_physical_mode_is_NOT_forced_by_default(self):
        # `video=Virtual-1:<mode>` looked like the fix for a GL boot coming up
        # 640x480. MEASURED: it changed nothing on the headless paths (already
        # at the panel size) and HUNG the windowed GL boot -- five minutes
        # before adbd, QEMU alive, guest stuck before userspace. See
        # qemu_proc.force_video_mode.
        cmd = _cmd("gaming", NO_CAP, base="x86", headless_gl=True)
        self.assertNotIn("video=Virtual-1", cmd)

    def test_the_forced_mode_is_still_available_behind_the_flag(self):
        cmd = _cmd("gaming", NO_CAP, base="x86",
                   env={"OMNI_FORCE_VIDEO_MODE": "1"})
        self.assertIn("video=Virtual-1:1280x800", cmd)

    def test_an_explicit_panel_reaches_the_kernel_arg_when_forced(self):
        cmd = _cmd("gaming", NO_CAP, base="x86",
                   env={"OMNI_PANEL": "1920x1080",
                        "OMNI_FORCE_VIDEO_MODE": "1"})
        self.assertIn("video=Virtual-1:1920x1080", cmd)


class TheSpikeEnvStillWorks(unittest.TestCase):
    """OMNI_GL_WINDOW predates the mode and is kept as an alias, so the B2
    runbook's one-liner still means something — but it now goes through the
    same capability gate instead of hardcoding args QEMU may not accept."""

    def test_env_requests_a_window_from_any_mode(self):
        cmd = _cmd("gaming", WINDOW_CAP, env={"OMNI_GL_WINDOW": "1"})
        self.assertIn("-display cocoa", cmd)

    def test_env_still_degrades_when_the_host_cannot(self):
        cmd = _cmd("gaming", NO_CAP, env={"OMNI_GL_WINDOW": "1"})
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


class MacOsAsksForGlTheOnlyWayMacOsCanGiveIt(unittest.TestCase):
    """macOS deprecated OpenGL, so every macOS QEMU that can do GL does it
    through ANGLE, which speaks OpenGL ES and translates to Metal. The option
    is `gl=es`; `gl=on`/`gl=core` refuse or render upside down. Getting this
    wrong makes a correctly-installed virgl QEMU look broken."""

    def test_macos_asks_for_gl_es(self):
        with mock.patch.object(qemu_proc, "_platform_key", return_value="macos"):
            cap = qemu_proc.default_display(
                qemu_display_help="cocoa", qemu_device_help=qemu_proc.GL_GPU_DEVICE)
        self.assertEqual(cap["display_args"], ["-display", "cocoa,gl=es"])

    def test_the_others_ask_for_gl_on(self):
        for key, backend in (("windows", "gtk"), ("linux", "gtk")):
            with mock.patch.object(qemu_proc, "_platform_key", return_value=key):
                cap = qemu_proc.default_display(
                    qemu_display_help=backend,
                    qemu_device_help=qemu_proc.GL_GPU_DEVICE)
            self.assertEqual(cap["display_args"], ["-display", f"{backend},gl=on"], key)

    def test_gl_es_still_counts_as_a_gl_context(self):
        """The one that bites. QEMU REFUSES `-vnc` together with a WINDOWED GL
        context and exits, so a gl=es boot that was not recognised as GL would
        keep -vnc and never start — the same one-line failure that made
        `gaming` exit before vnc_args existed."""
        self.assertTrue(qemu_proc.uses_gl_context(["-display", "cocoa,gl=es"]))
        self.assertTrue(qemu_proc.blocks_vnc(["-display", "cocoa,gl=es"]))
        self.assertEqual(qemu_proc.vnc_args(["-display", "cocoa,gl=es"], 1), [])

    def test_gl_off_is_not_a_gl_context(self):
        self.assertFalse(qemu_proc.uses_gl_context(["-display", "cocoa,gl=off"]))
        self.assertEqual(qemu_proc.vnc_args(["-display", "cocoa,gl=off"], 1),
                         ["-vnc", "127.0.0.1:1"])

    def test_a_plain_backend_is_not_a_gl_context(self):
        self.assertFalse(qemu_proc.uses_gl_context(["-display", "cocoa"]))
        self.assertFalse(qemu_proc.uses_gl_context(["-display", "none"]))


if __name__ == "__main__":
    unittest.main()
