"""Build the Omni pointer overlay: an Android RRO that blanks every pointer.

WHAT IT IS FOR. The Bliss guest draws its own mouse pointer -- a 30x39
software sprite composited into the framebuffer by SurfaceFlinger -- and the
host window shows that framebuffer. So the user sees a pointer that lags the
real one by the whole input->guest->scanout pipeline, and when Roblox paints
its own in-game cursor as well, TWO pointers. The fix is to make the guest
never paint one: the host pointer is the pointer (`-display gtk,show-cursor=on`,
zero latency), hidden by QMP while Roblox is in a place (qemu-patches/0010,
omnidroid/hostcursor.py).

HOW THE GUEST'S POINTER IS BLANKED. Android loads every pointer shape from
framework-res (`pointer_arrow_icon.xml` -> `@drawable/pointer_arrow`, etc.).
A Runtime Resource Overlay that maps each of those bitmaps to a fully
transparent PNG of the same size makes the sprite invisible without touching
input: the pointer still moves, hovers, clicks -- it just has no pixels.

TWO THINGS ABOUT THE OVERLAY ARE LOAD-BEARING, both measured 2026-09-02 on
Bliss 16.9.7 (Android 13):

  * It must be STATIC, on a system partition (`/system/product/overlay`).
    A mutable overlay installed to /data and enabled with `cmd overlay` DID
    resolve correctly (`cmd overlay lookup android android:drawable/pointer_hand`
    returned the overlay's PNG) and was mapped into system_server, and the
    pointer was STILL drawn after a full framework restart. The pointer
    bitmaps are loaded through the system context that zygote builds at
    startup, and only immutable partition overlays reach that context
    (AssetManager.createSystemAssetsInZygoteLocked ->
    OverlayConfig.createImmutableFrameworkIdmapsInZygote). So the APK is
    baked into the base image by `omnidroid bake-overlay`, not installed.

  * It must be signed with the platform certificate. PackageManager refuses a
    framework overlay signed with anything else ("Overlay ... and target
    android signed with different certificates, and the overlay lacks
    <overlay android:targetName>" -- framework-res declares no overlayable,
    so there is no targetName to give). Bliss signs framework-res with the
    PUBLIC AOSP platform test key (subject CN=Android, sha256
    c8a2e9bc...2ab8, identical to build/make/target/product/security/platform
    in AOSP), which is what tools/keys/aosp-platform.* is. It is not a
    secret; it is what makes this overlay installable at all.

Pure parts (the PNG encoder, the resource plan) have no SDK dependency and are
unit-tested; `build()` needs Android build-tools (aapt2 + apksigner) and the
android.jar of any platform, resolved the way engine.apk_package_name does.

    python tools/build_pointer_overlay.py            # -> overlay/build/OmniPointerOverlay.apk
    omnidroid bake-overlay --apk overlay/build/OmniPointerOverlay.apk
"""
from __future__ import annotations

import argparse
import glob
import os
import platform
import struct
import subprocess
import sys
import tempfile
import zlib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MANIFEST = REPO / "overlay" / "pointer" / "AndroidManifest.xml"
KEY_PK8 = REPO / "tools" / "keys" / "aosp-platform.pk8"
KEY_CERT = REPO / "tools" / "keys" / "aosp-platform.x509.pem"
DEFAULT_OUT = REPO / "overlay" / "build" / "OmniPointerOverlay.apk"

PACKAGE = "com.omni.pointeroverlay"
# Where `bake-overlay` puts it in the guest. A directory per overlay, like
# Bliss's own (/system/product/overlay/<Name>/<Name>.apk).
GUEST_DIR = "/system/product/overlay/OmniPointerOverlay"
GUEST_APK = GUEST_DIR + "/OmniPointerOverlay.apk"

# Android 13 requires an overlay to target Q (29) or later.
MIN_SDK = 29
TARGET_SDK = 33

# Every pointer bitmap in Bliss 16.9.7's framework-res, with its size. The
# framework ships them at mdpi only and scales for the display density, so
# one mdpi PNG per name is the whole overlay. Sizes are kept identical to the
# originals so every icon's hotspot (validated against the bitmap bounds
# when PointerIcon loads it) stays in range. Read out of framework-res.apk
# 2026-09-02; `--framework-res <apk>` regenerates the list from any other
# build's framework.
POINTER_BITMAPS = (
    ("pointer_alias", 25, 25), ("pointer_alias_large", 64, 64),
    ("pointer_all_scroll", 25, 25), ("pointer_all_scroll_large", 64, 64),
    ("pointer_arrow", 22, 28), ("pointer_arrow_large", 64, 64),
    ("pointer_cell", 25, 25), ("pointer_cell_large", 64, 64),
    ("pointer_context_menu", 25, 25), ("pointer_context_menu_large", 64, 64),
    ("pointer_copy", 25, 25), ("pointer_copy_large", 64, 64),
    ("pointer_crosshair", 25, 25), ("pointer_crosshair_large", 64, 64),
    ("pointer_grab", 25, 25), ("pointer_grab_large", 64, 64),
    ("pointer_grabbing", 25, 25), ("pointer_grabbing_large", 64, 64),
    ("pointer_hand", 25, 25), ("pointer_hand_large", 64, 64),
    ("pointer_help", 25, 25), ("pointer_help_large", 64, 64),
    ("pointer_horizontal_double_arrow", 25, 25),
    ("pointer_horizontal_double_arrow_large", 64, 64),
    ("pointer_nodrop", 25, 25), ("pointer_nodrop_large", 64, 64),
    ("pointer_spot_anchor", 45, 45), ("pointer_spot_hover", 45, 45),
    ("pointer_spot_touch", 33, 33),
    ("pointer_text", 25, 25), ("pointer_text_large", 64, 64),
    ("pointer_top_left_diagonal_double_arrow", 25, 25),
    ("pointer_top_left_diagonal_double_arrow_large", 64, 64),
    ("pointer_top_right_diagonal_double_arrow", 25, 25),
    ("pointer_top_right_diagonal_double_arrow_large", 64, 64),
    ("pointer_vertical_double_arrow", 25, 25),
    ("pointer_vertical_double_arrow_large", 64, 64),
    ("pointer_vertical_text", 25, 25), ("pointer_vertical_text_large", 64, 64),
) + tuple((f"pointer_wait_{i}", 16, 16) for i in range(36)) + (
    ("pointer_zoom_in", 25, 25), ("pointer_zoom_in_large", 64, 64),
    ("pointer_zoom_out", 25, 25), ("pointer_zoom_out_large", 64, 64),
)


def transparent_png(width: int, height: int) -> bytes:
    """A valid RGBA PNG of the given size, every pixel (0, 0, 0, 0). Pure."""
    if width < 1 or height < 1:
        raise ValueError(f"bad size {width}x{height}")

    def chunk(kind: bytes, body: bytes) -> bytes:
        return (struct.pack(">I", len(body)) + kind + body
                + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF))

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    raw = b"".join(b"\x00" + b"\x00" * (width * 4) for _ in range(height))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def png_size(data: bytes) -> tuple[int, int]:
    """(width, height) from a PNG header. Pure."""
    if data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
        raise ValueError("not a PNG")
    return struct.unpack(">II", data[16:24])


def bitmaps_from_framework_res(apk: Path):
    """The (name, w, h) table for another framework-res.apk. I/O, one zip."""
    import zipfile
    rows = []
    with zipfile.ZipFile(apk) as z:
        for n in sorted(z.namelist()):
            if (n.startswith("res/drawable-mdpi-v4/pointer_")
                    and n.endswith(".png")):
                w, h = png_size(z.read(n))
                rows.append((n.rsplit("/", 1)[1][:-4], w, h))
    return tuple(rows)


def resource_plan(bitmaps=POINTER_BITMAPS):
    """{relative res path: png bytes} for the overlay. Pure."""
    plan = {}
    for name, w, h in bitmaps:
        if not name.startswith("pointer_"):
            raise ValueError(f"not a pointer bitmap: {name}")
        plan[f"res/drawable-mdpi/{name}.png"] = transparent_png(w, h)
    return plan


def find_sdk_roots():
    roots = []
    for var in ("ANDROID_SDK_ROOT", "ANDROID_HOME"):
        v = os.environ.get(var)
        if v:
            roots.append(Path(v))
    home = Path.home()
    if os.name == "nt":
        lad = os.environ.get("LOCALAPPDATA")
        roots.append(Path(lad) / "Android/Sdk" if lad
                     else home / "AppData/Local/Android/Sdk")
    elif platform.system() == "Darwin":
        roots.append(home / "Library/Android/sdk")
    else:
        roots.append(home / "Android/Sdk")
    return roots


def find_build_tools():
    """(aapt2, apksigner, android.jar) or None. Newest build-tools wins.

    apksigner is a .bat on Windows and Git Bash will not resolve it bare --
    the absolute path is used everywhere (see the kiosk build memory)."""
    exe = ".exe" if os.name == "nt" else ""
    bat = ".bat" if os.name == "nt" else ""
    for root in find_sdk_roots():
        jars = sorted(glob.glob(str(root / "platforms" / "android-*"
                                    / "android.jar")), reverse=True)
        for bt in sorted(glob.glob(str(root / "build-tools" / "*")),
                         reverse=True):
            aapt2 = Path(bt) / f"aapt2{exe}"
            signer = Path(bt) / f"apksigner{bat}"
            if aapt2.exists() and signer.exists() and jars:
                return aapt2, signer, Path(jars[0])
    return None


def _run(argv, label):
    r = subprocess.run([str(a) for a in argv], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"[{label}] failed ({r.returncode}):\n{r.stdout}{r.stderr}")
    return r


def build(out: Path = DEFAULT_OUT, bitmaps=POINTER_BITMAPS,
          tools=None) -> Path:
    """Compile, link and platform-sign the overlay. Returns the APK path."""
    tools = tools or find_build_tools()
    if not tools:
        sys.exit("no Android build-tools (aapt2 + apksigner) and platform "
                 "android.jar found; set ANDROID_SDK_ROOT")
    aapt2, signer, jar = tools
    for f in (MANIFEST, KEY_PK8, KEY_CERT):
        if not f.exists():
            sys.exit(f"missing {f}")
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="omni-pointer-overlay-") as tmp:
        tmp = Path(tmp)
        res = tmp / "res"
        for rel, data in resource_plan(bitmaps).items():
            p = tmp / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(data)
        compiled = tmp / "compiled"
        compiled.mkdir()
        _run([aapt2, "compile", "--dir", res, "-o", str(compiled) + os.sep],
             "aapt2 compile")
        unsigned = tmp / "unsigned.apk"
        _run([aapt2, "link", "-o", unsigned, "--manifest", MANIFEST,
              "-I", jar, "--min-sdk-version", str(MIN_SDK),
              "--target-sdk-version", str(TARGET_SDK)]
             + sorted(compiled.glob("*.flat")), "aapt2 link")
        _run([signer, "sign", "--key", KEY_PK8, "--cert", KEY_CERT,
              "--out", out, unsigned], "apksigner")
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--framework-res", type=Path, default=None,
                    help="regenerate the bitmap table from this "
                         "framework-res.apk instead of the built-in one")
    a = ap.parse_args(argv)
    bitmaps = (bitmaps_from_framework_res(a.framework_res)
               if a.framework_res else POINTER_BITMAPS)
    out = build(a.out, bitmaps)
    print(f"built {out} ({out.stat().st_size} bytes, {len(bitmaps)} bitmaps, "
          f"package {PACKAGE}, static, platform-signed)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
