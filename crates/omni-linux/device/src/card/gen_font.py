#!/usr/bin/env python3
"""Generate `crates/omni-linux/device/card/font.bin`, the crash card's font (`crate::card`).

    python gen_font.py <DroidSansMono.ttf> ../../card/font.bin

The font is the Android image's own `/system/fonts/DroidSansMono.ttf` (Digitized data copyright
2007, Google Corporation; Apache License 2.0), rasterized by Pillow's FreeType at two sizes:

* body:  20 px, a 12 x 24 cell (advance 0.6 em; ascent 19 + descent 5);
* title: 40 px, a 24 x 48 cell (ascent 38 + descent 10).

Each of ASCII 32..126 is drawn on an 8-bit coverage image at its cell's origin (the ascender line,
Pillow's anchor "la"), cropped to the pixels it covers, and kept as 4-bit coverage (16 levels: no
step is visible at these sizes, and it halves the file). The format, every number little-endian:

    b"OCF1"                                      magic
    u8 faces                                     2: body, then title
    per face:
      u8 cell_w, u8 cell_h, u8 first, u8 count   first = 32, count = 95
      count x (i8 x, i8 y, u8 w, u8 h)           the glyph's bitmap: its top-left in the cell, its size
      u32 bytes                                  the bitmaps' length
      the bitmaps, in glyph order                each w*h 4-bit values, rows packed together, the high
                                                 nibble first; a bitmap starts on a byte

A glyph may reach outside its cell by a pixel (`_` does, either side), which is why x and y are
signed.
"""
import struct
import sys

from PIL import Image, ImageDraw, ImageFont

FIRST, COUNT = 32, 95
SIZES = (20, 40)
PAD = 8


def face(path, size):
    font = ImageFont.truetype(path, size)
    ascent, descent = font.getmetrics()
    cell_w, cell_h = round(font.getlength("M")), ascent + descent
    assert all(round(font.getlength(chr(c))) == cell_w for c in range(FIRST, FIRST + COUNT)), "not monospaced"
    records, bitmaps = b"", b""
    for c in range(FIRST, FIRST + COUNT):
        img = Image.new("L", (cell_w + 2 * PAD, cell_h + 2 * PAD), 0)
        ImageDraw.Draw(img).text((PAD, PAD), chr(c), font=font, fill=255, anchor="la")
        box = img.point(lambda v: 255 if round(v * 15 / 255) else 0).getbbox()
        if box is None:
            records += struct.pack("<bbBB", 0, 0, 0, 0)
            continue
        left, top, right, bottom = box
        w, h = right - left, bottom - top
        records += struct.pack("<bbBB", left - PAD, top - PAD, w, h)
        values = [round(v * 15 / 255) for v in img.crop(box).getdata()]
        if len(values) % 2:
            values.append(0)
        bitmaps += bytes((values[i] << 4) | values[i + 1] for i in range(0, len(values), 2))
    return struct.pack("<BBBB", cell_w, cell_h, FIRST, COUNT) + records + struct.pack("<I", len(bitmaps)) + bitmaps


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    out = b"OCF1" + struct.pack("<B", len(SIZES)) + b"".join(face(sys.argv[1], s) for s in SIZES)
    with open(sys.argv[2], "wb") as f:
        f.write(out)
    print(f"{sys.argv[2]}: {len(out)} bytes")


if __name__ == "__main__":
    main()
