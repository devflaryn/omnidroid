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
        """...and hides it BY PID, on a short timeout.

        Two defects in one call, both already fixed in `cmd_view` and never
        carried across to here. hostwin matches the window TITLE as a
        SUBSTRING, so "omni-farm3" also matches "omni-farm30": with several
        instances up, the X on one bar hid somebody else's window. And
        hide_qemu_window's default timeout is 20 s, spent on this bar's own
        UI thread inside a Tk callback -- an unrepaintable, unclickable bar
        for twenty seconds in exactly the case where there is no window to
        find. WindowBar.__init__ has stored `self.pid` since it was written
        and this is what it was stored for."""
        bar = windowbar.WindowBar("omni-farm3", pid=4242)
        with mock.patch.object(windowbar, "_ask_close",
                               return_value="hide"), \
             mock.patch("omnidroid.hostwin.hide_qemu_window") as hide:
            self.assertEqual(bar.on_close(), "hide")
        hide.assert_called_once_with("omni-farm3", pid=4242, timeout=2)

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


class FakeFollowUser32(FakeUser32):
    """FakeUser32 plus the three window-state queries the follow machinery
    asks: IsWindow, IsIconic, and (through `_window_rect`) GetWindowRect."""

    def __init__(self, alive=True, iconic=False):
        super().__init__()
        self.alive = alive
        self.iconic = iconic

    def IsWindow(self, _hwnd):
        return 1 if self.alive else 0

    def IsIconic(self, _hwnd):
        return 1 if self.iconic else 0


class TheBarFollowsTheWindow(unittest.TestCase):
    """`follow()` used to be called ONCE, at startup, and never again.

    There was no location hook and no poll, so resizing the guest window left
    the bar at the old width and position for the rest of the instance's
    life. The spec asked for SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE);
    the ruling was a bounded `root.after` poll instead -- no cross-process
    callback to marshal into Tk's event loop, and entirely adequate for a
    title bar. FOLLOW_POLL_MS is the interval.
    """

    def _bar(self, owner_rect, u=None):
        bar = windowbar.WindowBar("omni-farm3", pid=4242)
        bar.bar_hwnd, bar.owner_hwnd = 11, 99
        bar.synced_owner_rect = tuple(owner_rect)
        return bar, u or FakeFollowUser32()

    def test_a_resized_guest_window_drags_the_bar_with_it(self):
        start = (100, 200, 1280, 800)
        grown = (100, 200, 1600, 900)
        bar, u = self._bar(start)
        rects = {"owner": grown}
        with mock.patch.object(windowbar, "_user32", return_value=u), \
             mock.patch.object(windowbar, "_window_rect",
                               side_effect=lambda h: rects["owner"] if h == 99
                               else (100, 166, 1600, 34)):
            self.assertTrue(bar.poll_follow())
        # The bar was moved to the NEW width, not left at the old one.
        self.assertEqual(u.positions[0][3], 1600)
        self.assertEqual(bar.synced_owner_rect, grown)

    def test_an_unmoved_window_costs_one_getwindowrect_and_no_move(self):
        # The poll runs ~16 times a second for the life of the instance; the
        # idle tick has to be a read and nothing else.
        rect = (100, 200, 1280, 800)
        bar, u = self._bar(rect)
        with mock.patch.object(windowbar, "_user32", return_value=u), \
             mock.patch.object(windowbar, "_window_rect", return_value=rect):
            self.assertFalse(bar.poll_follow())
        self.assertEqual(u.positions, [])

    def test_a_minimised_guest_is_not_followed_to_its_parking_spot(self):
        # A minimised window's GetWindowRect is Windows' off-screen parking
        # position (-32000, -32000), not where it will be on restore. The bar
        # minimises with its owner anyway (that is what ownership buys), so
        # there is nothing to follow.
        u = FakeFollowUser32(iconic=True)
        bar, u = self._bar((100, 200, 1280, 800), u)
        with mock.patch.object(windowbar, "_user32", return_value=u), \
             mock.patch.object(windowbar, "_window_rect",
                               return_value=(-32000, -32000, 160, 28)):
            self.assertFalse(bar.poll_follow())
        self.assertEqual(u.positions, [])

    def test_a_bar_that_was_never_owned_polls_nothing(self):
        bar = windowbar.WindowBar("omni-farm3")
        self.assertFalse(bar.poll_follow())

    def test_the_poll_interval_is_bounded_and_sane(self):
        # Not an arbitrary pin: a poll this cheap has to stay under a frame
        # at 60Hz to look attached, and must not become a busy loop.
        self.assertGreaterEqual(windowbar.FOLLOW_POLL_MS, 16)
        self.assertLessEqual(windowbar.FOLLOW_POLL_MS, 100)


class DraggingTheBarMovesTheComposite(unittest.TestCase):
    """The bar IS the title bar, so dragging it has to move the window.

    It is also the ONLY way the window can be moved: apply_chrome strips
    WS_CAPTION *and* WS_SYSMENU from QEMU's window, which leaves nothing to
    drag and no Alt+Space -> Move either. Before this, the guest window could
    not be moved at all and our bar could be dragged off on its own and never
    came back.
    """

    def _dragged(self, owner_rect, bar_rect, u=None):
        bar = windowbar.WindowBar("omni-farm3")
        bar.bar_hwnd, bar.owner_hwnd = 11, 99
        bar.synced_owner_rect = tuple(owner_rect)
        u = u or FakeFollowUser32()
        rects = {11: bar_rect, 99: owner_rect}
        with mock.patch.object(windowbar, "_user32", return_value=u), \
             mock.patch.object(windowbar, "_window_rect",
                               side_effect=lambda h: rects[h]):
            moved = bar.drag_owner_to_bar()
        return bar, u, moved

    def test_the_guest_window_moves_by_the_same_delta_as_the_bar(self):
        owner = (100, 200, 1280, 800)
        # Bar dragged 40 right and 15 down from where it belongs
        # (owner_top - bar_height = 200 - 34 = 166).
        bar_rect = (140, 181, 1280, 34)
        bar, u, moved = self._dragged(owner, bar_rect)
        self.assertTrue(moved)
        self.assertEqual(len(u.positions), 1)
        hwnd, x, y, _cx, _cy = u.positions[0]
        self.assertEqual(hwnd, 99, "the OWNER is what moves, not the bar")
        self.assertEqual((x, y), (140, 215))

    def test_the_bars_actual_height_is_what_belonging_is_measured_against(self):
        """Windows enforces its own minimum caption height (measured 40px
        against a requested 34) and follow() already lands the bar's BOTTOM
        edge on the owner's TOP edge. Measuring against BAR_HEIGHT instead
        would read that 6px correction as a drag and walk the guest window
        down the screen a few pixels per tick, forever."""
        owner = (100, 200, 1280, 800)
        settled = (100, 160, 1280, 40)      # bottom edge == owner top, 40 tall
        _bar, u, moved = self._dragged(owner, settled)
        self.assertFalse(moved)
        self.assertEqual(u.positions, [])

    def test_the_owners_new_position_is_recorded_so_the_poll_does_not_fight(self):
        # Without this the poll reads the move the drag just made as one it
        # has to chase, and yanks the bar back mid-drag.
        owner = (100, 200, 1280, 800)
        bar, _u, _moved = self._dragged(owner, (140, 181, 1280, 34))
        self.assertEqual(bar.synced_owner_rect, (140, 215, 1280, 800))

    def test_an_unowned_bar_drags_nothing(self):
        bar = windowbar.WindowBar("omni-farm3")
        self.assertFalse(bar.drag_owner_to_bar())


class OwnershipFailureIsLoud(unittest.TestCase):
    """`bar.own()`'s return value was discarded.

    On failure `self.bar_hwnd` is never set, so follow() returns False on its
    first line and the bar sits at Tk's own default size -- reproducing the
    exact 216x239 square task-9-report.md had just fixed -- AND the bar is
    not owned, so `stop` destroying QEMU's window no longer takes it with it
    and the user is left with an orphan captioning nothing. A failed
    GetAncestor is already a hard failure with its own exit code for
    precisely those reasons; this is the same failure one call later.
    """

    def test_a_failed_own_returns_false_rather_than_pretending(self):
        class Boom:
            def SetWindowLongPtrW(self, *_a):
                raise OSError("no")

        bar = windowbar.WindowBar("omni-farm3")
        with mock.patch.object(windowbar, "_user32", return_value=Boom()):
            self.assertFalse(bar.own(11, 99))
        self.assertIsNone(bar.bar_hwnd)

    def test_run_window_bar_exits_5_and_says_so_when_ownership_fails(self):
        if not windowbar.IS_WINDOWS:
            self.skipTest("run_window_bar returns 3 off Windows")
        import io
        created = {}

        class FakeRoot:
            def destroy(self):
                created["destroyed"] = True

        err = io.StringIO()
        with mock.patch("omnidroid.hostwin.find_window", return_value=99), \
             mock.patch.object(windowbar, "_create_bar_window",
                               return_value=(FakeRoot(), 11)), \
             mock.patch.object(windowbar.WindowBar, "own",
                               return_value=False), \
             mock.patch.object(windowbar.sys, "stderr", err):
            rc = windowbar.run_window_bar("omni-farm3", pid=4242)
        self.assertEqual(rc, 5)
        self.assertTrue(created.get("destroyed"),
                        "a bar that could not be owned must not be left open")
        self.assertIn("own", err.getvalue().lower())

    def test_the_four_failure_codes_are_all_distinct(self):
        # 2 no window, 3 wrong platform, 4 no handle, 5 handle but no
        # ownership. Each needs a different answer, so none may collide.
        self.assertEqual(len({2, 3, 4, 5}), 4)


class TheFirstAlignmentUsesTheOwnerHandle(unittest.TestCase):
    """MEASURED on real windows, 2026-08-16.

    `run_window_bar` used to take its first rect from
    `hostwin.window_geometry(identity)`, which re-finds the window BY TITLE
    -- and hostwin matches the title as a SUBSTRING, while our own window by
    then is called "omni: <identity>", which CONTAINS <identity>. So it could
    hand back the BAR's own rect and follow() aligned the bar to itself: a
    216px-wide strip, the exact shape of the 216x239 bug, and now that the
    bar drags the composite, the guest window got pulled under it too.
    Reproduced end to end against a real stand-in guest window.

    `view` always passes the QEMU pid, which filters that collision out, so
    this never fired in the product -- it fired the moment anything called
    run_window_bar the way its own signature says it may.
    """

    def test_it_never_re_finds_the_window_by_title(self):
        if not windowbar.IS_WINDOWS:
            self.skipTest("run_window_bar returns 3 off Windows")
        followed = []

        class FakeRoot:
            def destroy(self):
                pass

            def protocol(self, *_a):
                pass

            def bind(self, *_a):
                pass

            def after(self, *_a):
                pass

            def mainloop(self):
                pass

        with mock.patch("omnidroid.hostwin.find_window", return_value=99), \
             mock.patch("omnidroid.hostwin.window_geometry") as by_title, \
             mock.patch("omnidroid.hostwin.apply_dwm_style"), \
             mock.patch.object(windowbar, "_create_bar_window",
                               return_value=(FakeRoot(), 11)), \
             mock.patch.object(windowbar, "_window_rect",
                               side_effect=lambda h: (5, 6, 700, 400)
                               if h == 99 else None), \
             mock.patch.object(windowbar.WindowBar, "follow",
                               side_effect=lambda r: followed.append(r)):
            windowbar.run_window_bar("omni-farm3")
        by_title.assert_not_called()
        # ...and the rect it aligned to came off the OWNER handle (99).
        self.assertEqual(followed, [(5, 6, 700, 400)])


class TheBarCarriesTheDwmStyling(unittest.TestCase):
    """Design spec 3a/3b: QEMU's window gives up its whole caption, so the
    strip is the window that HAS one -- "DWM styling (dark mode, rounded
    corners, border colour) therefore applies to the strip". None of it
    existed: there was no DwmSetWindowAttribute anywhere in the tree.
    """

    def test_run_window_bar_styles_the_bar_not_the_guest_window(self):
        if not windowbar.IS_WINDOWS:
            self.skipTest("run_window_bar returns 3 off Windows")
        styled = []

        class FakeRoot:
            def destroy(self):
                pass

            def protocol(self, *_a):
                pass

            def bind(self, *_a):
                pass

            def after(self, *_a):
                pass

            def mainloop(self):
                pass

        with mock.patch("omnidroid.hostwin.find_window", return_value=99), \
             mock.patch("omnidroid.hostwin.window_geometry",
                        return_value=None), \
             mock.patch("omnidroid.hostwin.apply_dwm_style",
                        side_effect=lambda h, **k: styled.append(h)), \
             mock.patch.object(windowbar, "_create_bar_window",
                               return_value=(FakeRoot(), 11)), \
             mock.patch.object(windowbar.WindowBar, "own", return_value=True):
            rc = windowbar.run_window_bar("omni-farm3", pid=4242)
        self.assertEqual(rc, 0)
        # 11 is the BAR's hwnd; 99 is QEMU's window, which has no caption
        # left for a dark caption to apply to.
        self.assertEqual(styled, [11])


if __name__ == "__main__":
    unittest.main()
