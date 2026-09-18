#!/usr/bin/env python3
"""Count the A64 atomics mix of a guest library, straight out of the APK.

Why this exists
---------------
D5 carries two risks that depend on which atomics the engine actually uses:

* **risk 3** -- dynarmic's ``ExclusiveMonitor`` is one global spinlock and anti-scales 21x from 1 to
  16 threads. It is entered by ``LDXR``/``STXR`` and friends.
* **risk 4** -- 231 of 874 decoder entries are unimplemented, LSE atomics among them. Each LSE site
  is a hard stop (``ExitReason::UnsupportedInstruction``), not a slow path.

D5's amendment adds that declining to advertise LSE through ``getauxval(AT_HWCAP)`` would steer the
engine off LSE and onto the exclusive path -- so the two must be decided together. Which way that
decision should go is a property of `libroblox.so`, and this measures it.

It also exists because the first version of this count, in the Task 4 report, was **wrong**. It
classified ``LDAR``/``STLR`` -- acquire/release *ordered* accesses, which touch no monitor -- as
``LDAXR``/``STLXR``, and reported 15,646 exclusive sites where there are 128. A review then found a
second one: ``CASP`` shares ``o2 = 0, o1 = 1`` with ``LDXP``/``STXP`` and is discriminated by bit 31
alone, so two more sites were on the exclusive side of the ratio instead of the LSE side. Both
errors ran the same way -- away from LSE, the direction that makes D5's risk 4 look smaller -- and
the encodings differ only in single bits of one top-level class, which is exactly the kind of thing
that should be pinned by a self-checking script rather than by a one-off command.

What it counts
--------------
Bits 29:24 == ``001000`` is the *Load/store exclusive* top-level class, and inside it:

===========  ===========  =========================================================
``o2`` (23)  ``o1`` (21)  Instructions
===========  ===========  =========================================================
0            0            ``LDXR``/``STXR``/``LDAXR``/``STLXR`` -- **exclusive monitor**
0            1            ``LDXP``/``STXP`` if bit 31 is set, else ``CASP`` -- see below
1            0            ``LDAR``/``STLR``/``LDLAR``/``STLLR`` -- ordered, **no monitor**
1            1            ``CAS`` -- **LSE**
===========  ===========  =========================================================

The ``o2 = 0, o1 = 1`` row is not one instruction family but two, told apart by **bit 31 alone**:
set means ``LDXP``/``STXP`` (exclusive monitor), clear means ``CASP`` (LSE). Missing that put two
LSE sites on the wrong side of the ratio this tool exists to report, which is why it is spelled out
rather than left to the table.

Separately, bits 29:24 == ``111000`` with bit 21 set and bits 11:10 == ``00`` is *Atomic memory
operations*: ``LDADD``/``LDCLR``/``LDEOR``/``LDSET``/``LDSMAX``/``LDSMIN``/``LDUMAX``/``LDUMIN`` and
``SWP`` -- all **LSE** -- plus ``LDAPR``, which shares the encoding (``o3`` = 1, ``opc`` = 100,
``Rs`` = 11111) and is an ordered load rather than a read-modify-write. ``LDAPR`` is separated out.

Two populations, and the difference matters
-------------------------------------------
* ``--population functions`` (the sounder one): only words inside the function bounds
  ``.eh_frame_hdr`` names, so every word counted really is an instruction.
* ``--population sections``: every word of every ``SHF_EXECINSTR`` section. Complete, but `.text`
  in a stripped release binary also holds literal pools and jump tables, and any 4-byte constant can
  alias an encoding -- so this over-counts, and by a lot for the rarer classes.

Both are printed. Where they disagree the function-restricted figure is the one to quote, and the
gap between them is itself the measure of how much aliasing noise a section scan carries.

Self-check
----------
Like ``branch_mix.py``, this recounts D13's marker: ``MRS Xt, TPIDR_EL0`` must land on **1,282** over
the executable sections, and ``.eh_frame_hdr`` must name **245,117** functions. A figure away from
either means the extraction is wrong and every other number with it.

``--check`` additionally asserts the figures recorded in the Task 4 report and D5's amendment, and
exits non-zero if the binary or the classifier has moved. That is what makes this a pinned
measurement rather than a command somebody once ran.

Usage
-----
    python tools/atomic_mix.py [APK] [--lib NAME] [--population both|functions|sections] [--check]

Defaults to ``Roblox-2.738.1397.apk`` in the repository root and ``libroblox.so``. Exits 2 with a
clear message if the APK is absent, since it is git-ignored (Global Constraint 2).
"""

from __future__ import annotations

import argparse
import struct
import sys
import zipfile
from pathlib import Path

# ------------------------------------------------------------------ classifiers

#: ``(mask, value)`` for the *Load/store exclusive* top-level class: bits 29:24 == 001000.
LOAD_STORE_EXCLUSIVE = (0x3F00_0000, 0x0800_0000)
#: ``(mask, value)`` for *Atomic memory operations*: bits 29:24 == 111000, bit 21 == 1,
#: bits 11:10 == 00.
ATOMIC_MEMORY_OP = (0x3F20_0C00, 0x3820_0000)


def exclusive_class(word: int) -> str | None:
    """Which row of the ``o2``/``o1`` table above `word` is, or ``None``.

    One extra discrimination the table does not show, and it caught this tool out once:
    ``CASP``/``CASPA``/``CASPL``/``CASPAL`` share ``o2 = 0, o1 = 1`` with ``LDXP``/``STXP`` and are
    told apart by **bit 31 alone** — the exclusive-pair forms have it set (``1 0`` in bits 31:30),
    while ``CASP`` uses bit 31 as a fixed 0 and bit 30 as its size. ``libroblox.so`` has two of them,
    ``0x48607c82`` (``CASPA``) at ``0x4d905d0`` and ``0x4820fc82`` (``CASPL``) at ``0x52db1d0``, and
    counting them as exclusives put two LSE sites on the wrong side of the one ratio this tool
    exists to report.
    """
    mask, value = LOAD_STORE_EXCLUSIVE
    if word & mask != value:
        return None
    o2 = (word >> 23) & 1
    o1 = (word >> 21) & 1
    if o2 == 1:
        return "cas" if o1 else "ordered"
    if o1 == 0:
        return "exclusive_single"
    # `o2 = 0, o1 = 1`: an exclusive pair only if bit 31 is set; otherwise CASP, which is LSE.
    return "exclusive_pair" if (word >> 31) & 1 else "casp"


def atomic_memory_class(word: int) -> str | None:
    """``lse_rmw``, ``ldapr``, or ``None``."""
    mask, value = ATOMIC_MEMORY_OP
    if word & mask != value:
        return None
    rs = (word >> 16) & 0x1F
    o3 = (word >> 15) & 1
    opc = (word >> 12) & 0x7
    # LDAPR/LDAPRB/LDAPRH share this encoding but are ordered loads, not read-modify-writes.
    if o3 == 1 and opc == 0b100 and rs == 0b11111:
        return "ldapr"
    return "lse_rmw"


def is_mrs_tpidr_el0(word: int) -> bool:
    """``MRS Xt, TPIDR_EL0`` -- D13's marker, recounted as a cross-check on the whole method."""
    return (word & 0xFFFF_FFE0) == 0xD53B_D040


CLASSES = ("exclusive_single", "exclusive_pair", "ordered", "cas", "casp", "lse_rmw", "ldapr")

LABELS = {
    "exclusive_single": "LDXR / STXR / LDAXR / STLXR   (exclusive monitor)",
    "exclusive_pair": "LDXP / STXP / LDAXP / STLXP   (exclusive monitor)",
    "cas": "CAS                           (LSE)",
    "casp": "CASP / CASPA / CASPL / CASPAL (LSE)",
    "lse_rmw": "LDADD / SWP / LDCLR / ...     (LSE)",
    "ordered": "LDAR / STLR / LDLAR / STLLR   (ordered, no monitor)",
    "ldapr": "LDAPR                         (ordered load, not an RMW)",
}

# ------------------------------------------------------------------------ ELF


def executable_sections(data: bytes) -> list[tuple[str, int, int]]:
    """``(name, offset, size)`` for every ``SHF_EXECINSTR`` section with contents."""
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

    SHT_NOBITS, SHF_EXECINSTR = 8, 0x4
    out = []
    for i in range(e_shnum):
        sh_name, sh_type, sh_flags, sh_offset, sh_size = header(i)
        if sh_type != SHT_NOBITS and sh_flags & SHF_EXECINSTR and sh_size:
            out.append((name_at(sh_name), sh_offset, sh_size))
    return out


def program_headers(data: bytes):
    (e_phoff,) = struct.unpack_from("<Q", data, 0x20)
    e_phentsize, e_phnum = struct.unpack_from("<HH", data, 0x36)
    for i in range(e_phnum):
        yield struct.unpack_from("<IIQQQQQQ", data, e_phoff + i * e_phentsize)


def eh_frame_functions(data: bytes) -> list[tuple[int, int]]:
    """``(file_offset, length)`` for every function ``.eh_frame_hdr`` names.

    A deliberately small reimplementation of ``omni_elf::eh_frame`` for the one shape every AArch64
    toolchain emits: a ``datarel|sdata4`` search table over ``pcrel|sdata4`` FDEs. Anything else is
    refused rather than guessed at, for the same reason the Rust one refuses it -- every encoding is
    a different width, so a wrong guess produces plausible addresses rather than an error.

    For this library ``p_vaddr == p_offset`` throughout the executable segment, which is asserted
    rather than assumed.
    """
    PT_GNU_EH_FRAME = 0x6474_E550
    hdr = next((p for p in program_headers(data) if p[0] == PT_GNU_EH_FRAME), None)
    if hdr is None:
        raise ValueError("no PT_GNU_EH_FRAME")
    _, _, p_offset, p_vaddr, _, _, _, _ = hdr
    if p_offset != p_vaddr:
        raise ValueError("this reader assumes p_vaddr == p_offset for .eh_frame_hdr")
    base = p_offset
    version, ptr_enc, count_enc, table_enc = data[base : base + 4]
    if (version, ptr_enc, count_enc, table_enc) != (1, 0x1B, 0x03, 0x3B):
        raise ValueError(
            f"unsupported .eh_frame_hdr encodings {version},{ptr_enc:#x},"
            f"{count_enc:#x},{table_enc:#x}"
        )
    (fde_count,) = struct.unpack_from("<I", data, base + 8)
    table = base + 12
    out = []
    for i in range(fde_count):
        _loc, fde = struct.unpack_from("<ii", data, table + i * 8)
        at = (fde + base) & 0xFFFF_FFFF
        (length,) = struct.unpack_from("<I", data, at)
        if length in (0, 0xFFFF_FFFF):
            raise ValueError(f"FDE at {at:#x} has length {length:#x}")
        # pc_begin is pcrel|sdata4 from its own position; pc_range is the same width, absolute.
        (pc_begin,) = struct.unpack_from("<i", data, at + 8)
        (pc_range,) = struct.unpack_from("<I", data, at + 12)
        out.append(((at + 8 + pc_begin) & 0xFFFF_FFFF, pc_range))
    return out


# ---------------------------------------------------------------------- counting


def count(data: bytes, spans) -> dict[str, int]:
    counts = dict.fromkeys(CLASSES, 0)
    counts["words"] = 0
    counts["mrs_tpidr"] = 0
    for offset, size in spans:
        words = size // 4
        counts["words"] += words
        view = memoryview(data)[offset : offset + words * 4]
        for (word,) in struct.iter_unpack("<I", view):
            k = exclusive_class(word)
            if k is not None:
                counts[k] += 1
            k = atomic_memory_class(word)
            if k is not None:
                counts[k] += 1
            if is_mrs_tpidr_el0(word):
                counts["mrs_tpidr"] += 1
    return counts


def report(title: str, counts: dict[str, int]) -> tuple[int, int]:
    exclusive = counts["exclusive_single"] + counts["exclusive_pair"]
    lse = counts["cas"] + counts["casp"] + counts["lse_rmw"]
    print(f"{title}: {counts['words']:,} words ({counts['words'] * 4:,} bytes)")
    for key in CLASSES:
        print(f"    {LABELS[key]:<48} {counts[key]:>8,}")
    print(f"    {'-' * 48} {'-' * 8}")
    print(f"    {'exclusive-monitor sites (D5 risk 3)':<48} {exclusive:>8,}")
    print(f"    {'LSE sites (D5 risk 4: each is a hard stop)':<48} {lse:>8,}")
    rmw = exclusive + lse
    if rmw:
        print(f"    {'LSE share of atomic read-modify-write sites':<48} {lse / rmw:>7.1%}")
    print(f"    {'MRS Xt, TPIDR_EL0 (D13 cross-check)':<48} {counts['mrs_tpidr']:>8,}")
    print()
    return exclusive, lse


# ------------------------------------------------------------------- recorded

#: The figures the Task 4 report and D5's amendment state, over the function-bound population.
#: `--check` asserts these, so the documents cannot drift from the binary without a red run.
RECORDED_FUNCTIONS = {
    "words": 17_485_957,
    "exclusive_single": 108,
    "exclusive_pair": 20,
    "ordered": 15_516,
    "cas": 14,
    "casp": 2,
    "lse_rmw": 37,
    "ldapr": 0,
}
RECORDED_SECTIONS = {
    "words": 18_156_033,
    "exclusive_single": 567,
    "exclusive_pair": 20,
    "ordered": 15_580,
    "cas": 14,
    "casp": 2,
    "lse_rmw": 37,
    "ldapr": 0,
    "mrs_tpidr": 1_282,
}
#: D9/`.eh_frame_hdr`: the function count the whole selection rests on.
RECORDED_FUNCTION_COUNT = 245_117


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("apk", nargs="?", default="Roblox-2.738.1397.apk")
    parser.add_argument("--lib", default="libroblox.so")
    parser.add_argument("--abi", default="arm64-v8a")
    parser.add_argument(
        "--population", choices=("both", "functions", "sections"), default="both"
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="assert the recorded figures and exit non-zero if they have moved",
    )
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

    print(f"{member} from {apk}\n")
    failures: list[str] = []

    sections = executable_sections(data)
    section_counts = None
    if args.population in ("both", "sections"):
        print("executable sections: " + ", ".join(f"{n} ({s:,} B)" for n, _, s in sections))
        section_counts = count(data, [(o, s) for _, o, s in sections])
        report("over every executable section (includes literal pools, so it over-counts)",
               section_counts)

    function_counts = None
    if args.population in ("both", "functions"):
        functions = eh_frame_functions(data)
        print(f".eh_frame_hdr names {len(functions):,} functions")
        if len(functions) != RECORDED_FUNCTION_COUNT:
            failures.append(
                f".eh_frame_hdr names {len(functions)} functions, recorded {RECORDED_FUNCTION_COUNT}"
            )
        function_counts = count(data, functions)
        report("over function bodies only (every word really is an instruction)", function_counts)

    if section_counts and function_counts:
        noise = (
            section_counts["exclusive_single"] - function_counts["exclusive_single"]
        )
        print(
            f"The section scan finds {noise:,} more single-exclusive words than the function scan, "
            f"in {(section_counts['words'] - function_counts['words']) * 4:,} bytes of text that no "
            f"FDE covers. Most of that is literal pools and jump tables aliasing the encoding, "
            f"which is why the function-restricted figure is the one to quote.\n"
        )

    if args.check:
        for label, counts, recorded in (
            ("functions", function_counts, RECORDED_FUNCTIONS),
            ("sections", section_counts, RECORDED_SECTIONS),
        ):
            if counts is None:
                failures.append(f"--check needs the {label} population; use --population both")
                continue
            for key, want in recorded.items():
                if counts[key] != want:
                    failures.append(f"{label}.{key} is {counts[key]:,}, recorded {want:,}")
        if failures:
            print("CHECK FAILED:", file=sys.stderr)
            for f in failures:
                print(f"  {f}", file=sys.stderr)
            print(
                "\nEither the binary changed or the classifier did. Both are things the documents "
                "have to be told about; neither is something to paper over.",
                file=sys.stderr,
            )
            return 1
        print("CHECK PASSED: every recorded figure still holds.")
    elif failures:
        for f in failures:
            print(f"WARNING: {f}", file=sys.stderr)

    print(
        "NOTE: a static count. It says how many *sites* exist, not how often each is executed -- "
        "a program spends its time in a small part of its text. D5's risk 3 is a contention cost "
        "per execution and risk 4 is a hard stop per execution, so both depend on the dynamic mix, "
        "which only a trace settles. What the static count does settle is that neither risk is "
        "absent: 128 exclusive sites and 53 LSE sites are both non-zero, and the LSE ones cannot "
        "be run at all on this pin."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
