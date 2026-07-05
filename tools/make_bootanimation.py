#!/usr/bin/env python3
"""Pack a folder of frames into an Android bootanimation.zip.

Android REQUIRES the zip to be STORED (uncompressed) — a deflated
bootanimation.zip silently fails to play. This tool enforces that.

Layout expected:
  <frames_dir>/
    desc.txt                 # "WIDTH HEIGHT FPS" + part lines
    part0/ 0001.png ...      # one subfolder per animation part
    part1/ ...

Usage:
  python make_bootanimation.py <frames_dir> <out.zip>
"""
import sys
import zipfile
from pathlib import Path


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    frames_dir = Path(sys.argv[1])
    out = Path(sys.argv[2])
    desc = frames_dir / "desc.txt"
    if not desc.exists():
        sys.exit(f"error: {desc} missing")

    # desc.txt must be first entry, everything STORED.
    with zipfile.ZipFile(out, "w", zipfile.ZIP_STORED) as z:
        z.write(desc, "desc.txt")
        for p in sorted(frames_dir.rglob("*")):
            if p.is_dir() or p.name == "desc.txt":
                continue
            if p.suffix.lower() not in (".png", ".jpg", ".jpeg"):
                continue
            z.write(p, p.relative_to(frames_dir).as_posix())
    print(f"wrote {out} ({out.stat().st_size} bytes, STORED)")


if __name__ == "__main__":
    main()
