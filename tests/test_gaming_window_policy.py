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

    window_flags() is gated on _WINDOW_FLAG_PLATFORMS (see
    TheFlagsAreGatedToPlatformsThatPresent below for the gates themselves),
    so every test here pins the platform to "windows" -- the one platform
    whose suboptions a real binary has accepted -- to test backend content in
    isolation from the gate. Without this the suite would pass or fail by
    accident depending on which OS runs it, exactly the coupling that made
    the gate's absence hard to catch.
    """

    def _flags(self, backend, platform_key="windows", policy=None):
        with mock.patch.object(qemu_proc, "_platform_key",
                               return_value=platform_key):
            return qemu_proc.window_flags(backend, policy)

    def test_gtk_gets_all_three(self):
        flags = self._flags("gtk")
        self.assertIn("show-menubar=off", flags)
        self.assertIn("window-close=off", flags)
        self.assertIn("zoom-to-fit=on", flags)

    def test_sdl_gets_only_window_close(self):
        flags = self._flags("sdl")
        self.assertIn("window-close=off", flags)
        self.assertNotIn("show-menubar", flags)
        self.assertNotIn("zoom-to-fit", flags)

    def test_cocoas_own_entry_never_carries_window_close(self):
        # The TABLE, not the gated result: macOS is out of
        # _WINDOW_FLAG_PLATFORMS (see the gate tests below), so
        # window_flags("cocoa") is "" on every platform today and could not
        # tell a correct table from an empty one. What must not be lost is
        # WHY cocoa's row is short -- QEMU's cocoa display takes neither
        # window-close nor show-menubar, which is one more reason macOS gets
        # its close behaviour from the QEMU patch rather than from a flag.
        cocoa = qemu_proc._WINDOW_FLAGS["cocoa"]
        self.assertIn("zoom-to-fit=on", cocoa)
        self.assertNotIn("window-close=off", cocoa)
        self.assertNotIn("show-menubar=off", cocoa)

    def test_an_unknown_backend_gets_nothing_rather_than_a_refused_boot(self):
        self.assertEqual(self._flags("wayland-thing"), "")


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


class TheFlagsAreGatedTwice(unittest.TestCase):
    """window_flags() is OUR chrome policy, and two separate gates keep it
    off boots that must not have it.

    PLATFORM (_WINDOW_FLAG_PLATFORMS): QEMU refuses an unknown suboption
    OUTRIGHT rather than ignoring it, so a platform whose backend has never
    been run against a real binary must not be guessed at -- getting it wrong
    does not cost the chrome, it costs the BOOT. Linux's gtk/sdl and macOS's
    cocoa are both unverified; only Windows has run them.

    POLICY: `--gpu window` is specified as "always a visible native window,
    UNSTYLED -- for debugging a GL problem with none of this code in the
    path" (design spec 3d), and flags are this code. It is also the one
    policy `_hide_window_if_wanted` leaves on screen and `view` spawns no bar
    for, so `window-close=off` on it produced a window with an inert X, no
    bar offering the close prompt, and no way to close it at all short of
    `omnidroid stop`.

    `--gpu window` is a real, documented, explicit escape hatch, and unlike
    `auto` -- which _presents_a_window() already keeps off Linux -- it
    reaches default_display() on EVERY platform, Linux included (see
    resolve_gpu_display: the GPU_HEADLESS/no-window-on-auto branch only
    early-returns for GPU_HEADLESS or GPU_AUTO, so GPU_WINDOW falls straight
    through to default_display() regardless of platform).
    """

    def resolve(self, mode, platform_key, cfg=None):
        return _resolve_display(mode, platform_key, cfg)

    def test_explicit_window_policy_on_linux_keeps_qemus_own_argv(self):
        _gpu, display = self.resolve(GAMING, "linux",
                                     {"qemu": {"gpu": "window"}})
        self.assertEqual(display, ["-display", "gtk,gl=on"])

    def test_explicit_window_policy_on_windows_is_unstyled_too(self):
        """The debugging hatch has the LEAST of our behaviour in it, not the
        most. Windows is the one platform whose flags are verified, so this
        is the policy gate on its own with the platform gate satisfied."""
        _gpu, display = self.resolve(GAMING, "windows",
                                     {"qemu": {"gpu": "window"}})
        self.assertEqual(display, ["-display", "gtk,gl=on"])

    def test_the_same_windows_boot_on_auto_DOES_carry_our_flags(self):
        # The control for the test above: without the policy gate firing,
        # this is the styled window the product path uses.
        _gpu, display = self.resolve(GAMING, "windows")
        self.assertIn("show-menubar=off", display[1])
        self.assertIn("window-close=off", display[1])
        self.assertIn("zoom-to-fit=on", display[1])

    def test_macos_gets_qemus_own_argv_until_a_real_mac_binary_accepts_ours(self):
        """Not hypothetical, and not confined to future work: today's
        Homebrew QEMU has no virglrenderer, so a Mac gaming boot takes the
        SOFTWARE window tier -- which briefly emitted `-display
        cocoa,zoom-to-fit=on` where it had always emitted plain `cocoa`. No
        Mac binary in this project has ever been asked whether cocoa accepts
        that suboption, and QEMU refuses an unknown one outright, so the
        downside was every Mac gaming boot failing rather than a plainer
        window. Same reasoning that keeps Linux out, applied consistently."""
        _gpu, display = self.resolve(GAMING, "macos")
        self.assertEqual(display, ["-display", "cocoa,gl=es"])

    def test_macos_still_PRESENTS_a_window_though(self):
        """The two gates are separate constants on purpose. Taking macOS out
        of the FLAG gate must not take it out of the PRESENT one: that would
        change which display a Mac gaming boot picks, which is a policy
        change nobody asked for."""
        self.assertIn("macos", qemu_proc._WINDOW_PRESENT_PLATFORMS)
        self.assertNotIn("macos", qemu_proc._WINDOW_FLAG_PLATFORMS)

    def test_linux_is_in_neither_gate(self):
        self.assertNotIn("linux", qemu_proc._WINDOW_PRESENT_PLATFORMS)
        self.assertNotIn("linux", qemu_proc._WINDOW_FLAG_PLATFORMS)


class RunRecordSaysWhatTheDisplayIs(unittest.TestCase):
    """`view` must not have to re-derive the boot's display policy: the argv
    IS what the process did, so it is read once at spawn and written down."""

    def test_a_gl_window_boot_is_recorded_as_such(self):
        cmd = ["qemu-system-x86_64", "-display",
               "gtk,gl=on,show-menubar=off,window-close=off,zoom-to-fit=on"]
        self.assertEqual(qemu_proc.display_kind(cmd), "gl-window")

    def test_a_software_window_boot_is_recorded_as_a_window(self):
        self.assertEqual(
            qemu_proc.display_kind(["qemu", "-display", "gtk"]), "window")

    def test_a_headless_boot_with_vnc_is_recorded_as_vnc(self):
        self.assertEqual(
            qemu_proc.display_kind(["qemu", "-display", "egl-headless",
                                    "-vnc", "127.0.0.1:1"]), "vnc")

    def test_a_headless_boot_with_no_vnc_is_none(self):
        self.assertEqual(
            qemu_proc.display_kind(["qemu", "-display", "none"]), "none")


if __name__ == "__main__":
    unittest.main()
