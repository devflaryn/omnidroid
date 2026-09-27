#!/usr/bin/env python3
"""Colour histogram of an omnidroid display screenshot (8-bit RGBA PNG, as `hal::framebuffer`
writes one): what a claim about a screenshot is checked against.

    python tools/shot_stats.py <png> [<png>...]

Prints, per file: size, the share of near-black pixels (all channels < 16), the number of distinct
colours (quantised to 5 bits a channel), the share of the five commonest colours, and the mean of
each channel. Stdlib only.
"""
import struct
import sys
import zlib
from collections import Counter


def rgba_rows(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    at, idat, width, height, ctype = 8, b"", 0, 0, 6
    while at < len(data):
        (n,) = struct.unpack(">I", data[at : at + 4])
        kind, body = data[at + 4 : at + 8], data[at + 8 : at + 8 + n]
        if kind == b"IHDR":
            width, height, depth, ctype = struct.unpack(">IIBB", body[:10])
            assert depth == 8 and ctype in (2, 6), "8-bit RGB/RGBA only"
        elif kind == b"IDAT":
            idat += body
        at += 12 + n
    raw = zlib.decompress(idat)
    bpp = 4 if ctype == 6 else 3
    stride = width * bpp
    prev = bytearray(stride)
    rows = []
    for y in range(height):
        f = raw[y * (stride + 1)]
        line = bytearray(raw[y * (stride + 1) + 1 : (y + 1) * (stride + 1)])
        for i in range(stride):
            a = line[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            if f == 1:
                line[i] = (line[i] + a) & 255
            elif f == 2:
                line[i] = (line[i] + b) & 255
            elif f == 3:
                line[i] = (line[i] + (a + b) // 2) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append(bytes(line))
        prev = line
    return width, height, bpp, rows


def stats(path):
    width, height, bpp, rows = rgba_rows(path)
    counts = Counter()
    black = 0
    sums = [0, 0, 0]
    for row in rows:
        for x in range(0, width * bpp, bpp):
            r, g, b = row[x], row[x + 1], row[x + 2]
            if r < 16 and g < 16 and b < 16:
                black += 1
            sums[0] += r
            sums[1] += g
            sums[2] += b
            counts[(r >> 3, g >> 3, b >> 3)] += 1
    total = width * height
    top = counts.most_common(5)
    return {
        "size": f"{width}x{height}",
        "black": round(black / total, 4),
        "distinct": len(counts),
        "top5": [("#%02x%02x%02x" % (c[0] << 3, c[1] << 3, c[2] << 3), round(n / total, 4)) for c, n in top],
        "mean": [round(s / total, 1) for s in sums],
    }


if __name__ == "__main__":
    for p in sys.argv[1:]:
        print(p, stats(p))
