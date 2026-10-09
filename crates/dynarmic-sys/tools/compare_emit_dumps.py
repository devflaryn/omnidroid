"""Compare two emit dumps (`tests/code_size.rs::the_speed_of_emission`, OMNI_EMIT_DUMP).

Each line is one block: guest PC, total size (with link slots), code bytes (hex, up to the slots).
Two builds of the emitter are the same when every block has the same guest PC, the same total size,
the same code length and the same bytes -- except inside the operand of a host *absolute address*,
which differs between builds and between runs (Windows ASLR moves the executable):

  * `mov r64, imm64` (REX.W 48/49, B8+r): the 8-byte immediate, e.g. a host function's address
    before `call rax`;
  * `call rel32` (E8): the 4-byte displacement, for a host function within 2 GiB.

A difference anywhere else, or one whose surrounding instruction is not of those forms in both
dumps, fails. Usage: python compare_emit_dumps.py before.dump after.dump
"""
import sys


def load(path):
    blocks = []
    with open(path, encoding="ascii") as f:
        for line in f:
            pc, total, code = line.split()
            blocks.append((int(pc, 16), int(total), bytes.fromhex(code)))
    return blocks


def address_operand(a, b, p):
    """Whether byte `p` lies in an absolute-address operand of the same instruction in both."""
    for s in range(max(0, p - 9), p + 1):
        # mov r64, imm64: REX.W prefix, B8+r, then 8 bytes.
        if s + 10 <= len(a) and s + 2 <= p < s + 10:
            if all(x[s] in (0x48, 0x49) and 0xB8 <= x[s + 1] <= 0xBF for x in (a, b)) and a[s:s + 2] == b[s:s + 2]:
                return True
        # call rel32: E8, then 4 bytes.
        if s + 5 <= len(a) and s + 1 <= p < s + 5:
            if a[s] == 0xE8 and b[s] == 0xE8:
                return True
    return False


def main():
    before, after = load(sys.argv[1]), load(sys.argv[2])
    if len(before) != len(after):
        print(f"FAIL: {len(before)} blocks before, {len(after)} after")
        return 1
    masked = 0
    bad = 0
    for i, ((pc_a, tot_a, a), (pc_b, tot_b, b)) in enumerate(zip(before, after)):
        if pc_a != pc_b or tot_a != tot_b or len(a) != len(b):
            print(f"FAIL block {i}: pc {pc_a:#x}/{pc_b:#x}, total {tot_a}/{tot_b}, code {len(a)}/{len(b)}")
            bad += 1
            if bad > 10:
                break
            continue
        if a == b:
            continue
        for p in range(len(a)):
            if a[p] != b[p]:
                if address_operand(a, b, p):
                    masked += 1
                else:
                    print(f"FAIL block {i} (pc {pc_a:#x}): byte {p}: {a[max(0, p - 8):p + 8].hex()} vs {b[max(0, p - 8):p + 8].hex()}")
                    bad += 1
                    break
        if bad > 10:
            break
    total = sum(len(x[2]) for x in before)
    if bad:
        print(f"FAIL: {bad} blocks differ")
        return 1
    print(f"same: {len(before)} blocks, {total} code bytes; {masked} bytes differ, all inside host-address operands")
    return 0


if __name__ == "__main__":
    sys.exit(main())
