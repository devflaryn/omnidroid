#!/usr/bin/env python3
"""Decode what the guest passes to an imported function, from its own instructions.

``tools/init_reach.py`` answers *which* imports the 3,594 initializers can reach. This answers a
different question, and M3's gate is what made it worth asking: when a handler refuses because a
constant could not be verified -- ``sysconf``'s ``_SC_*`` numbering is the live example, and D22
records why it refuses rather than guesses -- the guest's own call sites say what the constant is
used for. HANDOFF names this technique explicitly: decoding the guest's instructions is what turned
``__gcov_dump`` from "leave it Unbound" into "resolve it to nothing".

Method, and what it can and cannot see:

1. ``DT_JMPREL`` gives every ``JUMP_SLOT`` relocation, and ``init_reach``'s ``plt_map`` decodes each
   PLT stub's ``ADRP``/``LDR`` pair back to a ``.got.plt`` slot. A stub is mapped to a symbol only
   when those two independent encodings agree, which is what licenses the mapping at all.
2. Every ``BL`` in an executable ``PT_LOAD`` whose target is that stub is a call site.
3. The 24 instructions before the ``BL`` are scanned for the last write to the argument register
   asked for, and a write that is a ``MOVZ``/``MOVN``/``MOV (immediate)`` yields a constant.

**It sees direct calls only.** A call through a register -- the 17,698 unresolvable indirect sites
D17 counts -- is invisible here exactly as it is to the reachability scan, so an empty result is
"no direct call site", never "never called". The M3 gate's census is what answers that, because it
watches the guest actually call.

::

    python tools/call_sites.py sysconf --arg 0
    python tools/call_sites.py prctl --arg 0 --arg 1
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from init_reach import Elf, bl_target, load_library, plt_map  # noqa: E402

#: ``DT_JMPREL`` and ``DT_PLTRELSZ``.
DT_JMPREL = 23
DT_PLTRELSZ = 2
#: One ``Elf64_Rela``.
RELA_BYTES = 24
#: How far back from a ``BL`` to look for the write that set an argument register.
WINDOW = 24


def jmprel(elf: Elf) -> list[tuple[int, int, int]]:
    """Every ``DT_JMPREL`` entry, as ``(r_offset, r_info, r_addend)``."""
    vaddr = elf.dyn(DT_JMPREL)
    size = elf.dyn(DT_PLTRELSZ)
    if vaddr is None or size is None:
        raise SystemExit("ERROR: no DT_JMPREL")
    off = elf.file_offset(vaddr, size)
    if off is None:
        raise SystemExit("ERROR: DT_JMPREL is not inside a PT_LOAD")
    out = []
    for i in range(size // RELA_BYTES):
        at = off + i * RELA_BYTES
        out.append(
            (
                int.from_bytes(elf.data[at : at + 8], "little"),
                int.from_bytes(elf.data[at + 8 : at + 16], "little"),
                int.from_bytes(elf.data[at + 16 : at + 24], "little", signed=True),
            )
        )
    return out


def writes_to(word: int, reg: int) -> bool:
    """Whether `word` is an instruction whose destination register is ``X{reg}``/``W{reg}``.

    Deliberately over-inclusive on the *destination* and exact on nothing else: this is used to
    stop a backward scan at the last write, so treating an instruction as a write when it is not
    loses a constant (reported as "not a constant") while missing one would report the *previous*
    value as the argument. Erring toward "unknown" is the safe direction here.
    """
    if (word & 0x1F) != reg:
        return False
    op = word & 0x7F80_0000
    # MOVZ/MOVN/MOVK, ADD/SUB immediate, ORR immediate (which is how `MOV Xd, #imm` of a bitmask
    # immediate and `MOV Xd, Xm` are both encoded), LDR literal, ADRP/ADR.
    known = {
        0x5280_0000,  # MOVZ (32-bit)
        0xD280_0000,  # MOVZ (64-bit)
        0x1280_0000,  # MOVN (32-bit)
        0x9280_0000,  # MOVN (64-bit)
        0x7280_0000,  # MOVK (32-bit)
        0xF280_0000,  # MOVK (64-bit)
    }
    if op in known:
        return True
    if (word & 0x7F00_0000) in (0x1100_0000, 0x3100_0000, 0x5100_0000, 0x7100_0000):
        return True  # ADD/SUB immediate
    if (word & 0x7F80_0000) in (0x3200_0000, 0xB200_0000):
        return True  # ORR immediate -- `MOV Xd, #bitmask`
    if (word & 0x1F00_0000) == 0x0A00_0000:
        return True  # logical shifted register -- `MOV Xd, Xm` is `ORR Xd, XZR, Xm`
    if (word & 0x9F00_0000) == 0x9000_0000 or (word & 0x9F00_0000) == 0x1000_0000:
        return True  # ADRP / ADR
    if (word & 0x3B00_0000) == 0x1900_0000:
        return True  # load/store immediate -- a load writes its Rt
    return False


def constant_in(word: int) -> int | None:
    """The immediate a ``MOVZ``/``MOVN`` puts in its destination, or ``None``."""
    sf = (word >> 31) & 1
    hw = (word >> 21) & 0x3
    imm16 = (word >> 5) & 0xFFFF
    op = word & 0x7F80_0000
    if op in (0x5280_0000, 0xD280_0000):  # MOVZ
        return imm16 << (16 * hw)
    if op in (0x1280_0000, 0x9280_0000):  # MOVN
        value = ~(imm16 << (16 * hw))
        return value & (0xFFFF_FFFF_FFFF_FFFF if sf else 0xFFFF_FFFF)
    if (word & 0x7F80_0000) in (0x3200_0000, 0xB200_0000) and ((word >> 5) & 0x1F) == 31:
        # ORR Xd, XZR, #bitmask -- `MOV Xd, #imm`. Not decoded: the bitmask encoding is its own
        # algorithm and no constant this tool has been asked about uses it. Reported as unknown
        # rather than guessed.
        return None
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("symbol", help="the imported symbol whose call sites to decode")
    parser.add_argument("source", nargs="?", default="")
    parser.add_argument("--lib", default="libroblox.so")
    parser.add_argument("--abi", default="arm64-v8a")
    parser.add_argument(
        "--arg",
        type=int,
        action="append",
        default=[],
        help="argument index (0 = X0/W0) to decode; repeatable",
    )
    parser.add_argument("--context", type=int, default=0, help="words before the call to print")
    parser.add_argument("--after", type=int, default=0, help="words after the call to print")
    args = parser.parse_args()

    root = Path(__file__).resolve().parent.parent
    data, described = load_library(root, args.source, args.lib, args.abi)
    elf = Elf(data)
    stubs, agreed, disagreed = plt_map(elf, jmprel(elf))
    print(f"{described}")
    print(f"PLT stubs decoded: {agreed} agreed, {disagreed} disagreed")

    wanted = {stub for stub, name in stubs.items() if name == args.symbol}
    if not wanted:
        print(f"`{args.symbol}` has no PLT stub: it is not called directly anywhere in this image")
        return 0
    print(f"`{args.symbol}` PLT stub(s): {', '.join(hex(s) for s in sorted(wanted))}")

    argregs = args.arg or [0]
    sites = 0
    for start, end, off in elf.exec_ranges:
        blob = data[off : off + (end - start)]
        for i in range(0, len(blob) - 3, 4):
            word = int.from_bytes(blob[i : i + 4], "little")
            if (word & 0xFC00_0000) != 0x9400_0000:  # BL
                continue
            pc = start + i
            if bl_target(word, pc) not in wanted:
                continue
            sites += 1
            found = []
            for reg in argregs:
                value = None
                for back in range(1, WINDOW + 1):
                    at = i - back * 4
                    if at < 0:
                        break
                    prev = int.from_bytes(blob[at : at + 4], "little")
                    if writes_to(prev, reg):
                        value = constant_in(prev)
                        break
                found.append(
                    f"X{reg}={value} ({value:#x})" if value is not None else f"X{reg}=?"
                )
            print(f"  call at {pc:#x}   {'  '.join(found)}")
            for back in range(args.context, 0, -1):
                at = i - back * 4
                if at >= 0:
                    print(
                        f"      {start + at:#x}: "
                        f"{int.from_bytes(blob[at:at + 4], 'little'):08x}"
                    )
            if args.context:
                print(f"      {pc:#x}: {word:08x}   <- BL")
            for fwd in range(1, args.after + 1):
                at = i + fwd * 4
                if at + 4 <= len(blob):
                    print(
                        f"      {start + at:#x}: "
                        f"{int.from_bytes(blob[at:at + 4], 'little'):08x}"
                    )
    print(f"{sites} direct call site(s). Indirect calls are invisible here; see the module docs.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
