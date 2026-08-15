#!/usr/bin/env python3
"""Headless instances render on the HOST GPU when the host's QEMU can.

    python3 tests/test_headless_gpu.py

Every production instance is headless — no window, VNC only — and that used to
mean software rendering, unconditionally. Measured in-guest on the x86 base
before this change:

    GLES: Mesa, llvmpipe            (the CPU drawing every frame)

and after, on the same machine (Windows, QEMU 11.0.50, RTX 4060):

    GLES: Mesa, virgl (ANGLE (NVIDIA, NVIDIA GeForce RTX 4060 ...))

`-display egl-headless` is what makes that possible: a real host GL context
with NO window, so virglrenderer can use the GPU.

ITS DEFAULT IS PER-PLATFORM, and that is the single most important thing these
tests pin. `-display egl-headless` gives the guest a GL context everywhere, but
on Windows the frames never reach a scanout. MEASURED 2026-08-15, three boots
(plain, blob=true+hostmem=512M, and without the forced `video=` mode), all
identical:

    dmesg           [drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1203
                                                               (command 0x103)
    timestats       totalFrames = 0
    screencap       solid black
    VNC             1 update, mean brightness 0.0

0x103 is SET_SCANOUT and 0x1203 is ERR_INVALID_RESOURCE_ID: QEMU refuses to
scan out the buffer the guest offers. The guest's GL was fine — SurfaceFlinger
came up on `virgl (ANGLE (NVIDIA ... RTX 4060))` with no GL errors in logcat —
so this is presentation, not rendering. Linux is the platform egl-headless was
written for and keeps the default ON there.

The properties worth pinning are the ones that would silently break the
product rather than fail loudly:

  * the default follows HEADLESS_GL_PRESENTS, so a host that cannot present
    does not come up black,
  * an explicit setting wins in BOTH directions (that is how the measurement
    above gets re-taken on a newer QEMU),
  * it must degrade, never fail, on a QEMU without the pieces (the Homebrew
    macOS build has neither),
  * egl-headless must still count as HEADLESS — read as "a window opened",
    `start` stands its VNC viewer down and the user can see nothing at all, and
  * `-vnc` must SURVIVE it: egl-headless is the display QEMU documents as the
    one to pair with VNC, and dropping the server there is what produced the
    original "black viewer" report.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import qemu_proc  # noqa: E402

# What each host's QEMU actually answered when asked (-display help /
# -device help). Not invented: these are the two builds in play.
WINDOWS_QEMU = ("none\ngtk\nsdl\negl-headless\ncurses\nspice-app\ndbus\n",
                'name "virtio-gpu-gl-pci", bus PCI, alias "virtio-gpu-gl"\n'
                'name "virtio-gpu-pci", bus PCI, alias "virtio-gpu"\n')
BREW_MAC_QEMU = ("none\ncurses\ncocoa\ndbus\n",
                 'name "virtio-gpu-pci", bus PCI, alias "virtio-gpu"\n')


class Capability(unittest.TestCase):
    def test_a_qemu_with_both_pieces_can_do_it(self):
        cap = qemu_proc.headless_gl_capability(*WINDOWS_QEMU)
        self.assertTrue(cap["available"])
        self.assertEqual(cap["display_args"], ["-display", "egl-headless"])
        # The panel size is named explicitly: the device's own mode list
        # starts at 640x480 and the guest takes the first entry, so a GL boot
        # came up at 640x480 while the software path gave 1280x800.
        self.assertEqual(cap["gpu_args"],
                         ["-device", "virtio-gpu-gl-pci,xres=1280,yres=800"])

    def test_a_qemu_without_virglrenderer_cannot(self):
        cap = qemu_proc.headless_gl_capability(*BREW_MAC_QEMU)
        self.assertFalse(cap["available"])
        # The reason has to name what is missing: "unavailable" sends someone
        # hunting through their guest instead of their QEMU build.
        self.assertIn("virtio-gpu-gl-pci", cap["reason"])

    def test_a_qemu_that_could_not_be_asked_at_all_is_not_available(self):
        # _qemu_help_texts returns ("", "") on any failure — a missing binary
        # must read as "no GPU acceleration", never as "yes".
        self.assertFalse(qemu_proc.headless_gl_capability("", "")["available"])


class Resolution(unittest.TestCase):
    def _pair(self, help_texts, interactive=False, cfg=None, env=None):
        """(gpu_args, display_args) for a HEADLESS boot on a faked host."""
        mode = {}
        with mock.patch.dict(os.environ, env or {}, clear=False), \
             mock.patch.object(qemu_proc, "_qemu_help_texts",
                               return_value=help_texts):
            if not env:
                os.environ.pop("OMNI_HEADLESS_GL", None)
            for stale in ("OMNI_GL_WINDOW", "OMNI_PANEL", "OMNI_GPU_OPTS",
                          "OMNI_NO_WINDOW"):
                os.environ.pop(stale, None)
            # This class is about the HEADLESS tier only. Under the default
            # `auto` policy a host that cannot present a windowless GL guest
            # falls through to a native window, which is a different question
            # (tests/test_gaming_mode.py owns it).
            os.environ.setdefault("OMNI_GPU", qemu_proc.GPU_HEADLESS)
            return qemu_proc.resolve_gpu_display(mode, interactive,
                                                 "qemu-system-x86_64", cfg)

    def test_the_default_follows_what_this_platform_can_present(self):
        """The default is a MEASUREMENT, not caution. See the module docstring:
        on Windows an egl-headless guest renders and never scans out."""
        gpu, display = self._pair(WINDOWS_QEMU)
        if qemu_proc.headless_gl_presents():
            self.assertEqual(display, ["-display", "egl-headless"])
        else:
            self.assertEqual(display, qemu_proc.HEADLESS_DISPLAY_ARGS)
            self.assertEqual(gpu, qemu_proc.HEADLESS_GPU_ARGS)

    def test_windows_is_pinned_OFF_and_linux_ON(self):
        # Pinned as a table rather than as behaviour, so changing it is a
        # deliberate edit next to the measurement that justifies it.
        self.assertFalse(qemu_proc.HEADLESS_GL_PRESENTS["windows"])
        self.assertTrue(qemu_proc.HEADLESS_GL_PRESENTS["linux"])

    def test_a_capable_host_gets_the_gpu_when_asked(self):
        gpu, display = self._pair(WINDOWS_QEMU, cfg={"qemu": {"headless_gl": True}})
        self.assertEqual(display, ["-display", "egl-headless"])
        self.assertEqual(gpu, ["-device", "virtio-gpu-gl-pci,xres=1280,yres=800"])

    def test_an_incapable_host_gets_exactly_what_it_had_before(self):
        gpu, display = self._pair(BREW_MAC_QEMU,
                                  cfg={"qemu": {"headless_gl": True}})
        self.assertEqual(display, qemu_proc.HEADLESS_DISPLAY_ARGS)
        self.assertEqual(gpu, qemu_proc.HEADLESS_GPU_ARGS)

    def test_a_builder_boot_stays_on_the_plain_path(self):
        # Builder/maintenance boots exist to mutate an image, not to draw, and
        # must behave identically on every host.
        gpu, display = self._pair(WINDOWS_QEMU, interactive=True,
                                  cfg={"qemu": {"headless_gl": True}})
        self.assertEqual(display, qemu_proc.HEADLESS_DISPLAY_ARGS)
        self.assertEqual(gpu, qemu_proc.HEADLESS_GPU_ARGS)

    def test_config_can_turn_it_on(self):
        cfg = {"qemu": {"headless_gl": True}}
        gpu, display = self._pair(WINDOWS_QEMU, cfg=cfg)
        self.assertEqual(display, ["-display", "egl-headless"])

    def test_env_can_turn_it_on_without_touching_config(self):
        gpu, display = self._pair(WINDOWS_QEMU, env={"OMNI_HEADLESS_GL": "1"})
        self.assertEqual(display, ["-display", "egl-headless"])

    def test_env_can_turn_it_off_over_a_config_that_enables_it(self):
        cfg = {"qemu": {"headless_gl": True}}
        gpu, display = self._pair(WINDOWS_QEMU, cfg=cfg,
                                  env={"OMNI_HEADLESS_GL": "0"})
        self.assertEqual(display, qemu_proc.HEADLESS_DISPLAY_ARGS)

    def test_an_incapable_host_ignores_the_request_entirely(self):
        """Asking for GPU rendering on a QEMU that cannot do it must degrade,
        never produce args that make QEMU exit instead of booting."""
        gpu, display = self._pair(BREW_MAC_QEMU, cfg={"qemu": {"headless_gl": True}})
        self.assertEqual(display, qemu_proc.HEADLESS_DISPLAY_ARGS)
        self.assertEqual(gpu, qemu_proc.HEADLESS_GPU_ARGS)


class VncSurvivesHeadlessGl(unittest.TestCase):
    """The bug that cost the viewer, pinned so it cannot come back.

    QEMU refuses `-vnc` beside a WINDOWED display that has taken a GL context.
    It does NOT refuse egl-headless — that display exists to be paired with
    vnc/spice. Treating the two cases the same is what left every
    GPU-accelerated boot with no VNC server at all, which then read as "the
    viewer is black"."""

    def test_egl_headless_keeps_the_vnc_server(self):
        self.assertEqual(
            qemu_proc.vnc_args(["-display", "egl-headless"], 101),
            ["-vnc", "127.0.0.1:101"])

    def test_a_windowed_gl_display_still_drops_it(self):
        for d in ("gtk,gl=on", "sdl,gl=on", "cocoa,gl=es"):
            self.assertEqual(qemu_proc.vnc_args(["-display", d], 101), [], d)

    def test_a_windowed_display_without_gl_keeps_it(self):
        self.assertEqual(qemu_proc.vnc_args(["-display", "gtk"], 101),
                         ["-vnc", "127.0.0.1:101"])

    def test_plain_headless_keeps_it(self):
        self.assertEqual(qemu_proc.vnc_args(["-display", "none"], 101),
                         ["-vnc", "127.0.0.1:101"])

    def test_blocks_vnc_is_not_the_same_question_as_uses_gl_context(self):
        egl = ["-display", "egl-headless"]
        self.assertTrue(qemu_proc.uses_gl_context(egl))
        self.assertFalse(qemu_proc.blocks_vnc(egl))


class StillHeadless(unittest.TestCase):
    def test_egl_headless_does_not_count_as_a_window(self):
        cmd = ["qemu", "-display", "egl-headless", "-vnc", "127.0.0.1:1"]
        self.assertFalse(qemu_proc.command_opens_a_window(cmd))

    def test_a_real_window_still_counts(self):
        self.assertTrue(qemu_proc.command_opens_a_window(
            ["qemu", "-display", "gtk,gl=on"]))
        self.assertTrue(qemu_proc.command_opens_a_window(
            ["qemu", "-display", "cocoa"]))

    def test_display_none_is_still_headless(self):
        self.assertFalse(qemu_proc.command_opens_a_window(
            ["qemu", "-display", "none"]))


class TheViewerSaysWhyItIsGone(unittest.TestCase):
    """A GPU boot that took a window has no VNC server, and the message the
    user gets has to say THAT.

    Without it, `omnidroid view` fails with "the VNC port did not open in
    time", which points at a timeout, a firewall or a slow boot -- anything
    except the one thing that is true. Same for the app's View button, which
    calls `view --json`.
    """

    def setUp(self):
        from omnidroid import engine
        self.engine = engine

    def _reason(self, run):
        with mock.patch.object(self.engine, "_run_record", return_value=run):
            return self.engine.vnc_unavailable_reason("u1")

    def test_a_windowed_gl_boot_explains_itself(self):
        why = self._reason({"native_window": True, "gpu": "gl"})
        self.assertIsNotNone(why)
        # It has to name the escape hatch and the thing that still works.
        self.assertIn("--gpu headless", why)
        self.assertIn("screenshot", why)

    def test_a_headless_gpu_boot_has_nothing_to_explain(self):
        self.assertIsNone(self._reason({"native_window": False, "gpu": "gl"}))

    def test_a_software_boot_has_nothing_to_explain(self):
        self.assertIsNone(self._reason({"native_window": False,
                                        "gpu": "software"}))

    def test_a_windowed_boot_without_gl_keeps_its_server(self):
        # tier "window" is a native window with software rendering: no GL
        # context, so QEMU has no objection to -vnc and the viewer works.
        self.assertIsNone(self._reason({"native_window": True,
                                        "gpu": "software"}))

    def test_an_unknown_instance_does_not_invent_a_reason(self):
        self.assertIsNone(self._reason({}))


if __name__ == "__main__":
    unittest.main()
