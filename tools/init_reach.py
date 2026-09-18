#!/usr/bin/env python3
"""Which imported symbols are reachable from ``libroblox.so``'s 3,594 static initializers.

Why this exists
---------------
M3 has to run all 3,594 ``DT_INIT_ARRAY`` entries, and every imported symbol they reach must have a
host implementation behind the thunk boundary before the first of them can run. `libroblox.so`
imports **565** symbols; the initializers touch a subset, and *that* subset is the bionic work M3
actually owes. This computes it statically.

What it computes
----------------
Four nested answers, because one number would be dishonest:

* **Tier A0 — the sensitivity floor.** The same closure as Tier A but following ``BL`` only. The gap
  between A0 and A is what the tail-call and cold-split edges are worth, and therefore how sensitive
  the answer is to the one edge kind a literal pool can most easily fake.

* **Tier A — evidenced.** The transitive closure of *direct* control transfers (``BL``, plus ``B``
  and the conditional forms whose target leaves the function, which is how tail calls and
  hot/cold-split fragments appear) starting at the 3,594 initializer entry points, over the 245,117
  exact function bounds recovered from ``.eh_frame_hdr``. An import counts when a reachable function
  either branches to its PLT stub (**A1**) or materialises its GOT slot with an ``ADRP``/``LDR`` pair
  (**A2**, which is how the 31 non-PLT imports and any address-taken libc function are referenced).
  This is a **lower bound**: see the limits below.

* **Tier C — ceiling.** The same closure, additionally following every function whose *address is
  taken* by a reachable function (``ADRP``+``ADD``, ``ADR``) as if it were called, transitively.
  Indirect calls in C++ go through a pointer that has to be materialised somewhere, so this is a
  gross over-approximation in the other direction: it follows pointers that are stored, compared or
  never called at all.

* **The whole binary.** All 565 imports, the ceiling no reachability argument can exceed.

On `libroblox.so` these come out at **113 / 188 / 246 / 565**.

What this method cannot see, stated plainly
-------------------------------------------
A static call graph over a stripped binary is a **lower bound** on reachability, and these are the
specific holes. Each is *counted* in the output rather than merely acknowledged, because the size of
the hole is the only thing that makes the headline number usable.

1. **Indirect calls are unresolved.** ``BLR Xn`` and ``BR Xn`` targets are computed at run time. C++
   virtual dispatch, every function pointer, every ``std::function`` and Luau's own dispatch are all
   in this class. The output reports how many such sites the reachable set contains.
2. **PLT-mediated calls through a register are unresolved.** A call that loads the GOT slot and
   ``BLR``s it is counted as an A2 reference (the slot is named in the instruction stream), but a
   call through a GOT slot loaded by *another* function and passed in is not visible at all.
3. **Vtables and function-pointer tables are not followed.** They are built by
   ``R_AARCH64_RELATIVE`` relocations into ``.data.rel.ro``, and the output reports how many distinct
   function starts those relocations name — the population an indirect call can land on.
4. **``ADRP`` pairing is windowed, not a dataflow analysis.** A base register is matched to a later
   ``LDR``/``ADD`` within :data:`ADRP_USE_WINDOW` instructions, the same shape
   ``tools/branch_mix.py`` uses for D13's ``TPIDR_EL0`` pairs. A pair the compiler scheduled further
   apart is missed; a coincidental one is counted.
5. **Non-instruction words are decoded as instructions.** Literal pools and jump tables inside a
   function's FDE bounds can alias a branch encoding. This *adds* spurious edges, so it pushes Tier A
   up, not down — the opposite direction from 1-4.
6. **2,671,728 bytes of executable section (3.7%) have no FDE**, and 2,670,684 of them are **one
   contiguous region** at ``0x28b9db0``-``0x2b45e0c`` — every other gap is 300 bytes or less of
   padding. One of the 3,594 initializer entry points (slot 60, ``0x29f49ac``) is inside it. A call
   landing in that region resolves to no function, so :meth:`Reach.synthesize` guesses an extent
   ending at the first ``RET``; that guess is what takes Tier A from 121 imports to 188, so it is not
   a detail. It under-reads a multi-``RET`` function, which keeps Tier A a lower bound.
7. **Initializers run host code too.** ``__cxa_atexit`` handlers, and anything an initializer
   registers for later, are not initializer-time reachability but will need the same imports.

Task 4 compares this prediction against what the initializers actually call. The value of this
measurement is in the limits above being honest, not in the headline.

Self-check
----------
Five counts are pinned against figures established in M1 and M2 (`docs/DECISIONS.md` D9, D7,
`docs/ARCHITECTURE.md` section 4). Any disagreement exits non-zero rather than printing a number,
because an unpinned scanner is how a miscount survived two review rounds in M2:

===========================================  =========
``.eh_frame_hdr`` FDEs                       245,117
FDEs whose ``pc_begin`` disagrees with the
``.eh_frame_hdr`` search table               0
undefined (imported) ``.dynsym`` entries     565
``DT_INIT_ARRAY`` slots                      3,594
``APS2`` relocations, by type                568,194 ``RELATIVE`` + 56 ``GLOB_DAT`` + 22 ``ABS64``
``DT_JMPREL`` ``JUMP_SLOT`` relocations      534
PLT stubs whose decoded GOT slot matches
the ``DT_JMPREL`` entry of the same index    534 of 534
===========================================  =========

Usage
-----
    python tools/init_reach.py [APK-or-SO] [--lib NAME] [--abi NAME] [--list] [--self-check-only]

Defaults to ``Roblox-2.738.1397.apk`` in the repository root, falling back to the extraction cache
the test fixtures fill. Exits 2 with a clear message if neither is present, since the APK is
git-ignored (Global Constraint 2).
"""

from __future__ import annotations

import argparse
import bisect
import re
import struct
import sys
import zipfile
from collections import Counter, defaultdict
from pathlib import Path

# ------------------------------------------------------------------ pinned counts

EXPECTED_FDE_COUNT = 245_117
EXPECTED_IMPORTS = 565
EXPECTED_INIT_ARRAY = 3_594
EXPECTED_JMPREL = 534
EXPECTED_APS2_BY_TYPE = {1027: 568_194, 1025: 56, 257: 22}

R_AARCH64_ABS64 = 257
R_AARCH64_GLOB_DAT = 1025
R_AARCH64_JUMP_SLOT = 1026
R_AARCH64_RELATIVE = 1027

#: How far after an ``ADRP`` a ``LDR``/``ADD`` on the same base register still counts as a pair.
#:
#: Eight instructions, the same window and for the same reason as ``tools/branch_mix.py``'s
#: ``TPIDR_USE_WINDOW``: the compiler schedules other work between the two halves of an address
#: materialisation, so an adjacent-instruction test undercounts badly, and a window this short keeps
#: a match plausibly the same use. Reported alongside the counts so it can be judged.
ADRP_USE_WINDOW = 8


# --------------------------------------------------------------------- SLEB128 / APS2

APS2_MAGIC = b"APS2"

RELOCATION_GROUPED_BY_INFO_FLAG = 1
RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG = 2
RELOCATION_GROUPED_BY_ADDEND_FLAG = 4
RELOCATION_GROUP_HAS_ADDEND_FLAG = 8


class Sleb128:
    """A forward SLEB128 reader over a ``bytes``. Mirrors ``omni_elf::aps2::Sleb128Decoder``."""

    __slots__ = ("data", "pos")

    def __init__(self, data: bytes) -> None:
        self.data = data
        self.pos = 0

    def pop(self) -> int:
        value = 0
        shift = 0
        data = self.data
        while True:
            if self.pos >= len(data):
                raise ValueError("APS2: SLEB128 ran off the end of the blob")
            byte = data[self.pos]
            self.pos += 1
            value |= (byte & 0x7F) << shift
            shift += 7
            if not byte & 0x80:
                if byte & 0x40 and shift < 64:
                    value |= -(1 << shift)
                break
        # Two's-complement fold into a signed 64-bit value.
        value &= (1 << 64) - 1
        if value & (1 << 63):
            value -= 1 << 64
        return value


def decode_aps2(blob: bytes) -> list[tuple[int, int, int]]:
    """Decode a ``DT_ANDROID_RELA`` blob into ``(r_offset, r_info, r_addend)`` triples.

    A direct transcription of ``omni_elf::aps2::decode_with``, which is itself a transcription of
    bionic's ``linker_reloc_iterators.h``. The two addend bits are switched on *together*, because
    ``GROUPED_BY_ADDEND`` moves the addend delta into the group header and applies it **once per
    group**, not once per relocation.
    """
    if blob[:4] != APS2_MAGIC:
        raise ValueError(f"APS2: bad magic {blob[:4]!r}")
    dec = Sleb128(blob[4:])
    declared = dec.pop()
    if declared < 0:
        raise ValueError(f"APS2: negative relocation count {declared}")
    r_offset = dec.pop()
    r_info = 0
    r_addend = 0
    out: list[tuple[int, int, int]] = []
    mask = (1 << 64) - 1
    while len(out) < declared:
        group_size = dec.pop()
        if group_size <= 0:
            raise ValueError(f"APS2: group size {group_size} would not terminate")
        group_flags = dec.pop()
        group_offset_delta = (
            dec.pop() if group_flags & RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG else 0
        )
        if group_flags & RELOCATION_GROUPED_BY_INFO_FLAG:
            r_info = dec.pop() & mask
        addend_bits = group_flags & (
            RELOCATION_GROUP_HAS_ADDEND_FLAG | RELOCATION_GROUPED_BY_ADDEND_FLAG
        )
        per_reloc_addend = addend_bits == RELOCATION_GROUP_HAS_ADDEND_FLAG
        if per_reloc_addend:
            pass
        elif addend_bits == (
            RELOCATION_GROUP_HAS_ADDEND_FLAG | RELOCATION_GROUPED_BY_ADDEND_FLAG
        ):
            r_addend += dec.pop()
        else:
            r_addend = 0
        grouped_by_offset = bool(group_flags & RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG)
        grouped_by_info = bool(group_flags & RELOCATION_GROUPED_BY_INFO_FLAG)
        for _ in range(group_size):
            r_offset = (r_offset + (group_offset_delta if grouped_by_offset else dec.pop())) & mask
            if not grouped_by_info:
                r_info = dec.pop() & mask
            if per_reloc_addend:
                r_addend += dec.pop()
            out.append((r_offset, r_info, r_addend))
    consumed = len(APS2_MAGIC) + dec.pos
    if consumed != len(blob):
        raise ValueError(
            f"APS2: consumed {consumed} of {len(blob)} bytes; byte-exact consumption is the "
            f"correctness proof for this format"
        )
    return out


# --------------------------------------------------------------------------- ELF


class Elf:
    """Just enough AArch64 ELF64 to answer this question, read straight out of the file bytes."""

    def __init__(self, data: bytes) -> None:
        if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
            raise ValueError("not a little-endian 64-bit ELF")
        self.data = data
        (e_shoff,) = struct.unpack_from("<Q", data, 0x28)
        e_shentsize, e_shnum, e_shstrndx = struct.unpack_from("<HHH", data, 0x3A)
        if not e_shoff or not e_shnum:
            raise ValueError("no section headers (fully stripped)")

        def header(i: int) -> dict:
            b = e_shoff + i * e_shentsize
            (sh_name, sh_type) = struct.unpack_from("<II", data, b)
            sh_flags, sh_addr, sh_offset, sh_size = struct.unpack_from("<QQQQ", data, b + 8)
            sh_link, sh_info = struct.unpack_from("<II", data, b + 0x28)
            return dict(
                raw_name=sh_name,
                type=sh_type,
                flags=sh_flags,
                addr=sh_addr,
                off=sh_offset,
                size=sh_size,
                link=sh_link,
                info=sh_info,
            )

        strtab = header(e_shstrndx)

        def name_at(off: int) -> str:
            end = data.index(b"\0", strtab["off"] + off)
            return data[strtab["off"] + off : end].decode("utf-8", "replace")

        self.sections = []
        for i in range(e_shnum):
            h = header(i)
            h["name"] = name_at(h["raw_name"])
            self.sections.append(h)
        self.by_name = {h["name"]: h for h in self.sections if h["name"]}

        # Executable ranges, in vaddr space, with the file offset of each.
        SHT_NOBITS = 8
        SHF_EXECINSTR = 0x4
        self.exec_ranges = sorted(
            (h["addr"], h["addr"] + h["size"], h["off"])
            for h in self.sections
            if h["type"] != SHT_NOBITS and h["flags"] & SHF_EXECINSTR and h["size"]
        )
        self._exec_starts = [r[0] for r in self.exec_ranges]

        self.dynstr = self.by_name[".dynstr"]
        self.symbols = self._read_dynsym()
        self.dynamic = self._read_dynamic()

    # ------------------------------------------------------------------ helpers

    def dyn_string(self, off: int) -> str:
        base = self.dynstr["off"]
        end = self.data.index(b"\0", base + off)
        return self.data[base + off : end].decode("utf-8", "replace")

    def file_offset(self, vaddr: int, length: int = 1) -> int | None:
        """The file offset of ``vaddr``, or ``None`` if no section with contents covers it."""
        for h in self.sections:
            if h["type"] == 8 or not h["addr"] or not h["size"]:
                continue
            if h["addr"] <= vaddr and vaddr + length <= h["addr"] + h["size"]:
                return h["off"] + (vaddr - h["addr"])
        return None

    def word(self, vaddr: int) -> int | None:
        """The 32-bit word at an executable ``vaddr``, or ``None`` if it is not in one."""
        i = bisect.bisect_right(self._exec_starts, vaddr) - 1
        if i < 0:
            return None
        start, end, off = self.exec_ranges[i]
        if not (start <= vaddr and vaddr + 4 <= end):
            return None
        return struct.unpack_from("<I", self.data, off + (vaddr - start))[0]

    def is_executable(self, vaddr: int) -> bool:
        i = bisect.bisect_right(self._exec_starts, vaddr) - 1
        return i >= 0 and self.exec_ranges[i][0] <= vaddr < self.exec_ranges[i][1]

    # ------------------------------------------------------------------ tables

    def _read_dynsym(self) -> list[dict]:
        sec = self.by_name[".dynsym"]
        out = []
        for i in range(sec["size"] // 24):
            b = sec["off"] + i * 24
            st_name, st_info, _st_other, st_shndx = struct.unpack_from("<IBBH", self.data, b)
            st_value, st_size = struct.unpack_from("<QQ", self.data, b + 8)
            out.append(
                dict(
                    name=self.dyn_string(st_name) if st_name else "",
                    kind=st_info & 0xF,
                    shndx=st_shndx,
                    value=st_value,
                    size=st_size,
                )
            )
        return out

    def _read_dynamic(self) -> list[tuple[int, int]]:
        sec = self.by_name[".dynamic"]
        out = []
        for i in range(sec["size"] // 16):
            tag, val = struct.unpack_from("<qQ", self.data, sec["off"] + i * 16)
            out.append((tag, val))
            if tag == 0:
                break
        return out

    def dyn(self, tag: int) -> int | None:
        for t, v in self.dynamic:
            if t == tag:
                return v
        return None

    def needed(self) -> list[str]:
        return [self.dyn_string(v) for t, v in self.dynamic if t == 1]

    # ------------------------------------------------------ symbol version -> library

    def version_libraries(self) -> dict[int, str]:
        """``vna_other`` index to the ``DT_NEEDED`` file the version comes from."""
        sec = self.by_name.get(".gnu.version_r")
        if sec is None:
            return {}
        out: dict[int, str] = {}
        cur = sec["off"]
        while True:
            _v, vn_cnt, vn_file, vn_aux, vn_next = struct.unpack_from("<HHIII", self.data, cur)
            lib = self.dyn_string(vn_file)
            a = cur + vn_aux
            for _ in range(vn_cnt):
                _h, _f, vna_other, _n, vna_next = struct.unpack_from("<IHHII", self.data, a)
                out[vna_other & 0x7FFF] = lib
                if not vna_next:
                    break
                a += vna_next
            if not vn_next:
                break
            cur += vn_next
        return out

    def symbol_versions(self) -> list[int]:
        sec = self.by_name.get(".gnu.version")
        if sec is None:
            return [0] * len(self.symbols)
        return [
            struct.unpack_from("<H", self.data, sec["off"] + i * 2)[0] & 0x7FFF
            for i in range(len(self.symbols))
        ]

    # -------------------------------------------------------------- eh_frame_hdr

    def function_bounds(self) -> tuple[list[tuple[int, int]], int]:
        """``(start, length)`` per FDE, sorted, plus the number of table/FDE disagreements.

        Both encodings of the function's start address are read — the ``.eh_frame_hdr`` search
        table's ``initial_location`` and the FDE's own ``pc_begin`` — and compared, exactly as
        ``omni_elf::eh_frame`` does. A single disagreement means the header and ``.eh_frame`` do not
        describe the same binary.
        """
        eh = self.by_name[".eh_frame_hdr"]
        ef = self.by_name[".eh_frame"]
        data = self.data
        o = eh["off"]
        version, eh_ptr_enc, fde_count_enc, table_enc = data[o], data[o + 1], data[o + 2], data[o + 3]
        if version != 1:
            raise ValueError(f".eh_frame_hdr version {version} is not 1")
        # DW_EH_PE_pcrel|sdata4, DW_EH_PE_udata4, DW_EH_PE_datarel|sdata4. Refused rather than
        # guessed at, because a wrong guess yields a plausible but wrong function map.
        if (eh_ptr_enc, fde_count_enc, table_enc) != (0x1B, 0x03, 0x3B):
            raise ValueError(
                f".eh_frame_hdr encodings {eh_ptr_enc:#x}/{fde_count_enc:#x}/{table_enc:#x} are "
                f"not the 0x1b/0x03/0x3b this tool implements"
            )
        pos = o + 4
        (rel,) = struct.unpack_from("<i", data, pos)
        eh_frame_vaddr = (eh["addr"] + 4) + rel
        pos += 4
        (fde_count,) = struct.unpack_from("<I", data, pos)
        pos += 4
        if pos - o + fde_count * 8 > eh["size"]:
            raise ValueError("the .eh_frame_hdr search table does not fit in its own section")
        base = eh["addr"]
        frame_off = ef["off"]
        frame_addr = ef["addr"]
        bounds = []
        disagreements = 0
        unpack = struct.unpack_from
        for i in range(fde_count):
            initial_rel, fde_rel = unpack("<ii", data, pos + i * 8)
            initial_location = base + initial_rel
            fde_vaddr = base + fde_rel
            fo = frame_off + (fde_vaddr - frame_addr)
            (pc_begin_rel,) = unpack("<i", data, fo + 8)
            pc_begin = (fde_vaddr + 8) + pc_begin_rel
            (pc_range,) = unpack("<I", data, fo + 12)
            if pc_begin != initial_location:
                disagreements += 1
            bounds.append((initial_location, pc_range))
        bounds.sort()
        return bounds, disagreements


# ------------------------------------------------------------------- A64 decoding


def bl_target(word: int, pc: int) -> int:
    imm = word & 0x03FF_FFFF
    if imm & (1 << 25):
        imm -= 1 << 26
    return pc + imm * 4


def b19_target(word: int, pc: int) -> int:
    imm = (word >> 5) & 0x7FFFF
    if imm & (1 << 18):
        imm -= 1 << 19
    return pc + imm * 4


def b14_target(word: int, pc: int) -> int:
    imm = (word >> 5) & 0x3FFF
    if imm & (1 << 13):
        imm -= 1 << 14
    return pc + imm * 4


def adrp_page(word: int, pc: int) -> int:
    immlo = (word >> 29) & 3
    immhi = (word >> 5) & 0x7FFFF
    imm = (immhi << 2) | immlo
    if imm & (1 << 20):
        imm -= 1 << 21
    return (pc & ~0xFFF) + imm * 4096


def adr_target(word: int, pc: int) -> int:
    immlo = (word >> 29) & 3
    immhi = (word >> 5) & 0x7FFFF
    imm = (immhi << 2) | immlo
    if imm & (1 << 20):
        imm -= 1 << 21
    return pc + imm


# --------------------------------------------------------------------- PLT map


def plt_map(elf: Elf, jmprel: list[tuple[int, int, int]]) -> tuple[dict[int, str], int, int]:
    """``plt stub vaddr -> import name``, plus how many stubs agreed and disagreed.

    The stub is decoded rather than assumed: its ``ADRP``/``LDR`` pair names a ``.got.plt`` slot, and
    that slot must be the ``r_offset`` of the ``DT_JMPREL`` entry with the same index. The two are
    independent encodings of the same fact, so full agreement is what licenses the mapping.
    """
    plt = elf.by_name[".plt"]
    stub_bytes = 16
    header_bytes = 32
    out: dict[int, str] = {}
    agreed = 0
    disagreed = 0
    for i, (r_offset, r_info, _addend) in enumerate(jmprel):
        stub = plt["addr"] + header_bytes + i * stub_bytes
        w0 = elf.word(stub)
        w1 = elf.word(stub + 4)
        if w0 is None or w1 is None or (w0 & 0x9F00_0000) != 0x9000_0000:
            disagreed += 1
            continue
        if (w1 & 0xFFC0_0000) != 0xF940_0000:  # LDR Xt, [Xn, #imm12*8]
            disagreed += 1
            continue
        slot = adrp_page(w0, stub) + ((w1 >> 10) & 0xFFF) * 8
        if slot != r_offset:
            disagreed += 1
            continue
        agreed += 1
        out[stub] = elf.symbols[r_info >> 32]["name"]
    return out, agreed, disagreed


# ---------------------------------------------------------------- provider groups

#: Where the report's grouping comes from: the committed enumeration behind ARCHITECTURE section 5.
PROVIDER_DOC = Path("docs/research/apk-undefined-symbols.txt")


def provider_groups(root: Path) -> dict[str, str]:
    """``symbol -> providing library``, parsed from the committed undefined-symbol appendix.

    Reused rather than re-derived so that this report's grouping is the same one
    ``ARCHITECTURE.md`` section 5 states, and so a symbol cannot be silently regrouped here.
    ``.gnu.version_r`` independently attributes the 407 versioned symbols, and
    :func:`check_provider_agreement` compares the two.
    """
    path = root / PROVIDER_DOC
    if not path.is_file():
        return {}
    groups: dict[str, str] = {}
    current: str | None = None
    header = re.compile(r"^### (.+?)\s+\(\d+ symbols\)\s*$")
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        m = header.match(line)
        if m:
            current = m.group(1).strip()
            if current.startswith("PER-LIBRARY") or current.startswith("FULL "):
                current = None
            continue
        if line.startswith("### "):
            current = None
            continue
        if current is None or not line or line.startswith(("=", " ")):
            continue
        name = line.split()[0]
        if name and name not in groups:
            groups[name] = current
    return groups


# --------------------------------------------------------------------- the scan


class Reach:
    """The closure itself."""

    def __init__(self, elf: Elf, bounds: list[tuple[int, int]], stubs: dict[int, str],
                 got_imports: dict[int, str]) -> None:
        self.elf = elf
        self.bounds = bounds
        self.starts = [b[0] for b in bounds]
        self.starts_set = {b[0] for b in bounds}
        self.stubs = stubs
        self.got_imports = got_imports
        self.reset()

    def reset(self) -> None:
        self.visited: set[int] = set()
        self.synth: dict[int, int] = {}
        self.bl_only = False
        self.plt_hits: Counter = Counter()
        self.got_hits: Counter = Counter()
        self.blr_sites = 0
        self.br_sites = 0
        self.unresolved_targets: Counter = Counter()
        self.mid_function_targets = 0
        self.address_taken: set[int] = set()
        self.words_scanned = 0

    def containing(self, vaddr: int) -> int | None:
        """The start of the FDE-bounded function containing ``vaddr``, or ``None``."""
        i = bisect.bisect_right(self.starts, vaddr) - 1
        if i < 0:
            return None
        start, length = self.bounds[i]
        return start if vaddr < start + length else None

    def closure(self, roots: list[int], follow_address_taken: bool,
                bl_only: bool = False) -> None:
        self.reset()
        self.bl_only = bl_only
        work: list[int] = []
        for r in roots:
            self._add(r, work)
        while work:
            self._scan(work.pop(), work, follow_address_taken)

    #: Words to search for a ``RET`` before giving up on an FDE-less call target.
    SYNTH_LIMIT_WORDS = 16_384

    def synthesize(self, target: int) -> int | None:
        """An extent for a call target that no FDE covers: up to and including the first ``RET``.

        2,670,684 of ``libroblox.so``'s executable bytes are in **one** contiguous region with no
        FDE at all (``0x28b9db0``-``0x2b45e0c``), and one of the 3,594 initializer entry points is
        inside it. Refusing to scan it would drop that initializer's whole subtree, so the extent is
        guessed — and the guess is deliberately the one that *under*-reads: a function with more than
        one ``RET`` is truncated at the first, so this keeps Tier A a lower bound rather than
        inventing edges. Counted separately in the output so its contribution is visible.
        """
        if not self.elf.is_executable(target):
            return None
        pc = target
        for _ in range(self.SYNTH_LIMIT_WORDS):
            word = self.elf.word(pc)
            if word is None:
                return None
            if word == 0xD65F_03C0:  # RET X30
                return pc + 4 - target
            pc += 4
        return None

    def _add(self, target: int, work: list[int]) -> None:
        f = self.containing(target)
        if f is None:
            if target in self.synth:
                f = target
            else:
                length = self.synthesize(target)
                if length is None:
                    self.unresolved_targets[target] += 1
                    return
                self.synth[target] = length
                f = target
        elif f != target:
            self.mid_function_targets += 1
        if f not in self.visited:
            self.visited.add(f)
            work.append(f)

    def _scan(self, start: int, work: list[int], follow_address_taken: bool) -> None:
        length = self.synth.get(start)
        if length is None:
            idx = bisect.bisect_left(self.starts, start)
            length = self.bounds[idx][1]
        end = start + length
        elf = self.elf
        # (base register) -> (page, instructions of life remaining)
        pages: dict[int, tuple[int, int]] = {}
        pc = start
        while pc < end:
            word = elf.word(pc)
            if word is None:
                break
            self.words_scanned += 1
            top = word & 0xFC00_0000
            if top == 0x9400_0000:  # BL
                target = bl_target(word, pc)
                if target in self.stubs:
                    self.plt_hits[self.stubs[target]] += 1
                else:
                    self._add(target, work)
            elif top == 0x1400_0000 and not self.bl_only:  # B: a tail call, if it leaves
                target = bl_target(word, pc)
                if target in self.stubs:
                    self.plt_hits[self.stubs[target]] += 1
                elif not (start <= target < end):
                    self._add(target, work)
            elif (word & 0xFF00_0000) == 0x5400_0000 and not self.bl_only:  # B.cond
                target = b19_target(word, pc)
                if not (start <= target < end):
                    self._add(target, work)
            elif (word & 0x7E00_0000) == 0x3400_0000 and not self.bl_only:  # CBZ / CBNZ
                target = b19_target(word, pc)
                if not (start <= target < end):
                    self._add(target, work)
            elif (word & 0x7E00_0000) == 0x3600_0000 and not self.bl_only:  # TBZ / TBNZ
                target = b14_target(word, pc)
                if not (start <= target < end):
                    self._add(target, work)
            elif (word & 0xFFFF_FC1F) == 0xD63F_0000:  # BLR Xn
                self.blr_sites += 1
            elif (word & 0xFFFF_FC1F) == 0xD61F_0000:  # BR Xn
                self.br_sites += 1

            # Address materialisation, for the GOT-slot references and the Tier C closure.
            if (word & 0x9F00_0000) == 0x9000_0000:  # ADRP Xd, page
                pages[word & 0x1F] = (adrp_page(word, pc), ADRP_USE_WINDOW)
            elif (word & 0x9F00_0000) == 0x1000_0000:  # ADR Xd, imm
                if follow_address_taken:
                    t = adr_target(word, pc)
                    if t in self.starts_set:
                        self.address_taken.add(t)
                        if t not in self.visited:
                            self.visited.add(t)
                            work.append(t)
            elif pages:
                if (word & 0xFFC0_0000) == 0xF940_0000:  # LDR Xt, [Xn, #imm12*8]
                    base = (word >> 5) & 0x1F
                    live = pages.get(base)
                    if live is not None:
                        slot = live[0] + ((word >> 10) & 0xFFF) * 8
                        name = self.got_imports.get(slot)
                        if name is not None:
                            self.got_hits[name] += 1
                elif (word & 0xFF80_0000) == 0x9100_0000:  # ADD Xd, Xn, #imm12
                    base = (word >> 5) & 0x1F
                    live = pages.get(base)
                    if live is not None and follow_address_taken:
                        t = live[0] + ((word >> 10) & 0xFFF)
                        if t in self.starts_set:
                            self.address_taken.add(t)
                            if t not in self.visited:
                                self.visited.add(t)
                                work.append(t)
            if pages:
                for reg in list(pages):
                    page, life = pages[reg]
                    if life <= 1:
                        del pages[reg]
                    else:
                        pages[reg] = (page, life - 1)
            pc += 4


# ------------------------------------------------------------------------ report


def load_library(root: Path, source: str, lib: str, abi: str) -> tuple[bytes, str]:
    candidates = [Path(source)] if source else []
    if not source:
        candidates = [
            root / "Roblox-2.738.1397.apk",
            root / "target/omni-elf-fixtures" / lib,
        ]
    for path in candidates:
        if not path.is_file():
            continue
        if path.suffix == ".apk":
            with zipfile.ZipFile(path) as zf:
                member = f"lib/{abi}/{lib}"
                try:
                    return zf.read(member), f"{member} from {path}"
                except KeyError:
                    raise SystemExit(f"ERROR: {member} is not in {path}")
        return path.read_bytes(), str(path)
    raise SystemExit(
        f"SKIPPED: no library found. Tried {', '.join(str(c) for c in candidates)}. The APK is "
        f"git-ignored (Global Constraint 2); put it in the repository root, or run the omni-elf "
        f"tests once to fill the extraction cache."
    )


def check_provider_agreement(
    elf: Elf, imports: list[int], groups: dict[str, str]
) -> tuple[int, int, list[str]]:
    """Compare the doc's grouping with ``.gnu.version_r``, which is authoritative where it exists."""
    libs = elf.version_libraries()
    versions = elf.symbol_versions()
    agree = 0
    checked = 0
    complaints = []
    for i in imports:
        lib = libs.get(versions[i])
        if lib is None:
            continue
        checked += 1
        name = elf.symbols[i]["name"]
        group = groups.get(name, "")
        want = {"libc.so": ("libc",), "libm.so": ("libm",), "libdl.so": ("libdl",)}.get(lib)
        if want is None:
            checked -= 1
            continue
        if any(w in group for w in want):
            agree += 1
        elif len(complaints) < 10:
            complaints.append(f"{name}: .gnu.version_r says {lib}, the appendix says {group!r}")
    return checked, agree, complaints


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", nargs="?", default="")
    parser.add_argument("--lib", default="libroblox.so")
    parser.add_argument("--abi", default="arm64-v8a")
    parser.add_argument("--list", action="store_true", help="print the reachable import names")
    parser.add_argument("--self-check-only", action="store_true")
    parser.add_argument("--out", default="", help="write the grouped list to this file too")
    args = parser.parse_args()

    root = Path(__file__).resolve().parent.parent
    data, described = load_library(root, args.source, args.lib, args.abi)
    elf = Elf(data)
    print(f"{described}")
    print(f"{len(data):,} bytes, DT_NEEDED: {', '.join(elf.needed())}")
    print()

    failures: list[str] = []

    def pin(what: str, got, want) -> None:
        ok = got == want
        print(f"  {'ok  ' if ok else 'FAIL'} {what:<58} {got!s:>22}   (expected {want})")
        if not ok:
            failures.append(what)

    print("SELF-CHECK")
    bounds, disagreements = elf.function_bounds()
    pin(".eh_frame_hdr FDEs", len(bounds), EXPECTED_FDE_COUNT)
    pin("FDE pc_begin vs search-table initial_location", disagreements, 0)

    imports = [
        i for i, s in enumerate(elf.symbols) if s["shndx"] == 0 and s["name"]
    ]
    pin("undefined (imported) .dynsym entries", len(imports), EXPECTED_IMPORTS)

    # DT_INIT_ARRAY. The slots carry R_AARCH64_RELATIVE relocations, so the *file* value is already
    # the p_vaddr-space target and no relocation has to be applied to read it.
    init_vaddr = elf.dyn(25)
    init_size = elf.dyn(27)
    if init_vaddr is None or init_size is None:
        raise SystemExit("ERROR: no DT_INIT_ARRAY")
    init_slots = [init_vaddr + 8 * i for i in range(init_size // 8)]
    pin("DT_INIT_ARRAY slots", len(init_slots), EXPECTED_INIT_ARRAY)

    # The general relocation table, packed APS2 (D9).
    # DT_LOOS is 0x6000000d, so DT_ANDROID_RELA is DT_LOOS + 4 and DT_ANDROID_RELASZ DT_LOOS + 5,
    # which is what `omni_elf::consts` names them from bionic's `libc/include/elf.h`.
    android_rela = elf.dyn(0x6000_0011)  # DT_ANDROID_RELA
    android_relasz = elf.dyn(0x6000_0012)  # DT_ANDROID_RELASZ
    if android_rela is None or android_relasz is None:
        raise SystemExit("ERROR: no DT_ANDROID_RELA")
    blob_off = elf.file_offset(android_rela, android_relasz)
    general = decode_aps2(data[blob_off : blob_off + android_relasz])
    by_type = Counter(r_info & 0xFFFF_FFFF for _, r_info, _ in general)
    pin("APS2 R_AARCH64_RELATIVE", by_type[R_AARCH64_RELATIVE], EXPECTED_APS2_BY_TYPE[1027])
    pin("APS2 R_AARCH64_GLOB_DAT", by_type[R_AARCH64_GLOB_DAT], EXPECTED_APS2_BY_TYPE[1025])
    pin("APS2 R_AARCH64_ABS64", by_type[R_AARCH64_ABS64], EXPECTED_APS2_BY_TYPE[257])

    # DT_JMPREL, plain RELA.
    jmprel_vaddr = elf.dyn(23)
    jmprel_size = elf.dyn(2)
    jmprel_off = elf.file_offset(jmprel_vaddr, jmprel_size)
    jmprel = [
        struct.unpack_from("<QQq", data, jmprel_off + i * 24) for i in range(jmprel_size // 24)
    ]
    # **The slots are empty in the file.** `DT_ANDROID_RELA` is the RELA form, so every addend lives
    # in the relocation entry and the in-place word is zero -- all 3,594 of them. The initializer
    # targets are therefore the *addends* of the `R_AARCH64_RELATIVE` relocations that land on the
    # slots, and a tool that read the file words would get 3,594 zeroes and report nothing
    # reachable. (It did, before this check existed.)
    relative_by_offset = {
        r_offset: r_addend
        for r_offset, r_info, r_addend in general
        if (r_info & 0xFFFF_FFFF) == R_AARCH64_RELATIVE
    }
    init_entries = [relative_by_offset[s] for s in init_slots if s in relative_by_offset]
    pin("DT_INIT_ARRAY slots carrying a RELATIVE relocation", len(init_entries), EXPECTED_INIT_ARRAY)
    pin(
        "DT_INIT_ARRAY in-place file words (RELA, so all zero)",
        len(set(struct.unpack_from(f"<{init_size // 8}Q", data, elf.file_offset(init_vaddr, init_size)))),
        1,
    )
    pin("DT_JMPREL entries", len(jmprel), EXPECTED_JMPREL)
    pin(
        "DT_JMPREL all R_AARCH64_JUMP_SLOT",
        all((r_info & 0xFFFF_FFFF) == R_AARCH64_JUMP_SLOT for _, r_info, _ in jmprel),
        True,
    )
    stubs, agreed, disagreed = plt_map(elf, jmprel)
    pin("PLT stubs whose decoded GOT slot matches DT_JMPREL", agreed, EXPECTED_JMPREL)
    pin("PLT stubs that disagreed", disagreed, 0)
    print()

    if failures:
        print(f"SELF-CHECK FAILED: {', '.join(failures)}")
        print("Refusing to report a number from a scanner that does not agree with M1's figures.")
        return 1
    if args.self_check_only:
        print("self-check only: done")
        return 0

    # ---------------------------------------------------------------- the maps

    # GOT slots that hold an import, from every table: JUMP_SLOT into .got.plt, and the
    # symbol-bearing GLOB_DAT / ABS64 in the packed table, which is how the imports with no PLT
    # stub -- the data objects, and any function only ever called through a pointer -- are named.
    got_imports: dict[int, str] = {}
    import_set = set(imports)
    for r_offset, r_info, _ in jmprel:
        got_imports[r_offset] = elf.symbols[r_info >> 32]["name"]
    symbolic_general = 0
    for r_offset, r_info, _ in general:
        sym = r_info >> 32
        if sym and sym in import_set:
            got_imports[r_offset] = elf.symbols[sym]["name"]
            symbolic_general += 1
    print(
        f"import binding sites: {len(jmprel)} PLT/GOT via DT_JMPREL, {symbolic_general} via "
        f"symbol-bearing GLOB_DAT/ABS64 in the packed table, {len(got_imports)} distinct slots"
    )
    plt_names = set(stubs.values())
    import_names = {elf.symbols[i]["name"] for i in imports}
    non_plt = sorted(import_names - plt_names)
    # Not 534. One `R_AARCH64_JUMP_SLOT` binds a *defined* symbol of this library --
    # `Java_com_roblox_client_purchase_IAPPurchaseManager_nativeFinishPaymentsProtocolPurchaseWithReturn`,
    # an exported JNI entry point that the engine also calls through its own PLT -- so the PLT
    # covers 533 of the 565 imports, not 534, and 32 imports have no stub.
    print(f"PLT stubs: {len(plt_names)}, of which imports: {len(plt_names & import_names)}; "
          f"non-import PLT symbols: {', '.join(sorted(plt_names - import_names))}")
    print(f"imports with a PLT stub: {len(plt_names & import_names)}; without: {len(non_plt)}")
    print(f"  without a stub: {', '.join(non_plt)}")
    print()

    # Which function starts any R_AARCH64_RELATIVE relocation names: the population an indirect call
    # can land on, and therefore the size of limitation 3.
    starts_set = set(b[0] for b in bounds)
    relative_to_function = set()
    for _r_offset, r_info, r_addend in general:
        if (r_info & 0xFFFF_FFFF) == R_AARCH64_RELATIVE and r_addend in starts_set:
            relative_to_function.add(r_addend)

    # ---------------------------------------------------------------- the roots

    scan = Reach(elf, bounds, stubs, got_imports)

    on_start = sum(1 for e in init_entries if e in starts_set)
    inside = sum(1 for e in init_entries if e not in starts_set and scan.containing(e) is not None)
    print("ROOTS")
    print(f"  DT_INIT_ARRAY slots                          {len(init_entries):>10,}")
    print(f"  distinct target addresses                    {len(set(init_entries)):>10,}")
    print(f"  landing exactly on an .eh_frame_hdr function {on_start:>10,}")
    print(f"  landing inside one but not at its start      {inside:>10,}")
    print(f"  landing in no FDE at all                     "
          f"{len(init_entries) - on_start - inside:>10,}")
    print()

    results = {}
    for label, follow, bl_only in (("A0", False, True), ("A", False, False), ("C", True, False)):
        scan.closure(init_entries, follow, bl_only)
        results[label] = dict(
            functions=len(scan.visited),
            words=scan.words_scanned,
            plt=dict(scan.plt_hits),
            got=dict(scan.got_hits),
            blr=scan.blr_sites,
            br=scan.br_sites,
            unresolved=sum(scan.unresolved_targets.values()),
            unresolved_distinct=len(scan.unresolved_targets),
            mid=scan.mid_function_targets,
            address_taken=len(scan.address_taken),
            synth=len(scan.synth),
            synth_words=sum(scan.synth.values()) // 4,
        )

    groups = provider_groups(root)
    checked, agree, complaints = check_provider_agreement(elf, imports, groups)
    # `.gnu.version_r` is this binary's own statement about where a symbol comes from, so it wins
    # over the appendix wherever it exists. The appendix covers the 158 unversioned ones -- the
    # Android system libraries, which ship no symbol versions at all.
    verlibs = elf.version_libraries()
    versions = elf.symbol_versions()
    for i in imports:
        lib = verlibs.get(versions[i])
        if lib is not None:
            groups[elf.symbols[i]["name"]] = lib

    for label, title in (
        ("A0", "TIER A0 -- BL edges only (the sensitivity floor)"),
        ("A", "TIER A -- direct control transfers only (the lower bound)"),
        ("C", "TIER C -- also following every address-taken function (the ceiling)"),
    ):
        r = results[label]
        a1 = set(r["plt"])
        a2 = set(r["got"])
        both = a1 | a2
        print(title)
        print(f"  functions in the closure                     {r['functions']:>10,} "
              f"of {len(bounds):,} ({r['functions'] / len(bounds):.2%})")
        print(f"  instruction words scanned                    {r['words']:>10,}")
        print(f"  imports reached by a direct branch to a stub {len(a1):>10,}")
        print(f"  imports whose GOT slot is materialised       {len(a2):>10,}")
        print(f"  union                                        {len(both):>10,} of {EXPECTED_IMPORTS}")
        print(f"  indirect call sites (BLR) -- unresolvable    {r['blr']:>10,}")
        print(f"  indirect branch sites (BR) -- unresolvable   {r['br']:>10,}")
        print(f"  direct targets in no FDE (distinct)          {r['unresolved_distinct']:>10,}")
        print(f"  direct targets inside, not at, a function    {r['mid']:>10,}")
        print(f"  FDE-less targets given a synthesised extent  {r['synth']:>10,} "
              f"({r['synth_words']:,} words)")
        if label == "C":
            print(f"  functions added by an address-taken edge     {r['address_taken']:>10,}")
        print()
        results[label]["imports"] = both

    print("THE CEILING NO CLOSURE CAN EXCEED")
    print(f"  imports declared                             {EXPECTED_IMPORTS:>10,}")
    print(f"  function starts named by an R_AARCH64_RELATIVE relocation "
          f"{len(relative_to_function):>10,} of {len(bounds):,}")
    print("  (that last figure is the population an indirect call can land on, so it is the")
    print("   size of limitation 3 -- vtables and pointer tables are not followed)")
    print()

    tier_a = results["A"]["imports"]
    tier_c = results["C"]["imports"]
    by_group: dict[str, list[str]] = defaultdict(list)
    for name in sorted(tier_a):
        by_group[groups.get(name, "UNGROUPED")].append(name)

    print(f"TIER A REACHABLE IMPORTS BY PROVIDING LIBRARY  ({len(tier_a)} symbols)")
    for group in sorted(by_group, key=lambda g: (-len(by_group[g]), g)):
        names = by_group[group]
        print(f"  {group:<52} {len(names):>4}")
    print()
    print(f"  Tier C adds {len(tier_c - tier_a)} more, for {len(tier_c)}: "
          f"{', '.join(sorted(tier_c - tier_a)) if len(tier_c - tier_a) <= 40 else '(see --list)'}")
    print()
    print(f"provider grouping cross-check: .gnu.version_r attributes {checked} of "
          f"{EXPECTED_IMPORTS} imports; the committed appendix agrees on {agree}")
    for c in complaints:
        print(f"  disagreement: {c}")
    print()

    if args.list or args.out:
        lines = []
        for group in sorted(by_group, key=lambda g: (-len(by_group[g]), g)):
            lines.append(f"### {group}  ({len(by_group[group])} symbols)")
            lines.extend(f"  {n}" for n in by_group[group])
            lines.append("")
        lines.append(f"### Tier C only -- reached solely through an address-taken edge "
                     f"({len(tier_c - tier_a)} symbols)")
        lines.extend(f"  {n}" for n in sorted(tier_c - tier_a))
        lines.append("")
        lines.append(f"### Never referenced from the Tier C closure at all "
                     f"({EXPECTED_IMPORTS - len(tier_c)} symbols)")
        all_names = {elf.symbols[i]["name"] for i in imports}
        lines.extend(f"  {n}" for n in sorted(all_names - tier_c))
        text = "\n".join(lines)
        if args.list:
            print(text)
        if args.out:
            Path(args.out).write_text(text + "\n", encoding="utf-8")
            print(f"written: {args.out}")

    print(
        "NOTE: Tier A is a LOWER BOUND on reachability. A static call graph over a stripped binary "
        "cannot see an indirect call, and the closure contains the BLR/BR counts above. Tier A0 is "
        "the same closure with BL edges only, so the A0-to-A gap is how much of the answer rests on "
        "tail-call and cold-split edges. Tier C is an over-approximation in the other direction: it "
        "follows every materialised function address as if it were called. The true set lies between "
        "A and C, and only a run of the initializers settles it (Task 4)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
