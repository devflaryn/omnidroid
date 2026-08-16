#!/usr/bin/env python3
"""Host display capability detection and the window-vs-headless decision.

    python3 tests/test_gpu_display.py

Gaming mode wants the lowest-latency window the HOST can actually give it;
farming mode and every host that cannot open a window must keep today's
headless path byte-for-byte.

There are THREE tiers, not two, and the middle one is the whole reason this
is not a boolean. MEASURED on the dev Mac (2026-08-06, Homebrew QEMU 11.0.2,
Apple Silicon):

    $ qemu-system-aarch64 -display cocoa,gl=on
    qemu-system-aarch64: OpenGL support was not enabled in this build of QEMU
    $ qemu-system-aarch64 -device help | grep gpu
    name "virtio-gpu-pci", bus PCI, alias "virtio-gpu"      # no -gl variant

So `gl` is unavailable on the primary host today, while a NATIVE COCOA WINDOW
is available right now — and a native window is already the big input-latency
win over the VNC path (no framebuffer encode/decode round trip; host events go
straight to the guest's usb-tablet/usb-kbd). Collapsing "no virgl" to
"headless" would throw that away.

`default_display` reports what the host can do (pure — every host fact is
passed in). `gpu_display_args` decides what to hand QEMU and must NEVER raise:
a detection bug has to cost a window, never a boot.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402
from omnidroid import qemu_proc  # noqa: E402

# The platform flags are patched on qemu_proc, not on engine. `engine` does
# `from omnidroid.qemu_proc import *`, so engine.default_display IS
# qemu_proc.default_display and it resolves IS_MACOS in QEMU_PROC's globals —
# patching engine.IS_MACOS rebinds a name the function never reads, which
# silently passes on a macOS host and silently lies everywhere else.

# Real `-display help` / `-device help` shapes.
MAC_DISPLAY_HELP = "none\ncurses\ncocoa\ndbus\n"          # the actual Mac build
GL_DISPLAY_HELP = "none\ncocoa\ngtk\nsdl\nvnc\negl-headless\n"
GL_DEVICE_HELP = "virtio-gpu-pci\nvirtio-gpu-gl-pci\nvirtio-vga\n"
NO_GL_DEVICE_HELP = "virtio-gpu-device\nvirtio-gpu-pci\n"  # the actual Mac build

HEADLESS_GPU = ["-device", "virtio-gpu-pci"]
HEADLESS_DISPLAY = ["-display", "none"]


def _mac():
    return mock.patch.multiple(qemu_proc, IS_MACOS=True, IS_LINUX=False,
                               IS_WINDOWS=False)


def _linux():
    return mock.patch.multiple(qemu_proc, IS_MACOS=False, IS_LINUX=True,
                               IS_WINDOWS=False)


class DetectsTheGlTier(unittest.TestCase):
    def test_macos_with_a_gl_build_is_accelerated(self):
        """macOS asks for gl=ES, and that is not a detail.

        This test asserted `cocoa,gl=on` until 2026-08-15, which was wrong and
        had never been exercised because no macOS host here had a GL-capable
        QEMU to try it on. macOS DEPRECATED OpenGL in favour of Metal: every
        macOS QEMU that can do GL does it through ANGLE, which speaks OpenGL ES
        and translates to Metal. `gl=on`/`gl=core` refuse or render upside
        down. Had this shipped, the first Mac to get a virgl QEMU would have
        looked like "GPU acceleration does not work on macOS"."""
        with _mac():
            cap = omni.default_display(GL_DISPLAY_HELP, GL_DEVICE_HELP,
                                       has_gui=True)
        self.assertEqual(cap["tier"], "gl")
        self.assertIn("cocoa,gl=es", " ".join(cap["display_args"]))
        self.assertNotIn("gl=on", " ".join(cap["display_args"]))
        self.assertIn("virtio-gpu-gl-pci", " ".join(cap["gpu_args"]))

    def test_linux_with_a_gl_build_uses_gtk_or_sdl(self):
        with _linux():
            cap = omni.default_display(GL_DISPLAY_HELP, GL_DEVICE_HELP,
                                       has_gui=True)
        self.assertEqual(cap["tier"], "gl")
        self.assertRegex(" ".join(cap["display_args"]), r"gtk,gl=on|sdl,gl=on")


class DetectsTheWindowTier(unittest.TestCase):
    """The tier the primary host actually has today."""

    def test_macos_without_virgl_still_gets_a_native_window(self):
        """...and gets it with QEMU's OWN argv, no suboptions of ours.

        THIS IS TODAY'S MAC, not a hypothetical one: Homebrew's QEMU has no
        virglrenderer, so a Mac gaming boot lands here, on the software
        window tier. Between 2026-08-15 and 2026-08-16 this test asserted
        `zoom-to-fit=on` was appended, which meant every Mac gaming boot
        started emitting `-display cocoa,zoom-to-fit=on` where it had always
        emitted plain `cocoa`. QEMU refuses an unknown suboption OUTRIGHT,
        and no Mac binary in this project has ever been asked whether cocoa
        takes that one -- so the downside was not a plainer window, it was
        every Mac gaming boot failing to start. macOS is out of
        _WINDOW_FLAG_PLATFORMS until a real Mac QEMU has accepted it.
        """
        with _mac():
            cap = omni.default_display(MAC_DISPLAY_HELP, NO_GL_DEVICE_HELP,
                                       has_gui=True)
        self.assertEqual(cap["tier"], "window")
        self.assertEqual(cap["display_args"], ["-display", "cocoa"])
        self.assertEqual(cap["gpu_args"], HEADLESS_GPU)

    def test_the_reason_names_the_missing_piece(self):
        # The user has to be able to act on this: it is the difference between
        # "your Mac cannot do it" and "your QEMU was built without OpenGL".
        with _mac():
            cap = omni.default_display(MAC_DISPLAY_HELP, NO_GL_DEVICE_HELP,
                                       has_gui=True)
        self.assertIn("virtio-gpu-gl", cap["reason"])

    def test_linux_without_virgl_still_gets_a_native_window(self):
        with _linux():
            cap = omni.default_display("none\ngtk\nsdl\nvnc\n",
                                       NO_GL_DEVICE_HELP, has_gui=True)
        self.assertEqual(cap["tier"], "window")
        self.assertRegex(" ".join(cap["display_args"]), r"gtk|sdl")


class DegradesToHeadless(unittest.TestCase):
    def test_a_headless_host_session_has_no_window_tier(self):
        with _mac():
            cap = omni.default_display(GL_DISPLAY_HELP, GL_DEVICE_HELP,
                                       has_gui=False)
        self.assertEqual(cap["tier"], "none")
        self.assertTrue(cap["reason"])

    def test_a_build_with_no_windowing_backend_has_none(self):
        with _mac():
            cap = omni.default_display("none\ncurses\nvnc\n",
                                       GL_DEVICE_HELP, has_gui=True)
        self.assertEqual(cap["tier"], "none")

    def test_empty_help_text_reads_as_none_not_as_assume_it_works(self):
        # _qemu_help_texts returns ("", "") whenever the probe fails.
        with _mac():
            cap = omni.default_display("", "", has_gui=True)
        self.assertEqual(cap["tier"], "none")


class GpuDisplayArgs(unittest.TestCase):
    GL = {"available": True, "tier": "gl",
          "display_args": ["-display", "cocoa,gl=on"],
          "gpu_args": ["-device", "virtio-gpu-gl-pci"], "reason": "ok"}
    WINDOW = {"available": True, "tier": "window",
              "display_args": ["-display", "cocoa"],
              "gpu_args": ["-device", "virtio-gpu-pci"], "reason": "no virgl"}
    NONE = {"available": False, "tier": "none", "display_args": [],
            "gpu_args": [], "reason": "no host GUI"}

    def test_wanted_and_gl_available_is_accelerated(self):
        gpu, disp = omni.gpu_display_args(True, self.GL)
        self.assertEqual(gpu, ["-device", "virtio-gpu-gl-pci"])
        self.assertEqual(disp, ["-display", "cocoa,gl=on"])

    def test_wanted_and_window_available_opens_the_window(self):
        gpu, disp = omni.gpu_display_args(True, self.WINDOW)
        self.assertEqual(disp, ["-display", "cocoa"])

    def test_wanted_but_nothing_available_degrades_to_headless(self):
        gpu, disp = omni.gpu_display_args(True, self.NONE)
        self.assertEqual(gpu, HEADLESS_GPU)
        self.assertEqual(disp, HEADLESS_DISPLAY)

    def test_not_wanted_is_headless_even_when_a_window_is_available(self):
        for cap in (self.GL, self.WINDOW):
            gpu, disp = omni.gpu_display_args(False, cap)
            self.assertEqual(gpu, HEADLESS_GPU)
            self.assertEqual(disp, HEADLESS_DISPLAY)

    def test_malformed_capability_degrades_instead_of_raising(self):
        for junk in ({}, None, "yes", 7, {"available": True}, {"tier": "gl"}):
            gpu, disp = omni.gpu_display_args(True, junk)
            self.assertEqual(gpu, HEADLESS_GPU,
                             f"junk capability {junk!r} must degrade")
            self.assertEqual(disp, HEADLESS_DISPLAY)


class HostGuiProbe(unittest.TestCase):
    def test_macos_always_has_a_gui(self):
        with _mac():
            self.assertTrue(qemu_proc._host_has_gui())

    def test_a_headless_linux_session_has_none(self):
        with _linux(), mock.patch.dict(os.environ, {}, clear=True):
            self.assertFalse(qemu_proc._host_has_gui())

    def test_linux_under_wayland_has_one(self):
        with _linux(), mock.patch.dict(
                os.environ, {"WAYLAND_DISPLAY": "wayland-0"}, clear=True):
            self.assertTrue(qemu_proc._host_has_gui())


class QemuHelpProbe(unittest.TestCase):
    def setUp(self):
        # The probe is MEMOISED per process (every headless boot asks it now,
        # and two QEMU launches per boot would be pure waste). Tests that
        # exercise the probe itself must start from a cold cache or they read
        # back whatever an earlier test cached.
        qemu_proc._HELP_CACHE.clear()

    tearDown = setUp

    def test_a_failing_probe_returns_empty_strings_not_an_exception(self):
        with mock.patch.object(qemu_proc, "qemu_bin",
                               side_effect=OSError("no qemu")):
            self.assertEqual(qemu_proc._qemu_help_texts("qemu-system-aarch64"),
                             ("", ""))

    def test_the_answer_is_asked_for_once_and_reused(self):
        with mock.patch.object(qemu_proc, "qemu_bin", side_effect=lambda t: t), \
             mock.patch.object(qemu_proc.subprocess, "run",
                               return_value=mock.Mock(stdout="cocoa\n")) as run:
            first = qemu_proc._qemu_help_texts("qemu-system-aarch64")
            second = qemu_proc._qemu_help_texts("qemu-system-aarch64")
        self.assertEqual(first, second)
        self.assertEqual(run.call_count, 2)      # -display help, -device help

    def test_a_failure_is_not_cached(self):
        """A QEMU that is still being installed by `setup` must not be
        remembered as "has no GPU" for the rest of the process."""
        with mock.patch.object(qemu_proc, "qemu_bin",
                               side_effect=OSError("not installed yet")):
            self.assertEqual(qemu_proc._qemu_help_texts("qemu-system-x86_64"),
                             ("", ""))
        with mock.patch.object(qemu_proc, "qemu_bin", side_effect=lambda t: t), \
             mock.patch.object(qemu_proc.subprocess, "run",
                               return_value=mock.Mock(stdout="egl-headless\n")):
            self.assertIn("egl-headless",
                          qemu_proc._qemu_help_texts("qemu-system-x86_64")[0])


if __name__ == "__main__":
    unittest.main()
