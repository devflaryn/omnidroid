#!/usr/bin/env python3
"""Minimal Android binary XML (AXML) decoder - enough to read AndroidManifest.xml.

    python3 tools/axml.py <AndroidManifest.xml extracted from an APK>

Why this is in the repo: the whole session design rests on facts about the Roblox
build we ship (is ActivityProtocolLaunch still exported? does it still handle
roblox://?), and those facts have to be re-checked every time the agent produces
a new Roblox APK. See contracts/omni-session.md §6.

Stdlib only, no aapt/apktool/Android SDK needed, so it also runs on a machine
that only has the frozen engine. Reads the string pool + start/end tags; it is a
reader, not a full AXML implementation (no styles, no namespace prefixes).
"""
import struct
import sys

RES_STRING_POOL = 0x0001
RES_XML_START_NS = 0x0100
RES_XML_END_NS = 0x0101
RES_XML_START_TAG = 0x0102
RES_XML_END_TAG = 0x0103
RES_XML_CDATA = 0x0104
RES_XML_RESOURCE_MAP = 0x0180

TYPE_NULL = 0x00
TYPE_REFERENCE = 0x01
TYPE_STRING = 0x03
TYPE_INT_DEC = 0x10
TYPE_INT_HEX = 0x11
TYPE_INT_BOOL = 0x12


def read_string_pool(data, off):
    _typ, hdr_size, size = struct.unpack_from("<HHI", data, off)
    string_count, _style_count, flags, strings_start, _styles_start = \
        struct.unpack_from("<IIIII", data, off + 8)
    utf8 = bool(flags & (1 << 8))
    offsets = struct.unpack_from("<%dI" % string_count, data, off + hdr_size)
    base = off + strings_start
    out = []
    for o in offsets:
        p = base + o
        if utf8:
            # u8len (chars), u8len (bytes), then bytes
            n, p2 = decode_len8(data, p)
            n2, p3 = decode_len8(data, p2)
            out.append(data[p3:p3 + n2].decode("utf-8", "replace"))
        else:
            n, p2 = decode_len16(data, p)
            out.append(data[p2:p2 + n * 2].decode("utf-16-le", "replace"))
    return out, off + size


def decode_len8(data, p):
    n = data[p]
    if n & 0x80:
        n = ((n & 0x7F) << 8) | data[p + 1]
        return n, p + 2
    return n, p + 1


def decode_len16(data, p):
    n = struct.unpack_from("<H", data, p)[0]
    if n & 0x8000:
        n2 = struct.unpack_from("<H", data, p + 2)[0]
        return ((n & 0x7FFF) << 16) | n2, p + 4
    return n, p + 2


def s(pool, idx):
    if idx == 0xFFFFFFFF or idx >= len(pool):
        return None
    return pool[idx]


def fmt_value(pool, typ, val):
    if typ == TYPE_STRING:
        return s(pool, val)
    if typ == TYPE_INT_BOOL:
        return "true" if val else "false"
    if typ == TYPE_REFERENCE:
        return "@0x%08x" % val
    if typ == TYPE_INT_HEX:
        return "0x%x" % val
    return str(struct.unpack("<i", struct.pack("<I", val))[0])


def decode(path):
    data = open(path, "rb").read()
    magic, _fsize = struct.unpack_from("<II", data, 0)
    if magic & 0xFFFF != 0x0003:
        print("not AXML (magic=0x%08x)" % magic, file=sys.stderr)
    off = 8
    pool = []
    lines = []
    depth = 0
    while off < len(data):
        if off + 8 > len(data):
            break
        typ, hdr, size = struct.unpack_from("<HHI", data, off)
        if size == 0:
            break
        if typ == RES_STRING_POOL:
            pool, _ = read_string_pool(data, off)
        elif typ == RES_XML_START_TAG:
            ns_i, name_i = struct.unpack_from("<II", data, off + 8 + 8)
            attr_start, _attr_size, attr_count = struct.unpack_from(
                "<HHH", data, off + 8 + 16)
            name = s(pool, name_i)
            attrs = []
            ap = off + 8 + 8 + attr_start
            for i in range(attr_count):
                a_ns, a_name, a_raw, a_typed_size_res0, a_data = \
                    struct.unpack_from("<IIIII", data, ap + i * 20)
                a_type = (a_typed_size_res0 >> 24) & 0xFF
                an = s(pool, a_name)
                av = fmt_value(pool, a_type, a_data)
                if a_type == TYPE_STRING and s(pool, a_raw) is not None:
                    av = s(pool, a_raw)
                attrs.append('%s="%s"' % (an, av))
            lines.append("  " * depth + "<%s %s>" % (name, " ".join(attrs)))
            depth += 1
        elif typ == RES_XML_END_TAG:
            depth = max(0, depth - 1)
            ns_i, name_i = struct.unpack_from("<II", data, off + 8 + 8)
            lines.append("  " * depth + "</%s>" % s(pool, name_i))
        off += size
    return "\n".join(lines)


if __name__ == "__main__":
    print(decode(sys.argv[1]))
