#!/usr/bin/env python3
"""Count the A64 branch mix of a guest library, straight out of the APK.

Why this exists
---------------
D16 measured what `optimization::INTERRUPTIBLE` costs and found that the cost is **per indirect
transfer** (about 3.9 ns), so it is zero for a guest with no indirect branches and up to ~4.7x for
one saturated with them. Which end of that band Omnidroid actually pays is a property of
`libroblox.so`, and D16 records that it "has not been measured against `libroblox.so`". This
measures it.

What it counts
--------------
Every 4-byte word of each executable section, classified by the ARM ARM's top-level branch
encodings:

* **direct** transfers: ``B``, ``BL`` (unconditional branch immediate), ``B.cond``, ``CBZ``/``CBNZ``,
  ``TBZ``/``TBNZ``. These leave a block through ``LinkBlock``, which `INTERRUPTIBLE` does not touch.
* **indirect** transfers: ``BR``, ``BLR``, ``RET``. These leave through ``PopRSBHint`` or
  ``FastDispatchHint``, which are the two terminals `INTERRUPTIBLE` redirects, and are therefore the
  ones that pay.

What it does **not** claim
--------------------------
This is a static count over section bytes, not a dynamic trace. `.text` in a stripped release binary
holds literal pools and padding as well as instructions, and any 4-byte constant can look like a
branch. So the figures are an **upper bound on density**, and the ratio between the two classes is
sounder than either absolute count. What a real workload executes is weighted by hot loops, which
only a trace would give. Both caveats belong in any report that quotes this.

Usage
-----
    python tools/branch_mix.py [APK] [--lib NAME]

Defaults to ``Roblox-2.738.1397.apk`` in the repository root and ``libroblox.so``. Exits 2 with a
clear message if the APK is absent, since it is git-ignored (Global Constraint 2).
"""

from __future__ import annotations

import argparse
import io
import struct
import sys
import zipfile
from pathlib import Path

# ---------------------------------------------------------------- classifiers


def is_b_or_bl(word: int) -> bool:
    """``B``/``BL``: ``op 00101 imm26``, i.e. bits 30:26 == 00101."""
    return (word & 0x7C00_0000) == 0x1400_0000


def is_b_cond(word: int) -> bool:
    """``B.cond``/``BC.cond``: ``0101010 0 imm19 o0 cond``."""
    return (word & 0xFF00_0000) == 0x5400_0000


def is_compare_branch(word: int) -> bool:
    """``CBZ``/``CBNZ``: ``sf 011010 0 op imm19 Rt``."""
    return (word & 0x7E00_0000) == 0x3400_0000


def is_test_branch(word: int) -> bool:
    """``TBZ``/``TBNZ``: ``b5 011011 op b40 imm14 Rt``."""
    return (word & 0x7E00_0000) == 0x3600_0000


def is_branch_register(word: int) -> bool:
    """Unconditional branch (register): ``1101011 opc op2 op3 Rn op4``.

    Covers ``BR``/``BLR``/``RET`` and the EL-changing forms (``ERET``, ``DRPS``), which do not occur
    in user-space code. Counted as one class because dynarmic ends all of them with an indirect
    terminal.
    """
    return (word & 0xFE00_0000) == 0xD600_0000


def is_mrs_tpidr_el0(word: int) -> bool:
    """``MRS Xt, TPIDR_EL0`` — D13's marker, recounted here as a cross-check on the whole method.

    If this script's own reading of the file is sound, this count should land on the 1,282 that D13
    records. A figure far from it means the section extraction is wrong, and every other number here
    with it.
    """
    return (word & 0xFFFF_FFE0) == 0xD53B_D040


def is_ldr_x_off_0x28(word: int) -> bool:
    """``LDR Xt, [Xn, #0x28]`` — the load 1,276 of those 1,282 perform."""
    return (word & 0xFFC0_0000) == 0xF940_0000 and ((word >> 10) & 0xFFF) == 0x28 // 8


def ldr_base_register(word: int) -> int:
    """``Rn`` of a load/store with an unsigned immediate offset."""
    return (word >> 5) & 0x1F


#: How far after an ``MRS Xt, TPIDR_EL0`` to look for the ``[Xt, #0x28]`` load.
#:
#: Not 1. The compiler schedules the rest of the prologue between them, so an
#: adjacent-instruction test undercounts badly (500 of 1,282 on this binary). Eight instructions is
#: generous enough to catch a scheduled prologue and short enough that a match is still plausibly
#: the same use; the count is reported alongside the window so it can be judged.
TPIDR_USE_WINDOW = 8


# ---------------------------------------------------------------------- ELF


def executable_sections(data: bytes) -> list[tuple[str, int, int]]:
    """Return ``(name, offset, size)`` for every SHF_EXECINSTR section with contents."""
    if data[:4] != b"\x7fELF" or data[4] != 2:
        raise ValueError("not a 64-bit ELF")
    (e_shoff,) = struct.unpack_from("<Q", data, 0x28)
    e_shentsize, e_shnum, e_shstrndx = struct.unpack_from("<HHH", data, 0x3A)
    if e_shoff == 0 or e_shnum == 0:
        raise ValueError("no section headers (fully stripped)")

    def header(i: int) -> tuple[int, int, int, int, int]:
        base = e_shoff + i * e_shentsize
        sh_name, sh_type = struct.unpack_from("<II", data, base)
        (sh_flags,) = struct.unpack_from("<Q", data, base + 8)
        sh_offset, sh_size = struct.unpack_from("<QQ", data, base + 0x18)
        return sh_name, sh_type, sh_flags, sh_offset, sh_size

    _, _, _, strtab_off, _ = header(e_shstrndx)

    def name_at(off: int) -> str:
        end = data.index(b"\0", strtab_off + off)
        return data[strtab_off + off : end].decode("utf-8", "replace")

    SHT_NOBITS = 8
    SHF_EXECINSTR = 0x4
    out = []
    for i in range(e_shnum):
        sh_name, sh_type, sh_flags, sh_offset, sh_size = header(i)
        if sh_type != SHT_NOBITS and sh_flags & SHF_EXECINSTR and sh_size:
            out.append((name_at(sh_name), sh_offset, sh_size))
    return out


# --------------------------------------------------------------------- main


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("apk", nargs="?", default="Roblox-2.738.1397.apk")
    parser.add_argument("--lib", default="libroblox.so")
    parser.add_argument("--abi", default="arm64-v8a")
    args = parser.parse_args()

    apk = Path(args.apk)
    if not apk.is_file():
        print(
            f"SKIPPED: {apk} is not present. It is git-ignored (Global Constraint 2); put the APK "
            f"in the repository root to run this.",
            file=sys.stderr,
        )
        return 2

    member = f"lib/{args.abi}/{args.lib}"
    with zipfile.ZipFile(apk) as zf:
        try:
            data = zf.read(member)
        except KeyError:
            print(f"ERROR: {member} is not in {apk}", file=sys.stderr)
            return 1

    sections = executable_sections(data)
    totals = {
        "words": 0,
        "b_bl": 0,
        "b_cond": 0,
        "cbz": 0,
        "tbz": 0,
        "indirect": 0,
        "mrs_tpidr": 0,
        "ldr_0x28": 0,
    }
    per_section = []
    for name, offset, size in sections:
        words = size // 4
        counts = dict.fromkeys(totals, 0)
        counts["words"] = words
        view = memoryview(data)[offset : offset + words * 4]
        # (register, words remaining in which a use still counts) for each live MRS result.
        pending_tpidr: dict[int, int] = {}
        for (word,) in struct.iter_unpack("<I", view):
            if is_b_or_bl(word):
                counts["b_bl"] += 1
            elif is_b_cond(word):
                counts["b_cond"] += 1
            elif is_compare_branch(word):
                counts["cbz"] += 1
            elif is_test_branch(word):
                counts["tbz"] += 1
            elif is_branch_register(word):
                counts["indirect"] += 1
            if is_mrs_tpidr_el0(word):
                counts["mrs_tpidr"] += 1
                pending_tpidr[word & 0x1F] = TPIDR_USE_WINDOW
                continue
            if pending_tpidr:
                if is_ldr_x_off_0x28(word):
                    base = ldr_base_register(word)
                    if base in pending_tpidr:
                        counts["ldr_0x28"] += 1
                        del pending_tpidr[base]
                for register in list(pending_tpidr):
                    pending_tpidr[register] -= 1
                    if pending_tpidr[register] <= 0:
                        del pending_tpidr[register]
        per_section.append((name, counts))
        for key, value in counts.items():
            totals[key] += value

    direct = totals["b_bl"] + totals["b_cond"] + totals["cbz"] + totals["tbz"]
    indirect = totals["indirect"]
    words = totals["words"]

    print(f"{member} from {apk}")
    print(f"executable sections: {', '.join(n for n, _ in per_section)}")
    print(f"words examined: {words:,} ({words * 4:,} bytes)")
    print()
    print(f"  direct transfers      {direct:>10,}  ({direct / words:7.3%} of words)")
    print(f"    B / BL              {totals['b_bl']:>10,}")
    print(f"    B.cond              {totals['b_cond']:>10,}")
    print(f"    CBZ / CBNZ          {totals['cbz']:>10,}")
    print(f"    TBZ / TBNZ          {totals['tbz']:>10,}")
    print(f"  indirect transfers    {indirect:>10,}  ({indirect / words:7.3%} of words)")
    print()
    if direct:
        print(f"  indirect : direct     1 : {direct / indirect:.2f}")
    print(f"  one indirect transfer every {words / indirect:.1f} words")
    print()
    print("D13 cross-check (these should land on 1,282 and 1,276):")
    print(f"  MRS Xt, TPIDR_EL0     {totals['mrs_tpidr']:>10,}")
    print(
        f"  ... with LDR Xt2, [Xt, #0x28] on the same register within {TPIDR_USE_WINDOW} "
        f"instructions  {totals['ldr_0x28']:>10,}"
    )
    print()
    print(
        "NOTE: a static count over section bytes. Literal pools and padding are counted as words "
        "and can alias branch encodings, so these are upper bounds on density; the ratio is sounder "
        "than either absolute. What a workload executes is weighted by its hot loops, which needs a "
        "trace, not a scan."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
