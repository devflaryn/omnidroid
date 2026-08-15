#!/usr/bin/env python3
"""The strip is OWNED BY QEMU's window, and that direction is the design.

    python3 -m pytest tests/test_windowbar.py -q

Today (embedview.py, deleted by this work) QEMU's window is a CHILD of our
viewer, and Windows destroys a child with its parent: a force-killed viewer
took the guest's display with it permanently -- instance alive, answering adb,
totalFrames = 0. An OWNED window has the properties we want and not that one:

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


if __name__ == "__main__":
    unittest.main()
