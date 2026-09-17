# Windows x86-64 Virtual Memory Model for Omnidroid

Empirical investigation. Every number below was produced by a Rust probe program written for
this document, compiled with `rustc 1.89.0 (29483883e 2025-08-04)` / `cargo 1.89.0`, MSVC
toolchain, and run on the host described below. No number is quoted from documentation.

**Host measured:** Windows 11 Pro 10.0.26200 (build 26200), x86-64, 24 logical processors,
32.604 GB physical RAM, commit limit **47963.7 MB (46.84 GB)**, 128.00 TB user virtual address
space (`lpMinimumApplicationAddress = 0x0000000000010000`,
`lpMaximumApplicationAddress = 0x00007ffffffeffff`).

**Probe source:** `C:\Users\berat\AppData\Local\Temp\claude\C--Users-berat-Desktop-Omni-Apps-omnidroid\2692e040-d7b4-4250-b114-62e3f26c66c9\scratchpad\memprobe\`

| probe | file | covers |
|---|---|---|
| p1 | `src/bin/p1_granularity.rs` | Q1 |
| p2 | `src/bin/p2_reserve_commit.rs` | Q2 |
| p2b | `src/bin/p2b_pagetables.rs` | Q2 (page-table commit overhead) |
| p3 | `src/bin/p3_reclaim.rs` | Q3 |
| p4 | `src/bin/p4_placeholder.rs` | Q4 |
| p5 | `src/bin/p5_apk_mapping.rs` | Q4/Q5 |
| p5b | `src/bin/p5b_offset_sweep.rs` | Q4/Q5 (exhaustive confirmation) |
| p6 | `src/bin/p6_instances.rs` | Q6 |
| p7 | `src/bin/p7_faults.rs` | Q7 |
| p8 | `src/bin/p8_largepages.rs` | Q8 |
| p9 | `src/bin/p9_wx_jit.rs` | Q9 |
| p10 | `src/bin/p10_design_validation.rs` | end-to-end validation of the recommended design |
| p11 | `src/bin/p11_view_protections.rs` | section/view protection matrix for the ELF loader |

Shared Win32 bindings are hand-rolled in `src/lib.rs` (no external crates; `windows-sys` was
not needed and was deliberately avoided so that DLL resolution is observable — see Q4).

All commit-charge figures are `PROCESS_MEMORY_COUNTERS_EX.PrivateUsage` via
`K32GetProcessMemoryInfo`; all working-set figures are `WorkingSetSize`; system-wide commit is
`PERFORMANCE_INFORMATION.CommitTotal * PageSize` via `K32GetPerformanceInfo`.

---

## 1. Page and allocation granularity

**Measured result.** `dwPageSize = 4096`, `dwAllocationGranularity = 65536`. The 64 KB figure
is confirmed, but it constrains **far less** than folklore suggests. Exactly two things are
64 KB-constrained; everything else is 4 KB-granular.

| operation | granularity actually enforced | evidence |
|---|---|---|
| `VirtualAlloc(MEM_RESERVE)` **base address** | **64 KB** (request is rounded *down*) | asked for `…d15d681000`, got `…d15d680000` |
| `VirtualAlloc(MEM_COMMIT)` inside a reservation | **4 KB**, at any page-aligned address | committed 1 page at `res+0x3000` (64 KB-misaligned), returned base == requested base |
| `VirtualProtect` | **4 KB** single page | page 7 of a 16-page run set to `R`, then `RX`, while pages 6 and 8 stayed `RW` |
| `VirtualFree(MEM_DECOMMIT)` | **4 KB** single page | one page went `COMMIT`→`RESERVE`, commit charge fell by exactly 4096 bytes, neighbours kept their data |
| `VirtualFree(MEM_RELEASE)` | whole reservation only | partial release returned `ok=0 err=87 ERROR_INVALID_PARAMETER` |
| sub-page requests | silently rounded to the containing page | `MEM_DECOMMIT` of 16 bytes at an unaligned address decommitted the whole 4 KB page; `MEM_COMMIT` of 8 bytes returned the page base |
| classic `MapViewOfFile` **file offset** | **64 KB** | see Q5 |
| `MapViewOfFile3` into a **placeholder** | **4 KB** for both base and offset | see Q4 |

So: **yes** to committing an individual 4 KB page inside a larger `MEM_RESERVE`d range,
**yes** to `VirtualProtect` of a single 4 KB page, **yes** to decommitting a single 4 KB page.

Re-committing a decommitted page yields **zero-filled** memory (wrote `0x07`, decommitted,
re-committed, read back `0x00`). This matches Linux `MADV_DONTNEED`/anonymous-mmap semantics,
which is what the guest expects.

**16 KB alignment (newer NDK ELF alignment) is fully honourable.** A 256 MB reservation came
back 16 KB-aligned, and 16 KB commits succeeded at `+0x0000`, `+0x4000`, `+0xc000` and even at
`+0x1000`.

**Timings** (p1, Q1.10):

| operation | cost |
|---|---|
| 10 000 individual 4 KB `MEM_COMMIT` calls | 2.842 ms → **284 ns each** |
| one bulk `MEM_COMMIT` of the same 10 000 pages (39 MB) | 25.4 µs → **3 ns/page** |
| first touch of committed-but-untouched pages (kernel zero-page soft fault) | **379 ns/page** |
| second touch (no fault) | 12.0 ns/page |
| 10 000 single-page `VirtualProtect` calls | 3.599 ms → **360 ns each** |

Bulk commit is ~95x cheaper per page than per-page commit. The syscall, not the page, is the cost.

---

## 2. Reserve-then-commit-on-demand — the central experiment

**Measured result: a pure `MEM_RESERVE` costs exactly zero commit charge, zero working set,
at any size up to the full 128 TB address space.** Commit charge tracks `MEM_COMMIT`, not
`MEM_RESERVE`, and working set tracks *touch*, not commit.

### 2.A Pure reservations (p2)

Nothing is printed between samples, so no heap traffic pollutes the deltas.

| step | priv commit (MB) | Δ commit (MB) | working set (MB) | system commit (MB) |
|---|---|---|---|---|
| baseline (process start) | 0.656 | 0.000 | 4.617 | 35750.2 |
| after `MEM_RESERVE` 1 GB | 0.656 | **0.000** | 4.617 | 35750.2 |
| after `MEM_RESERVE` 4 GB (cumul. 5 GB) | 0.656 | **0.000** | 4.617 | 35750.2 |
| after `MEM_RESERVE` 16 GB (cumul. 21 GB) | 0.656 | **0.000** | 4.617 | 35750.2 |
| after `MEM_RESERVE` 64 GB (cumul. 85 GB) | 0.656 | **0.000** | 4.617 | 35750.2 |
| after `MEM_RESERVE` 256 GB (cumul. **341 GB**) | 0.656 | **0.000** | 4.617 | 35750.2 |
| after releasing all reservations | 0.656 | 0.000 | 4.625 | 35750.2 |

341 GB of live reservations in a process on a machine with a 46.84 GB commit limit, at zero
cost. A separate check (p2b, Q2b.2) reserved **100 000 GB (97.7 TB) in one call**: `dCommit = 0
bytes, dWS = 0 bytes`.

Largest single contiguous `MEM_RESERVE`, found by binary search (p2, Q2.E): **128 581 GB =
125.57 TB**. `GlobalMemoryStatusEx` reported `TotalVirtual = 128.00 TB`,
`AvailVirtual = 128.00 TB`.

### 2.B Commit is not touch (p2)

| step | priv commit (MB) | working set (MB) | page faults |
|---|---|---|---|
| reserved 4 GB, nothing committed | 0.656 | 4.676 | 1 270 |
| committed 256 MB (untouched) | 257.156 | **4.680** | 1 399 |
| committed 512 MB (untouched) | 513.656 | **4.680** | 1 527 |
| committed 768 MB (untouched) | 770.160 | **4.680** | 1 656 |
| committed 1024 MB (untouched) | 1026.660 | **4.680** | 1 784 |
| touched every page of 256 MB | 1026.660 | 260.680 | 67 320 |
| touched every page of 512 MB | 1026.660 | 516.680 | 132 856 |
| touched every page of 768 MB | 1026.660 | 772.680 | 198 392 |
| touched every page of 1024 MB | 1026.660 | **1028.680** | 263 928 |

This is the crux the brief asked about. **`MEM_COMMIT` charges the commit limit immediately,
before a single byte is touched.** A design that commits 4 GB per instance consumes 4 GB of the
46.84 GB commit limit whether or not the guest ever touches it. Working set, by contrast, only
grows on touch. Therefore commit — not working set — is the resource that caps instance count,
exactly as the brief suspected.

### 2.C Demand growth to a 3 GB peak, settling to 512 MB (the Roblox scenario)

| step | priv commit (MB) | working set (MB) | system commit (MB) |
|---|---|---|---|
| guest AS reserved (4 GB), 0 committed | 0.656 | 4.688 | 35747.4 |
| startup peak: 1024 MB committed+touched | 1026.660 | 1028.688 | 36772.2 |
| startup peak: 2048 MB committed+touched | 2052.664 | 2052.730 | 37794.0 |
| startup peak: 3072 MB committed+touched | 3078.668 | 3076.730 | 38897.2 |
| **after `MEM_DECOMMIT` down to 512 MB** | **513.656** | 516.730 | 36394.8 |
| after `EmptyWorkingSet` | 513.656 | **0.148** | 36417.0 |

The instance grew to 3 GB and shrank back to 513 MB of commit charge and (after an explicit
trim) essentially zero working set, while the 4 GB guest address space stayed reserved
throughout: `VirtualQuery` showed `guest[0]` `COMMIT` 512 MB, `guest[512MB]` `RESERVE` 3.5 GB,
`guest[3GB]` `RESERVE`. **The hard requirement is satisfiable.**

### 2.D Exact per-page accounting

Commit 1 page → **+12288 B**; commit another page in the same region → **+4096 B**; commit
16 pages → **+65536 B**. The first commit in a fresh region costs 8 KB extra: page-table pages,
which Windows charges to process commit. Quantified in 2.E.

### 2.E Hidden commit cost of page tables (p2b) — matters for a sparse guest heap

| pattern | data pages | Δ commit | commit per data page |
|---|---|---|---|
| dense: 1 GB committed in one call | 262 144 | 1 050 628 KB | 4.0078 KB |
| sparse: 1 page per 2 MB across 1 GB | 512 | 4 096 KB | **8.0000 KB** |
| sparse: 1 page per 1 GB across 64 GB | 64 | 764 KB | **11.9375 KB** |
| 64 KB committed every 128 KB (50 % density, 512 MB) | 131 072 | 526 340 KB | 4.0157 KB |

Dense commit overhead is 2.004 MB per GB = **0.196 % = 1/511** (one 4 KB PT page per 2 MB of
VA). A maximally sparse pattern (one page per 2 MB) suffers **2.0x commit amplification**; one
page per 1 GB suffers 3.0x. Page-table commit **is** returned by `MEM_DECOMMIT`
(p2b, Q2b.3: commit 512 MB + touch = 513.000 MB including 1024 KB of page tables; after
`MEM_DECOMMIT` the delta versus the start was 0.000 MB).

**Verdict on Q2: confirmed.** Reservation is free; commit is the charged resource; commit
tracks actual usage when the runtime decommits; page-table overhead is 1/511 of committed VA
for dense allocations and up to 3x for pathologically sparse ones.

---

## 3. Reclaiming memory

**Measured result: only `VirtualFree(MEM_DECOMMIT)` and `VirtualFree(MEM_RELEASE)` return
commit charge. Every other primitive returns working set only and leaves commit charge
untouched.** Since commit is the scarce resource, `MEM_DECOMMIT` is the only real reclamation
primitive for Omnidroid.

256 MB region, every page dirtied first; each row is an independent fresh region
(p3, `src/bin/p3_reclaim.rs`). "dWS+400 ms" is a second sample after a 400 ms settle, to catch
asynchronous trimming.

| primitive | Δ commit (MB) | Δ WS (MB) | Δ WS +400 ms | state after | addr still reserved | data | ns/page | return |
|---|---|---|---|---|---|---|---|---|
| `VirtualFree(MEM_DECOMMIT)` | **−256.50** | −255.98 | −255.97 | RESERVE | yes | **ZEROED** | 210.8 | ok=1 |
| `VirtualAlloc(MEM_RESET)` | **0.00** | +0.01 | +0.06 | COMMIT | yes | PRESERVED | 32.5 | ptr |
| `MEM_RESET` + `EmptyWorkingSet` | **0.00** | −260.68 | −260.58 | COMMIT | yes | PRESERVED | – | – |
| `MEM_RESET_UNDO` (after `MEM_RESET`) | 0.00 | +0.02 | +0.02 | COMMIT | yes | PRESERVED | 22.9 | ptr, err=0 |
| `DiscardVirtualMemory` | **0.00** | −255.97 | −255.97 | COMMIT | yes | **ZEROED** | 1016.1 | rc=0 |
| `OfferVirtualMemory(VeryLow)` | **0.00** | −255.99 | −255.99 | COMMIT | yes | inaccessible while offered | 543.8 | rc=0 |
| → `ReclaimVirtualMemory` | 0.00 | +256.00 | 0.00 | COMMIT | yes | PRESERVED | 838.3 | rc=0 (content kept) |
| `EmptyWorkingSet` | **0.00** | −256.42 | −256.31 | COMMIT | yes | PRESERVED | 278.0 | ok=1 |
| `SetProcessWorkingSetSizeEx(32/64 MB, HARDWS_MAX_DISABLE)` | **0.00** | −192.19 | −192.09 | COMMIT | yes | PRESERVED | – | ok=1 |
| `VirtualFree(MEM_RELEASE)` (whole VAD) | **−256.50** | −255.97 | −255.91 | FREE | **no** | address gone | 150.3 | ok=1 |

Notes from the measurements:

- `MEM_RESET` is a trap for this design. It costs 32.5 ns/page (cheapest of all), and it does
  **not** even reduce working set on its own (+0.01 MB) — it only tells the kernel not to bother
  writing the pages to the pagefile. Commit charge is untouched. **`MEM_RESET` is useless for
  Omnidroid's requirement.**
- `DiscardVirtualMemory` does return working set and zeroes the data, but keeps the full commit
  charge. It is `MADV_DONTNEED` *without* the accounting benefit, and it is 4.8x slower per page
  than `MEM_DECOMMIT`.
- `OfferVirtualMemory` removed 256 MB from the working set and `ReclaimVirtualMemory` returned
  `rc=0`, meaning the contents survived (a `170 ERROR_BUSY` return would mean they were
  discarded). Commit charge never moved. Useful for a *cache* whose contents you would like back
  cheaply; useless for relieving commit pressure.
- Cost of bringing 256 MB back (p3, Q3.9): `MEM_DECOMMIT` → recommit + first touch
  **253 ns/page**; `DiscardVirtualMemory` → touch **231 ns/page**; `EmptyWorkingSet` → touch
  **913 ns/page** (the last is a genuine hard fault against the pagefile, hence 4x the cost).

**Verdict on Q3: `MEM_DECOMMIT` is the only primitive that frees commit charge, and it also
frees working set, keeps the address reserved, and zero-fills on re-commit — which is exactly
the semantics a guest `munmap`/`MADV_DONTNEED` needs.** It costs 210.8 ns/page to release and
253 ns/page to bring back. `EmptyWorkingSet` is a useful *additional* lever to shed physical
pages from an idle-but-committed instance without giving up its data, at the price of hard
faults later.

---

## 4. Placeholder reservations — the key modern API

### 4.0 Availability and linkage (measured, p3/p4 Q4.0)

| function | exported from `kernel32.dll` | exported from `kernelbase.dll` |
|---|---|---|
| `VirtualAlloc2` | **no** | **yes** |
| `MapViewOfFile3` | **no** | **yes** |
| `UnmapViewOfFile2` | **no** | **yes** |
| `DiscardVirtualMemory` | yes | yes |
| `OfferVirtualMemory` | yes | yes |
| `ReclaimVirtualMemory` | yes | yes |

The three placeholder APIs exist on this host but are **not** in `kernel32.dll`. The probes
resolved them with `LoadLibraryA("kernelbase.dll")` + `GetProcAddress` and called them through
`unsafe extern "system" fn` pointers; this worked for every call in this document. Whether
`windows-sys` links them correctly out of the box is **UNVERIFIED** — no `windows-sys` build was
attempted here. The safe route, and the one proven by these probes, is explicit
`GetProcAddress` from `kernelbase.dll` at startup (or linking `onecore.lib`/`mincore.lib`).

### 4.1–4.2 Reserve and split

`VirtualAlloc2(NULL, 4 GB, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS)` succeeded and
cost **0.645 MB total process commit** (i.e. nothing above the process baseline).
`VirtualQuery` reports it as `RESERVE / PRIVATE`.

Splitting with `VirtualFree(base, len, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)` (p4, Q4.2):

| split requested | result |
|---|---|
| 64 KB at the start (granularity aligned) | ok, piece size 65536 |
| **4 KB** at +1 MB | ok, piece size **4096** |
| **4 KB at a 4 KB-aligned, 64 KB-MISALIGNED base** (`…777b1000`) | **ok**, piece size 4096 |
| **16 KB** at +3 MB | ok, piece size 16384 |

Placeholders split at **page granularity**, at page-aligned addresses. 64 KB does not apply.

`MEM_COALESCE_PLACEHOLDERS` merged two adjacent 64 KB placeholders into one 128 KB placeholder
(`ok=1`, verified by `VirtualQuery`).

### 4.3 Private committed memory into a placeholder

| call | result |
|---|---|
| `VirtualAlloc2(64 KB, MEM_RESERVE\|MEM_COMMIT\|MEM_REPLACE_PLACEHOLDER, RW)` | ok, Δcommit = **65536 B** exactly, `COMMIT/PRIVATE`, writable |
| same for a **4 KB** placeholder | ok |
| same at a **64 KB-misaligned** 4 KB base | **ok** |

### 4.4–4.6 Alignment rules for `MapViewOfFile3` into a placeholder

File-offset sweep, base always 64 KB-aligned (p4, Q4.4) — content column is the tag written at
that file page, so it proves the offset was honoured:

| file offset | view size | result | content seen |
|---|---|---|---|
| 0 | 64 KB | ok | `s0p000000` |
| **4096** | 64 KB | **ok** | `s0p000001` |
| **8192** | 64 KB | **ok** | `s0p000002` |
| **16384** | 64 KB | **ok** | `s0p000004` |
| 65536 | 64 KB | ok | `s0p000016` |
| **69632** (64 K + 4 K) | 64 KB | **ok** | `s0p000017` |
| 4660 (`0x1234`, not page-aligned) | 64 KB | **FAIL err 1132** `ERROR_MAPPED_ALIGNMENT` |
| 131072 | 4 KB | ok | `s0p000032` |
| 131072 | 16 KB | ok | `s0p000032` |

Base-address sweep, offset 0: `+0x0`, `+0x1000`, `+0x2000`, `+0x4000`, `+0x8000`, `+0x10000`
**all ok**, and (p5, Q5.C) the returned base equalled the requested base in every case.

View size: 4 KB, 16 KB, 60 KB, 64 KB, 68 KB, 128 KB all ok; `VirtualQuery` reported the exact
requested size as the region size. Size must be a multiple of 4 KB.

`MEM_REPLACE_PLACEHOLDER` requires an **exact-fit** placeholder: a 64 KB view into a 1 MB
placeholder fails with **err 487 `ERROR_INVALID_ADDRESS`**. Split first, then replace.

### 4.7 Mixed layout and per-page protection

Three adjacent 64 KB sub-ranges of one placeholder were populated as
file view (`R/MAPPED`) | private commit (`RW/PRIVATE`) | file view at file offset 64 K
(`R/MAPPED`), each landing at its requested address with correct content. A **single 4 KB page
inside a 64 KB file view** was then set to `PAGE_NOACCESS` while its neighbours stayed `R`.

### 4.8 `UnmapViewOfFile2` flags

`UnmapViewOfFile2(proc, view, MEM_PRESERVE_PLACEHOLDER)` → `ok=1`, and the range reverted to
`RESERVE/PRIVATE`, i.e. back to being a placeholder; a fresh `MapViewOfFile3` into it succeeded.
`UnmapViewOfFile2(proc, view, 0)` → `ok=1` and the range became `FREE` — the address is handed
back to the OS and is no longer owned by the instance.

### 4.9 Commit charge of views (p4 Q4.10, p11)

| view | Δ commit at map | Δ commit after dirtying every page | file modified |
|---|---|---|---|
| `PAGE_READONLY`, 4 MB file view | **16 384 B** (page tables only) | 16 384 B | – |
| `PAGE_EXECUTE_READ`, 4 MB file view | **8 KB** | – | – |
| `PAGE_WRITECOPY`, 4 MB file view | **4 210 688 B (= 4 MB + PTs)** | +0 further | **no (private COW)** |

This is an important asymmetry: **shared file views cost essentially zero commit; copy-on-write
views are charged their full size at map time**, before anything is written. 16 concurrent
8 MB views of the *same* file (128 MB of VA, all pages touched) cost **0.254 MB of commit** and
128.004 MB of working set — physical pages are shared through the file cache, only the PTEs are
per-mapping.

### 4.10 Timings (p4, Q4.13)

| operation | cost |
|---|---|
| placeholder split (`MEM_RELEASE\|MEM_PRESERVE_PLACEHOLDER`) | **1047 ns** |
| `MapViewOfFile3(MEM_REPLACE_PLACEHOLDER)` | **979 ns** |
| `UnmapViewOfFile2(MEM_PRESERVE_PLACEHOLDER)` | **1244 ns** |

About 3 µs per mapped ELF segment, so a 24-segment 8-library load costs ~72 µs of VM
bookkeeping. p10 measured the real thing at **6971 ns per segment** including the protection
flip, i.e. 167 µs for 24 segments.

### 4.11 Exhaustive confirmation (p5b) — this is the load-bearing claim, so it was hammered

| test | result |
|---|---|
| **512** consecutive 4 KB-step file offsets (0 … 2 MB−4 K), placeholder path, content verified | **512 mapped ok, 0 failed, 512 content correct, 0 wrong** |
| 64 consecutive 4 KB-step offsets, **NULL-base** path (no placeholder) | **4 ok** (0, 64 K, 128 K, 192 K), **60 failed**, all `err 1132` |
| sub-page offsets 1, 512, 1024, 2048, 4095, 4097, placeholder path | **all FAIL `err 1132`** |
| 16 combinations of 16 KB-aligned base × 16 KB-aligned offset, 16 KB views | **all ok, content verified** |

**Verdict on Q4: `VirtualAlloc2`, `MapViewOfFile3` and `UnmapViewOfFile2` are available (from
`kernelbase.dll`, not `kernel32.dll`) and give real `mmap(MAP_FIXED)` semantics. When — and only
when — the view replaces a placeholder, BOTH the base address AND the file offset are
constrained to `dwPageSize` = 4 KB, not to `dwAllocationGranularity` = 64 KB. Sub-page offsets
are rejected with `ERROR_MAPPED_ALIGNMENT`. Guest ELF segments at 4 KB or 16 KB alignment can
therefore be file-backed.**

---

## 5. File-backed mapping of APK contents

**Measured result: `MapViewOfFile3` into a placeholder accepts any 4 KB-aligned file offset, so
a STORED `.so` inside a `zipalign`ed APK can be mapped zero-copy at the guest's chosen address.
An unaligned entry cannot be mapped at all and must be copied.**

### 5.A/5.B The 64 KB rule is specific to the non-placeholder paths (p5)

| API / path | offset 0 | 4096 | 8192 | 16384 | 32768 | 65536 | 69632 |
|---|---|---|---|---|---|---|---|
| classic `MapViewOfFile` | ok | **FAIL 1132** | FAIL | FAIL | FAIL | ok | **FAIL 1132** |
| `MapViewOfFile3`, `BaseAddress = NULL` | ok | **FAIL 1132** | FAIL | FAIL | FAIL | ok | **FAIL 1132** |
| `MapViewOfFile3` + `MEM_REPLACE_PLACEHOLDER` | ok | **ok** | **ok** | **ok** | **ok** | ok | **ok** |

Same function, same offset; the only difference is whether the view replaces a placeholder.
This is the single finding that decides the ELF-loading strategy.

### 5.D Realistic `.so`-in-APK scenario (p5)

Three PT_LOAD segments (`R`, `RX`, `RW`-COW) mapped at chosen guest addresses directly out of
the APK file:

| APK layout | `.so` file offset | result |
|---|---|---|
| `zipalign 4` (4 KB-aligned STORED entry) | 1 048 576 | **all 3 segments ok — direct zero-copy mapping POSSIBLE** |
| `zipalign -P 16` (16 KB-aligned STORED entry) | 2 113 536 | **all 3 segments ok — POSSIBLE** |
| no `zipalign` / DEFLATEd (arbitrary offset) | 4 195 538 | all 3 segments **FAIL err 1132 — IMPOSSIBLE** |

Content was verified at every mapped segment start.

### 5.E Cost of the fallback (p5, best of 5 runs, 4 MB `.so`, file warm in cache)

| fallback step | cost |
|---|---|
| `ReadFile` of 4 MB from the OS cache into a fresh buffer | **1.012 ms (4.1 GB/s)** |
| `memcpy` of 4 MB from an already-mapped warm view into private commit | 1.331 ms (3.2 GB/s) |
| commit + first-touch 4 MB of private memory (unavoidable floor) | 0.382 ms |

So the fallback costs **~1 ms and ~4 MB of permanent commit charge per 4 MB of `.so`**, versus
~3 µs and ~0 commit charge for a direct mapping. For a game with 200 MB of native libraries
that is **~50 ms of load time and 200 MB of commit charge per instance** — which, against a
46.84 GB commit limit shared with the rest of Windows, is the difference between ~30 and
~200 instances.

### 5.F Section/view protection constraints for the ELF loader (p10 §0, p11)

Mapping guest `.text` executable straight from the APK has extra prerequisites that were
measured, not assumed:

| step | result |
|---|---|
| `CreateFileW(GENERIC_READ)` then `CreateFileMapping(PAGE_EXECUTE_READ)` | **FAIL err 6** — the file handle must carry `GENERIC_EXECUTE` |
| `CreateFileW(GENERIC_READ\|GENERIC_EXECUTE)` then `CreateFileMapping(PAGE_EXECUTE_READ)` | ok |
| `VirtualProtect` an `R` view of a `PAGE_READONLY` section up to `RX` | **FAIL err 87** — the section caps maximum protection |

View protections accepted when mapping into a placeholder (p11):

| view protection | from a `PAGE_READONLY` section | from a `PAGE_EXECUTE_READ` section | Δ commit at map (4 MB view) |
|---|---|---|---|
| `PAGE_READONLY` | ok | ok | 8 KB |
| `PAGE_READWRITE` | FAIL 5 | FAIL 5 | – |
| `PAGE_WRITECOPY` | ok | ok | **4.008 MB** |
| `PAGE_EXECUTE_READ` | FAIL 5 | **ok** | 8 KB |
| `PAGE_EXECUTE_WRITECOPY` | FAIL 5 | FAIL 5 | – |
| `PAGE_EXECUTE_READWRITE` | FAIL 5 | FAIL 5 | – |

`VirtualProtect` transitions available on a single 4 KB page of a view of a `PAGE_EXECUTE_READ`
section: → `R` ok, → `WRITECOPY` **ok**, → `RX` ok, → `EXECUTE_WRITECOPY` **ok**, → `NOACCESS`
ok; → `RW` and → `RWX` fail with err 87. So relocations can be applied to file-backed `.text`
by flipping the affected pages to `EXECUTE_WRITECOPY`/`WRITECOPY`, writing (which privatises
just those pages), and flipping back to `RX` — verified working in p10
(`RX->WRITECOPY ok=1 err=0, back to RX ok=1`).

**Verdict on Q5: Omnidroid CAN map `.so` segments zero-copy directly out of the APK, provided
the entry is STORED and its payload begins at a 4 KB-aligned file offset — which is exactly
what `zipalign 4` (and `zipalign -P 16`) guarantees, and which is mandatory for Android's own
loader anyway. The APK must be opened with `GENERIC_READ|GENERIC_EXECUTE` and the section
created `PAGE_EXECUTE_READ`. For DEFLATEd or unaligned entries, direct mapping is impossible
(`ERROR_MAPPED_ALIGNMENT`) and the fallback costs ~1 ms and ~4 MB of permanent commit per 4 MB
— extract once to a 4 KB-aligned cache file on disk and map that instead of copying into
private memory on every launch.**

---

## 6. Many instances

**Measured result: reservations are per-process address space only; they impose no system-wide
cost and there is no system-wide limit on them. 64 processes holding 1 TB of reserved guest
address space between them cost 240.8 MB of system commit in total.**

### 6.A Multi-process (p6, `parent` mode; each child reserves a placeholder, commits and
touches a working set, then idles)

| instances | reserve each | touched each | total guest VA | Δ system commit | per instance |
|---|---|---|---|---|---|
| 32 | 16 GB | 64 MB | 512 GB | 3577.2 MB | **111.79 MB** |
| **64** | 16 GB | 8 MB | **1024 GB** | **240.8 MB** | **3.76 MB** |
| 16 | 64 GB | 0 MB | 1024 GB | 661.1 MB | 41.32 MB |

All children reported success in every run (`children reported ok: 32` / `64` / `16`). System
commit before/after for the 64-instance run: 37158.4 → 37399.1 MB against a 47963.7 MB limit,
i.e. **1 TB of guest address space consumed 0.5 % of the commit limit**. Note the 3.76 MB per
instance is dominated by the Rust process image and thread stacks, not by the reservation; the
16 × 64 GB row (41.32 MB/instance with *nothing* touched) shows how noisy system-wide commit is
when other processes are running — treat these figures as upper bounds. The post-kill figures
(`returned 4093.8 MB` / `469.3 MB` / `−71.8 MB`) confirm that noise: system commit is a
whole-machine counter, so single-digit-MB precision is not available from it. Per-process
`PrivateUsage` (§2, §7) is the trustworthy counter.

For contrast, the same probe computed what a committing design would need: **at 16 GB committed
per instance the maximum instance count on this machine is 0** (9950 MB of commit was available
against 16 384 MB needed for one instance). A single committing instance would not fit.

### 6.B Single address space (p6, `vaspace` mode)

| placeholder size | how many succeeded | total VA | process commit | process WS |
|---|---|---|---|---|
| 4 GB | **32 763** | **127.98 TB** | 0.910 MB | 4.477 MB |
| 16 GB | **8 188** | 127.94 TB | 0.910 MB | 4.531 MB |
| 64 GB | **2 045** | 127.81 TB | 0.910 MB | 4.531 MB |

Each run terminated with `err = 8 ERROR_NOT_ENOUGH_MEMORY` — i.e. it ran out of *address space*,
not commit. The 128 TB user VA is the only limit, and it is effectively unlimited for this
purpose: **~32 700 four-GB guest address spaces, or ~8 100 sixteen-GB ones, fit in one
process**, for under 1 MB of commit.

### 6.C Region-count limit (p6, Q6.C)

**300 000** separate 64 KB reservations were created in one process in **162.8 ms**, costing
4.012 MB of process commit (**14.0 bytes per reservation**) and 6.926 MB of working set; the run
stopped at the self-imposed 300 000 cap, not at a failure. There is no practical ceiling on the
number of sub-regions (mapped segments, guarded pages, heap chunks) per instance.

**Trade-off.** Multi-process gives true isolation (a guest wild write cannot corrupt another
instance or the host runtime; a crash kills one instance; per-process working-set and CPU limits
via job objects apply) at the cost of ~4–40 MB of host overhead per instance and IPC for shared
services. Single-process multi-instance gives ~0 additional overhead and trivially cheap sharing
of JIT code and host GPU state, but **no memory isolation whatsoever** — any guest can reach any
other instance's memory, and one wild write takes down all of them. Since Omnidroid runs
untrusted third-party ARM64 code with no VM boundary, and since the multi-process cost was
measured at 3.76 MB/instance, isolation is nearly free and should be taken.

**Verdict on Q6: no system-wide reservation limit; 64 processes × 16 GB reserved = 1 TB of guest
VA for 240.8 MB of system commit; ~32 700 × 4 GB reservations fit in a single 128 TB address
space for 0.91 MB of commit. Use one process per instance.**

---

## 7. Guard pages and fault handling

**Measured result: self-managed demand paging works and is 100 % reliable, but it costs 2053 ns
per fault versus 398 ns for a kernel-handled soft fault — 5.2x. Viable as a rare/cold path,
must not be a hot path.**

`AddVectoredExceptionHandler(1, handler)` installed successfully. The handler filters on
`ExceptionInformation[1]` (faulting address) against the guest range and returns
`EXCEPTION_CONTINUE_EXECUTION` after committing.

### 7.1 `MEM_RESERVE` + access violation + commit-in-handler + resume (65 536 pages, 256 MB)

| metric | value |
|---|---|
| pages faulted | **65 536 of 65 536** |
| write faults / read faults | 65 536 / 0 (`ExceptionInformation[0]` distinguishes them correctly) |
| **incorrect values after resume** | **0** |
| total time | 134.55 ms → **2053 ns per self-handled fault** |
| Δ commit / Δ WS | 256.50 MB / 256.02 MB |

**Plain `MEM_RESERVE` + access violation + commit-in-handler + resume works reliably on
Windows**: every one of 65 536 faulting stores completed correctly after the handler committed
the page and resumed. This is the mechanism Omnidroid needs for lazy guest paging.

### 7.2 Baseline

Pre-committed first touch: 26.11 ms for 65 536 pages = **398 ns/page**. The self-handled fault
is **5.2x** the kernel path.

### 7.3 `PAGE_GUARD`

A 256 MB `PAGE_READWRITE|PAGE_GUARD` region produced **65 536 guard faults of 65 536** at
**1430 ns per fault**, data intact afterwards. A second pass produced **0 faults**
(845 µs, 12.9 ns/page), confirming `PAGE_GUARD` is **one-shot per page** — the kernel clears the
guard bit when it delivers `STATUS_GUARD_PAGE_VIOLATION`. Good for "tell me the first time this
page is touched" (stack-growth probes, first-touch instrumentation); useless as a persistent
barrier unless re-armed.

### 7.4 Re-armable barrier via `VirtualProtect` (write barriers / dirty tracking)

| operation | cost |
|---|---|
| 16 384 × `VirtualProtect` RW→R, one page each | **375 ns each** |
| 16 384 × `VirtualProtect` R→RW, one page each | **246 ns each** |
| 1 × `VirtualProtect` of the whole 64 MB | 324.5 µs = **19.8 ns per page covered** |

Re-arming in bulk is ~19x cheaper per page than per page. Any dirty-tracking scheme must batch.

### 7.5 Scalability of self-handled faults

| threads | faults | wall time | ns/fault (wall) | throughput |
|---|---|---|---|---|
| 1 | 8 192 | 18.11 ms | 2210 | 0.45 M/s |
| 4 | 32 768 | 25.44 ms | 776 | **1.29 M/s** |
| 8 | 65 536 | 52.17 ms | 796 | 1.26 M/s |

Throughput saturates at ~1.3 M faults/s around 4 threads — the kernel's per-process address
space lock, not the handler, is the bottleneck. At 2053 ns each, faulting in a 500 MB working
set one page at a time would cost **~0.26 s** of pure fault overhead.

**Verdict on Q7: technically viable and reliable, but do not use it as the primary paging
mechanism.** Use bulk `MEM_COMMIT` on coarse regions (bulk commit is 3 ns/page) and reserve
VEH-based demand paging for cases where laziness is semantically required (guest `mmap` with no
backing, sparse guest heaps, stack guard growth) or where the region is genuinely cold. When
lazily committing, commit a **64 KB–1 MB block** around the faulting address rather than a
single page: this amortises the 2053 ns over 16–256 pages and brings the effective per-page cost
below the 398 ns kernel path.

---

## 8. Large pages and performance

**Measured result: 2 MB large pages are NOT available. `SeLockMemoryPrivilege` is not granted to
this account.**

| check | result |
|---|---|
| `GetLargePageMinimum()` | **2 097 152 (2 MB)** |
| `AdjustTokenPrivileges(SeLockMemoryPrivilege)` | call succeeded but **`err = 1300 ERROR_NOT_ALL_ASSIGNED`** → the privilege is not in the token at all |
| `VirtualAlloc(2 MB, MEM_RESERVE\|MEM_COMMIT\|MEM_LARGE_PAGES, RW)` | **FAIL `err = 1314 ERROR_PRIVILEGE_NOT_HELD`** |

Enabling it requires an administrator to grant "Lock pages in memory" in Local Security Policy
and a logoff/logon. It also **locks the pages in physical RAM permanently** (large pages are
never pageable), which directly contradicts the "reclaimable, demand-driven" requirement.

TLB cost that large pages would have addressed, measured with 4 KB pages (p8, Q8.3, dependent
random pointer chase, one node per page, 2 M dependent accesses):

| working set | ns per dependent access |
|---|---|
| 512 MB (131 072 pages) | **108.5 ns** |
| 2 MB (512 pages, TLB-resident) | **5.0 ns** |
| → page-walk/TLB-miss penalty | **103.5 ns/access (21.9x)** |

The large-page counterpart could not be measured (no privilege) — **UNVERIFIED**. The 21.9x
figure is an upper bound on what 2 MB pages could recover for a pathological
one-access-per-page pattern; real guest code with spatial locality will see far less.

**Verdict on Q8: do not depend on large pages.** They are unavailable by default, they require
an admin policy change per machine, and they are non-pageable, which conflicts with requirement
#1. Treat them as an optional expert tuning knob for the JIT code cache only, never as part of
the guest-memory design.

---

## 9. W^X and JIT memory

**Measured result: dual mapping is 14x faster than flipping `VirtualProtect`, needs no
instruction-cache flush on x86-64, and was bit-exact across 200 000 trials. Use dual mapping.**

### 9.1 Single mapping flipped with `VirtualProtect`

A stub `mov eax, imm32; ret` was written, flipped to `PAGE_EXECUTE_READ`, and called — returned
42 as expected. Flip costs:

| region flipped | RW→RX→RW round trip | per single flip |
|---|---|---|
| 4 KB | 1749 ns | **874 ns** |
| 64 KB | 2000 ns | 1000 ns |
| 1 MB | 5029 ns | 2514 ns |

Full realistic publish cycle (emit stub + RW→RX + call + RX→RW), 20 000 iterations:
**2259 ns each**.

### 9.2 Ceiling: `PAGE_EXECUTE_READWRITE` (no W^X)

`VirtualAlloc(PAGE_EXECUTE_READWRITE)` **succeeded** on this host (no ACG/CFG blocking it).
Emit + call on the same RWX page: **168 ns each**. This is the speed ceiling — and it is what
W^X exists to prevent.

### 9.3 Dual mapping via a pagefile-backed section + placeholders

`CreateFileMappingW(INVALID_HANDLE_VALUE, PAGE_EXECUTE_READWRITE | SEC_COMMIT, 16 MB)`
succeeded. Two views of that one section were mapped into two placeholder sub-ranges 512 MB
apart: `PAGE_READWRITE` at `0x1e480000000` and `PAGE_EXECUTE_READ` at `0x1e4a0000000`.
`VirtualQuery` confirmed `RW/MAPPED` and `RX/MAPPED`. The constant delta
**`RX = RW + 0x20000000`** can be folded into the JIT as a compile-time constant.

| measurement | value |
|---|---|
| write via RW view, call via RX view (no protect change, no flush) | returned **4242** as expected |
| 200 000 × (emit through RW + call through RX), no flush | **162 ns each**, **mismatches = 0** |
| same across 4 096 distinct pages, round-robin | **178 ns each**, **mismatches = 0** |
| `FlushInstructionCache(4 KB)` | **4 ns** (effectively a no-op on x86-64, but not free) |

**No instruction-cache or pipeline flush was needed in 200 000 trials**, including the
round-robin variant designed to defeat any single-page caching effect. x86-64 guarantees
i-cache coherence with stores; the only requirement is that the store retires before the jump,
which the dependency chain already ensures. (An ARM64 *host* would require
`FlushInstructionCache`; this codebase targets x86-64 hosts only, so the 4 ns call can be
omitted — or kept, as it is nearly free, for portability.)

**Dual mapping is 162 ns per publish versus 2259 ns for the `VirtualProtect` cycle: a 13.9x
improvement, and within 4 % of the RWX ceiling (168 ns) while preserving W^X.**

### 9.4 Accounting

The pagefile-backed section charged **16.000 MB of system commit at `CreateFileMapping` time**
(process `PrivateUsage` delta was 0.000 MB — section commit is charged system-wide, not to the
process's private usage). Dirtying all 16 MB through the RW view added 0.000 MB further — it had
already been charged. A byte written via RW read back as `0x90` through the RX view, proving the
two views **share** pages rather than copy-on-write. Unmapping both views and closing the
section returned 11.492 MB of system commit and 31.953 MB of working set (16 MB × 2 views of
PTEs); the shortfall versus 16 MB is system-commit measurement noise from other processes.

### 9.5 Reclaiming cold JIT code

`MEM_DECOMMIT` of 32 MB of `PAGE_EXECUTE_READ` **private** code: `ok=1`,
**Δcommit = −32.062 MB, ΔWS = −32.000 MB**, range reverted to `RESERVE`. Executable private
memory is reclaimable exactly like data.

**Verdict on Q9: use dual mapping — one pagefile-backed section per instance's code cache, an
RW view and an RX view at a fixed constant offset, both carved from the instance placeholder.
162 ns per emit-and-execute cycle, no flush, no `VirtualProtect` in the hot path.** Keep
`VirtualProtect` only for coarse, rare operations. Note the trade-off: the RW view is a
permanent writable alias of executable memory, which weakens W^X against an attacker who already
has arbitrary-write. Mitigate by keeping the RW view's base in a register-allocated/secret
location rather than a global, and by sizing the section to the code cache rather than mapping
all of it — do not present this as equivalent to hardware-enforced W^X.

---

## Recommended memory model for Omnidroid

### Layout: one placeholder per instance, one process per instance

```
  one Omnidroid process = one guest instance
  └── VirtualAlloc2(NULL, 4 GB, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS)
      = the guest's entire ARM64 address space, cost: 0 bytes of commit
      │
      ├── [ 0 .. 512 MB )   guest low / null-guard / reserved, never mapped
      ├── [ 512 MB .. 2 GB ) guest linker area
      │     per .so, per PT_LOAD segment:
      │       VirtualFree(seg_addr, seg_len, MEM_RELEASE|MEM_PRESERVE_PLACEHOLDER)   (~1047 ns)
      │       MapViewOfFile3(apk_section, .., seg_addr, apk_off + seg_off, seg_len,
      │                      MEM_REPLACE_PLACEHOLDER, R | RX | WRITECOPY)            (~979 ns)
      │     -> zero-copy, ~0 commit charge, 4 KB-granular address AND file offset
      │
      ├── [ 2 GB .. 3 GB )   guest heap window
      │     VirtualAlloc2(heap_base, 1 GB, MEM_RESERVE|MEM_REPLACE_PLACEHOLDER, NOACCESS)
      │     then grow with VirtualAlloc(MEM_COMMIT) in >=1 MB blocks   (3 ns/page bulk)
      │     shrink with VirtualFree(MEM_DECOMMIT)                      (210.8 ns/page)
      │
      ├── [ 3 GB .. 3.5 GB ) guest thread stacks: one sub-VAD each, PAGE_GUARD sentinel page
      │
      └── [ 3.5 GB .. 4 GB ) JIT code cache
            CreateFileMappingW(INVALID_HANDLE_VALUE, PAGE_EXECUTE_READWRITE|SEC_COMMIT, N MB)
            MapViewOfFile3(sec, .., rw_base, 0, N, MEM_REPLACE_PLACEHOLDER, PAGE_READWRITE)
            MapViewOfFile3(sec, .., rx_base, 0, N, MEM_REPLACE_PLACEHOLDER, PAGE_EXECUTE_READ)
            rx_base - rw_base = compile-time constant; emit via RW, execute via RX (162 ns)
```

### APIs to use

| purpose | API | why |
|---|---|---|
| guest address space | `VirtualAlloc2(MEM_RESERVE\|MEM_RESERVE_PLACEHOLDER)` | 0 commit, 4 KB-splittable, enables `MAP_FIXED` |
| carve a sub-range | `VirtualFree(MEM_RELEASE\|MEM_PRESERVE_PLACEHOLDER)` | splits at 4 KB granularity |
| map an ELF segment | `MapViewOfFile3(MEM_REPLACE_PLACEHOLDER)` | **the only path with 4 KB file-offset granularity** |
| guest anonymous memory | `VirtualAlloc2(MEM_RESERVE\|MEM_REPLACE_PLACEHOLDER)` then `VirtualAlloc(MEM_COMMIT)` | demand commit at 4 KB, bulk commit at 3 ns/page |
| guest `munmap` / `MADV_DONTNEED` | `VirtualFree(MEM_DECOMMIT)` | **the only primitive that returns commit charge** |
| guest `dlclose` | `UnmapViewOfFile2(MEM_PRESERVE_PLACEHOLDER)` | returns the range to placeholder state, address stays owned |
| guest `mprotect` | `VirtualProtect` | 4 KB granular, ~360 ns; batch where possible |
| idle-instance trim | `K32EmptyWorkingSet` / `SetProcessWorkingSetSizeEx` | sheds physical pages, keeps data (hard-faults back at 913 ns/page) |
| JIT | dual-mapped pagefile section (§9.3) | 162 ns publish, no flush |
| lazy/cold paging | `AddVectoredExceptionHandler` + commit-in-handler | reliable; 2053 ns/fault, so commit 64 KB–1 MB per fault |
| instance teardown | unmap every view, then `VirtualFree(base, 0, MEM_RELEASE)` | see constraint below |

Resolve `VirtualAlloc2` / `MapViewOfFile3` / `UnmapViewOfFile2` with `GetProcAddress` on
`kernelbase.dll` at startup — they are **not** exported from `kernel32.dll`.

### How reclamation works

1. Guest `munmap`/`free` of anonymous memory → `VirtualFree(MEM_DECOMMIT)` on the page range.
   Commit charge and working set both drop immediately; the address stays `RESERVE` and owned by
   the instance; a later touch gets zero-filled memory, matching Linux.
2. Guest `dlclose` → `UnmapViewOfFile2(view, MEM_PRESERVE_PLACEHOLDER)`. The placeholder comes
   back and can be re-used for a different library at the same guest address (verified in p10 §5).
3. Instance goes idle/background → `K32EmptyWorkingSet` to release physical pages while keeping
   commit and data. Measured: 3078.668 MB commit / 3076.730 MB WS → after decommit to the live
   set 513.656 / 516.730 → after `EmptyWorkingSet` 513.656 / **0.148** MB.
4. Under host memory pressure, the runtime should decommit its own caches (translation caches,
   texture staging) with `MEM_DECOMMIT` — **not** `MEM_RESET`, **not** `DiscardVirtualMemory`,
   **not** `OfferVirtualMemory`, none of which return commit charge.
5. Instance exit → unmap all views, then one `MEM_RELEASE` of the placeholder base. Measured at
   **43.1 µs** for a 4 GB / 24-view instance.

### Measured footprint of the recommended design (p10, end to end)

A real instance was built: 4 GB placeholder, 8 libraries × 3 PT_LOAD segments (24 file views,
9.53 MB) mapped zero-copy at 4 KB-aligned APK offsets, a 1 GB heap window grown to 256 MB and
then shrunk to 36 MB.

| metric | value |
|---|---|
| guest address space reserved | 4 GB |
| commit charge for all 24 mapped library segments | **0.566 MB** |
| commit charge for the 1 GB heap **reservation** | **0 bytes** |
| heap grown to 256 MB | Δcommit +256.508 MB, ΔWS +256.000 MB |
| heap shrunk to 32 MB via `MEM_DECOMMIT` | **Δcommit −224.438 MB**, ΔWS −223.992 MB |
| re-commit into the decommitted range | ok, reads back `0x00` (zero-filled, as the guest expects) |
| **total process commit charge** | **37.250 MB** |
| **total process working set** | **36.457 MB** |
| content verification across all 24 segments | **0 wrong** |
| teardown | 43.1 µs |

A 4 GB guest address space with a real library set costs **37 MB of commit** — 0.08 % of the
commit limit. The same design with a committed 4 GB region would cost 4 GB, i.e. 8.5 % of the
limit, and 11 instances would exhaust the machine.

---

## Constraints the rest of the design must respect

1. **Never `MEM_COMMIT` a guest region larger than what the guest has actually asked for.**
   Commit charge is debited at `MEM_COMMIT`, not at first touch (§2.B). One 4 GB committed region
   = 8.5 % of this machine's commit limit, touched or not.
2. **`MEM_DECOMMIT` (or `MEM_RELEASE`) is the only way to give commit charge back.** `MEM_RESET`,
   `MEM_RESET_UNDO`, `DiscardVirtualMemory`, `OfferVirtualMemory` and `EmptyWorkingSet` all
   measured **0.00 MB** change in commit charge (§3). Any "free memory" path in the runtime must
   end in `MEM_DECOMMIT`.
3. **File views must replace a placeholder, or the 64 KB file-offset rule applies.** The same
   `MapViewOfFile3` call fails at offset 4096 with `ERROR_MAPPED_ALIGNMENT` when
   `BaseAddress = NULL` and succeeds when it replaces a placeholder (§5.A/B, 512/512 vs 4/64).
   The ELF loader must always pre-carve a placeholder.
4. **`MEM_REPLACE_PLACEHOLDER` requires an exact-size placeholder.** Size mismatch →
   `err 487 ERROR_INVALID_ADDRESS`. Split to the exact segment length first.
5. **File offsets and view sizes must be 4 KB multiples; sub-page offsets are impossible.** So
   the APK must be `zipalign`ed (4 KB or 16 KB) and the `.so` entries STORED. DEFLATEd entries
   can never be mapped directly — plan an extract-to-aligned-cache-file step, not a
   copy-into-private-memory step, or pay ~1 ms and ~4 MB of permanent commit per 4 MB (§5.E).
6. **To map guest `.text` executable, open the APK with `GENERIC_READ|GENERIC_EXECUTE` and create
   the section `PAGE_EXECUTE_READ`.** A `PAGE_READONLY` section caps view protection permanently:
   `VirtualProtect` R→RX fails with `err 87` (§5.F). This decision is made at
   `CreateFileMapping` time and cannot be revised later.
7. **Views of a file section cannot be `PAGE_READWRITE` or `PAGE_EXECUTE_READWRITE`** (`err 5`).
   Writable guest segments must be `PAGE_WRITECOPY`, and `VirtualProtect` on such a view can
   reach `R`, `WRITECOPY`, `RX`, `EXECUTE_WRITECOPY`, `NOACCESS` — never `RW` or `RWX` (§5.F).
   Relocations on file-backed `.text` go RX → `EXECUTE_WRITECOPY` → write → RX.
8. **`PAGE_WRITECOPY` views are charged their full size in commit at map time** (4.008 MB for a
   4 MB view, §4.9). Keep COW views to actual writable segments; never COW-map a whole `.so`.
9. **`MEM_RELEASE` of a placeholder fails while any view is still mapped into it**
   (`ok=0 err=87`, p10 §8). Instance teardown must unmap every view first, then release. Track
   every live view per instance.
10. **Reserved address space, not commit, is what a placeholder consumes — and there is plenty.**
    128 TB / ~32 700 four-GB instances per process (§6.B). Address-space frugality is not a design
    concern; commit frugality is the only one.
11. **Page tables cost 1/511 of committed VA, and up to 3x for sparse patterns** (§2.E). A guest
    allocator that scatters single pages across a wide range doubles or triples its real commit
    cost. Keep guest allocations clustered; prefer ≥64 KB commit granules.
12. **Self-handled faults cost 2053 ns and saturate at ~1.3 M/s process-wide** (§7). Demand
    paging via VEH must commit a 64 KB–1 MB block per fault, and nothing on a hot path may rely
    on it. Bulk `MEM_COMMIT` is 3 ns/page — prefer it whenever the size is known.
13. **`PAGE_GUARD` is one-shot per page** (§7.3, second pass = 0 faults). Any use as a barrier
    must re-arm, and re-arming must be batched (19.8 ns/page in bulk vs 375 ns/page individually).
14. **Do not design for 2 MB large pages.** Unavailable without an admin policy change
    (`err 1314`), and non-pageable, which contradicts requirement #1 (§8).
15. **The JIT must not use `VirtualProtect` per publish.** 2259 ns vs 162 ns for dual mapping
      (§9). The JIT's code-cache design must accommodate two views at a fixed offset from the
      start; retrofitting it later means changing every code-address computation.
16. **Pagefile-backed sections charge their full size in system commit at creation**
    (16.000 MB for a 16 MB section, §9.4). Size the JIT code cache to what is needed and grow it
    with additional sections, rather than reserving one huge section per instance.
17. **System-wide commit (`PERFORMANCE_INFORMATION.CommitTotal`) is too noisy for fine
    measurement** — it moved by tens of MB from unrelated processes during these runs, and one
    teardown measurement even came back negative (§6.A). Regression tests for memory behaviour
    must assert on per-process `PrivateUsage`, not on system commit.
18. **`VirtualAlloc2`, `MapViewOfFile3` and `UnmapViewOfFile2` are not in `kernel32.dll`** (§4.0).
    Resolve them dynamically at startup and fail fast with a clear message on Windows builds older
    than 10 1803. Whether `windows-sys` links them correctly is **UNVERIFIED** here; if that crate
    is adopted, verify the link explicitly before relying on it.

### Things that could not be measured

- **Large-page throughput**: no `SeLockMemoryPrivilege`, so no 2 MB-page counterpart to the
  108.5 ns/access figure. **UNVERIFIED.**
- **`windows-sys` linkage of the placeholder APIs**: hand-rolled bindings were used throughout.
  **UNVERIFIED.**
- **Behaviour under genuine commit exhaustion** (commit charge at 100 % of the limit): not
  induced, because doing so on the live host risks destabilising it. How gracefully
  `MEM_COMMIT`/`MapViewOfFile3` fail at the limit, and whether Windows expands the pagefile
  first, is **UNVERIFIED**. The design must handle `MEM_COMMIT` returning NULL at any time.
- **Real Roblox / real APK measurements**: all file-mapping tests used synthetic files with
  verifiable per-page tags. The alignment and commit results are properties of the OS, not of the
  file, so they transfer; the ~200 MB library-set extrapolation in §5.E does not and should be
  re-measured against a real APK.
