#!/usr/bin/env python3
"""Pins the auto-screenshot change detector against the cases it exists to tell
apart: ignorable animation (a spinner, a filling progress bar) must NOT produce a
keyframe, while a real UI event (toast, dialog, menu/scene change) must.

KeyframeSelector is pure — it takes a PIL image and returns a verdict — so this
runs offline with no VM, no VNC server and no adb.

    python3 tests/test_keyframe_thresholds.py     (or: pytest tests/)
"""
import math
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                os.pardir, "manager"))

from PIL import Image, ImageDraw  # noqa: E402

import capture  # noqa: E402

W, H = 1280, 720
BG = (20, 20, 24)


def _screen(bg=BG):
    return Image.new("RGB", (W, H), bg)


def _spinner(angle):
    """A loading indicator mid-rotation: the classic 'do not screenshot this'."""
    img = _screen()
    d = ImageDraw.Draw(img)
    cx, cy, r = W // 2, H // 2, 28
    d.arc([cx - r, cy - r, cx + r, cy + r], angle, angle + 90,
          fill=(230, 230, 230), width=6)
    return img


def _loading_bar(pct):
    """A progress bar filling — a bigger animation than a spinner, still noise."""
    img = _screen()
    d = ImageDraw.Draw(img)
    x0, y0, x1, y1 = W // 4, H - 80, W * 3 // 4, H - 66
    d.rectangle([x0, y0, x1, y1], outline=(90, 90, 90))
    d.rectangle([x0, y0, x0 + int((x1 - x0) * pct), y1], fill=(120, 200, 120))
    return img


def _popup(frac):
    """A centered dialog covering `frac` of the screen area."""
    img = _screen()
    d = ImageDraw.Draw(img)
    side_w = int(math.sqrt(frac * W * H * (W / H)))
    side_h = int(side_w * H / W)
    x0, y0 = (W - side_w) // 2, (H - side_h) // 2
    d.rectangle([x0, y0, x0 + side_w, y0 + side_h], fill=(245, 245, 245))
    return img


def _menu():
    img = Image.new("RGB", (W, H), (40, 90, 160))
    d = ImageDraw.Draw(img)
    for i in range(6):
        d.rectangle([60, 60 + i * 100, W - 60, 140 + i * 100],
                    fill=(230, 230, 240))
    return img


def _change_vs_idle(img):
    """% of pixels `img` changes relative to a settled idle screen — i.e. the
    magnitude the detector actually sees for this single event, measured from a
    fresh selector so no earlier frame is baked into the comparison."""
    sel = capture.KeyframeSelector()
    sel.consider(_screen())          # baseline = the idle screen
    return sel.consider(img)["changed_percent"]


class KeyframeThresholds(unittest.TestCase):

    def setUp(self):
        self.sel = capture.KeyframeSelector()

    def feed(self, img):
        return self.sel.consider(img)

    def test_first_frame_is_the_baseline(self):
        d = self.feed(_screen((0, 0, 0)))
        self.assertTrue(d["keep"])
        self.assertEqual(d["reason"], "baseline")

    def test_spinner_rotation_never_keyframes(self):
        """The whole point of diffing against the last KEPT frame: a looping
        animation never drifts far from what was saved, so it stays invisible."""
        self.feed(_screen())                       # baseline
        for angle in (0, 45, 90, 135, 180, 225, 270, 315, 0, 45):
            d = self.feed(_spinner(angle))
            self.assertFalse(d["keep"],
                             f"spinner @{angle} produced a keyframe "
                             f"({d['changed_percent']}% changed)")

    def test_loading_bar_filling_never_keyframes(self):
        self.feed(_screen())                       # baseline
        for pct in (0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0):
            d = self.feed(_loading_bar(pct))
            self.assertFalse(d["keep"],
                             f"loading bar @{pct:.0%} produced a keyframe "
                             f"({d['changed_percent']}% changed)")

    def test_popup_dialog_is_kept(self):
        """A 6%-of-screen dialog is a real event. It was silently dropped by the
        old 8%/14.0 defaults — that regression is what this pins."""
        self.feed(_screen())                       # baseline
        d = self.feed(_popup(0.06))
        self.assertTrue(d["keep"],
                        f"6% popup was dropped ({d['changed_percent']}% changed, "
                        f"mean {d['diff_score']})")

    def test_small_toast_is_kept(self):
        self.feed(_screen())
        d = self.feed(_popup(0.02))
        self.assertTrue(d["keep"],
                        f"2% toast was dropped ({d['changed_percent']}% changed)")

    def test_menu_change_is_kept(self):
        self.feed(_screen())
        d = self.feed(_menu())
        self.assertTrue(d["keep"])
        self.assertEqual(d["reason"], "scene_change")

    def test_black_transitions_are_kept_both_ways(self):
        """Boot screen -> content -> black (app died?) -> content. Both edges
        must land even if the diff itself is small."""
        self.feed(_menu())
        d = self.feed(_screen((0, 0, 0)))
        self.assertTrue(d["keep"])
        self.assertTrue(d["black_screen"])
        d = self.feed(_menu())
        self.assertTrue(d["keep"])
        self.assertFalse(d["black_screen"])

    def test_threshold_separates_animation_noise_from_real_events(self):
        """The tuning rationale itself, asserted rather than asserted-by-comment:
        the scene threshold must sit strictly ABOVE the loudest ignorable
        animation and strictly BELOW the quietest event we promise to catch.

        Both bounds are measured here, so if a future change to the detector (or
        to the sample downscale) moves either one past the threshold, this fails
        and the defaults get re-measured instead of nudged."""
        noise = max(_change_vs_idle(_loading_bar(p))
                    for p in (0.1, 0.5, 0.9, 1.0))
        noise = max(noise, *(_change_vs_idle(_spinner(a))
                             for a in (0, 90, 180, 270)))
        quietest_real = min(_change_vs_idle(_popup(f)) for f in (0.02, 0.06))

        threshold = capture.DEFAULT_CHANGE_PERCENT
        self.assertLess(noise, threshold,
                        f"animation noise ({noise}%) reaches the scene "
                        f"threshold ({threshold}%) — spinners would keyframe")
        self.assertLess(threshold, quietest_real,
                        f"scene threshold ({threshold}%) is above the quietest "
                        f"real event ({quietest_real}%) — popups would be dropped")


if __name__ == "__main__":
    unittest.main(verbosity=2)
