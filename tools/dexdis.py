#!/usr/bin/env python3
"""Read the APK's Java side: a Dalvik disassembler and a whole-dex search.

D7 says the Java side is *defined*, not executed -- which makes reading it the way to decide what
a host answers. Every value decoded this way in M6 had been guessed before it was read, and four
of the guesses were wrong (`NativeUserJavaInterface`'s signed-out answers; see its declaration in
`jni::classes`). So when a question is "what does the app pass here?", read the dex.

::

    python tools/dexdis.py 'Lfi/e;' F                  # one method
    python tools/dexdis.py 'Lok/c;'                    # every method of a class
    python tools/dexdis.py --grep 'Lfi/e$d;->d:'       # every instruction mentioning it
    python tools/dexdis.py --grep 'nativeSetAssetPath' --dex classes2.dex

``classes4.dex`` is skipped: D6 and ``jni-surface.md``'s scope note put the injected payload out
of scope. The instruction-format table is the Dalvik bytecode spec's; payload pseudo-instructions
(switch tables, array data) are skipped by their own size headers. Needs only the standard library.
"""

from __future__ import annotations

import struct
import sys
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
APK = REPO / "Roblox-2.738.1397.apk"


def uleb(b: bytes, o: int) -> tuple[int, int]:
    r = s = 0
    while True:
        x = b[o]
        o += 1
        r |= (x & 0x7F) << s
        if x < 0x80:
            return r, o
        s += 7


class Dex:
    def __init__(self, b: bytes):
        self.b = b
        (self.string_ids_size, self.string_ids_off, self.type_ids_size, self.type_ids_off,
         self.proto_ids_size, self.proto_ids_off, self.field_ids_size, self.field_ids_off,
         self.method_ids_size, self.method_ids_off, self.class_defs_size,
         self.class_defs_off) = struct.unpack_from("<12I", b, 56)
        self._str: dict[int, str] = {}

    def string(self, i: int) -> str:
        if i not in self._str:
            off = struct.unpack_from("<I", self.b, self.string_ids_off + 4 * i)[0]
            _, o = uleb(self.b, off)
            end = self.b.index(b"\0", o)
            self._str[i] = self.b[o:end].decode("utf-8", "replace")
        return self._str[i]

    def type(self, i: int) -> str:
        return self.string(struct.unpack_from("<I", self.b, self.type_ids_off + 4 * i)[0])

    def proto(self, i: int) -> str:
        _, ret, params = struct.unpack_from("<III", self.b, self.proto_ids_off + 12 * i)
        ps = []
        if params:
            cnt = struct.unpack_from("<I", self.b, params)[0]
            ps = [self.type(struct.unpack_from("<H", self.b, params + 4 + 2 * k)[0]) for k in range(cnt)]
        return "(" + "".join(ps) + ")" + self.type(ret)

    def field(self, i: int) -> str:
        c, t, n = struct.unpack_from("<HHI", self.b, self.field_ids_off + 8 * i)
        return f"{self.type(c)}->{self.string(n)}:{self.type(t)}"

    def method(self, i: int) -> str:
        c, p, n = struct.unpack_from("<HHI", self.b, self.method_ids_off + 8 * i)
        return f"{self.type(c)}->{self.string(n)}{self.proto(p)}"

    def method_name(self, i: int) -> tuple[str, str]:
        _, p, n = struct.unpack_from("<HHI", self.b, self.method_ids_off + 8 * i)
        return self.string(n), self.proto(p)

    def classes(self):
        for i in range(self.class_defs_size):
            cidx, *_, data, _ = struct.unpack_from("<8I", self.b, self.class_defs_off + 32 * i)
            yield self.type(cidx), data

    def methods(self, data: int):
        """(name, proto, code offset) for every direct and virtual method of a class."""
        b = self.b
        o = data
        sf, o = uleb(b, o)
        inf, o = uleb(b, o)
        dm, o = uleb(b, o)
        vm, o = uleb(b, o)
        for _ in range(sf + inf):
            _, o = uleb(b, o)
            _, o = uleb(b, o)
        idx = 0
        for k in range(dm + vm):
            if k == dm:
                idx = 0
            d, o = uleb(b, o)
            _, o = uleb(b, o)
            code, o = uleb(b, o)
            idx += d
            yield (*self.method_name(idx), code)


# ------------------------------------------------------------------------------ the opcode table

FMT: dict[int, tuple[str, str]] = {}


def _op(code: int, name: str, fmt: str) -> None:
    FMT[code] = (name, fmt)


for _c, _n, _f in [
    (0x00, "nop", "10x"), (0x01, "move", "12x"), (0x02, "move/from16", "22x"), (0x03, "move/16", "32x"),
    (0x04, "move-wide", "12x"), (0x05, "move-wide/from16", "22x"), (0x06, "move-wide/16", "32x"),
    (0x07, "move-object", "12x"), (0x08, "move-object/from16", "22x"), (0x09, "move-object/16", "32x"),
    (0x0A, "move-result", "11x"), (0x0B, "move-result-wide", "11x"), (0x0C, "move-result-object", "11x"),
    (0x0D, "move-exception", "11x"), (0x0E, "return-void", "10x"), (0x0F, "return", "11x"),
    (0x10, "return-wide", "11x"), (0x11, "return-object", "11x"), (0x12, "const/4", "11n"),
    (0x13, "const/16", "21s"), (0x14, "const", "31i"), (0x15, "const/high16", "21h"),
    (0x16, "const-wide/16", "21s"), (0x17, "const-wide/32", "31i"), (0x18, "const-wide", "51l"),
    (0x19, "const-wide/high16", "21h"), (0x1A, "const-string", "21c"), (0x1B, "const-string/jumbo", "31c"),
    (0x1C, "const-class", "21c"), (0x1D, "monitor-enter", "11x"), (0x1E, "monitor-exit", "11x"),
    (0x1F, "check-cast", "21c"), (0x20, "instance-of", "22c"), (0x21, "array-length", "12x"),
    (0x22, "new-instance", "21c"), (0x23, "new-array", "22c"), (0x24, "filled-new-array", "35c"),
    (0x25, "filled-new-array/range", "3rc"), (0x26, "fill-array-data", "31t"), (0x27, "throw", "11x"),
    (0x28, "goto", "10t"), (0x29, "goto/16", "20t"), (0x2A, "goto/32", "30t"),
    (0x2B, "packed-switch", "31t"), (0x2C, "sparse-switch", "31t"),
]:
    _op(_c, _n, _f)
for _i, _n in enumerate(["cmpl-float", "cmpg-float", "cmpl-double", "cmpg-double", "cmp-long"]):
    _op(0x2D + _i, _n, "23x")
for _i, _n in enumerate(["if-eq", "if-ne", "if-lt", "if-ge", "if-gt", "if-le"]):
    _op(0x32 + _i, _n, "22t")
for _i, _n in enumerate(["if-eqz", "if-nez", "if-ltz", "if-gez", "if-gtz", "if-lez"]):
    _op(0x38 + _i, _n, "21t")
for _c in [*range(0x3E, 0x44), 0x73, 0x79, 0x7A, *range(0xE3, 0xFA)]:
    _op(_c, "unused", "10x")
_KINDS = ["", "-wide", "-object", "-boolean", "-byte", "-char", "-short"]
for _i, _k in enumerate(_KINDS):
    _op(0x44 + _i, "aget" + _k, "23x")
    _op(0x4B + _i, "aput" + _k, "23x")
    _op(0x52 + _i, "iget" + _k, "22c")
    _op(0x59 + _i, "iput" + _k, "22c")
    _op(0x60 + _i, "sget" + _k, "21c")
    _op(0x67 + _i, "sput" + _k, "21c")
for _i, _n in enumerate(["virtual", "super", "direct", "static", "interface"]):
    _op(0x6E + _i, "invoke-" + _n, "35c")
    _op(0x74 + _i, "invoke-" + _n + "/range", "3rc")
for _i, _n in enumerate(["neg-int", "not-int", "neg-long", "not-long", "neg-float", "neg-double",
                         "int-to-long", "int-to-float", "int-to-double", "long-to-int", "long-to-float",
                         "long-to-double", "float-to-int", "float-to-long", "float-to-double",
                         "double-to-int", "double-to-long", "double-to-float", "int-to-byte",
                         "int-to-char", "int-to-short"]):
    _op(0x7B + _i, _n, "12x")
_BIN = ["add", "sub", "mul", "div", "rem", "and", "or", "xor", "shl", "shr", "ushr"]
_FBIN = ["add", "sub", "mul", "div", "rem"]
for _i, _n in enumerate([f"{b}-int" for b in _BIN] + [f"{b}-long" for b in _BIN]
                        + [f"{b}-float" for b in _FBIN] + [f"{b}-double" for b in _FBIN]):
    _op(0x90 + _i, _n, "23x")
    _op(0xB0 + _i, _n + "/2addr", "12x")
for _i, _n in enumerate(["add", "rsub", "mul", "div", "rem", "and", "or", "xor"]):
    _op(0xD0 + _i, f"{_n}-int/lit16", "22s")
for _i, _n in enumerate(["add", "rsub", "mul", "div", "rem", "and", "or", "xor", "shl", "shr", "ushr"]):
    _op(0xD8 + _i, f"{_n}-int/lit8", "22b")
_op(0xFA, "invoke-polymorphic", "45cc")
_op(0xFB, "invoke-polymorphic/range", "4rcc")
_op(0xFC, "invoke-custom", "35c")
_op(0xFD, "invoke-custom/range", "3rc")
_op(0xFE, "const-method-handle", "21c")
_op(0xFF, "const-method-type", "21c")

SIZE = {"10x": 1, "12x": 1, "11n": 1, "11x": 1, "10t": 1, "20t": 2, "22x": 2, "21t": 2, "21s": 2,
        "21h": 2, "21c": 2, "23x": 2, "22b": 2, "22t": 2, "22s": 2, "22c": 2, "30t": 3, "32x": 3,
        "31i": 3, "31t": 3, "31c": 3, "35c": 3, "3rc": 3, "45cc": 4, "4rcc": 4, "51l": 5}


def _s16(v: int) -> int:
    return v - 0x10000 if v & 0x8000 else v


def _s32(v: int) -> int:
    return v - 0x100000000 if v & 0x80000000 else v


def _ref(d: Dex, name: str, idx: int) -> str:
    if name.startswith("const-string"):
        return repr(d.string(idx))
    if name in ("const-class", "check-cast", "new-instance", "instance-of", "new-array",
                "filled-new-array", "filled-new-array/range"):
        return d.type(idx)
    if name.startswith(("iget", "iput", "sget", "sput")):
        return d.field(idx)
    if name.startswith("invoke"):
        return d.method(idx)
    return f"@{idx}"


def disasm(d: Dex, insns: tuple[int, ...]) -> list[str]:
    out = []
    i = 0
    while i < len(insns):
        w = insns[i]
        if w in (0x0100, 0x0200, 0x0300):
            if w == 0x0100:
                i += insns[i + 1] * 2 + 4
            elif w == 0x0200:
                i += insns[i + 1] * 4 + 2
            else:
                count = insns[i + 2] | (insns[i + 3] << 16)
                i += (count * insns[i + 1] + 1) // 2 + 4
            continue
        name, fmt = FMT[w & 0xFF]
        a = w >> 8
        at = lambda off: f"-> {i + off:04x}"  # noqa: E731
        text = {
            "10x": lambda: "",
            "12x": lambda: f"v{a & 0xF}, v{a >> 4}",
            "11n": lambda: f"v{a & 0xF}, #{(a >> 4) - 16 if a & 0x80 else a >> 4}",
            "11x": lambda: f"v{a}",
            "10t": lambda: at(a - 256 if a & 0x80 else a),
            "20t": lambda: at(_s16(insns[i + 1])),
            "22x": lambda: f"v{a}, v{insns[i + 1]}",
            "21t": lambda: f"v{a}, " + at(_s16(insns[i + 1])),
            "21s": lambda: f"v{a}, #{_s16(insns[i + 1])}",
            "21h": lambda: f"v{a}, #{insns[i + 1]:#x}<<",
            "21c": lambda: f"v{a}, {_ref(d, name, insns[i + 1])}",
            "23x": lambda: f"v{a}, v{insns[i + 1] & 0xFF}, v{insns[i + 1] >> 8}",
            "22b": lambda: f"v{a}, v{insns[i + 1] & 0xFF}, #{(insns[i + 1] >> 8) - (256 if insns[i + 1] & 0x8000 else 0)}",
            "22t": lambda: f"v{a & 0xF}, v{a >> 4}, " + at(_s16(insns[i + 1])),
            "22s": lambda: f"v{a & 0xF}, v{a >> 4}, #{_s16(insns[i + 1])}",
            "22c": lambda: f"v{a & 0xF}, v{a >> 4}, {_ref(d, name, insns[i + 1])}",
            "30t": lambda: at(_s32(insns[i + 1] | (insns[i + 2] << 16))),
            "32x": lambda: f"v{insns[i + 1]}, v{insns[i + 2]}",
            "31i": lambda: f"v{a}, #{_s32(insns[i + 1] | (insns[i + 2] << 16))}",
            "31t": lambda: f"v{a}, payload " + at(_s32(insns[i + 1] | (insns[i + 2] << 16))),
            "31c": lambda: f"v{a}, {_ref(d, name, insns[i + 1] | (insns[i + 2] << 16))}",
            "35c": lambda: "{" + ", ".join(f"v{r}" for r in [insns[i + 2] & 0xF, (insns[i + 2] >> 4) & 0xF,
                                                                  (insns[i + 2] >> 8) & 0xF, insns[i + 2] >> 12,
                                                                  a & 0xF][: a >> 4]) + "}, " + _ref(d, name, insns[i + 1]),
            "3rc": lambda: f"{{v{insns[i + 2]} .. v{insns[i + 2] + a - 1}}}, " + _ref(d, name, insns[i + 1]),
            "51l": lambda: f"v{a}, #{insns[i + 1] | (insns[i + 2] << 16) | (insns[i + 3] << 32) | (insns[i + 4] << 48)}",
        }
        text["45cc"] = text["35c"]
        text["4rcc"] = text["3rc"]
        out.append(f"  {i:04x}: {name} {text[fmt]()}".rstrip())
        i += SIZE[fmt]
    return out


def code_of(d: Dex, code: int) -> tuple[int, tuple[int, ...]]:
    regs, _, _, _, _, size = struct.unpack_from("<HHHHII", d.b, code)
    return regs, struct.unpack_from(f"<{size}H", d.b, code + 16)


def dex_files(only: str | None = None):
    z = zipfile.ZipFile(APK)
    for name in sorted(n for n in z.namelist() if n.startswith("classes") and n.endswith(".dex")):
        if name == "classes4.dex" or (only and name != only):
            continue
        yield name, Dex(z.read(name))


def main() -> int:
    args = sys.argv[1:]
    only = args[args.index("--dex") + 1] if "--dex" in args else None
    if "--grep" in args:
        needle = args[args.index("--grep") + 1]
        for name, d in dex_files(only):
            for cls, data in d.classes():
                if not data:
                    continue
                for method, proto, code in d.methods(data):
                    if not code:
                        continue
                    try:
                        lines = disasm(d, code_of(d, code)[1])
                    except (KeyError, IndexError, struct.error):
                        continue
                    for line in lines:
                        if needle in line:
                            print(f"{name} {cls}.{method}{proto}: {line.strip()}")
        return 0
    want_cls = args[0]
    want = args[1] if len(args) > 1 and not args[1].startswith("--") else None
    for name, d in dex_files(only):
        for cls, data in d.classes():
            if cls != want_cls or not data:
                continue
            for method, proto, code in d.methods(data):
                if want and method != want:
                    continue
                if not code:
                    print(f"{name} {cls}.{method}{proto} (no code)")
                    continue
                regs, insns = code_of(d, code)
                print(f"{name} {cls}.{method}{proto} regs={regs}")
                for line in disasm(d, insns):
                    print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
