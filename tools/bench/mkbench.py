#!/usr/bin/env python3
"""Hand-assembled static ELFs that measure the arm64 translation tax in-guest.

WHY THIS EXISTS
---------------
"The guest is CPU-bound on arm64 translation and no flag removes it" was the
standing explanation for every unexplained slowness in this project, and it had
never been measured -- because measuring it needs an arm64 binary and an
x86-64 binary running the SAME work, and there is no NDK on the Windows box.

So the benchmark is emitted byte by byte from here: an ELF header, one PROGBITS
program header, a loop over a register dependency chain, and `exit(0)`. No
libc, no dynamic linker, no allocator, no runtime -- nothing between the
instructions under test and the CPU (or the translator).

HOW TO RUN IT
-------------
    python tools/bench/mkbench.py
    adb push bench_* /data/local/tmp/
    adb shell chmod 755 /data/local/tmp/bench_*

    # the arm64 binaries need the base's own binfmt handler registered
    # (adbd is root on this base):
    adb shell 'mount -t binfmt_misc none /proc/sys/fs/binfmt_misc'
    adb shell 'cat /system/etc/binfmt_misc/arm64_exe > /proc/sys/fs/binfmt_misc/register'

    adb shell 'cd /data/local/tmp; for f in bench_x64_nul bench_x64_int         bench_x64_sse bench_x64_icall bench_a64_nul bench_a64_int         bench_a64_neon bench_a64_icall; do         S=$(date +%s%N); ./$f; E=$(date +%s%N);         echo "$f $(( (E-S)/1000000 ))ms"; done'

`bench_*_nul` is the same binary with ONE iteration: subtract it to take the
process startup out of the number. The native bridge's runner costs ~70 ms of
it, against ~5 ms for a native exec, so leaving it in would have read as a 30%
translation tax that is really just exec.

TWO TRAPS, BOTH HIT
-------------------
* **The ELF needs a real section-header table.** With `e_shnum = 0` the native
  bridge's program runner refuses the file outright --
  `has invalid e_shstrndx` -- where the kernel's own ELF loader does not care.
  Three sections (NULL, .text, .shstrtab) with e_shstrndx pointing at the last.
* **binfmt_misc is not mounted on this base**, even though
  `ro.vendor.enable.native.bridge.exec64` is 1 and
  /system/etc/binfmt_misc/arm64_exe exists. Without the mount+register above,
  an arm64 binary fails as `not executable: 64-bit ELF file` and looks exactly
  like a broken ELF.

WHAT IT MEASURED (2026-09-01, i7-13700F, WHPX, Bliss 16.9.7, ndk_translation
0.2.3), 200M iterations, startup subtracted:

    integer chain    x86-64 221 ms   arm64 251 ms   1.14x
    SIMD chain       x86-64 476 ms   arm64 466 ms   0.98x
    indirect call    x86-64 173 ms   arm64 266 ms   1.54x

See MODES.md, "The translator is not the wall".
"""
import struct
BASE = 0x400000

def elf(machine, code, out):
    ehsize, phentsize, shentsize = 64, 56, 64
    codeoff = ehsize + phentsize
    entry   = BASE + codeoff
    strtab  = b"\x00.text\x00.shstrtab\x00"
    stroff  = codeoff + len(code)
    shoff   = stroff + len(strtab)
    filesz  = codeoff + len(code)
    e  = b"\x7fELF\x02\x01\x01\x00" + b"\x00" * 8
    e += struct.pack("<HHI", 2, machine, 1)
    e += struct.pack("<QQQ", entry, ehsize, shoff)
    e += struct.pack("<IHHHHHH", 0, ehsize, phentsize, 1, shentsize, 3, 2)
    p  = struct.pack("<IIQQQQQQ", 1, 5, 0, BASE, BASE, filesz, filesz, 0x1000)
    sh  = struct.pack("<IIQQQQIIQQ", 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
    sh += struct.pack("<IIQQQQIIQQ", 1, 1, 6, BASE + codeoff, codeoff, len(code), 0, 0, 16, 0)
    sh += struct.pack("<IIQQQQIIQQ", 7, 3, 0, 0, stroff, len(strtab), 0, 0, 1, 0)
    open(out, "wb").write(e + p + code + strtab + sh)

def w(*words):
    return b"".join(struct.pack("<I", x) for x in words)

def movz(rd, imm, sh=0): return 0xD2800000 | (sh // 16 << 21) | (imm << 5) | rd
def movk(rd, imm, sh=0): return 0xF2800000 | (sh // 16 << 21) | (imm << 5) | rd

def a64(n, body):
    pre  = [movz(0, n & 0xFFFF), movk(0, (n >> 16) & 0xFFFF, 16),
            movz(1, 0x1234), movz(2, 0x5678), movz(3, 0x9abc), movz(4, 0xdef0)]
    loop = list(body) + [0xD1000400]
    back = -(len(loop) + 1)
    loop += [0xB5000000 | ((back & 0x7FFFF) << 5) | 0]
    tail = [movz(8, 93), movz(0, 0), 0xD4000001]
    return w(*(pre + loop + tail))

INT_A64  = [0x9B027C21, 0x8B030021, 0xCA040021]
NEON_A64 = [0x6E62DC00, 0x4E63D400, 0x6E61DC00]

def x64(n, body):
    c  = b"\x48\xb8" + struct.pack("<Q", n)
    c += b"\x48\xc7\xc3\x34\x12\x00\x00"
    c += b"\x48\xc7\xc1\x78\x56\x00\x00"
    c += b"\x48\xc7\xc2\xbc\x9a\x00\x00"
    c += b"\x48\xc7\xc6\xf0\xde\x00\x00"
    loop = body + b"\x48\xff\xc8"
    c += loop + b"\x75" + struct.pack("b", -(len(loop) + 2))
    c += b"\xb8\x3c\x00\x00\x00\x31\xff\x0f\x05"
    return c

INT_X64 = b"\x48\x0f\xaf\xd9\x48\x01\xd3\x48\x31\xf3"
SSE_X64 = b"\x66\x0f\x59\xc2\x66\x0f\x58\xc3\x66\x0f\x59\xc1"

N = 200_000_000
elf(0xB7, a64(N, INT_A64),  "bench_a64_int")
elf(0xB7, a64(N, NEON_A64), "bench_a64_neon")
elf(0xB7, a64(1,  INT_A64), "bench_a64_nul")
elf(0x3E, x64(N, INT_X64),  "bench_x64_int")
elf(0x3E, x64(N, SSE_X64),  "bench_x64_sse")
elf(0x3E, x64(1,  INT_X64), "bench_x64_nul")
print("built N =", N)

# ---- indirect call: the case a binary translator actually pays for ----
def a64_icall(n):
    codeoff = 120
    tgt = BASE + codeoff + 10 * 4
    ws = [movz(0, n & 0xFFFF), movk(0, (n >> 16) & 0xFFFF, 16),
          movz(5, tgt & 0xFFFF), movk(5, (tgt >> 16) & 0xFFFF, 16),
          0xD63F0000 | (5 << 5),                  # blr x5
          0xD1000400,                             # sub x0, x0, #1
          0xB5000000 | (((-2) & 0x7FFFF) << 5),   # cbnz x0, loop
          movz(8, 93), movz(0, 0), 0xD4000001,
          0xD65F03C0]                             # ret
    return w(*ws)

def x64_icall(n):
    c  = b"\x48\xb8" + struct.pack("<Q", n)       # mov rax, n
    c += b"\x48\x8d\x3d" + struct.pack("<i", 16)  # lea rdi,[rip+16]
    c += b"\xff\xd7"                              # call rdi
    c += b"\x48\xff\xc8"                          # dec rax
    c += b"\x75\xf9"                              # jnz loop
    c += b"\xb8\x3c\x00\x00\x00\x31\xff\x0f\x05"  # exit(0)
    c += b"\xc3"                                  # target: ret
    return c

elf(0xB7, a64_icall(N), "bench_a64_icall")
elf(0x3E, x64_icall(N), "bench_x64_icall")
print("icall built")
