"""What DT_INIT_ARRAY actually holds in both files, read from the bytes.

Prints, for each file: every PT_DYNAMIC's DT_INIT_ARRAY / DT_INIT_ARRAYSZ /
DT_FINI_ARRAY / DT_FINI_ARRAYSZ, the file offset that address maps to, and the
first and last few slots' contents. No pyelftools: the program headers and the
dynamic table are read by hand so the answer does not depend on a library's
opinion of a packed binary.
"""

import struct
import sys

PT_LOAD, PT_DYNAMIC = 1, 2
DT_INIT_ARRAY, DT_FINI_ARRAY = 25, 26
DT_INIT_ARRAYSZ, DT_FINI_ARRAYSZ = 27, 28
TAGS = {25: "INIT_ARRAY", 26: "FINI_ARRAY", 27: "INIT_ARRAYSZ", 28: "FINI_ARRAYSZ"}


def vaddr_to_offset(segments, vaddr):
    for p_type, p_offset, p_vaddr, p_filesz in segments:
        if p_type != PT_LOAD:
            continue
        if p_vaddr <= vaddr < p_vaddr + p_filesz:
            return p_offset + (vaddr - p_vaddr)
    return None


def report(path):
    data = open(path, "rb").read()
    print(f"\n=== {path} ({len(data)} bytes) ===")
    if data[:4] != b"\x7fELF":
        print("  not an ELF")
        return
    (e_phoff,) = struct.unpack_from("<Q", data, 0x20)
    (e_phentsize, e_phnum) = struct.unpack_from("<HH", data, 0x36)
    segments = []
    for i in range(e_phnum):
        at = e_phoff + i * e_phentsize
        p_type, _flags = struct.unpack_from("<II", data, at)
        p_offset, p_vaddr = struct.unpack_from("<QQ", data, at + 8)
        p_filesz = struct.unpack_from("<Q", data, at + 32)[0]
        segments.append((p_type, p_offset, p_vaddr, p_filesz))
        if p_type == PT_DYNAMIC:
            dynamic = (p_offset, p_filesz)

    found = {}
    offset, size = dynamic
    for at in range(offset, offset + size, 16):
        tag, value = struct.unpack_from("<QQ", data, at)
        if tag in TAGS:
            found.setdefault(TAGS[tag], value)
        if tag == 0:
            break
    for key in ("INIT_ARRAY", "INIT_ARRAYSZ", "FINI_ARRAY", "FINI_ARRAYSZ"):
        print(f"  DT_{key:<13} = {found.get(key, 0):#x}")

    array_vaddr = found.get("INIT_ARRAY", 0)
    array_bytes = found.get("INIT_ARRAYSZ", 0)
    at = vaddr_to_offset(segments, array_vaddr)
    print(f"  init_array maps to file offset {at}")
    if at is None or not array_bytes:
        print("  no init_array contents to read")
        return
    count = array_bytes // 8
    slots = [
        struct.unpack_from("<Q", data, at + i * 8)[0] for i in range(min(count, 8))
    ]
    print(f"  {count} slots; first {len(slots)}: " + ", ".join(f"{s:#x}" for s in slots))
    nonzero = sum(
        1
        for i in range(count)
        if struct.unpack_from("<Q", data, at + i * 8)[0] != 0
    )
    print(f"  non-zero slots: {nonzero} of {count}")


for argument in sys.argv[1:]:
    report(argument)
