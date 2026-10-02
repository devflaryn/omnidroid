#!/usr/bin/env python3
"""Is this frame Pet Simulator 99's own loading screen (the place's, not Roblox's join screens)?

    python tools/ingame_screen.py <frame.png|dir> [...]

PS99's loading screen is a flat white page (BIG Games) with a row of five dots at the bottom
centre. It fades in over Roblox's dark join screen: its first frames are a flat mid-gray, then the
dots appear, faint, then white. Measured over the left 70% below the top 30% (the Delta APK's key
panel can cover the right quarter, its notes the top corners), a frame counts when that is a fade's
flat gray (>= 95% one neutral level of 100-199: r, g, b within 12 of each other), or >= 80% one
bright neutral level with the dots' band (x 40-60%, y 90-97%) holding 3-40% pixels more than 5 off
it. Roblox's Home dimmed behind a dialog is a gray too, but not a flat one. Roblox's own join screens are dark; its white join page
(a thin bar at the middle, nothing at the bottom) is a flat 255; its splash comes before
"Joining game", which the caller waits for.
"""
import sys
from pathlib import Path


def measure(path):
    """(share of the left 70% at one bright neutral level, that level, share of the dots' band off it)"""
    from PIL import Image
    im = Image.open(path).convert("RGB")
    w, h = im.size
    small = im.crop((0, int(h * 0.30), int(w * 0.70), h)).resize((112, 63)).tobytes()
    n, levels = len(small) // 3, []
    for i in range(0, len(small), 3):
        r, g, b = small[i], small[i + 1], small[i + 2]
        if r > 100 and g > 100 and b > 100 and max(r, g, b) - min(r, g, b) < 12:
            levels.append(r)
    if not levels:
        return 0.0, 0, 0.0
    levels.sort()
    bg = levels[len(levels) // 2]
    band = im.crop((int(w * 0.40), int(h * 0.90), int(w * 0.60), int(h * 0.97))).tobytes()
    m = len(band) // 3
    off = sum(1 for i in range(0, len(band), 3)
              if abs(band[i] - bg) > 5 or abs(band[i + 1] - bg) > 5 or abs(band[i + 2] - bg) > 5)
    return len(levels) / n, bg, off / max(m, 1)


def is_loading_screen(path) -> bool:
    flat, level, dots = measure(path)
    return (level < 200 and flat >= 0.95) or (flat >= 0.80 and 0.03 <= dots <= 0.40)


if __name__ == "__main__":
    for arg in sys.argv[1:]:
        p = Path(arg)
        for f in sorted(p.glob("[0-9]*.png")) if p.is_dir() else [p]:
            flat, level, dots = measure(f)
            print(f"{f.name} flat {flat:.3f} level {level} dots {dots:.3f}{'  <- loading screen' if is_loading_screen(f) else ''}")
