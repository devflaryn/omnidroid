# Windows guest-RAM backing: the two probes, and how to re-run them

2026-08-19. These answer the two questions `docs/windows-ram-discard.md` left
open, and they answer them by running rather than by reading QEMU's source.
Both probes are standalone C, ~100 lines each, no QEMU involved.

Host they were run on: i7-13700F / 32 GB / RTX 4060 / Win11 26200,
103 GB free on `C:`, WHPX enabled, pagefile 30 GB system-managed.

## Build and run

MSYS2 is already installed at `C:\msys64` with `mingw-w64-x86_64-gcc`.

```bash
export PATH="/c/msys64/mingw64/bin:$PATH"
cd tools/probes
gcc -O2 -o commit_probe.exe commit_probe.c -lpsapi
gcc -O2 -o whpx_probe.exe   whpx_probe.c   -lpsapi -lwinhvplatform
./commit_probe.exe
./whpx_probe.exe
```

`whpx_probe` exits 2 if the hypervisor is absent, 3 if WHPX refuses the
mapping, 0 on success.

## Question 1 — does a file-backed section cost commit?

`commit_probe.c` allocates 3 GiB twice: once the way QEMU does today
(`VirtualAlloc(MEM_RESERVE|MEM_COMMIT)`), once as a mapped view of a sparse
file. It reads `GetPerformanceInfo().CommitTotal` around each.

```
baseline                sys_commit= 35780 MB  proc_private=     1 MB  proc_ws=     3 MB
after VirtualAlloc 3G   sys_commit= 38858 MB  proc_private=  3079 MB  proc_ws=     3 MB
after touch 512M        sys_commit= 38858 MB  proc_private=  3079 MB  proc_ws=   516 MB
after free              sys_commit= 35778 MB  proc_private=     1 MB  proc_ws=     4 MB
after CreateFileMapping sys_commit= 35778 MB  proc_private=     1 MB  proc_ws=     4 MB
after MapViewOfFile 3G  sys_commit= 35790 MB  proc_private=     7 MB  proc_ws=     4 MB
after touch 512M (file) sys_commit= 35796 MB  proc_private=     7 MB  proc_ws=   516 MB
FSCTL_SET_ZERO_DATA -> 1 (err 0)
after punch hole        sys_commit= 35790 MB  proc_private=     7 MB  proc_ws=     4 MB
after unmap/close       sys_commit= 35778 MB  proc_private=     1 MB  proc_ws=     4 MB
```

**+3078 MB of commit for private RAM, +12 MB for the same size file-backed.**
Touching 512 MB of the file view cost +6 MB of commit, not +512 MB, and
`FSCTL_SET_ZERO_DATA` over the touched range took the working set from
516 MB back to 4 MB.

## Question 2 — will WHPX accept it, and does it pin the range?

This is the one `docs/windows-ram-discard.md` called *"the single question that
decides whether the patch works at all"*. `whpx_probe.c` creates a real WHPX
partition, maps the file-backed view as guest physical memory, and then tries
to punch a hole in it **while it is mapped**.

```
hypervisor present: hr=0x00000000 value=1
partition ready                sys_commit=  35630 MB  proc_private=      1 MB
file view mapped 3G            sys_commit=  35642 MB  proc_private=      7 MB
WHvMapGpaRange(file-backed) -> 0x00000000  OK
after WHvMapGpaRange           sys_commit=  35648 MB  proc_private=      7 MB
after host touch 256M          sys_commit=  35647 MB  proc_private=      7 MB
FSCTL_SET_ZERO_DATA while mapped -> 1 (err 0)
after punch hole               sys_commit=  35637 MB  proc_private=      7 MB
readback[0]=0x00 (0x00 means the hole is visible to the guest)
file EOF=3072 MB  allocated=0 MB
cleaned up                     sys_commit=  35619 MB  proc_private=      1 MB
VERDICT: WHPX ACCEPTS file-backed guest RAM
```

Three results, in order of how much they change:

1. **`WHvMapGpaRange` returns `S_OK` on a file-backed view.** The whole
   approach was gated on this.
2. **The hypervisor does not pin the pages.** The punch succeeded on a live
   GPA mapping and the readback is zero — so a discard through this path is a
   real `MADV_DONTNEED`, not an advisory hint.
3. **The disk comes back too.** `AllocationSize` returned to 0 MB against a
   3072 MB `EndOfFile`. On Linux the equivalent is `fallocate(PUNCH_HOLE)`,
   which is exactly what `ram_block_discard_range` already calls for a
   file-backed block — so the Windows arm is a like-for-like port, not a new
   mechanism.

## What this invalidates

`docs/windows-ram-discard.md` is superseded on two points. Its status line
("not built ... this host has 6.3 GiB of free disk") no longer holds — the host
has 103 GB free and `C:\qemubuild` is a working QEMU 11.1.0 tree. And its
capacity table, which stops at *"~15 instances"*, was computed on the
assumption that commit is immovable. It is not.

`qemu_proc.py:2601` (*"COMMIT 3072 MB = -m, and NOTHING reduces it"*) and
`qemu_proc.py:2607` (*"memory-backend-file is not registered in the Windows
QEMU"*) both remain true **of the shipped binary**. Both are properties of the
build, not of Windows.

## What is still unmeasured

The probes prove the primitives. They do not prove the fleet. Open:

* what a real guest's RAM file *allocates* over a 30-minute farm, with
  `free-page-reporting=on` punching holes as the guest frees pages
* whether soft-fault latency through a file-backed mapping is acceptable at
  the 384 MB working-set ceiling the governor already imposes
* the resulting instance count on this host, where disk rather than commit is
  now expected to be the binding wall
