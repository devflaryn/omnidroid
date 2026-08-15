#!/usr/bin/env python3
"""Gaming presents in a window; farming renders without one.

    python3 -m pytest tests/test_gaming_window_policy.py -q

The performance profile must NOT take the windowless GL pair even where one is
available. On Linux `egl-headless` presents, so `auto` used to resolve there
and gaming paid a readback + RFB encode + Python RFB decode per frame. The
window costs the VNC server and buys zero copies and native input.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import qemu_proc

# Includes "cocoa" alongside gtk/sdl/egl-headless so the same fixture can
# stand in for whichever platform _platform_key() is patched to -- the tests
# below only vary the platform, not the QEMU build.
DISPLAY_HELP = "none\ngtk\nsdl\ncocoa\negl-headless\ncurses\ndbus\n"
DEVICE_HELP = 'name "virtio-gpu-gl-pci", bus PCI, alias "virtio-gpu-gl"\n'

GAMING = {"profile": "performance", "gpu": "auto", "panel": (1280, 800)}
FARMING = {"profile": "density", "gpu": "auto", "panel": (640, 480)}


def _resolve_display(mode, platform_key, cfg=None):
    """Shared by every TestCase below that needs a fully-mocked
    resolve_gpu_display() call. A module-level function rather than a method
    repeated on each class, so the mock.patch block that makes this file's
    tests deterministic (fixed platform, fixed `-display help`/`-device
    help`, a GUI host, a clean environ) exists in exactly one place.
    """
    with mock.patch.object(qemu_proc, "_platform_key",
                           return_value=platform_key), \
         mock.patch.object(qemu_proc, "_qemu_help_texts",
                           return_value=(DISPLAY_HELP, DEVICE_HELP)), \
         mock.patch.object(qemu_proc, "_host_has_gui", return_value=True), \
         mock.patch.dict(os.environ, {}, clear=True):
        return qemu_proc.resolve_gpu_display(mode, False,
                                              "qemu-system-x86_64", cfg)


class ProfileDecidesTheDisplay(unittest.TestCase):

    def resolve(self, mode, platform_key, cfg=None):
        return _resolve_display(mode, platform_key, cfg)

    def test_gaming_on_linux_keeps_egl_headless_until_a_host_verifies_it(self):
        """DEFERRED, not a design decision reversed.

        Linux is the one platform whose egl-headless actually presents, so
        gaming works there today via VNC -- at the cost of a readback, an RFB
        encode and a Python RFB decode per frame. The window is better and the
        spec says so, but switching it blind would trade a working copy path
        for an unrun one AND drop the VNC server (QEMU refuses -vnc beside a GL
        window), leaving a Linux user with a raw QEMU frame and no viewer.

        Flip this by adding "linux" to _WINDOW_PRESENT_PLATFORMS once a Linux
        host has run it.
        """
        _gpu, display = self.resolve(GAMING, "linux")
        self.assertEqual(display, ["-display", "egl-headless"])

    def test_gaming_on_windows_takes_a_gl_window(self):
        _gpu, display = self.resolve(GAMING, "windows")
        self.assertIn("gl=on", display[1])

    def test_gaming_on_macos_takes_cocoa_with_gl_es(self):
        _gpu, display = self.resolve(GAMING, "macos")
        self.assertTrue(display[1].startswith("cocoa,"), display[1])
        self.assertIn("gl=es", display[1])
        self.assertNotIn("gl=on", display[1])

    def test_farming_still_prefers_the_windowless_pair(self):
        _gpu, display = self.resolve(FARMING, "linux")
        self.assertEqual(display, ["-display", "egl-headless"])

    def test_a_gaming_pair_blocks_vnc_and_a_farming_pair_does_not(self):
        # Gaming resolves on "windows" here, not "linux": Linux is the
        # deferred platform (see the test above) and its gaming pair is
        # STILL egl-headless, which does not block vnc -- asserting True
        # there would contradict the deferred test in this same file. A
        # window-presenting platform is what this assertion is about.
        _gpu, gaming = self.resolve(GAMING, "windows")
        _gpu, farming = self.resolve(FARMING, "linux")
        self.assertTrue(qemu_proc.blocks_vnc(gaming))
        self.assertFalse(qemu_proc.blocks_vnc(farming))


class TheProfileOverridesAnExplicitHeadlessGlRequest(unittest.TestCase):
    """The only place this predicate actually changes today's behaviour.

    HEADLESS_GL_PRESENTS already defaults `_headless_gl_wanted` to False on
    Windows/macOS, so gaming's `auto` was already landing on the window there
    without this predicate -- the five tests above hold on both the old and
    the new code for that reason. The gap they do not cover is an EXPLICIT
    `qemu.headless_gl: true` (the escape hatch that re-takes the presentation
    measurement on a newer QEMU, see HEADLESS_GL_PRESENTS): before this
    change gaming obeyed it like any other mode and could come up on a
    non-presenting egl-headless context; now the profile wins and gaming
    still takes the window.
    """

    def resolve(self, mode, platform_key):
        return _resolve_display(mode, platform_key,
                                {"qemu": {"headless_gl": True}})

    def test_gaming_still_takes_the_window_on_windows(self):
        _gpu, display = self.resolve(GAMING, "windows")
        self.assertIn("gl=on", display[1])
        self.assertNotEqual(display, ["-display", "egl-headless"])

    def test_gaming_still_takes_the_window_on_macos(self):
        _gpu, display = self.resolve(GAMING, "macos")
        self.assertIn("gl=es", display[1])
        self.assertNotEqual(display, ["-display", "egl-headless"])

    def test_farming_still_honours_the_explicit_request(self):
        # `density` is not gated: an explicit override still reaches the
        # windowless pair for farming, exactly as it did before.
        _gpu, display = self.resolve(FARMING, "windows")
        self.assertEqual(display, ["-display", "egl-headless"])


class WindowFlagsAreBackendSpecific(unittest.TestCase):
    """QEMU rejects an unknown suboption outright, so this cannot be one list.

    -display gtk  takes show-menubar, window-close, zoom-to-fit
    -display sdl  takes window-close only
    -display cocoa takes zoom-to-fit only -- no window-close, which is one
    more reason macOS gets its close behaviour from the QEMU patch.
    """

    def test_gtk_gets_all_three(self):
        flags = qemu_proc.window_flags("gtk")
        self.assertIn("show-menubar=off", flags)
        self.assertIn("window-close=off", flags)
        self.assertIn("zoom-to-fit=on", flags)

    def test_sdl_gets_only_window_close(self):
        flags = qemu_proc.window_flags("sdl")
        self.assertIn("window-close=off", flags)
        self.assertNotIn("show-menubar", flags)
        self.assertNotIn("zoom-to-fit", flags)

    def test_cocoa_never_gets_window_close(self):
        flags = qemu_proc.window_flags("cocoa")
        self.assertIn("zoom-to-fit=on", flags)
        self.assertNotIn("window-close", flags)
        self.assertNotIn("show-menubar", flags)

    def test_an_unknown_backend_gets_nothing_rather_than_a_refused_boot(self):
        self.assertEqual(qemu_proc.window_flags("wayland-thing"), "")


class TheFlagsReachTheCommand(unittest.TestCase):

    def resolve(self, mode, platform_key):
        return _resolve_display(mode, platform_key)

    def test_a_gaming_gtk_boot_carries_our_flags(self):
        _gpu, display = self.resolve(GAMING, "windows")
        self.assertIn("show-menubar=off", display[1])
        self.assertIn("window-close=off", display[1])
        self.assertIn("zoom-to-fit=on", display[1])

    def test_it_is_still_recognised_as_a_gl_boot(self):
        _gpu, display = self.resolve(GAMING, "windows")
        self.assertTrue(qemu_proc.uses_gl_context(display))
        self.assertTrue(qemu_proc.blocks_vnc(display))
        self.assertTrue(qemu_proc.command_opens_a_window(
            ["-display", display[1]]))


if __name__ == "__main__":
    unittest.main()
