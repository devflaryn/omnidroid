# Plan: Omnidroid foundation (milestones M0 and M1)

Spec: `docs/ARCHITECTURE.md`. Rationale: `docs/DECISIONS.md`. Measurements: `docs/research/`.

Scope: everything needed to reach **M1** — the real `libroblox.so` loaded into memory with all
568,272 APS2 relocations applied and its 565 imports accounted for. Deliberately excludes the CPU
backend (decision D5 open) and the JNI layer (decision D7 open), so no task here depends on either.

## Global Constraints

These bind every task. A reviewer should treat a violation as a defect.

1. **No fabricated implementations and no placeholder success.** A function that cannot do its job
   returns an error. Never log or return success for work not done. No `todo!()` or
   `unimplemented!()` on any path a test claims to exercise.
2. **Tests use the real APK.** `Roblox-2.738.1397.apk` in the repo root is the test fixture. Tests
   that can assert against it must, using the exact numbers from `docs/research/apk-analysis.md`.
   Tests must **skip gracefully rather than fail** when the APK is absent, since it is git-ignored.
3. **Exact values are mandatory.** For `libroblox.so`:
   - The `DT_ANDROID_RELA` blob is **2,100,778 bytes**, magic `APS2`, and a correct decoder consumes
     **all** of it.
   - That blob declares and yields **568,272** relocations: **568,194** `R_AARCH64_RELATIVE` (1027)
     + **56** `R_AARCH64_GLOB_DAT` (1025) + **22** `R_AARCH64_ABS64` (**257**). Exactly **78** carry a
     non-zero `r_sym`. The type is `ABS64`, **not** `ABS32` (258) — an earlier draft had the wrong
     name, which would have written 4 bytes where 8 are required.
   - The dynamic tags are **`DT_ANDROID_RELA` = 0x60000011** and **`DT_ANDROID_RELASZ` = 0x60000012**.
     `0x6000000F`/`0x60000010` are `DT_ANDROID_REL`/`RELSZ`, a *different* pair this binary does not
     use; looking for those finds zero packed relocations.
   - Every `PT_LOAD` has **`p_align = 0x4000` (16 KiB)**, not 4 KiB.
   - `libroblox.so` has **only `DT_GNU_HASH`** — there is no `DT_HASH` to fall back on.
   - **Separately**, `.rela.plt` via `DT_JMPREL` holds **534** `R_AARCH64_JUMP_SLOT` (1026). These
     are **not** part of the APS2 blob. Grand total across both: **568,806**.
   - **3,594** `init_array` entries; `PT_GNU_RELRO` covers **5,205,568** bytes; the file is
     **109,193,800** bytes; **565** undefined symbols in this library alone.
   - Across all 11 `.so` under `lib/arm64-v8a/`, the **union** of undefined symbols is **669**.
     565 and 669 are both correct and not in conflict: 565 is `libroblox.so` alone, 669 is the union
     over all eleven libraries (which includes the injected one).
   - No library in the APK uses `DT_RELR` or `DT_ANDROID_REL`; the other ten use plain `DT_RELA`
     plus `DT_JMPREL`.

   A test asserting a rounded or approximate version of any of these is a defect.
4. **Platform code is confined.** `#[cfg(target_os)]` and OS APIs appear **only** in
   `omni-platform`. Every other crate must compile for all five targets without `cfg`. Do not add
   `windows-sys`, `libc`, or any OS-specific crate as a dependency of any crate other than
   `omni-platform`.
5. **`unsafe` is localized and justified.** Every `unsafe` block carries a comment stating the
   invariant that makes it sound. Public APIs are safe wherever that is possible.
6. **Commit charge is the scarce resource** (D10). Never commit memory speculatively. Only
   `MEM_DECOMMIT` and `MEM_RELEASE` return commit charge; `MEM_RESET`, `DiscardVirtualMemory`,
   `OfferVirtualMemory` and `EmptyWorkingSet` do **not**, and must never be used as if they did.
7. **Errors are typed and diagnostic.** Use `thiserror`. An error must say what failed and with
   which value. "Invalid ELF" is a defect; "invalid ELF: e_machine 62, expected 183 (EM_AARCH64)"
   is correct.
8. **No network access** at runtime. Dependencies resolve from crates.io at build time only.
9. **Stable Rust, edition 2021 or later.** No nightly features. `cargo build` and `cargo test` must
   pass, and leaving compiler warnings in our own crates is a defect.
10. **Document discoveries.** If implementation reveals that the research got something wrong or
    missed something, say so in the task report. Never silently work around a documented fact.
11. **Hostile input is the expected case, and you must test it yourself.** Decision D6 records that
    this project's own primary test APK is adversarially modified, so malformed and malicious input
    is normal input here. Every task so far — four for four — shipped a defect in this class that its
    own passing test suite could not see: two process **aborts** (a 98-byte file killed `Apk::open`),
    an arena overflow that wrapped past its own limit check in release, and a bound derived from
    unvalidated attacker-controlled data. Each was found only because a reviewer *constructed* hostile
    input rather than reading code.

    So, before you report done, construct and run hostile inputs against your own work, and put the
    results in your report. At minimum: a field that drives an allocation or a loop count set absurdly
    large; sizes and offsets at and past `usize::MAX`/`u64::MAX`; a length that overflows when added
    to a base or rounded up to alignment; zero-length and one-byte inputs; truncated structures; and a
    value that is internally inconsistent with another (a declared size disagreeing with an actual
    size). A panic or abort reachable from untrusted input is a **Critical** defect, not a robustness
    nicety — an abort in particular cannot be contained by any caller. Remember also that **a bound is
    only as trustworthy as its least-validated input**, and that **saturating arithmetic on a limit
    turns hostile input into a larger permission**, which is backwards.
12. **A test that cannot fail is worse than no test.** Prefer assertions that pin values against an
    independent source over ones that check a call succeeded. Where a test guards a specific bug,
    verify by mutation that reverting the fix actually makes that test fail, and say so in your report.
    Watch for the two directions: a test can fail to catch the bug, and it can also fail to catch a
    *fix that goes too far* — Task 2's copy-on-write work needed assertions in both directions, because
    the obvious over-broad fix would have passed every correctness test while silently destroying the
    multi-instance sharing property. Concrete precedents worth internalising: removing SLEB128 sign
    extension still passed byte-exact blob consumption *and* every per-type relocation count; and a
    `GROUPED_BY_ADDEND` test starting from addend 0 could not distinguish `+= delta` from `= delta`.

---

## Task 1: Cargo workspace and the `omni-platform` virtual-memory abstraction

Create the workspace, and implement virtual memory for Windows in the one crate allowed to touch
the OS.

### Workspace layout

Root `Cargo.toml` with `[workspace]`, `resolver = "2"`, members under `crates/`. Create all nine
crates from `ARCHITECTURE.md` section 2 as empty-but-compiling libraries — `omni-platform`,
`omni-mem`, `omni-apk`, `omni-elf`, `omni-cpu`, `omni-android`, `omni-gfx`, `omni-core`,
`omni-cli` — so later tasks have somewhere to land. Only `omni-platform` gets real code in this
task. Use `[workspace.package]` for shared metadata and `[workspace.dependencies]` for shared
versions. Set `[profile.release]` with `lto = "thin"`, `codegen-units = 1`.

### The abstraction

Define virtual-memory operations behind an explicit, documented seam. Whether that is a trait or a
`cfg`-selected module of free functions is the implementer's call, but the seam must be obvious and
the non-Windows backends must be substitutable at it.

Operations required:

- `reserve(size, align) -> Reservation` — address space only, **no commit**. Must support
  reservations far larger than physical RAM. Alignment honourable to at least 64 KB.
- `reserve_placeholder(size, align) -> Reservation` — a placeholder reservation suitable for later
  `MAP_FIXED`-style replacement.
- `split_placeholder(&Reservation, offset, size)` — split at **4 KB** granularity.
- `commit(ptr, size, protection)` — lazily commit inside a reservation.
- `decommit(ptr, size)` — return commit charge, keep the address reserved.
- `protect(ptr, size, protection)` — change protection on a 4 KB page range.
- `map_file(file, file_offset, size, ptr, protection)` — file-backed mapping replacing a
  placeholder, at **4 KB** file-offset granularity.
- `unmap(ptr, size)` — preserving the placeholder where applicable.
- `release(Reservation)`.
- `page_size()` and `allocation_granularity()`.
- `process_commit_charge()` and `process_working_set()` — required so tests can assert on commit
  charge rather than assume it (Global Constraint 6).

`Protection` is an enum covering at least `None`, `Read`, `ReadWrite`, `ReadExecute`. A
`ReadWriteExecute` variant is **deliberately absent** per D12 — we never hold a W+X page. Document
that absence as intentional so nobody adds it as a convenience.

### Windows implementation notes (all measured, see D11)

- `VirtualAlloc2`, `MapViewOfFile3` and `UnmapViewOfFile2` are **not exported from `kernel32.dll`**.
  Resolve them from `kernelbase.dll` via `GetProcAddress` once, lazily, and cache them. If a symbol
  is missing, return a typed error — do not panic.
- Placeholder replacement requires an **exact-size** placeholder, otherwise it fails with error 487.
- A file that will be mapped executable must be opened `GENERIC_READ | GENERIC_EXECUTE` with the
  section created `PAGE_EXECUTE_READ`. Getting this wrong means `.text` can never be made
  executable afterwards, and the failure appears much later.
- A misaligned file offset fails with `ERROR_MAPPED_ALIGNMENT`; surface that clearly.

### Other platforms

Provide `mmap`-based Linux and macOS modules that compile. Do **not** use `compile_error!`, and do
**not** imply they work. Any path not genuinely implemented must return a typed
"unimplemented on this platform" error so a non-Windows build fails honestly and immediately rather
than misbehaving silently. State in your report that they are unverified.

### Tests

Assert measured behaviour, not merely absence of error:

- A 4 GB reservation adds **no** commit charge. Allow a small tolerance for page-table overhead;
  state the tolerance you chose and why.
- Committing 64 MB raises commit charge by approximately 64 MB, and decommitting returns it.
- A single 4 KB page inside a large reservation can be committed, protected, and decommitted
  independently.
- Placeholder split at 4 KB followed by a file-backed map at a 4 KB-aligned offset yields exactly
  the expected file bytes at the expected address. Build the fixture file inside the test.
- A file-backed map at a **misaligned** offset fails with a clear typed error.
- `page_size() == 4096` and `allocation_granularity() == 65536` on Windows.

### Report

The final shape of the seam; which `kernelbase` symbols were resolved; the commit-charge numbers
your tests actually observed; and anything about the abstraction that felt wrong for the
Linux/macOS backends, since that is design signal for later.

---

## Task 2: `omni-mem` guest address space manager

Build the per-instance guest address space on top of `omni-platform`. **No OS calls in this crate.**

### Responsibilities

- Own one large placeholder reservation per instance: the guest address space. Size is configurable.
  4 GB is a sensible default given it measured 37.25 MB of commit charge, but no type may assume
  4 GB.
- Track regions in a sorted, non-overlapping map from guest address range to region state
  (reserved / committed with protection / file-mapped with its backing identity). Address lookup
  must be better than linear; the guest will query constantly.
- `map_anonymous(addr_hint, size, protection) -> GuestAddr` and `map_file(...)`, both honouring a
  **fixed** address when one is demanded — this is what the ELF loader needs — and choosing one
  otherwise.
- `unmap(addr, size)` — decommit and mark reserved. Never release the outer reservation.
- `protect(addr, size, protection)`.
- **Lazy commit in granules** of 64 KB to 1 MB, never per page (D10: 3 ns/page for bulk commit
  versus 2053 ns per fault). Expose the granule as configuration and document the measured basis.
- `reclaim_idle()` — decommit regions the guest has released. Must use decommit; using `MEM_RESET`
  here is a defect, because it frees nothing.
- A region-enumeration query, shaped for later use by `/proc/self/maps` synthesis and
  `dl_iterate_phdr`.

### The JIT code arena

Same crate, its own type. A dual-mapped region: one RW view for emission and one RX view for
execution **of the same pages** (D12: 162 ns per cycle versus 2259 ns for protection flipping).
Expose `alloc(size) -> (write_ptr, exec_ptr)`. The API must make it impossible to obtain a single
pointer that is both writable and executable. The dual-mapping mechanism belongs in
`omni-platform` (a pagefile-backed section mapped twice on Windows); the policy belongs here.

### Tests

- Reserving a guest space costs no meaningful commit charge, asserted against a measurement.
- Map, write, read back, unmap, and assert commit charge returns.
- Fixed-address mapping lands exactly where demanded, and fails cleanly when the range is occupied.
- Region tracking handles overlapping and adjacent regions, including splitting a region by
  unmapping its middle.
- Growing to 1 GB of live mappings and then unmapping everything returns commit charge to near
  baseline. This is the D10 requirement expressed as a test, and it is the most important one here.
- JIT arena: write a tiny host-native function through the RW view, execute it through the RX view,
  and assert the two pointers differ. Gate on `target_arch = "x86_64"` and use a few bytes of real
  x86-64 that returns a constant, so the test genuinely exercises execution.

### Report

The region-tracking data structure and why; the commit granule and its justification; and the
measured commit-charge behaviour of the grow-then-release test.

---

## Task 3: `omni-apk` — APK reading and the extraction cache

Read the APK and produce 4 KB-aligned library files ready to be mapped. This is milestone **M0**.

### Responsibilities

- Parse the zip end-of-central-directory and central directory. Support Zip64. Expose per entry:
  name, compression method, compressed and uncompressed size, CRC-32, and the **absolute file
  offset of the entry payload** (which requires reading the local file header, since the central
  directory records the local-header offset, not the payload offset).
- Report, per entry, whether it is STORED and whether its payload offset is 4 KB-aligned — that
  combination is what decides direct mappability (D11). Our APK has none, but the check must exist
  because correctly aligned APKs are common and are the fast path.
- Read entries: STORED via direct slice, DEFLATED via inflate. Verify CRC-32 and reject on mismatch.
- **The extraction cache.** For each `lib/<abi>/*.so`, decompress once into a content-addressed
  cache file whose payload begins 4 KB-aligned:
  `<cache-root>/libs/<sha256-of-uncompressed-bytes>/<name>`.
  Keyed by content hash, not APK path (D11), so identical libraries from different APKs share an
  entry and a modified library can never collide with a stock one.
- Writes must be atomic: write to a temporary file then rename, so a concurrent or interrupted
  extraction can never leave a partial file that a later run maps and trusts. Multiple instances
  will race here.
- If a cache entry already exists, verify its size and reuse it without re-extracting.
- Expose the `AndroidManifest.xml` bytes and an asset-reading path. **Do not** implement binary XML
  decoding in this task — just expose the bytes.

### Explicitly out of scope

Signature verification. Note in the report that the supplied APK is signed by a non-Roblox key
(D6); do not build anything that validates or reports on signatures in this task.

### Tests, against the real APK

- Exactly **11** `.so` under `lib/arm64-v8a/`, and **no other** `lib/<abi>/` directory exists.
- `libroblox.so` uncompressed size is exactly **109,193,800** bytes.
- All 11 are DEFLATED, and **none** is 4 KB-aligned — so the direct-mapping predicate returns false
  for every one of them. This test pins down the fact that forced D11.
- CRC-32 verification passes for several entries including a large one, and a deliberately corrupted
  buffer is rejected.
- Extraction produces a cache file whose payload offset is 4 KB-aligned and whose sha256 matches the
  cache key.
- Extracting twice does not re-extract; the second call is observably a cache hit.
- `shaders_vulkan_mobile.pack` is present and **STORED** (14.7 MB) — the asset-mapping fast path
  depends on it.

All APK tests skip gracefully when the file is absent.

### Report

Which zip fields required reading local headers; the measured one-time extraction cost for
`libroblox.so`; whether any entry in the real APK turned out directly mappable; and the atomicity
strategy used for concurrent extraction.

---

## Task 4: `omni-elf` — ELF64 parsing and the APS2 packed-relocation decoder

Parse AArch64 ELF and decode Android packed relocations. **This is the highest-risk piece of the
whole foundation**: `libroblox.so` has no `DT_RELA` and no `DT_RELR`, so a wrong decoder applies
zero relocations and the failure appears much later as inexplicable crashes.

Parsing and decoding only — no mapping, no relocation application. That is Task 5.

### Responsibilities

- ELF64 header validation: reject anything that is not `ELFCLASS64`, `ELFDATA2LSB`, `ET_DYN`,
  `EM_AARCH64` (183), with the offending value named in the error.
- Program headers: `PT_LOAD` (with `p_offset`, `p_vaddr`, `p_filesz`, `p_memsz`, `p_flags`,
  `p_align`), `PT_DYNAMIC`, `PT_GNU_RELRO`, `PT_NOTE`, `PT_TLS`.
  **If a `PT_TLS` segment is ever found, return an error rather than ignoring it** — the research
  established this APK has none, and silently ignoring TLS would be a serious correctness bug in a
  binary that did use it.
- The dynamic section: `DT_NEEDED`, `DT_SONAME`, `DT_INIT`, `DT_FINI`, `DT_INIT_ARRAY(SZ)`,
  `DT_FINI_ARRAY(SZ)`, `DT_SYMTAB`, `DT_STRTAB`, `DT_HASH`, `DT_GNU_HASH`, `DT_PLTGOT`,
  `DT_JMPREL`, `DT_PLTRELSZ`, `DT_RELA(SZ/ENT)`, `DT_REL(SZ/ENT)`, `DT_RELR(SZ/ENT)`, and the
  Android tags **`DT_ANDROID_RELA` (0x60000011)** and **`DT_ANDROID_RELASZ` (0x60000012)**. Also
  accept the `DT_ANDROID_REL`/`RELSZ` pair (`0x6000000F`/`0x60000010`) for completeness, but note this
  binary uses the **RELA** pair; searching for the REL pair finds zero packed relocations.
- Dynamic symbol table and string table access; symbol lookup via both `DT_HASH` and `DT_GNU_HASH`.
- Enumerate undefined symbols (imports) and exported symbols, distinguishing `STT_FUNC` from
  `STT_OBJECT` — 10 of Roblox's imports are **data** objects, not functions, and conflating them
  produces a failure that names no symbol.
- `.note.android.ident` and `.note.gnu.property` parsing, reporting NDK version and any
  BTI/PAC/MTE feature bits found.

### The APS2 decoder

Format: the blob begins with the magic `APS2` (`0x41 0x50 0x53 0x32`), followed by SLEB128-encoded
values: a relocation count, an initial offset, then a series of **groups**. Each group carries a
group size, group flags, and — depending on the flags — a shared offset delta, a shared `r_info`,
and a shared addend, followed by per-relocation deltas for whatever the flags did not make shared.
Implement it from the format rather than from memory of it: decode SLEB128 correctly for negative
values, and treat a truncated or over-long stream as an error.

The decoder must expose the decoded relocations as `(r_offset, r_info, r_addend)` triples, and it
must report **how many bytes of the blob it consumed**, because that is the single strongest
correctness signal available — a correct decoder consumes the blob exactly.

### Tests, against the real `libroblox.so`

These are golden-data tests against a 109 MB real binary, far stronger than synthetic input:

- Header: `ET_DYN`, `EM_AARCH64`, `ELFCLASS64`.
- `DT_ANDROID_RELA` is **present**, and `DT_RELA` and `DT_RELR` are **absent**. Assert all three.
- The APS2 decoder yields exactly **568,272** relocations: exactly **568,194**
  `R_AARCH64_RELATIVE` (1027), **56** `R_AARCH64_GLOB_DAT` (1025), and **22** `R_AARCH64_ABS64`
  (**257**, not `ABS32`/258), with exactly **78** carrying a non-zero `r_sym`. Assert all five, and
  assert **zero** relocations of type 258 so the off-by-one cannot creep back.
- The **534** `R_AARCH64_JUMP_SLOT` (1026) relocations come from `.rela.plt` via `DT_JMPREL` and are
  **not** in the APS2 blob. Assert that separately, and assert the grand total is **568,806**. An
  earlier draft of this plan wrongly folded the 534 into the APS2 count; do not reproduce that.
- The decoder consumes exactly **2,100,778 of 2,100,778** bytes — byte-exact, no remainder.
- `DT_INIT_ARRAY` has exactly **3,594** entries.
- `PT_GNU_RELRO` covers exactly **5,205,568** bytes.
- **No `PT_TLS` segment** exists, and no symbol has type `STT_TLS`.
- Exactly **565** undefined symbols in `libroblox.so` alone, and exactly **669** in the union across
  all 11 libraries, cross-checked against `docs/research/apk-undefined-symbols.txt`. Assert both.
- The other ten libraries use plain `DT_RELA` plus `DT_JMPREL`, and **no** library uses `DT_RELR` or
  `DT_ANDROID_REL`, so the parser must handle both relocation styles.
- SLEB128 round-trip unit tests including negative values and multi-byte boundaries.
- Truncated and corrupt APS2 blobs produce typed errors, never panics and never silent success.

### Report

Any divergence between the APS2 format as you implemented it and as described here; the exact
consumed-byte count you measured; decode wall-time for 568,272 relocations; and whether both
`DT_HASH` and `DT_GNU_HASH` were present and agreed on lookups.

---

## Task 5: `omni-elf` loader — map, relocate, resolve. Milestone M1.

Bring Tasks 1 through 4 together: take the extraction-cache file and produce a loaded, relocated
library in the guest address space with its imports accounted for.

Depends on the APIs delivered by Tasks 1, 2 and 4. Use them as delivered rather than reshaping them;
if one is genuinely unfit, say so in the report instead of working around it silently.

### Responsibilities

- Compute total mapped size from the `PT_LOAD` set, reserve that span at a single base through
  `omni-mem`, and map each `PT_LOAD` at `base + p_vaddr` from the cache file at 4 KB granularity,
  honouring `p_align` (4 KB here, 16 KB for newer NDKs).
- Zero the `p_memsz > p_filesz` tail (`.bss`) without disturbing file-backed pages.
- Apply relocations: `R_AARCH64_RELATIVE` (1027) as `*target = base + addend`, and
  `R_AARCH64_GLOB_DAT` (1025), `R_AARCH64_JUMP_SLOT` (1026) and **`R_AARCH64_ABS64` (257)** as symbol
  resolution. The 534 `JUMP_SLOT`s come from `DT_JMPREL`, separately from the APS2 blob.
  **`ABS64` is a 64-bit store.** An earlier draft of this plan said `ABS32` (258); writing 4 bytes
  where 8 are required would corrupt 22 pointers in ways that surface arbitrarily far away.
- **Honour `p_align = 0x4000` (16 KiB).** Every `PT_LOAD` in `libroblox.so` is 16 KiB-aligned, not
  4 KiB. Windows placeholder splitting works at 4 KiB so 16 KiB is satisfiable, but the segment base
  arithmetic must use the real `p_align`.
  Relocation targets land in pages that must be **private and writable** at that moment — this is
  where the file-backed mapping needs copy-on-write or a private overlay, so be deliberate about it
  and explain the choice in the report.
- **Symbol resolution** against a provider registry: a trait through which `omni-android` will later
  supply host implementations. In this task, register a provider that supplies **nothing** and
  report every unresolved symbol. Reaching M1 does not require any import to resolve; it requires
  all 565 to be *enumerated and accounted for*.
- Apply `PT_GNU_RELRO` read-only protection **after** relocations, and assert it covers 5,205,568
  bytes.
- Record the loaded-object metadata that `dl_iterate_phdr` will need (base, phdr pointer, phnum,
  name) — do not implement `dl_iterate_phdr` itself, just keep the state it will require, since the
  in-guest unwinder depends on its fidelity.
- Collect but **do not call** the 3,594 `init_array` entries — calling them needs the CPU backend,
  which is out of scope. Expose them as an ordered list.

### Tests — this is the M1 gate

- The real `libroblox.so` loads: all `PT_LOAD` segments mapped at their expected addresses, total
  span matching the program headers.
- All **568,806** relocations apply without error (568,272 from APS2 plus 534 from `DT_JMPREL`), and
  a sample of `R_AARCH64_RELATIVE` targets
  verifiably contains `base + addend` afterwards. Assert on actual memory contents, not on a
  counter — a counter alone cannot distinguish applied from skipped.
- RELRO is read-only afterwards, covering exactly 5,205,568 bytes, and a write attempt to it fails.
- Exactly **565** distinct unresolved imports are reported for `libroblox.so` with the empty
  provider, and the report distinguishes `STT_FUNC` from `STT_OBJECT`. 565 is this library alone;
  the 669 union figure is a different assertion belonging to Task 4.
- Exactly **3,594** `init_array` entries are collected, in order, each a plausible in-range address.
- Loading is idempotent and leak-free: load and unload repeatedly, and assert commit charge returns
  to baseline. This catches the class of bug where relocation privatizes pages that are never
  released.
- Peak commit charge for loading `libroblox.so` is measured and **reported as a number**, since D11
  predicts text and rodata stay file-backed and only RELRO, `.data` and `.bss` become private. If
  the measurement contradicts that prediction, that is an important discovery — report it rather
  than adjusting the expectation.

### Report

The measured peak and steady-state commit charge for a loaded `libroblox.so`, compared against the
D11 prediction; how relocation-target privatization was handled; load wall-time; and the full
breakdown of the 565 unresolved imports by provider library.
