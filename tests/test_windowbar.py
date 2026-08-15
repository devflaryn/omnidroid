#!/usr/bin/env python3
"""The strip is OWNED BY QEMU's window, and that direction is the design.

    python3 -m pytest tests/test_windowbar.py -q

Before this work QEMU's window was made a CHILD of a viewer (the embedded
viewer, now deleted), and Windows destroys a child with its parent: a
force-killed viewer took the guest's display with it permanently -- instance
alive, answering adb, totalFrames = 0. An OWNED window has the properties we
want and not that one:

  * it always floats above its owner (z-order solved without polling)
  * it minimises and restores with its owner
  * destroying it does NOTHING to the owner

So the strip is owned BY the guest window, never the other way round. These
tests pin the direction, because getting it backwards reintroduces exactly the
failure this design exists to remove.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import windowbar


class FakeUser32:
    def __init__(self):
        self.owner_calls = []
        self.positions = []

    def SetWindowLongPtrW(self, hwnd, index, value):
        self.owner_calls.append((hwnd, index, value))
        return 1

    def SetWindowPos(self, hwnd, after, x, y, cx, cy, flags):
        self.positions.append((hwnd, x, y, cx, cy))
        return 1


class OwnershipDirection(unittest.TestCase):

    def test_the_bar_is_owned_by_the_guest_window(self):
        u = FakeUser32()
        bar = windowbar.WindowBar("omni-farm3")
        with mock.patch.object(windowbar, "_user32", return_value=u):
            self.assertTrue(bar.own(bar_hwnd=11, owner_hwnd=99))
        self.assertEqual(u.owner_calls,
                         [(11, windowbar.GWLP_HWNDPARENT, 99)])

    def test_it_is_never_the_other_way_round(self):
        u = FakeUser32()
        bar = windowbar.WindowBar("omni-farm3")
        with mock.patch.object(windowbar, "_user32", return_value=u):
            bar.own(bar_hwnd=11, owner_hwnd=99)
        for hwnd, index, value in u.owner_calls:
            self.assertNotEqual(
                (hwnd, value), (99, 11),
                "the GUEST window must never be owned by the bar -- that is "
                "the relationship that lets a dead viewer blind the guest")


class BarGeometry(unittest.TestCase):

    def test_the_bar_sits_directly_above_the_window_and_matches_its_width(self):
        x, y, w, h = windowbar.bar_geometry((100, 200, 1280, 800),
                                            bar_height=34)
        self.assertEqual((x, w, h), (100, 1280, 34))
        self.assertEqual(y, 200 - 34)

    def test_a_window_at_the_top_of_the_screen_does_not_get_a_negative_y(self):
        _x, y, _w, _h = windowbar.bar_geometry((0, 10, 640, 480),
                                               bar_height=34)
        self.assertGreaterEqual(y, 0)


class FollowReadsBackReality(unittest.TestCase):
    """`follow()` no longer trusts its own SetWindowPos request.

    Measured on real hardware (task-9-report.md's "Geometry fix"): Windows
    will not shrink a WS_CAPTION window below its own minimum caption
    height, so a request for BAR_HEIGHT (34) came back 40px tall. `y` was
    computed assuming the REQUESTED height, so the bar's bottom edge
    overlapped the guest window's top edge by the 6px difference -- a small
    version of the same class of bug as the 216x239 square (a mismatch
    between what was asked for and what Windows actually did). follow() now
    reads the real result back and, if it differs, repositions once more so
    the bar's bottom lands exactly on the guest's top edge no matter what
    floor this machine's DPI/theme enforces.
    """

    def test_no_correction_when_windows_granted_the_exact_height(self):
        u = FakeUser32()
        bar = windowbar.WindowBar("omni-farm3")
        bar.bar_hwnd = 11
        owner_rect = (100, 200, 1280, 800)
        _x, y, _w, h = windowbar.bar_geometry(owner_rect)
        with mock.patch.object(windowbar, "_user32", return_value=u), \
             mock.patch.object(windowbar, "_window_rect",
                               return_value=(100, y, 1280, h)):
            self.assertTrue(bar.follow(owner_rect))
        # Exactly one SetWindowPos: the initial request, no correction.
        self.assertEqual(len(u.positions), 1)

    def test_a_taller_than_requested_bar_is_repositioned_not_left_overlapping(self):
        u = FakeUser32()
        bar = windowbar.WindowBar("omni-farm3")
        bar.bar_hwnd = 11
        owner_rect = (100, 200, 1280, 800)  # owner top = 200
        x, y, w, h = windowbar.bar_geometry(owner_rect)
        self.assertEqual(h, 34)
        granted_height = 40  # Windows' floor, taller than the 34 requested
        with mock.patch.object(windowbar, "_user32", return_value=u), \
             mock.patch.object(windowbar, "_window_rect",
                               return_value=(x, y, w, granted_height)):
            self.assertTrue(bar.follow(owner_rect))
        self.assertEqual(len(u.positions), 2)
        # The correction: bottom of the bar (new_y + granted_height) must
        # equal the owner's top edge (200) exactly -- no overlap.
        _hwnd, corrected_x, corrected_y, _cx, _cy = u.positions[1]
        self.assertEqual(corrected_x, x)
        self.assertEqual(corrected_y + granted_height, owner_rect[1])

    def test_a_read_back_failure_still_leaves_the_first_move_in_place(self):
        u = FakeUser32()
        bar = windowbar.WindowBar("omni-farm3")
        bar.bar_hwnd = 11
        owner_rect = (100, 200, 1280, 800)
        with mock.patch.object(windowbar, "_user32", return_value=u), \
             mock.patch.object(windowbar, "_window_rect", return_value=None):
            self.assertTrue(bar.follow(owner_rect))
        self.assertEqual(len(u.positions), 1)


# Style bits Tk's default toplevel carries before anything strips them:
# WS_VISIBLE|WS_CLIPSIBLINGS|WS_CLIPCHILDREN|WS_CAPTION|WS_SYSMENU|
# WS_MINIMIZEBOX|WS_MAXIMIZEBOX|WS_THICKFRAME. Matches this box's own
# pre-strip measurement (0x16cf0008) from the geometry experiments below.
_WS_CAPTION = 0x00C00000
_WS_SYSMENU = 0x00080000
_WS_MINIMIZEBOX = 0x00020000
_TK_DEFAULT_TOPLEVEL_STYLE = 0x16CF0008


class FakeStyleUser32:
    """A user32 stand-in that actually tracks GWL_STYLE, for
    _strip_resize_border -- FakeUser32 above only records ownership/position
    calls, not style bits."""

    def __init__(self, style):
        self.style = style
        self.setwindowpos_calls = []

    def GetWindowLongPtrW(self, hwnd, index):
        return self.style

    def SetWindowLongPtrW(self, hwnd, index, value):
        self.style = value
        return 1

    def SetWindowPos(self, hwnd, after, x, y, cx, cy, flags):
        self.setwindowpos_calls.append((x, y, cx, cy, flags))
        return 1


class ResizeBorderStrip(unittest.TestCase):
    """`_strip_resize_border` replaced Tk's `wm resizable(False, False)` --
    see its docstring for the measured bug that call caused (the strip
    rendering as a ~216x239 square instead of a thin strip): that Tk call
    does not just remove these two style bits, it also locks the window's
    min/max track size to whatever Tk's own default size was at that
    instant, and the raw SetWindowPos in follow() loses to that lock for the
    rest of the window's life. Stripping the same bits by hand never
    installs that lock.
    """

    def test_strips_thickframe_and_maximizebox(self):
        u = FakeStyleUser32(_TK_DEFAULT_TOPLEVEL_STYLE)
        with mock.patch.object(windowbar, "_user32", return_value=u):
            self.assertTrue(windowbar._strip_resize_border(hwnd=11))
        self.assertFalse(u.style & windowbar.WS_THICKFRAME)
        self.assertFalse(u.style & windowbar.WS_MAXIMIZEBOX)

    def test_caption_sysmenu_and_minimizebox_survive(self):
        # These are what give the bar its own real title bar (close box,
        # system menu, minimise) -- only the SIZING chrome goes.
        u = FakeStyleUser32(_TK_DEFAULT_TOPLEVEL_STYLE)
        with mock.patch.object(windowbar, "_user32", return_value=u):
            windowbar._strip_resize_border(hwnd=11)
        self.assertTrue(u.style & _WS_CAPTION)
        self.assertTrue(u.style & _WS_SYSMENU)
        self.assertTrue(u.style & _WS_MINIMIZEBOX)

    def test_the_result_matches_the_hardware_measured_bar_style(self):
        # task-9-report.md, Step 3: 0x16CA0008, decoded there as
        # WS_VISIBLE|WS_CLIPSIBLINGS|WS_CLIPCHILDREN|WS_CAPTION|WS_SYSMENU|
        # WS_MINIMIZEBOX. Stripping by hand must produce byte-for-byte the
        # same style Tk's resizable(False, False) produced -- this changes
        # HOW the bits are stripped, never WHAT the bar looks like.
        u = FakeStyleUser32(_TK_DEFAULT_TOPLEVEL_STYLE)
        with mock.patch.object(windowbar, "_user32", return_value=u):
            windowbar._strip_resize_border(hwnd=11)
        self.assertEqual(u.style, 0x16CA0008)

    def test_frame_changed_is_signalled_so_windows_redraws_the_caption(self):
        u = FakeStyleUser32(_TK_DEFAULT_TOPLEVEL_STYLE)
        with mock.patch.object(windowbar, "_user32", return_value=u):
            windowbar._strip_resize_border(hwnd=11)
        self.assertEqual(len(u.setwindowpos_calls), 1)
        _x, _y, _cx, _cy, flags = u.setwindowpos_calls[0]
        self.assertTrue(flags & windowbar.SWP_FRAMECHANGED)

    def test_a_failing_call_is_a_false_not_an_exception(self):
        with mock.patch.object(windowbar, "_user32",
                               side_effect=OSError("boom")):
            self.assertFalse(windowbar._strip_resize_border(hwnd=11))


class RealWindowGeometryRegression(unittest.TestCase):
    """The bar_geometry() unit tests above pin the pure arithmetic and
    passed throughout the measured 216x239 bug, because nothing about them
    touches a real window -- the bug lived entirely in how Tk's
    `resizable(False, False)` interacts with a REAL window's
    WM_GETMINMAXINFO handling. This test creates one, drives it through
    `_create_bar_window` + `WindowBar.follow()` -- the exact functions
    `run_window_bar` calls -- and reads the result back with the real
    Win32 GetWindowRect, the same measurement task-9-report.md used on
    hardware.

    Skips itself where this process cannot open a real window at all (no
    interactive window station) rather than failing -- this is the one test
    in the suite that cannot be satisfied with a fake.
    """

    def test_a_real_bar_window_lands_on_the_real_target_rect(self):
        if not windowbar.IS_WINDOWS:
            self.skipTest("windowbar owns nothing outside Windows")
        import ctypes

        try:
            root, bar_hwnd = windowbar._create_bar_window(
                "omni: geometry-regression-test")
        except Exception as e:      # noqa: BLE001
            self.skipTest(f"could not create a real window here: {e}")
            return
        try:
            self.assertIsNotNone(
                bar_hwnd, "GetAncestor could not resolve a top-level hwnd "
                "for a freshly created Tk toplevel -- see run_window_bar's "
                "return code 4")

            bar = windowbar.WindowBar("omni-geometry-regression-test")
            bar.bar_hwnd = bar_hwnd  # what own() would have set; ownership
                                      # itself is pinned elsewhere

            owner_rect = (156, 122, 656, 800)  # matches task-9-report.md
            target_x, _naive_y, target_w, _h = windowbar.bar_geometry(
                owner_rect)
            self.assertTrue(bar.follow(owner_rect))
            root.update_idletasks()

            class RECT(ctypes.Structure):
                _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                            ("right", ctypes.c_long),
                            ("bottom", ctypes.c_long)]

            rect = RECT()
            ok = ctypes.windll.user32.GetWindowRect(bar_hwnd,
                                                     ctypes.byref(rect))
            self.assertTrue(ok)
            actual_x, actual_y = rect.left, rect.top
            actual_w = rect.right - rect.left
            actual_h = rect.bottom - rect.top

            # x and width are the whole point of "same width, directly
            # above" and must be exact.
            self.assertEqual(actual_x, target_x)
            self.assertEqual(actual_w, target_w)
            # y is NOT required to equal bar_geometry()'s naive
            # owner_top - BAR_HEIGHT: follow() reads back whatever height
            # Windows actually granted (its own WS_CAPTION minimum may
            # exceed the requested BAR_HEIGHT -- measured 40px on real
            # hardware) and repositions so the bar's BOTTOM edge lands
            # exactly on the guest's top edge instead. That is the
            # invariant that actually matters: no overlap, no gap.
            self.assertEqual(actual_y + actual_h, owner_rect[1])
            # Height: Windows enforces its own minimum caption height and
            # this module does not fight it (see _strip_resize_border), so
            # it may exceed BAR_HEIGHT slightly -- but nowhere NEAR the
            # measured 239px bug, which is what this bound catches.
            self.assertGreater(actual_h, 0)
            self.assertLess(
                actual_h, 100,
                f"bar height regressed toward the measured 216x239-square "
                f"bug (task-9-report.md): got {actual_h}px")
        finally:
            root.destroy()


class ClosePrompt(unittest.TestCase):

    def test_hide_hides_the_window_and_leaves_the_instance_running(self):
        bar = windowbar.WindowBar("omni-farm3")
        with mock.patch.object(windowbar, "_ask_close",
                               return_value="hide"), \
             mock.patch("omnidroid.hostwin.hide_qemu_window") as hide:
            self.assertEqual(bar.on_close(), "hide")
        hide.assert_called_once_with("omni-farm3")

    def test_stop_calls_the_stop_hook(self):
        stopped = []
        bar = windowbar.WindowBar("omni-farm3",
                                  on_stop=lambda name: stopped.append(name))
        with mock.patch.object(windowbar, "_ask_close", return_value="stop"):
            self.assertEqual(bar.on_close(), "stop")
        self.assertEqual(stopped, ["omni-farm3"])

    def test_cancel_does_nothing_at_all(self):
        stopped = []
        bar = windowbar.WindowBar("omni-farm3",
                                  on_stop=lambda name: stopped.append(name))
        with mock.patch.object(windowbar, "_ask_close",
                               return_value="cancel"), \
             mock.patch("omnidroid.hostwin.hide_qemu_window") as hide:
            self.assertEqual(bar.on_close(), "cancel")
        hide.assert_not_called()
        self.assertEqual(stopped, [])

    def test_a_failed_stop_is_flagged_and_never_reported_as_success(self):
        def boom(_name):
            raise RuntimeError("could not kill the process")

        bar = windowbar.WindowBar("omni-farm3", on_stop=boom)
        self.assertFalse(bar.stop_failed)
        with mock.patch.object(windowbar, "_ask_close", return_value="stop"):
            # The CHOICE is still reported as "stop" -- on_close() never
            # lies about which button was pressed -- but the failure must be
            # discoverable, or a caller has no way to tell this apart from a
            # real stop.
            self.assertEqual(bar.on_close(), "stop")
        self.assertTrue(bar.stop_failed)

    def test_a_system_exit_from_the_hook_is_caught_not_let_through(self):
        # cmd_stop ends in sys.exit(1) on a failed shutdown, and
        # load_account (called first) does sys.exit(str) on a bad account --
        # both raise SystemExit, which a bare `except Exception` does NOT
        # catch. Uncaught, it would escape on_close, Tkinter would re-raise
        # it out of mainloop(), and this detached, console-less process
        # would simply vanish -- indistinguishable from a successful stop on
        # an instance that may still be running with several GB attached.
        def boom(_name):
            raise SystemExit(1)

        bar = windowbar.WindowBar("omni-farm3", on_stop=boom)
        with mock.patch.object(windowbar, "_ask_close", return_value="stop"):
            # Must not raise SystemExit out of on_close, and must still
            # report the CHOICE the user made.
            self.assertEqual(bar.on_close(), "stop")
        self.assertTrue(bar.stop_failed)

    def test_a_keyboard_interrupt_from_the_hook_still_propagates(self):
        # The fix for SystemExit must not become a bare `except:` -- Ctrl-C
        # during a stop has to actually interrupt the process.
        def boom(_name):
            raise KeyboardInterrupt()

        bar = windowbar.WindowBar("omni-farm3", on_stop=boom)
        with mock.patch.object(windowbar, "_ask_close", return_value="stop"):
            with self.assertRaises(KeyboardInterrupt):
                bar.on_close()


if __name__ == "__main__":
    unittest.main()
