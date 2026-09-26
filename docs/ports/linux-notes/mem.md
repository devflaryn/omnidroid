# Linux: memory (`vm`) and guest faults (`fault`)

Measured on the port host (`../linux.md`: i5-4460, kernel 7.0, `vm.overcommit_memory = 0`,
`vm.max_map_count = 1048576`, the owner's desktop live, release builds). The seam's Linux calls
and why each was chosen are in `crates/omni-platform/src/vm/linux.rs`'s module docs; this file
holds the figures. Probes: `omni-mem/tests/probe_linux`, `omni-platform/tests/vm_commit_charge_linux.rs`,
`omni-mem/tests/{commit_charge,pager}_linux`, `omni-elf/tests/{loader_commit,cache_sharing}_linux`.

## Decisions the kernel forced (C probe first, then the suites)

| primitive | RSS | `Committed_AS` |
|---|---|---|
| `madvise(MADV_DONTNEED)` on 64 MiB touched | -65,536 kB | **0**: Linux's `MEM_RESET` trap |
| then `mprotect(PROT_NONE)` | 0 | 0 |
| `mmap(MAP_FIXED, PROT_NONE)` over it (the decommit used) | -65,536 kB | **-65,536 kB** |
| 64 MiB `mprotect`ed RW, reservation without / with `MAP_NORESERVE` | -- | +65,536 kB / **+0** |

`MAP_NORESERVE` makes a reservation no cheaper (a `PROT_NONE` private mapping is not accountable
either way) and switches commit accounting off, so it is not used (row `lnx-vm-B1`). The kernel
refuses none of the calls Windows refuses (a `munmap` of a reused address unmaps someone else's
memory), so `vm/linux.rs` keeps a ledger of every range it handed out and refuses from it.

## D10 and D12 on Linux

| quantity | Linux | Windows (D10/D12) |
|---|---|---|
| reserve 1 GiB .. 64 TiB | ~2 us per call, 0 B commit, 0 B resident (n = 31 each) | 0 B |
| largest single reservation | 96.44 TiB | 125.57 TB |
| 64 guest spaces x 16 GiB | 0 B commit | 240.8 MB |
| kernel soft fault (first touch of a committed page) | 1,437 ns/page (n = 11 x 16,384) | 398 ns |
| our `SIGSEGV` demand-pager fault, 4 KiB granule | 7,181 ns (n = 11 x 8,192) | 2,053 ns (VEH) |
| demand paging at the default 64 KiB granule | 1,761 ns per page touched | -- |
| `ensure_committed`, 4 / 16 / 64 / 256 KiB / 1 MiB granule | 3,282 / 783 / 180 / 56 / 10 ns per page | 2,414 / 596 / 150 / 35 / 9 |
| JIT emit + execute, dual-mapped memfd (`rw-s` + `r-xs`) | **117.5 ns**, 0 mismatches in 1,000,000 | 162 ns |
| JIT emit + execute, `mprotect` flipping (with the ledger) | 2,418 ns | 2,259 ns |
| grow to 3 GiB and release | +3,072.000 MiB, back to +0.000 (no page-table term) | +3,078.020 |

The soft fault costing 3.6x Windows' is probably the Meltdown mitigation (PTI) on this Haswell: a
hypothesis the numbers fit, not one they establish. D10's rule holds: commit in 64 KiB granules
ahead of use; fault-driven paging is for correctness, not the hot path.

**The extraction cache is shared across processes**: a second process mapping the 104.1 MiB
`libroblox.so` entry r-x and reading every page pays PSS 53,316 kB of 106,632 (exactly half),
private 0, `VM_ACCOUNT` 0.

## Guest faults (`fault/linux.rs`)

* dynarmic installs its `SIGSEGV` handler at the first jit and does not chain for faults in its
  code cache, so it would sit in front of the pager. `reassert_precedence()`, called by `omni-cpu`
  after every `od_jit_new`, puts ours back on top (`pager_precedence_linux`: 0 dynarmic slow-path
  entries).
* **Alternate stacks**: Rust gives each `std::thread` `max(SIGSTKSZ, AT_MINSIGSTKSZ)` = 8,192 bytes
  here; the whole pager path, kernel frame included, used at most **3,128 bytes** (n = 32 faults on
  a painted stack, `omni-mem/tests/pager_linux`). So `SA_ONSTACK` is kept and no extra alternate
  stacks are needed.
