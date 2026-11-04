#!/usr/bin/env python3
"""Turn rendered PNGs of the app icon into what the three platforms build in. Needs Pillow.

    make_icon.py <folder of icon-<size>.png> <icon-mac-1024.png> <crates/omni-platform/icon>

Writes:
    omnidroid.ico      the icon as a file
    omnidroid.res      the same as a compiled Windows resource (icon group 1), which the two
                       programs' build.rs hand to the linker: no resource compiler at build time
    omnidroid.x11      the sizes X11's _NET_WM_ICON takes: little-endian u32 width, height, then
                       height*width ARGB pixels, repeated
    omnidroid-mac.png  the 1024 tile with Apple's margin, for the Dock
"""
import io
import shutil
import struct
import sys
from pathlib import Path

from PIL import Image

ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
X11_SIZES = [16, 32, 48, 64, 128]
RT_ICON, RT_GROUP_ICON = 3, 14


def dib(img):
    """A 32-bit BITMAPINFOHEADER icon image: bottom-up BGRA, then an empty AND mask."""
    w, h = img.size
    head = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    px = img.convert("RGBA").tobytes()
    rows = [px[y * w * 4:(y + 1) * w * 4] for y in range(h)][::-1]
    bgra = bytearray(b"".join(rows))
    bgra[0::4], bgra[2::4] = bgra[2::4], bgra[0::4]
    mask = bytes((((w + 31) // 32) * 4) * h)
    return head + bytes(bgra) + mask


def images(folder):
    """Each size as an icon entry: DIB below 256, PNG at 256, as Windows' own icons are stored."""
    out = []
    for z in ICO_SIZES:
        img = Image.open(folder / f"icon-{z}.png")
        if z == 256:
            buf = io.BytesIO()
            img.save(buf, "PNG", optimize=True)
            out.append((z, buf.getvalue()))
        else:
            out.append((z, dib(img)))
    return out


def ico(entries):
    out = struct.pack("<HHH", 0, 1, len(entries))
    at = 6 + 16 * len(entries)
    for z, data in entries:
        out += struct.pack("<BBBBHHII", z % 256, z % 256, 0, 0, 1, 32, len(data), at)
        at += len(data)
    return out + b"".join(d for _, d in entries)


def res_entry(kind, name, flags, data):
    head = struct.pack("<II", len(data), 32) + struct.pack("<HHHH", 0xFFFF, kind, 0xFFFF, name)
    head += struct.pack("<IHHII", 0, flags, 0x0409, 0, 0)
    return head + data + bytes(-len(data) % 4)


def res(entries):
    out = res_entry(0, 0, 0, b"")  # A .res file opens with an empty entry.
    group = struct.pack("<HHH", 0, 1, len(entries))
    for i, (z, data) in enumerate(entries, 1):
        out += res_entry(RT_ICON, i, 0x1010, data)
        group += struct.pack("<BBBBHHIH", z % 256, z % 256, 0, 0, 1, 32, len(data), i)
    return out + res_entry(RT_GROUP_ICON, 1, 0x1030, group)


def x11(folder):
    out = bytearray()
    for z in X11_SIZES:
        img = Image.open(folder / f"icon-{z}.png").convert("RGBA")
        out += struct.pack("<II", z, z)
        px = img.tobytes()
        for i in range(0, len(px), 4):
            r, g, b, a = px[i:i + 4]
            out += struct.pack("<I", a << 24 | r << 16 | g << 8 | b)
    return bytes(out)


def main():
    folder, mac, dest = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
    entries = images(folder)
    (dest / "omnidroid.ico").write_bytes(ico(entries))
    (dest / "omnidroid.res").write_bytes(res(entries))
    (dest / "omnidroid.x11").write_bytes(x11(folder))
    shutil.copyfile(mac, dest / "omnidroid-mac.png")


if __name__ == "__main__":
    main()
