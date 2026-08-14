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

IT IS OFF BY DEFAULT, and that is the single most important thing these tests
pin. On this QEMU/ANGLE build the guest renders into a host GL texture that is
never read back into the 2D surface the VNC server publishes, so `omnidroid
view` shows a BLACK SCREEN while the guest is drawing perfectly well.
Measured on one host, same image, same account, only the flag differing:

    headless_gl on    VNC framebuffer (0,0) on every channel      [black]
    headless_gl off   VNC framebuffer (0,255) on every channel    [content]

QEMU's own `screendump` had content either way, which is what pins it on the
GL->VNC readback rather than on the guest. It shipped default-ON for a few
hours on the strength of a claim that was never tested — every screenshot
taken while developing it came from `adb exec-out screencap`, which reads
Android's compositor and looks identical either way.

The properties worth pinning are the ones that would silently break the
product rather than fail loudly:

  * it stays OFF unless asked for, so the viewer works,
  * it must degrade, never fail, on a QEMU without the pieces (the Homebrew
    macOS build has neither), and
  * egl-headless must still count as HEADLESS — read as "a window opened",
    `start` stands its VNC viewer down and the user can see nothing at all.
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
        self.assertEqual(cap["gpu_args"], ["-device", "virtio-gpu-gl-pci"])

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
        mode = {"window": False}
        with mock.patch.dict(os.environ, env or {}, clear=False), \
             mock.patch.object(qemu_proc, "_qemu_help_texts",
                               return_value=help_texts):
            if not env:
                os.environ.pop("OMNI_HEADLESS_GL", None)
            return qemu_proc.resolve_gpu_display(mode, interactive,
                                                 "qemu-system-x86_64", cfg)

    def test_it_is_OFF_by_default_because_it_blacks_out_the_viewer(self):
        """The default is a MEASUREMENT, not caution.

        egl-headless renders into a host GL texture this QEMU/ANGLE build never
        reads back into the 2D surface VNC publishes, so `omnidroid view` shows
        a black screen while the guest draws normally. Measured on one host,
        same image, only this flag differing: VNC framebuffer (0,0) on every
        channel with it on, (0,255) with it off, and QEMU's own screendump had
        content either way.
        """
        gpu, display = self._pair(WINDOWS_QEMU)
        self.assertEqual(display, qemu_proc.HEADLESS_DISPLAY_ARGS)
        self.assertEqual(gpu, qemu_proc.HEADLESS_GPU_ARGS)

    def test_a_capable_host_gets_the_gpu_when_asked(self):
        gpu, display = self._pair(WINDOWS_QEMU, cfg={"qemu": {"headless_gl": True}})
        self.assertEqual(display, ["-display", "egl-headless"])
        self.assertEqual(gpu, ["-device", "virtio-gpu-gl-pci"])

    def test_an_incapable_host_gets_exactly_what_it_had_before(self):
        gpu, display = self._pair(BREW_MAC_QEMU)
        self.assertEqual(display, qemu_proc.HEADLESS_DISPLAY_ARGS)
        self.assertEqual(gpu, qemu_proc.HEADLESS_GPU_ARGS)

    def test_a_builder_boot_stays_on_the_plain_path(self):
        # Builder/maintenance boots exist to mutate an image, not to draw, and
        # must behave identically on every host.
        gpu, display = self._pair(WINDOWS_QEMU, interactive=True)
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


if __name__ == "__main__":
    unittest.main()
