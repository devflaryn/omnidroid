#!/usr/bin/env python3
"""Fetch the pinned official Magisk APK and extract util_functions.sh + busybox.

The files are GPL and are NOT committed; they land in sysroot/magisk-<version>/.
Stdlib only. Exits nonzero on any failure, including a sha256 mismatch.
"""
import hashlib
import os
import re
import sys
import tempfile
import urllib.request
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MEMBERS = {
    "assets/util_functions.sh": "util_functions.sh",
    "lib/arm64-v8a/libbusybox.so": "busybox",
}


def read_pin(path):
    pin = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            k, v = line.split("=", 1)
            pin[k.strip()] = v.strip()
    for k in ("version", "versionCode", "url", "sha256"):
        if not pin.get(k):
            sys.exit(f"fetch_magisk: pin is missing '{k}'")
    v = pin["version"]
    if not re.fullmatch(r"[A-Za-z0-9._-]+", v) or ".." in v:
        sys.exit(f"fetch_magisk: unsafe version {v!r} in pin; refusing")
    return pin


def main():
    pin = read_pin(REPO / "tools" / "magisk.pin")
    out = REPO / "sysroot" / f"magisk-{pin['version']}"
    fd, tmp = tempfile.mkstemp(suffix=".apk")
    os.close(fd)
    try:
        print(f"downloading {pin['url']}")
        h = hashlib.sha256()
        with urllib.request.urlopen(pin["url"]) as r, open(tmp, "wb") as f:
            while True:
                chunk = r.read(1 << 20)
                if not chunk:
                    break
                h.update(chunk)
                f.write(chunk)
        if h.hexdigest().lower() != pin["sha256"].lower():
            sys.exit(f"fetch_magisk: sha256 mismatch: got {h.hexdigest()}, pinned {pin['sha256']}; refusing")
        out.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(tmp) as z:
            for member, name in MEMBERS.items():
                dest = out / name
                dest.write_bytes(z.read(member))
                print(f"extracted {member} -> {dest} ({dest.stat().st_size} bytes)")
    finally:
        os.unlink(tmp)


if __name__ == "__main__":
    main()
