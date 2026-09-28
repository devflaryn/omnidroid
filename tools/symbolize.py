"""Name offsets inside a Windows executable from its PDB (dbghelp), e.g. `[thread-cpu]`'s `exe +0x31ab00`.

    python tools/symbolize.py target/release/omni-linux-run.exe 0x31ab00 0x317b00 ...

Offsets are from the image base (RVAs). Prints `offset symbol+displacement` per offset.
"""
import ctypes
import ctypes.wintypes as wt
import sys

dbghelp = ctypes.WinDLL("dbghelp.dll")
BASE = 0x10000000


class SYMBOL_INFO(ctypes.Structure):
    _fields_ = [
        ("SizeOfStruct", wt.ULONG), ("TypeIndex", wt.ULONG), ("Reserved", ctypes.c_uint64 * 2),
        ("Index", wt.ULONG), ("Size", wt.ULONG), ("ModBase", ctypes.c_uint64), ("Flags", wt.ULONG),
        ("Value", ctypes.c_uint64), ("Address", ctypes.c_uint64), ("Register", wt.ULONG),
        ("Scope", wt.ULONG), ("Tag", wt.ULONG), ("NameLen", wt.ULONG), ("MaxNameLen", wt.ULONG),
        ("Name", ctypes.c_char * 1024),
    ]


def main():
    exe = sys.argv[1]
    proc = wt.HANDLE(0x7FFF1234)  # any unique value: no live process is read
    dbghelp.SymSetOptions(0x2)  # SYMOPT_UNDNAME
    import os
    search = os.path.dirname(os.path.abspath(exe))
    assert dbghelp.SymInitialize(proc, f"{search};{search}\deps".encode(), False), "SymInitialize"
    dbghelp.SymLoadModuleEx.restype = ctypes.c_uint64
    dbghelp.SymLoadModuleEx.argtypes = [wt.HANDLE, wt.HANDLE, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint64, wt.DWORD, ctypes.c_void_p, wt.DWORD]
    base = dbghelp.SymLoadModuleEx(proc, None, exe.encode(), None, BASE, 0, None, 0)
    assert base, "SymLoadModuleEx"
    for arg in sys.argv[2:]:
        off = int(arg, 16)
        info = SYMBOL_INFO()
        info.SizeOfStruct = 88  # sizeof(SYMBOL_INFO) in C, Name[1] included
        info.MaxNameLen = 1024
        disp = ctypes.c_uint64()
        ok = dbghelp.SymFromAddr(proc, ctypes.c_uint64(base + off), ctypes.byref(disp), ctypes.byref(info))
        name = info.Name.decode(errors="replace") if ok else "?"
        print(f"{arg} {name}+{disp.value:#x}")


if __name__ == "__main__":
    main()
