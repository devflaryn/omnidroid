# Windows x86-64 virtual memory, measured

Rust probe programs on Windows 11 Pro 26200 (24 threads, 32.6 GB RAM, commit limit 46.84 GB,
user VA `0x10000..0x7ffffffeffff`, 128 TB), 2026-09-18. Commit is `PrivateUsage`
(`K32GetProcessMemoryInfo`); system commit is `CommitTotal` and is noisy (tens of MB from other
processes), so tests assert on per-process commit. Decisions built on this: D10 (memory model),
D11 (extraction cache), D12 (dual-mapped arena). Implemented in `crates/omni-platform/src/vm/`
and `crates/omni-mem`; `crates/omni-platform/tests/vm_windows.rs` asserts the behaviour.

## 1. Granularity

`dwPageSize` 4096, `dwAllocationGranularity` 65536.

| operation | granularity |
|---|---|
| `MEM_RESERVE` base | 64 KB (rounded down) |
| `MEM_COMMIT`, `VirtualProtect`, `MEM_DECOMMIT` | 4 KB, any page-aligned address |
| `MEM_RELEASE` | whole reservation only (partial: err 87) |
| classic `MapViewOfFile` file offset | 64 KB |
| `MapViewOfFile3` into a placeholder | 4 KB base and offset |

Sub-page requests round to the containing page. A recommitted page reads zero. 16 KB alignment is
available. Costs: per-page commit 284 ns, bulk commit 3 ns/page, first touch 379 ns/page,
`VirtualProtect` of one page 360 ns.

## 2. Reserve, then commit on demand

- `MEM_RESERVE` costs zero commit and zero working set at any size (341 GB live; 97.7 TB in one
  call). Largest single reservation: 125.57 TB.
- `MEM_COMMIT` charges commit immediately, before any touch (1024 MB committed, untouched: 1026.66
  MB commit, 4.68 MB working set). Commit, not working set, caps instance count.
- Grow to 3 GB then `MEM_DECOMMIT` to 512 MB: commit fell to 513.66 MB; `EmptyWorkingSet` then took
  working set to 0.15 MB.
- Page tables are charged to process commit: 1/511 of committed VA when dense; one page per 2 MB
  costs 2x, one page per 1 GB 3x. `MEM_DECOMMIT` returns them.

## 3. Reclaiming

256 MB dirtied, then:

| primitive | commit returned | working set | data |
|---|---|---|---|
| `VirtualFree(MEM_DECOMMIT)` | **256.5 MB** (210.8 ns/page) | freed | zeroed |
| `VirtualFree(MEM_RELEASE)` | 256.5 MB | freed | address gone |
| `MEM_RESET` (+/- `EmptyWorkingSet`), `MEM_RESET_UNDO` | 0 | unchanged / freed | kept |
| `DiscardVirtualMemory` | 0 | freed | zeroed |
| `OfferVirtualMemory` / `Reclaim` | 0 | freed / back | kept |
| `EmptyWorkingSet`, `SetProcessWorkingSetSizeEx` | 0 | freed | kept |

Only decommit and release return commit. Bringing pages back: decommitted 253 ns/page, trimmed
(`EmptyWorkingSet`, hard fault) 913 ns/page.

## 4. Placeholders

- `VirtualAlloc2`, `MapViewOfFile3`, `UnmapViewOfFile2` are exported from `kernelbase.dll`, not
  `kernel32.dll`; `vm/windows.rs` resolves them with `GetProcAddress` and returns
  `VmError::MissingSymbol` if absent (Windows 10 1803+).
- A placeholder reservation costs no commit. `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER` splits at any
  4 KB boundary; `MEM_COALESCE_PLACEHOLDERS` merges.
- `MEM_REPLACE_PLACEHOLDER` needs an exact-size placeholder (else err 487).
- `UnmapViewOfFile2(MEM_PRESERVE_PLACEHOLDER)` returns the range to a placeholder; flag 0 frees it.
- Views: shared read-only or execute views cost only page tables; `PAGE_WRITECOPY` views are charged
  their full size at map time. 16 views of one 8 MB file: 0.25 MB commit.
- Costs: split 1.05 us, map 0.98 us, unmap 1.24 us; ~7 us per ELF segment with the protection flip.
- Confirmed 512/512 maps at 4 KB offset steps with content checked; sub-page offsets fail with
  1132 `ERROR_MAPPED_ALIGNMENT`.

Found later by `omni-mem` and `vm_windows.rs`: splitting a range that is exactly one placeholder
fails (487), splitting across two placeholders fails (87), merging one placeholder fails (487);
releasing a non-live base fails 487 (a partial release of a live one is 87). A view cannot be
partially unmapped, so `GuestSpace::unmap` remaps the surviving head and tail.

## 5. Mapping files (APK contents)

5.A/5.B: the same offset (4096, 69632...) fails with 1132 through `MapViewOfFile` or
`MapViewOfFile3(BaseAddress = NULL)` and succeeds when replacing a placeholder.

5.D/5.E: a STORED entry at a 4 KB (or 16 KB) aligned offset can be mapped zero-copy; a DEFLATEd or
unaligned one cannot. Copying instead costs ~1 ms and ~4 MB of permanent commit per 4 MB. Hence D11:
decompress each `.so` once into a 4 KB-aligned cache file (`crates/omni-apk/src/cache.rs`) and map
that.

5.F: executable file views need the file opened `GENERIC_READ | GENERIC_EXECUTE` and the section
`PAGE_EXECUTE_READ` (else err 6 at section creation, err 87 raising a view to RX). Views may be `R`,
`WRITECOPY`, `RX` only (`RW`, `RWX`, `EXECUTE_WRITECOPY` at map: err 5). `VirtualProtect` on a view
of an RX section reaches `R`, `WRITECOPY`, `RX`, `EXECUTE_WRITECOPY`, `NOACCESS`, never `RW`/`RWX`;
relocating file-backed text goes RX -> `EXECUTE_WRITECOPY` -> write -> RX. Measured later
(`vm_windows.rs`): a view mapped `PAGE_READONLY` cannot be raised to RX even from an RX section, so
executability is chosen both at file open and at view map.

## 6. Many instances

Reservations have no system-wide cost: 64 processes x 16 GB reserved (1 TB) cost ~241 MB of system
commit, mostly process images and stacks. One process fits ~32,700 4 GB placeholders (0.9 MB
commit) before running out of address space. 300,000 separate 64 KB reservations: 162.8 ms, 14 bytes
of commit each.

## 7. Faults and guard pages

- Reserve, fault, commit in a vectored handler, resume: 65,536 of 65,536 correct, 2,053 ns per fault
  (kernel first-touch: 398 ns). Throughput saturates at ~1.3 M faults/s from 4 threads (address
  space lock). So commit in bulk, and on a fault commit a 64 KB-1 MB granule, not a page.
- `PAGE_GUARD` is one-shot per page (1,430 ns per fault).
- Re-protecting: 375 ns (RW->R) / 246 ns (R->RW) per page individually, 19.8 ns/page in one call.

## 8. Large pages

`GetLargePageMinimum` is 2 MB, but `SeLockMemoryPrivilege` is not held (err 1300/1314), and large
pages are non-pageable. Not used. TLB-miss penalty with 4 KB pages on a pointer chase: 108.5 vs
5.0 ns per access (512 MB vs 2 MB working set).

## 9. W^X and JIT memory

| method | per emit-and-call |
|---|---|
| `VirtualProtect` RW->RX->RW cycle | 2,259 ns |
| RWX page (allowed on this host) | 168 ns |
| dual mapping: one pagefile-backed section, RW view + RX view | 162 ns, 0 mismatches in 200,000, no flush needed |

The section is charged its full size to system commit at creation, not to the process's
`PrivateUsage` (hence `omni_mem::CommitBudget`). Decommitting 32 MB of private RX code returned
32.06 MB. `omni_mem::CodeArena` is this dual-mapped arena (D12). dynarmic's own code cache is not:
it is committed `PAGE_EXECUTE_READWRITE` (D12 exception).

## Design summary

End-to-end probe: a 4 GB placeholder, 24 file views of 8 libraries (0.57 MB commit), a heap grown
to 256 MB and decommitted to 32 MB: 37.25 MB total commit, teardown 43 us. Rules that follow:
never commit more than the guest asked for; free only with `MEM_DECOMMIT`/`MEM_RELEASE`; always map
files into an exact-size placeholder; unmap every view before releasing a placeholder; keep
allocations clustered; do not rely on fault-driven paging on hot paths.
