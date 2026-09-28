#!/usr/bin/env python3
"""Is the system's chrome on a capture of the display window? (`tests/d8_app_only.rs`'s regions,
measured on any PNG: a window capture from `tools/window_shot.ps1`, or a framebuffer screenshot.)

    python tools/chrome_check.py <png> [<png>...]
    python tools/chrome_check.py --expect absent <png>     # exit 1 unless the chrome is absent
    python tools/chrome_check.py --expect present <png>

The strips are where the chrome is at the display's 160 dpi: the status bar's top 24 rows and the
taskbar's bottom 56 (scaled by the image's height against the display's 720 when the window shows
the display at another size). For each: the share of near-white pixels (every channel >= 235: the
status bar's clock and icons, the taskbar's bar) and the number of distinct colours (5 bits a
channel). The chrome is absent when the top strip has no near-white and the bottom strip under 5%;
present when the top has over 0.2% and the bottom over 50% (D8's thresholds). Needs Pillow.
"""
import sys

from PIL import Image


def strip(img, top, bottom):
    w = img.width
    raw = img.crop((0, top, w, bottom)).tobytes()
    n = white = 0
    colours = set()
    for i in range(0, len(raw), 3):
        p = raw[i:i + 3]
        n += 1
        if p[0] >= 235 and p[1] >= 235 and p[2] >= 235:
            white += 1
        colours.add((p[0] >> 3, p[1] >> 3, p[2] >> 3))
    return white / max(n, 1), len(colours)


def check(path):
    img = Image.open(path).convert("RGB")
    h = img.height
    top_rows = max(1, round(24 * h / 720))
    bottom_rows = max(1, round(56 * h / 720))
    top = strip(img, 0, top_rows)
    bottom = strip(img, h - bottom_rows, h)
    absent = top[0] < 0.001 and bottom[0] < 0.05
    present = top[0] > 0.002 and bottom[0] > 0.5
    verdict = "absent" if absent else "present" if present else "unclear"
    print(f"{path}: {img.width}x{h}; top {top_rows} rows: white {top[0]:.4f}, {top[1]} colours; "
          f"bottom {bottom_rows} rows: white {bottom[0]:.4f}, {bottom[1]} colours; chrome {verdict}")
    return verdict


def main(argv):
    expect = None
    if len(argv) > 2 and argv[1] == "--expect":
        expect, argv = argv[2], argv[2:]
    verdicts = [check(p) for p in argv[1:]]
    if expect and any(v != expect for v in verdicts):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
