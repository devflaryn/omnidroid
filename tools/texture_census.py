#!/usr/bin/env python3
"""Which compressed texture formats the Roblox APK actually ships, and in what proportion.

Why this exists
---------------
``docs/HANDOFF.md`` records a hard constraint measured on the development host: the GPU supports
**neither ETC2 nor ASTC**, and does support BC1/BC3/BC7 (``docs/research/graphics-spike.md`` §3). So
every compressed texture the guest hands to ``glCompressedTexImage2D`` in a format the host cannot
sample has to be transcoded at load time, and that transcoder is mandatory infrastructure rather
than an optimisation.

What was *not* known is **which** formats. "ETC2 or ASTC" is a family, not a format: ETC2 has RGB8,
RGBA8, sRGB, punch-through-alpha (RGB8A1) and the EAC R11/RG11 forms, and ASTC has fourteen block
footprints from 4x4 to 12x12, each in LDR and HDR. Implementing all of both is weeks of work and
most of it may be dead code. This project's governing principle is *the import list is the
specification*; this tool is that principle applied to textures.

Method
------
Every entry in the APK is classified by **sniffing its leading bytes**, never by its extension, and
every container found is parsed down to the format field:

* **KTX1** (``\\xabKTX 11\\xbb\\r\\n\\x1a\\n``) -- ``glInternalFormat`` is read from the header at
  offset 28 and named from the GL enum table below. Each mip level is walked through its
  ``imageSize`` prefix and 4-byte padding so the block payload is located exactly.
* **DDS** (``DDS ``) -- the ``DDS_PIXELFORMAT`` FourCC at offset 84, and for ``DX10`` the
  ``DXGI_FORMAT`` at offset 128.
* **KTX2**, **PVR3**, raw **ASTC** (``0x5CA1AB13``), **PKM**, **PNG**, **JPEG** and **RIFF** are
  detected and counted so that "none present" is a measurement rather than an assumption.

For every ETC container the tool additionally walks **every 4x4 block** and classifies its mode from
the two bytes the ETC1/ETC2 bit layout puts it in. That histogram is the measurement that sets the
decoder's scope, because ETC1 and ETC2's RGB8 form share a container and an ``internalFormat``-level
description but not a decoder: a block with ``diffbit = 1`` whose 5-bit base plus 3-bit delta leaves
``[0, 31]`` is **not** a differential block, it is ETC2's T, H or planar mode, and an ETC1 decoder
reading it produces plausible, silently wrong pixels.

Why extension-sniffing would have got this wrong
------------------------------------------------
``docs/research/apk-analysis.md`` §8.2 lists ``.ktx`` 26 and ``.tex`` 12 as separate rows, the second
described only as "Roblox texture container". The twelve ``.tex`` files are **KTX1 files with ETC1
payloads** -- the skybox -- so an extension-driven census reports 26 ETC containers where there are
38, and misses every skybox face. That is the same class of error as the "a count cannot see a
substitution" lesson in HANDOFF: this tool therefore asserts the **exact path-to-format mapping**,
not a total.

Self-check
----------
``python tools/texture_census.py --check`` re-derives everything and compares it against the
constants below, printing every disagreement and exiting non-zero on any. It is the same shape as
``tools/os_surface.py --check`` and ``tools/init_reach.py``.
"""

from __future__ import annotations

import argparse
import collections
import struct
import sys
import zipfile
from pathlib import Path

APK = Path(__file__).resolve().parent.parent / "Roblox-2.738.1397.apk"

KTX1_IDENTIFIER = b"\xabKTX 11\xbb\r\n\x1a\n"
KTX2_IDENTIFIER = b"\xabKTX 20\xbb\r\n\x1a\n"

# Container magics, longest first so a prefix cannot shadow a longer match.
CONTAINER_MAGICS = (
    (KTX1_IDENTIFIER, "KTX1"),
    (KTX2_IDENTIFIER, "KTX2"),
    (b"\x89PNG\r\n\x1a\n", "PNG"),
    (b"\x13\xab\xa1\x5c", "ASTC-raw"),  # little-endian 0x5CA1AB13
    (b"DDS ", "DDS"),
    (b"PVR\x03", "PVR3"),
    (b"PKM ", "PKM"),
    (b"RIFF", "RIFF"),
    (b"\xff\xd8\xff", "JPEG"),
)

# The GL compressed-texture internal formats that can appear in a KTX1 header. Values are from the
# OpenGL ES 3.2 specification table 8.17 (ETC2/EAC), OES_compressed_ETC1_RGB8_texture (ETC1) and
# KHR_texture_compression_astc_ldr (ASTC). Anything not here is reported by its raw hex value
# rather than guessed at.
GL_INTERNAL_FORMATS = {
    0x8D64: "GL_ETC1_RGB8_OES",
    0x9274: "GL_COMPRESSED_RGB8_ETC2",
    0x9275: "GL_COMPRESSED_SRGB8_ETC2",
    0x9276: "GL_COMPRESSED_RGB8_PUNCHTHROUGH_ALPHA1_ETC2",
    0x9277: "GL_COMPRESSED_SRGB8_PUNCHTHROUGH_ALPHA1_ETC2",
    0x9278: "GL_COMPRESSED_RGBA8_ETC2_EAC",
    0x9279: "GL_COMPRESSED_SRGB8_ALPHA8_ETC2_EAC",
    0x9270: "GL_COMPRESSED_R11_EAC",
    0x9271: "GL_COMPRESSED_SIGNED_R11_EAC",
    0x9272: "GL_COMPRESSED_RG11_EAC",
    0x9273: "GL_COMPRESSED_SIGNED_RG11_EAC",
    0x93B0: "GL_COMPRESSED_RGBA_ASTC_4x4_KHR",
    0x93B1: "GL_COMPRESSED_RGBA_ASTC_5x4_KHR",
    0x93B2: "GL_COMPRESSED_RGBA_ASTC_5x5_KHR",
    0x93B3: "GL_COMPRESSED_RGBA_ASTC_6x5_KHR",
    0x93B4: "GL_COMPRESSED_RGBA_ASTC_6x6_KHR",
    0x93B5: "GL_COMPRESSED_RGBA_ASTC_8x5_KHR",
    0x93B6: "GL_COMPRESSED_RGBA_ASTC_8x6_KHR",
    0x93B7: "GL_COMPRESSED_RGBA_ASTC_8x8_KHR",
    0x93B8: "GL_COMPRESSED_RGBA_ASTC_10x5_KHR",
    0x93B9: "GL_COMPRESSED_RGBA_ASTC_10x6_KHR",
    0x93BA: "GL_COMPRESSED_RGBA_ASTC_10x8_KHR",
    0x93BB: "GL_COMPRESSED_RGBA_ASTC_10x10_KHR",
    0x93BC: "GL_COMPRESSED_RGBA_ASTC_12x10_KHR",
    0x93BD: "GL_COMPRESSED_RGBA_ASTC_12x12_KHR",
    0x83F0: "GL_COMPRESSED_RGB_S3TC_DXT1_EXT",
    0x83F1: "GL_COMPRESSED_RGBA_S3TC_DXT1_EXT",
    0x83F2: "GL_COMPRESSED_RGBA_S3TC_DXT3_EXT",
    0x83F3: "GL_COMPRESSED_RGBA_S3TC_DXT5_EXT",
}

# The formats whose payload this tool knows how to walk block by block.
ETC_RGB_FORMATS = {0x8D64, 0x9274, 0x9275}

# ---------------------------------------------------------------------------------------------
# Expected values. Membership, not totals -- HANDOFF records two occasions on which a total stayed
# right while the set under it was wrong by two in each direction.
# ---------------------------------------------------------------------------------------------

EXPECTED_ENTRY_COUNT = 2365

# Every texture container in the APK, by path, with the format actually in its header.
EXPECTED_TEXTURES = {
    "assets/android/textures/plastic/diffuse.dds": "DDS:uncompressed:flags0x20000:bits8",
    "assets/android/textures/plastic/normal.dds": "DDS:uncompressed:flags0x41:bits32",
    "assets/android/textures/plastic/normaldetail.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/indoor512_bk.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/indoor512_dn.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/indoor512_ft.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/indoor512_lf.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/indoor512_rt.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/indoor512_up.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/sky512_bk.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/sky512_dn.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/sky512_ft.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/sky512_lf.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/sky512_rt.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/sky/sky512_up.tex": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/studs.dds": "DDS:uncompressed:flags0x20000:bits8",
    "assets/android/textures/wangIndex.dds": "DDS:uncompressed:flags0x20001:bits16",
    "assets/android/textures/water/normal_01.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_02.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_03.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_04.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_05.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_06.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_07.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_08.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_09.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_10.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_11.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_12.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_13.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_14.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_15.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_16.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_17.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_18.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_19.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_20.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_21.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_22.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_23.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_24.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/android/textures/water/normal_25.ktx": "KTX1:GL_ETC1_RGB8_OES",
    "assets/content/sky/bn.dds": "DDS:uncompressed:flags0x41:bits32",
    "assets/content/sky/cloudAdvection.dds": "DDS:DXT1",
    "assets/content/sky/cloudDetail.dds": "DDS:DXT1",
    "assets/content/sky/cloudDetail3D.dds": "DDS:DX10:dxgi61",
    "assets/content/sky/clouds.dds": "DDS:DX10:dxgi61",
    "assets/content/sky/cloudsfb.dds": "DDS:DX10:dxgi61",
    "assets/content/sky/noise.dds": "DDS:DX10:dxgi61",
    "assets/content/sky/noisefb.dds": "DDS:uncompressed:flags0x41:bits8",
    "assets/content/textures/noise.dds": "DDS:uncompressed:flags0x41:bits32",
    "assets/content/textures/particles/common_alpha.dds": "DDS:DXT5",
    "assets/content/textures/particles/explosion01_core_main.dds": "DDS:DXT5",
    "assets/content/textures/particles/explosion01_implosion_main.dds": "DDS:DXT5",
    "assets/content/textures/particles/explosion01_shockwave_main.dds": "DDS:DXT5",
    "assets/content/textures/particles/explosion01_smoke_main.dds": "DDS:DXT5",
    "assets/content/textures/particles/explosion_color.dds": "DDS:DXT5",
    "assets/content/textures/particles/fire_main.dds": "DDS:DXT5",
    "assets/content/textures/particles/fire_sparks_color.dds": "DDS:DXT5",
    "assets/content/textures/particles/fire_sparks_main.dds": "DDS:DXT5",
    "assets/content/textures/particles/forcefield_glow_main.dds": "DDS:DXT5",
    "assets/content/textures/particles/forcefield_vortex_color.dds": "DDS:DXT3",
    "assets/content/textures/particles/forcefield_vortex_main.dds": "DDS:DXT5",
    "assets/content/textures/particles/smoke_main.dds": "DDS:DXT5",
    "assets/content/textures/particles/sparkles_color.dds": "DDS:DXT5",
    "assets/content/textures/particles/sparkles_main.dds": "DDS:DXT5",
}

# The block-mode histogram over every 4x4 block of every ETC container above. The last three are
# the whole reason this tool walks blocks at all: they are ETC2's extensions to the ETC1 bit
# layout, and they are what an ETC1 decoder would silently mis-decode.
EXPECTED_BLOCK_MODES = {
    "individual (ETC1)": 133801,
    "differential (ETC1)": 680001,
    "T mode (ETC2 only)": 0,
    "H mode (ETC2 only)": 0,
    "planar (ETC2 only)": 0,
}
EXPECTED_TOTAL_BLOCKS = 813802

# Decoded RGBA8 bytes the ETC payload above expands to.
#
# There are two figures here and the difference is not rounding. `813,802 blocks x 16 texels x 4
# bytes = 52,083,328` counts whole blocks; the *images* are 52,079,224 bytes, because the last two
# mip levels of every texture are 2x2 and 1x1 and each still occupies a full 4x4 block. The gap is
# 1,026 texels -- 27 per texture across all 38.
#
# The first version of this tool reported only the block-padded number and called it "decoded to
# RGBA8", which is wrong for the thing that matters: what gets uploaded is the image. Found by
# `crates/omni-texture/tests/real_assets.rs` disagreeing with it, which is the cross-check between
# two implementations doing its job.
EXPECTED_IMAGE_RGBA8_BYTES = 52079224
EXPECTED_BLOCK_PADDED_RGBA8_BYTES = 52083328

# The compression vocabulary `libroblox.so` negotiates assets in. Found as literal JSON in
# `.rodata`; see the report for why it settles the runtime-downloaded half of the question.
EXPECTED_CLIENT_COMPRESSION_CAPS = ("dxt", "etc", "etc2", "uncompressed")


def signed3(v: int) -> int:
    """The 3-bit two's-complement delta ETC1's differential mode stores."""
    return v - 8 if v >= 4 else v


def etc_block_mode(block: bytes) -> str:
    """Classify one 8-byte ETC RGB block by the mode its bits select.

    The 64-bit block is big-endian. Byte 3 holds ``table1(3) | table2(3) | diffbit | flipbit``, so
    ``diffbit`` is bit 1. With ``diffbit = 0`` the block is ETC1's *individual* mode: two 4-bit base
    colours, and nothing else is possible. With ``diffbit = 1`` bytes 0..2 hold a 5-bit base and a
    3-bit signed delta per channel, and ETC2 reuses the *out-of-range* encodings -- which an ETC1
    encoder is forbidden to emit -- as escapes into three new modes, tested in channel order R, G,
    B (OpenGL ES 3.2 specification, section 8.7.3 "ETC Compressed Texture Image Formats").
    """
    if (block[3] >> 1) & 1 == 0:
        return "individual (ETC1)"
    r, dr = block[0] >> 3, signed3(block[0] & 7)
    g, dg = block[1] >> 3, signed3(block[1] & 7)
    b, db = block[2] >> 3, signed3(block[2] & 7)
    if not 0 <= r + dr <= 31:
        return "T mode (ETC2 only)"
    if not 0 <= g + dg <= 31:
        return "H mode (ETC2 only)"
    if not 0 <= b + db <= 31:
        return "planar (ETC2 only)"
    return "differential (ETC1)"


def sniff(data: bytes) -> str | None:
    for magic, name in CONTAINER_MAGICS:
        if data.startswith(magic):
            return name
    return None


def parse_ktx1(data: bytes) -> dict:
    (endianness, gl_type, _type_size, _gl_format, gl_internal_format, gl_base_internal_format,
     width, height, depth, array_elements, faces, levels, kv_bytes) = struct.unpack_from(
        "<13I", data, 12)
    if endianness != 0x04030201:
        raise ValueError(f"big-endian KTX not handled (endianness field {endianness:#x})")
    return {
        "gl_type": gl_type,
        "internal_format": gl_internal_format,
        "base_internal_format": gl_base_internal_format,
        "width": width,
        "height": height,
        "depth": depth,
        "array_elements": array_elements,
        "faces": faces,
        "levels": levels,
        "payload_offset": 64 + kv_bytes,
    }


def ktx1_levels(data: bytes, header: dict):
    """Yield each mip level's payload, walking the ``imageSize`` prefixes and 4-byte padding."""
    offset = header["payload_offset"]
    for _ in range(max(header["levels"], 1)):
        (image_size,) = struct.unpack_from("<I", data, offset)
        offset += 4
        payload = data[offset:offset + image_size]
        if len(payload) != image_size:
            raise ValueError("KTX level runs past end of file")
        yield payload
        offset += image_size + (-image_size) % 4


def parse_dds(data: bytes) -> str:
    fourcc = data[84:88]
    if fourcc == b"DX10":
        (dxgi_format,) = struct.unpack_from("<I", data, 128)
        return f"DDS:DX10:dxgi{dxgi_format}"
    if fourcc in (b"DXT1", b"DXT2", b"DXT3", b"DXT4", b"DXT5", b"ATI1", b"ATI2", b"BC4U", b"BC5U"):
        return "DDS:" + fourcc.decode("ascii")
    (pf_flags,) = struct.unpack_from("<I", data, 80)
    (rgb_bit_count,) = struct.unpack_from("<I", data, 88)
    return f"DDS:uncompressed:flags{pf_flags:#x}:bits{rgb_bit_count}"


def census(apk_path: Path) -> dict:
    containers = collections.Counter()
    textures: dict[str, str] = {}
    texture_bytes: collections.Counter = collections.Counter()
    block_modes: collections.Counter = collections.Counter()
    etc_payload_bytes = 0
    image_rgba_bytes = 0
    unknown_formats: collections.Counter = collections.Counter()
    entries = 0

    with zipfile.ZipFile(apk_path) as apk:
        for name in sorted(apk.namelist()):
            if name.endswith("/"):
                continue
            entries += 1
            data = apk.read(name)
            kind = sniff(data)
            containers[kind or "other"] += 1
            if kind == "KTX1":
                header = parse_ktx1(data)
                fmt = header["internal_format"]
                named = GL_INTERNAL_FORMATS.get(fmt)
                if named is None:
                    unknown_formats[f"KTX1 glInternalFormat {fmt:#06x}"] += 1
                    named = f"{fmt:#06x}"
                textures[name] = f"KTX1:{named}"
                texture_bytes[f"KTX1:{named}"] += len(data)
                if fmt in ETC_RGB_FORMATS:
                    level_width, level_height = header["width"], header["height"]
                    for payload in ktx1_levels(data, header):
                        etc_payload_bytes += len(payload)
                        # The image is what gets uploaded; the block grid is what gets decoded.
                        image_rgba_bytes += level_width * level_height * 4
                        level_width = max(level_width // 2, 1)
                        level_height = max(level_height // 2, 1)
                        for i in range(0, len(payload) - 7, 8):
                            block_modes[etc_block_mode(payload[i:i + 8])] += 1
            elif kind == "DDS":
                described = parse_dds(data)
                textures[name] = described
                texture_bytes[described] += len(data)
            elif kind in ("KTX2", "ASTC-raw", "PVR3", "PKM"):
                textures[name] = kind
                texture_bytes[kind] += len(data)

    for mode in EXPECTED_BLOCK_MODES:
        block_modes.setdefault(mode, 0)

    return {
        "entries": entries,
        "containers": containers,
        "textures": textures,
        "texture_bytes": texture_bytes,
        "block_modes": block_modes,
        "etc_payload_bytes": etc_payload_bytes,
        "image_rgba_bytes": image_rgba_bytes,
        "unknown_formats": unknown_formats,
    }


def client_compression_caps(apk_path: Path) -> list[str]:
    """The compression vocabulary `libroblox.so` negotiates CDN assets in.

    The engine embeds a literal JSON table of ``"clientCompressionCaps": [ "<name>" ]`` entries in
    `.rodata`. Every distinct name is extracted here rather than described, because it decides the
    *runtime-downloaded* half of the format question: the APK's baked assets are fixed, but the
    streamed ones are whatever the client says it can accept.
    """
    needle = b'"clientCompressionCaps": [ "'
    with zipfile.ZipFile(apk_path) as apk:
        blob = apk.read("lib/arm64-v8a/libroblox.so")
    found: list[str] = []
    start = 0
    while True:
        at = blob.find(needle, start)
        if at < 0:
            break
        start = at + len(needle)
        end = blob.find(b'"', start)
        value = blob[start:end].decode("latin1")
        if value != "{}" and value not in found:  # "{}" is the format template, not a value
            found.append(value)
    return sorted(found)


def report(result: dict, caps: list[str]) -> list[str]:
    lines = [
        "Texture format census -- Roblox-2.738.1397.apk",
        "=" * 78,
        f"APK entries scanned (files, not directories): {result['entries']}",
        "",
        "Containers found, by leading bytes (NOT by extension):",
    ]
    for kind, count in sorted(result["containers"].items(), key=lambda kv: -kv[1]):
        lines.append(f"  {kind:<12} {count:>6}")

    lines += ["", "Texture formats, by what the container header actually says:"]
    by_format: collections.Counter = collections.Counter()
    for described in result["textures"].values():
        by_format[described] += 1
    for described, count in sorted(by_format.items(), key=lambda kv: (-kv[1], kv[0])):
        lines.append(f"  {described:<44} {count:>4} files"
                     f"  {result['texture_bytes'][described]:>10} B")

    total_blocks = sum(result["block_modes"].values())
    lines += [
        "",
        f"ETC block modes over all {total_blocks} 4x4 blocks "
        f"({result['etc_payload_bytes']} B of payload):",
    ]
    for mode in ("individual (ETC1)", "differential (ETC1)",
                 "T mode (ETC2 only)", "H mode (ETC2 only)", "planar (ETC2 only)"):
        count = result["block_modes"][mode]
        share = (100.0 * count / total_blocks) if total_blocks else 0.0
        lines.append(f"  {mode:<24} {count:>9}  {share:7.3f}%")
    lines += [
        f"  the images decode to {result['image_rgba_bytes']} B of RGBA8",
        f"  (whole blocks would be {total_blocks * 16 * 4} B; the difference is the 2x2 and 1x1 "
        f"mip levels, which still occupy a full 4x4 block)",
        "",
        "Compression vocabulary the engine negotiates streamed assets in "
        "(literal JSON in libroblox.so .rodata):",
        "  " + ", ".join(caps),
    ]
    if result["unknown_formats"]:
        lines += ["", "UNRECOGNISED format values (each needs a name before it can be claimed):"]
        for what, count in result["unknown_formats"].most_common():
            lines.append(f"  {what}  x{count}")
    return lines


def check(result: dict, caps: list[str]) -> int:
    failures: list[str] = []

    if result["entries"] != EXPECTED_ENTRY_COUNT:
        failures.append(f"entry count {result['entries']} != {EXPECTED_ENTRY_COUNT}")

    # Membership, not totals: name every path that appeared, vanished or changed format.
    found = result["textures"]
    for path in sorted(set(EXPECTED_TEXTURES) - set(found)):
        failures.append(f"expected texture missing from APK: {path}")
    for path in sorted(set(found) - set(EXPECTED_TEXTURES)):
        failures.append(f"texture in APK that the record does not list: {path} -> {found[path]}")
    for path in sorted(set(found) & set(EXPECTED_TEXTURES)):
        if found[path] != EXPECTED_TEXTURES[path]:
            failures.append(
                f"format changed for {path}: {EXPECTED_TEXTURES[path]} -> {found[path]}")

    for mode, expected in EXPECTED_BLOCK_MODES.items():
        actual = result["block_modes"][mode]
        if actual != expected:
            failures.append(f"block mode {mode}: {actual} != {expected}")
    total_blocks = sum(result["block_modes"].values())
    if total_blocks != EXPECTED_TOTAL_BLOCKS:
        failures.append(f"total ETC blocks {total_blocks} != {EXPECTED_TOTAL_BLOCKS}")
    if result["image_rgba_bytes"] != EXPECTED_IMAGE_RGBA8_BYTES:
        failures.append(
            f"decoded image RGBA8 size {result['image_rgba_bytes']} != "
            f"{EXPECTED_IMAGE_RGBA8_BYTES}")
    if total_blocks * 16 * 4 != EXPECTED_BLOCK_PADDED_RGBA8_BYTES:
        failures.append(
            f"block-padded RGBA8 size {total_blocks * 16 * 4} != "
            f"{EXPECTED_BLOCK_PADDED_RGBA8_BYTES}")

    if tuple(caps) != EXPECTED_CLIENT_COMPRESSION_CAPS:
        failures.append(
            f"clientCompressionCaps {tuple(caps)} != {EXPECTED_CLIENT_COMPRESSION_CAPS}")

    if result["unknown_formats"]:
        for what, count in result["unknown_formats"].most_common():
            failures.append(f"unrecognised format value {what} x{count}")

    if failures:
        print("CHECK FAILED")
        for failure in failures:
            print(f"  {failure}")
        return 1
    print(f"CHECK OK: {len(EXPECTED_TEXTURES)} texture containers, "
          f"{EXPECTED_TOTAL_BLOCKS} ETC blocks, "
          f"{EXPECTED_BLOCK_MODES['T mode (ETC2 only)'] + EXPECTED_BLOCK_MODES['H mode (ETC2 only)'] + EXPECTED_BLOCK_MODES['planar (ETC2 only)']}"
          " of them in an ETC2-only mode, caps "
          f"{', '.join(EXPECTED_CLIENT_COMPRESSION_CAPS)}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--apk", type=Path, default=APK)
    parser.add_argument("--check", action="store_true",
                        help="compare against the recorded values and exit non-zero on any "
                             "disagreement")
    parser.add_argument("--list", action="store_true",
                        help="print every texture container and its format")
    args = parser.parse_args()

    if not args.apk.exists():
        print(f"APK not found: {args.apk}", file=sys.stderr)
        return 2

    result = census(args.apk)
    caps = client_compression_caps(args.apk)

    print("\n".join(report(result, caps)))
    if args.list:
        print("\nEvery texture container:")
        for path in sorted(result["textures"]):
            print(f"  {result['textures'][path]:<44} {path}")
    print()
    if args.check:
        return check(result, caps)
    return 0


if __name__ == "__main__":
    sys.exit(main())
