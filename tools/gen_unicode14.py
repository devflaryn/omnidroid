#!/usr/bin/env python3
"""Generate `crates/omni-bionic/src/unicode14.rs` from the Unicode 14.0 Character Database.

    python tools/gen_unicode14.py <dir holding the three UCD files>

The three files, exactly as published:

    https://www.unicode.org/Public/14.0.0/ucd/UnicodeData.txt
    https://www.unicode.org/Public/14.0.0/ucd/DerivedCoreProperties.txt
    https://www.unicode.org/Public/14.0.0/ucd/PropList.txt

**Why Unicode 14.0.** bionic's wide-character classifiers (`libc/bionic/wctype.cpp`) answer through
ICU, and the platform this runtime reports -- Android 13, SDK 33 -- ships ICU 71, which is Unicode
14.0. Rust's own `char` tables are a later Unicode, so a character assigned since would classify
differently from a device; generating from the release the device carries is what makes the
answers the device's.

**What is generated is exactly what bionic asks ICU for**, and nothing else:

* the binary properties `Alphabetic`, `Lowercase`, `Uppercase` (DerivedCoreProperties) and
  `White_Space` (PropList) -- `UCHAR_ALPHABETIC`, `UCHAR_LOWERCASE`, `UCHAR_UPPERCASE`,
  `UCHAR_WHITE_SPACE`;
* the general categories ICU's POSIX predicates are defined on: `Nd` (`u_isdigit`, and
  `u_isxdigit`'s second half), `P*` (`u_ispunct`), `Cc` (`u_charType == U_CONTROL_CHAR`), `Zs`
  (`u_isblank` above U+009F), and the union `Cc | Cs | Cn | Z*` that `u_isgraphPOSIX` excludes --
  `Cn` being every code point UnicodeData.txt does not list;
* the simple case mappings of UnicodeData.txt's fields 12 and 13 -- `u_toupper`, `u_tolower`.

Ranges are merged and sorted, so every lookup is one binary search.
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "crates" / "omni-bionic" / "src" / "unicode14.rs"


def ranges(points: set[int]) -> list[tuple[int, int]]:
    out: list[tuple[int, int]] = []
    for p in sorted(points):
        if out and out[-1][1] + 1 == p:
            out[-1] = (out[-1][0], p)
        else:
            out.append((p, p))
    return out


def unicode_data(path: Path):
    """(code point -> general category, lower map, upper map), with First/Last ranges expanded."""
    gc: dict[int, str] = {}
    lower: dict[int, int] = {}
    upper: dict[int, int] = {}
    first = None
    for line in path.read_text(encoding="utf-8").splitlines():
        f = line.split(";")
        cp = int(f[0], 16)
        name, cat = f[1], f[2]
        if name.endswith(", First>"):
            first = cp
            continue
        if name.endswith(", Last>"):
            for p in range(first, cp + 1):
                gc[p] = cat
            first = None
            continue
        gc[cp] = cat
        if f[12]:
            upper[cp] = int(f[12], 16)
        if f[13]:
            lower[cp] = int(f[13], 16)
    return gc, lower, upper


def binary_property(path: Path, name: str) -> set[int]:
    points: set[int] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        cps, prop = (part.strip() for part in line.split(";")[:2])
        if prop != name:
            continue
        if ".." in cps:
            a, b = (int(x, 16) for x in cps.split(".."))
            points.update(range(a, b + 1))
        else:
            points.add(int(cps, 16))
    return points


def emit_ranges(name: str, doc: str, points: set[int]) -> str:
    rs = ranges(points)
    body = "\n".join(f"    ({a:#x}, {b:#x})," for a, b in rs)
    return f"/// {doc}\npub static {name}: &[(u32, u32)] = &[\n{body}\n];\n"


def emit_map(name: str, doc: str, mapping: dict[int, int]) -> str:
    body = "\n".join(f"    ({a:#x}, {b:#x})," for a, b in sorted(mapping.items()))
    return f"/// {doc}\npub static {name}: &[(u32, u32)] = &[\n{body}\n];\n"


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    ucd = Path(sys.argv[1])
    gc, lower, upper = unicode_data(ucd / "UnicodeData.txt")
    derived = ucd / "DerivedCoreProperties.txt"
    header = (ucd / "DerivedCoreProperties.txt").read_text(encoding="utf-8").splitlines()[0]
    assert "14.0.0" in header, header
    alphabetic = binary_property(derived, "Alphabetic")
    lowercase = binary_property(derived, "Lowercase")
    uppercase = binary_property(derived, "Uppercase")
    white_space = binary_property(ucd / "PropList.txt", "White_Space")

    def cat(pred) -> set[int]:
        return {p for p, c in gc.items() if pred(c)}

    nd = cat(lambda c: c == "Nd")
    punct = cat(lambda c: c.startswith("P"))
    cc = cat(lambda c: c == "Cc")
    zs = cat(lambda c: c == "Zs")
    listed = set(gc)
    not_graph = cat(lambda c: c in ("Cc", "Cs") or c.startswith("Z"))
    not_graph |= set(range(0, 0x110000)) - listed  # Cn: not listed at all

    parts = [
        "//! Unicode 14.0 character data: the properties bionic's `wctype.cpp` asks ICU 71 for.\n"
        "//!\n"
        "//! **Generated** by `tools/gen_unicode14.py` from the Unicode 14.0 UCD (`UnicodeData.txt`,\n"
        "//! `DerivedCoreProperties.txt`, `PropList.txt`); do not edit by hand. See that script for why\n"
        "//! 14.0 and why these properties. Every table is sorted and its ranges are inclusive.\n",
        emit_ranges("ALPHABETIC", "`Alphabetic` (`UCHAR_ALPHABETIC`).", alphabetic),
        emit_ranges("LOWERCASE", "`Lowercase` (`UCHAR_LOWERCASE`).", lowercase),
        emit_ranges("UPPERCASE", "`Uppercase` (`UCHAR_UPPERCASE`).", uppercase),
        emit_ranges("WHITE_SPACE", "`White_Space` (`UCHAR_WHITE_SPACE`).", white_space),
        emit_ranges("GC_ND", "General category `Nd` (`u_isdigit`).", nd),
        emit_ranges("GC_P", "General categories `P*` (`u_ispunct`).", punct),
        emit_ranges("GC_CC", "General category `Cc` (`u_charType == U_CONTROL_CHAR`).", cc),
        emit_ranges("GC_ZS", "General category `Zs` (`u_isblank` above U+009F).", zs),
        emit_ranges(
            "NOT_GRAPH",
            "`Cc | Cs | Cn | Z*`: what `u_isgraphPOSIX` excludes (`Cn` = not in UnicodeData.txt).",
            not_graph,
        ),
        emit_map("TO_LOWER", "Simple lowercase mappings, UnicodeData.txt field 13 (`u_tolower`).", lower),
        emit_map("TO_UPPER", "Simple uppercase mappings, UnicodeData.txt field 12 (`u_toupper`).", upper),
    ]
    OUT.write_text("\n".join(parts), encoding="utf-8", newline="\n")
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
