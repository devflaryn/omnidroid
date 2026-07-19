#!/usr/bin/env python3
"""Pins the VNC pixel-format decode so it can't silently regress to the R/B swap.

The framebuffer QEMU delivers on this base is RGBX (verified against an
`adb screencap` ground-truth frame: exact match). The code used to decode it as
BGRX, which swaps red and blue — the "red looks purple / colours a bit off" bug.
This test proves the decoder maps a known RGBX byte pattern to the right RGB, and
that the RFB SetPixelFormat request still asks for the shifts that pairing needs.

    python3 tests/test_pixel_format.py
"""
import os
import struct
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                os.pardir, "manager"))

import capture  # noqa: E402


def _pixel(rgbx_bytes):
    """Decode a single 1x1 pixel through the real capture decoder -> (R,G,B)."""
    img = capture._image_from_bgrx(1, 1, bytes(rgbx_bytes))
    return img.getpixel((0, 0))


class PixelFormat(unittest.TestCase):

    def test_red_stays_red_not_blue(self):
        # RGBX byte order: a pure-red pixel is [R=255, G=0, B=0, x=255].
        # The old BGRX decode would read this as blue — the actual bug.
        self.assertEqual(_pixel([255, 0, 0, 255]), (255, 0, 0))

    def test_blue_stays_blue(self):
        self.assertEqual(_pixel([0, 0, 255, 255]), (0, 0, 255))

    def test_green_unaffected(self):
        self.assertEqual(_pixel([0, 255, 0, 255]), (0, 255, 0))

    def test_padding_byte_is_ignored(self):
        # The 4th byte (x) must not bleed into any channel.
        self.assertEqual(_pixel([10, 20, 30, 0]), (10, 20, 30))
        self.assertEqual(_pixel([10, 20, 30, 255]), (10, 20, 30))

    def test_orange_is_orange(self):
        # A representative real-UI colour (Jailbreak's CRIMINAL button). Under
        # the old swap it rendered blue-ish.
        self.assertEqual(_pixel([255, 165, 0, 255]), (255, 165, 0))

    def test_request_shifts_match_the_decoder(self):
        """vncview requests the exact shifts that make QEMU emit what the RGBX
        decoder expects. Measured: changing these makes QEMU emit a different
        byte order the decoder then gets wrong, so request and decode are a
        matched pair — this guards the request half."""
        import vncview
        sent = {}
        client = vncview.RFBClient.__new__(vncview.RFBClient)
        client._send = lambda data: sent.setdefault("pf", data)
        client._wlock = None
        vncview.RFBClient._set_pixel_format(client)
        pf = sent["pf"]
        # message: type(1) + pad(3) + PIXEL_FORMAT(16)
        self.assertEqual(len(pf), 20)
        bpp, depth, big_endian, true_colour = struct.unpack_from("!BBBB", pf, 4)
        r_max, g_max, b_max = struct.unpack_from("!HHH", pf, 8)
        r_shift, g_shift, b_shift = struct.unpack_from("!BBB", pf, 14)
        self.assertEqual((bpp, depth, true_colour), (32, 24, 1))
        self.assertEqual((r_max, g_max, b_max), (255, 255, 255))
        self.assertEqual((r_shift, g_shift, b_shift), (16, 8, 0))


if __name__ == "__main__":
    unittest.main(verbosity=2)
