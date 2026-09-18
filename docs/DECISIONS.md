# Decisions log

Every entry records what was decided, why, and what it costs if the decision turns out wrong.
Decisions made without the user present are marked **Ruling**; they are reversible and are
surfaced so they can be overridden.

---

## D1 — Implementation language: Rust
**Ruling.** Rust is the primary language; C/C++ is reachable via FFI where a specific reusable
component earns its integration cost.

Why: it is the only systems toolchain verified working end-to-end on this machine with no extra
installs (`cargo build` compiles *and* links; `clang`/`gcc` are absent). Cargo's target model
serves the five-platform requirement. The JIT and guest-memory layers still get raw pointers via
localized `unsafe`, while the loader, object model, and resource tracking stay memory-safe — which
matters in a process whose entire job is hosting foreign binaries.

Cost if wrong: a language switch is expensive. Mitigated by keeping module seams
language-agnostic, and by putting the most likely C++ borrow (a guest CPU JIT) behind a narrow
interface.

Evidence: `research/host-environment.md`.

---

## D2 — Baseline host ISA is x86-64-v3, AVX-512 optional only
**Ruling.** The translator targets AVX2 + BMI2 + FMA + F16C + LZCNT + MOVBE + CMPXCHG16B. AVX-512
may only ever be an opportunistic fast path.

Why: measured on the dev machine — AVX2/BMI2 present, AVX-512 absent (Raptor Lake disables it).
BMI2's flag-preserving shifts map directly onto AArch64 shifted-register operands, which are
pervasive. `CMPXCHG16B` is required for AArch64 128-bit atomics and is already part of x86-64-v2,
so requiring it is safe.

Cost if wrong: hosts older than ~2013 (pre-Haswell) are excluded. Acceptable; a v2 fallback path
could be added later behind runtime feature detection.

Evidence: `research/host-environment.md`.

---

## D3 — Reuse only permissively-licensed components
**Ruling.** Omnidroid links only MIT / BSD / Apache-2.0 / ISC / 0BSD components. Copyleft projects
(GPL/LGPL) may be *studied* as architecture references but their code is not linked or copied.

Why: this keeps every future licensing option for Omnidroid open, including a closed or
permissively-licensed release. Choosing the most restrictive-safe path now costs little and
avoids a decision that would be very expensive to unwind. The user has not stated an intended
license for Omnidroid, so assuming the permissive-only constraint is the safe default rather than
a blocking question.

Consequence: `libhybris` (mixed, incl. LGPL/GPL3) and `android_translation_layer` (GPL-3.0+) are
reference-only. A bionic-compatible ELF loader must be written from scratch, because no
permissively-licensed standalone one exists.

Cost if wrong: if the user is happy with GPL, we did more original work than strictly necessary —
but the original work is clean-room and unencumbered, which has independent value.

Evidence: `research/prior-art.md`.

---

## D4 — Design target: guest virtual address == host virtual address
**Ruling (provisional, under test).** The guest ARM64 code runs in the host process's own address
space with no address translation: a guest pointer is a host pointer.

Why: it removes memory-translation overhead from every guest load and store, and it is what makes
the demand-driven memory requirement achievable — guest `mmap` becomes a host reservation/commit
directly, rather than carving out of a preallocated guest RAM blob. This is the design difference
between Omnidroid and a QEMU-style guest.

Cost if wrong: if the chosen CPU core cannot support identity mapping, every guest memory access
pays either a base-register add or a callback, and the memory model has to be reconsidered. This
is exactly why it is being verified by spike before the architecture is finalized, rather than
assumed.

Status: being measured (dynarmic spike, question 3; Windows memory model, questions 2-5).

---

## D5 — CPU core: pending spike
Prior art establishes dynarmic as the only permissively-licensed, direction-correct
A64-guest → x86-64-host JIT. Whether Omnidroid adopts it, adopts-then-replaces it, or writes a
custom JIT is **not yet decided** — it depends on whether it builds here, whether it supports
identity-mapped memory, its real throughput, and its 64-bit address-space assumptions.

No code will be written against either choice until the spike reports.

---

## D6 — The supplied APK is adversarially modified; design against the stock engine only
**Ruling.** Omnidroid's required API surface is derived from **`libroblox.so` and the legitimate
Roblox/AGDK/AndroidX components only**. The injected components are explicitly out of scope and
will not be supported.

What was found in `Roblox-2.738.1397.apk` (verified from the APK's own bytes):
- It is signed by `O=Gloop, CN=Gloopiest Man` — self-signed, expired 2025-07-12 — **not by Roblox**.
  The Play source stamp has been stripped.
- `classes4.dex` contains an injected `com.roblox.gloop.Loader`; `assets/gloop/dlt.zip` and a macOS
  `.DS_Store` are injected.
- `libzstd-jni-1.5.7-6.so` is an 18 MB trojanised blob that is not zstd: it bundles a Luau
  decompiler, Dear ImGui, libcurl/OpenSSL, and hooks on `eglSwapBuffers` and
  `vkGetInstanceProcAddr`. It reads `/proc/self/maps`, walks `dl_iterate_phdr`, and `mprotect`s
  pages.
- The manifest requests `MANAGE_EXTERNAL_STORAGE`, which stock Roblox does not need.
- `libroblox.so` itself appears stock and unmodified.

Why this ruling: three independent reasons point the same way.
1. **Correctness of the design.** The library set, permission list, and dex graph are contaminated,
   so treating them as "what Roblox requires" would bake an attacker's requirements into
   Omnidroid's API surface.
2. **Engineering cost.** The injected library hooks exactly the two graphics entry points
   Omnidroid must implement. Debugging our renderer against a binary that is also hooking it
   would waste large amounts of time on self-inflicted confusion.
3. **Scope.** Omnidroid is a compatibility runtime. Making a script executor work is not part of
   that, and no work will be directed at it.

Action needed from the user: supply a **stock, Play-signed** `com.roblox.client` arm64-v8a APK
before the Android API surface is frozen. Until then, design proceeds from `libroblox.so` (stock)
plus the documented AGDK/NDK contracts, both of which are trustworthy.

Cost if wrong: if the stock APK's library set differs from what we inferred, some stub surface is
wrong and has to be adjusted — cheap, because the surface is generated from the ELF imports rather
than hand-written.

Evidence: `research/apk-analysis.md`.

---

## D7 — No JVM, no ART: implement JNI natively
**Ruling (provisional, being sized).** Omnidroid will not host ART or any JVM. It implements
`JavaVM`/`JNIEnv` in host-native code and services the engine's JNI calls with native
implementations of the Java classes the engine actually touches.

Why: 26,620 dex classes exist but only ~636 are Roblox's own, and essentially all engine logic
lives in `libroblox.so`. Hosting ART would mean translating ART's own ARM64 code as well —
an enormous cost to run a Java layer that is mostly a thin shim over native code. Prior art
(`android_translation_layer`) hosts real ART, but it is GPL and targets a Linux host with Android
libraries available, neither of which applies here.

Cost if wrong: if something forces real dex execution (Java-side reflection, `RegisterNatives`
from Java static initializers, Java-implemented networking or login), a minimal dex interpreter
becomes necessary. This is being measured before any code is written against the assumption.

Status: under analysis — the native-to-Java JNI surface is being extracted from `libroblox.so`.

---

## D8 — Graphics: forward Vulkan to the host; EGL/GLES exist to satisfy the linker
**Ruling.** The primary renderer path provides the guest a `libvulkan.so` that forwards to the
host Vulkan driver. The hard-linked EGL/GLES symbols are provided so the ELF loader can resolve
them, with a real GLES path deferred until it is shown to be needed.

Why, from measured APK evidence:
- Vulkan is **`dlopen`-only** in `libroblox.so` (volk-style: 593 `vk*` name strings, zero `vk*`
  imports), so Omnidroid controls it entirely by supplying the library the engine opens.
- `shaders_vulkan_mobile.pack` is 14.7 MB **STORED** and contains **1,364 SPIR-V modules**, while
  `shaders_glsles3.pack` contains zero SPIR-V. Roblox ships SPIR-V for the Vulkan path, which
  removes shader translation from the critical path entirely — a large saving.
- EGL and GLESv2 *are* hard-linked (`DT_NEEDED`, 91 EGL+GL imports), so those symbols must resolve
  at load time or the library will not load at all. Resolving them is a link-time requirement,
  not necessarily a rendering requirement.
- The manifest declares `android:glEsVersion=0x30000` required and **no Vulkan `uses-feature` at
  all**, i.e. GLES3 is the guaranteed fallback and Vulkan is the preferred opportunistic path.

Important constraint found in the engine's own strings: `Vulkan: Device %s is emulated, skipping`,
alongside device/vendor/driver blacklists. Roblox **rejects emulated Vulkan devices**. Because
Omnidroid forwards to the real host GPU, the reported `deviceType` and vendor/device IDs will be
genuine, which is what this check wants. A software or virtual Vulkan device would be refused.

Cost if wrong: if the Vulkan path is rejected on some host for a reason we cannot control, a real
GLES3-on-Vulkan translation layer becomes necessary — a substantial piece of work. The renderer
abstraction is therefore kept general enough that GLES3 can be implemented behind it later.

Evidence: `research/apk-analysis.md`, `research/graphics-spike.md`.

---

## D9 — ELF loader must implement Android packed relocations (APS2)
**Fact, not a choice.** `libroblox.so` uses `DT_ANDROID_RELA` (APS2, SLEB128-packed) for its main
relocations and has **no `DT_RELA` and no `DT_RELR`**. The exact breakdown, corrected after an
earlier draft of this file folded the `JUMP_SLOT`s into the packed count:

| Source | Count | Types |
|---|---|---|
| `DT_ANDROID_RELA` blob (2,100,778 B, magic `APS2`) | **568,272** | 568,194 `R_AARCH64_RELATIVE` (1027) + 56 `R_AARCH64_GLOB_DAT` (1025) + 22 `R_AARCH64_ABS64` (**257**) — 78 with non-zero `r_sym` |
| `.rela.plt` via `DT_JMPREL` — **separate** | **534** | `R_AARCH64_JUMP_SLOT` |
| Grand total | **568,806** | |

The 534 `JUMP_SLOT` relocations are **not** inside the APS2 blob. The other ten libraries use plain
`DT_RELA` plus `DT_JMPREL`, and no library in the APK uses `DT_RELR` or `DT_ANDROID_REL`, so the
loader must handle both styles. A loader that handles only standard or `RELR` relocations applies literally zero
relocations to Roblox and cannot work at all.

Other loader requirements measured from the same binary:
- **3,594 `DT_INIT_ARRAY` entries** run before `JNI_OnLoad` — all must succeed.
- `PT_GNU_RELRO` covers 5,205,568 bytes and must be made read-only at the right moment.
- **No `PT_TLS` and no `STT_TLS` symbols anywhere in the APK** — there is no ELF TLS to implement.
  Thread-local storage is `pthread_key_*` only. TLSDESC, `__tls_get_addr`, and DTV modelling are
  **not needed**, which is a meaningful saving; that effort belongs in `pthread_key_*` performance
  instead.
- No ifunc, no BTI/PAC/MTE, no `DT_TEXTREL`.
- `libroblox.so` imports **565** undefined symbols; the **union** across all 11 libraries is 669.
  Both figures are correct and are not in conflict, since the union includes the injected library.
- The C++ runtime is **statically linked** (no `libc++_shared.so`), so the unwinder lives inside the
  guest and reads 11.5 MB of `.eh_frame`. It resolves frames via **`dl_iterate_phdr`**, so that
  function must be faithful, not a stub — C++ exceptions will not work otherwise.
- All 11 `.so` are **DEFLATED and only 4-byte aligned**, so they cannot be mapped in place from the
  APK. They must be decompressed, which makes `extractNativeLibs` effectively true and rules out
  zero-copy segment mapping for this APK.
- Two `DT_NEEDED` libraries (`libOpenSLES.so`, `libOpenMAXAL.so`) import **zero** symbols but must
  still exist as loadable objects, and **23** imports are `STT_OBJECT` **data** symbols rather than
  functions (the 10 `AMEDIAFORMAT_KEY_*` are only a subset of those 23) — both are failure modes with no symbol name to guide diagnosis.

Evidence: `research/apk-analysis.md`. The APS2 decoder used was validated byte-exact: it consumed
2,100,778 of 2,100,778 bytes and produced exactly the 568,272 declared relocations.

### Corrections from building the loader on it (Task 5)

All 568,806 relocations now apply to the real binary and the results are read back out of mapped
memory. Four things the loader measured that this entry did not say, or said differently:

- **`DT_PLTGOT` is *inside* `PT_GNU_RELRO`, and `DT_FLAGS` carries `DF_BIND_NOW`.** The PLT GOT sits
  at `0x67d16f8`, inside the relro region `0x62dc1c0..0x67d3000`, and all 534 `JUMP_SLOT` targets are
  in there with it. So **lazy PLT binding is impossible for this library**: the whole GOT is sealed
  read-only at the end of the load. Every `JUMP_SLOT` must therefore be applied *before* relro is
  sealed, which fixes the order of the entire load. The task brief assumed the opposite, and so did
  the first draft of the test.
- **The 565 imports split 539 `STT_FUNC` / 23 `STT_OBJECT` / 3 `STT_NOTYPE`, and 4 are weak.** The 3
  `STT_NOTYPE` are a third bucket that a `func`-versus-`object` binary split loses.
- **`DT_VERNEED` is what attributes an import to a library.** `libroblox.so` has three `Elf64_Verneed`
  records, and `DT_VERSYM` attributes **407 of the 565** to `libc.so` (345), `libm.so` (56) and
  `libdl.so` (6) straight out of the file. The other **158** reference `VER_NDX_GLOBAL`, because the
  Android libraries providing them ship no version definitions, so the file records no provider for
  them and a loader must not invent one. Without this, "565 imports" cannot be turned into a
  per-library work list at all.
- **Exactly one of the 612 symbolic relocations resolves inside the object**, not zero: a `JUMP_SLOT`
  for `Java_com_roblox_client_purchase_IAPPurchaseManager_nativeFinishPaymentsProtocolPurchaseWithReturn`,
  which `libroblox.so` both imports and exports. A loader that consulted its provider registry before
  the object's own symbol table would leave that one null.
- **All 3,594 `DT_INIT_ARRAY` slots are zero in the file.** The pointers are produced by
  `R_AARCH64_RELATIVE` relocations, so the array must be read from *relocated memory*. A loader that
  reads the file image collects 3,594 null pointers and has no way to notice.

---

## D10 — Memory model: free address space, lazily committed, decommit to reclaim
**Decided from measurements**, not assumption. Every number below was produced by a probe program
run on this machine; see `research/windows-memory-model.md`.

The requirement was: isolated guest address spaces, no large fixed RAM reservation, demand-driven
usage, reclaimable, and many instances without a huge pagefile. The measurements show exactly how
to satisfy it, and also which plausible approaches silently fail.

**What is free:** address space. A pure `MEM_RESERVE` costs **0 bytes** of commit charge and 0
working set — verified at 1, 4, 16, 64 and 256 GB, and at 97.7 TB in a single call (largest single
successful reserve: 125.57 TB). 64 processes each reserving 16 GB — 1 TB of guest address space in
total — cost **240.8 MB** of system commit between them. So reserving a generous per-instance guest
address space is not the thing to economize on.

**What is scarce:** commit charge. All figures in this decision were measured on **fully-touched,
privately-committed anonymous memory**; they are not established for file- or section-backed views,
and an attempt during M2 to extend the `size/512` page-table model to a section view was retracted as
unverified rather than confirmed. `MEM_COMMIT` debits the system commit limit **immediately on
commit, not on first touch** — 1024 MB committed showed up as 1026.66 MB of commit charge while the
working set was only 4.68 MB. This is the key asymmetry: a design that commits a multi-GB region
per instance fails the requirement even though its working set looks small. Commit must therefore
be lazy and granular.

**Reclamation — only one primitive actually works.** Measured effect on commit charge:

| Primitive | Frees commit? | Frees working set? | Address stays reserved? | Data |
|---|---|---|---|---|
| `VirtualFree(MEM_DECOMMIT)` | **yes, 256.50 MB returned** | yes | yes | zero-filled on re-commit |
| `MEM_RESET` | **no, 0.00 MB** | no | yes | may be discarded |
| `MEM_RESET_UNDO` | no | no | yes | restores |
| `DiscardVirtualMemory` | **no, 0.00 MB** | yes | yes | discarded |
| `OfferVirtualMemory` | **no, 0.00 MB** | yes | yes | recoverable |
| `EmptyWorkingSet` | **no, 0.00 MB** | yes | yes | preserved |

`MEM_DECOMMIT` is the only primitive that returns the scarce resource, and it gives Linux
`munmap`/`MADV_DONTNEED` semantics. `MEM_RESET` is an outright trap: it is the cheapest call
(32.5 ns/page) and frees nothing. `EmptyWorkingSet` is a useful *secondary* lever for backgrounded
instances — it preserves data and costs 913 ns/page to fault back in — but it must never be
mistaken for reclamation.

**End-to-end validation of the requirement:** an instance holding a **4 GB guest address space
costs 37.25 MB of commit charge**. Grown to 3 GB of live use and then released, it fell back to
513.656 MB of commit and 0.148 MB of working set *with the 4 GB reservation still intact*. The
"several GB at startup, ~500 MB later" scenario in the project goal is therefore directly
achievable, and ~32,700 separate 4 GB guest spaces fit in one 128 TB address space.

**Do not use per-page fault-driven paging.** A VEH-based demand-pager was measured 100% reliable
(0 bad resumes across 65,536 faults) but costs **2053 ns/fault**, versus 398 ns for a kernel soft
fault and **3 ns/page** for bulk commit. Commit in 64 KB to 1 MB blocks ahead of use; reserve VEH
for correctness edge cases, never as the hot path.

**Large pages are unavailable** (`SeLockMemoryPrivilege` not held, `err 1314`), so 2 MB pages are
not part of the design.

---

## D11 — Guest libraries are mapped from a 4 KB-aligned extraction cache, not from the APK
**Decided from measurements.** `VirtualAlloc2`, `MapViewOfFile3` and `UnmapViewOfFile2` exist and
give genuine `mmap(MAP_FIXED)` semantics — but they live **only in `kernelbase.dll`**, not
`kernel32.dll`, so they must be resolved with `GetProcAddress`.

The decisive measurement: when replacing a placeholder, **both the base address and the file offset
are constrained to 4 KB, not 64 KB**. Proven by 512/512 successful maps at 4 KB file-offset steps
with content verification, against only 4/64 successes on the `BaseAddress=NULL` path
(`err 1132`). Sub-page offsets always fail. This 4 KB capability is placeholder-only — it is not
reachable through plain `MapViewOfFile`.

So the sequence is: reserve with `MEM_RESERVE | MEM_RESERVE_PLACEHOLDER`, split at 4 KB with
`MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER`, then `MapViewOfFile3(MEM_REPLACE_PLACEHOLDER)` for
file-backed segments and `VirtualAlloc2(MEM_REPLACE_PLACEHOLDER)` for private commit. Replacement
requires an **exact-size** placeholder or it fails with `err 487`. Cost is about 1 microsecond per
operation; 24 real ELF segments mapped in 167 microseconds for 0.566 MB of commit.

**Why an extraction cache rather than the APK directly:** zero-copy mapping straight out of an APK
is possible, but only for **STORED** entries whose payload begins at a 4 KB-aligned offset — i.e.
what `zipalign 4` / `zipalign -P 16` produces. Our APK's 11 `.so` are **DEFLATED and only 4-byte
aligned**, so none of them qualify. The measured fallback of copying into private memory costs
about 1 ms and about 4 MB of **permanent** commit per 4 MB, per launch — unacceptable for a 109 MB
library across multiple instances.

Therefore: **decompress each `.so` once into a shared, content-addressed, 4 KB-aligned cache file,
and map that.** This turns a per-launch cost into a one-time cost, and has a large second benefit
for the multi-instance requirement: file-backed read-only and execute mappings of the shared cache
are backed by the file rather than by commit charge, so the roughly 109 MB of `libroblox.so` text
and rodata is shared between instances at near-zero marginal commit. Only genuinely private pages —
the 5.2 MB RELRO region after relocation, `.data`, `.bss`, and heap — cost per-instance commit.

Two mechanical requirements found the hard way: the APK or cache file must be opened
`GENERIC_READ | GENERIC_EXECUTE` and the section created `PAGE_EXECUTE_READ`, or `.text` can never
be made executable afterwards; and a misaligned offset fails with `ERROR_MAPPED_ALIGNMENT`.

### Correction (found during Task 1 implementation)

The paragraph above was **incomplete in a way that would have caused a confusing failure at M2
rather than M1**. Opening the file and creating the section for execute is necessary but *not
sufficient*: **the view protection caps executability too.** A view mapped `PAGE_READONLY` out of a
`PAGE_EXECUTE_READ` section **cannot later be raised** to `PAGE_EXECUTE_READ` — it fails with
error 87, the same error as the read-only-section case, which is what makes the two easy to
conflate.

So executability must be chosen **twice**: once when opening the file and creating the section, and
again at **every** `map_file` call. `.text` must be mapped `ReadExecute` from the outset; it cannot
be mapped read-only and promoted later.

This directly shapes how the ELF loader applies relocations, since relocation targets must be
writable at that moment while the same pages must end up executable. The working sequence, verified
byte-exact in Task 1's tests, is: **map `ReadExecute` → drop to `ReadWrite` → write → restore
`ReadExecute`.** Mapping read-only first and hoping to promote does not work.

Two further measured corrections from the same work:
- Double release reports error **487**, not 87.
- `Protection::ReadWrite` requires `PAGE_WRITECOPY` for a *view* but `PAGE_READWRITE` for *private*
  memory, so the protect operation performs a `VirtualQuery` to determine which applies rather than
  assuming.


### Further corrections from building on it (Task 2)

**Copy-on-write is charged at `protect` time, not at write time.** Measured twice independently:
mapping an 8 MiB `ReadExecute` view costs +0.020 MiB; protecting it to `ReadWrite` costs
**+8.020 MiB immediately**, before a single byte is written; writing two pages adds nothing further;
restoring `ReadExecute` refunds it down to +0.027 MiB.

The consequence is a hard constraint on the ELF loader: **relocation must proceed in windows.** A
loader that drops the whole 109 MB library to `ReadWrite` in order to relocate it would transiently
charge 109 MB of commit *per instance* — which, multiplied across concurrent instances, defeats the
memory requirement at precisely the worst moment. Protect a window, relocate within it, restore it,
move on.

**Partial unmap of a file view must preserve copy-on-write content in the surviving pieces.** On
Windows a view can only be unmapped whole, so a partial unmap is emulated by unmapping the view and
re-mapping the survivors — and a naive implementation re-maps them *fresh from the backing file*,
silently discarding any relocated content while returning success. This is not hypothetical: it was
reproduced in exactly the relocation shape above, reading back `0x0` instead of the written byte.

The working approach preserves both correctness and sharing: compare each survivor that has **ever
been writable** page-by-page against a pristine second view of the same section, streaming in
windows, and write back **only the pages that differ**. Measured cost: clean views are never compared
at all (11.8 us per hole punch), one dirty page costs 38 us, a fully-writable 4 MiB view costs
1.51 ms (~0.5 ms/MiB, so ~2.6 ms for RELRO). Sharing lost: **none** — commit charge across the unmap
is +0.000 MiB, confirmed by re-measurement.

Two approaches that look right and are not: wholesale snapshot-and-restore privatises clean pages
(measured +8.453 MiB on an 8 MiB case, i.e. the multi-instance property visibly failing), and
`QueryWorkingSetEx`-based privatisation detection is unreliable because **its shared bit is only
meaningful for resident pages** — it would lose data precisely under the memory pressure that causes
instances to be backgrounded.

**A pagefile-backed section does not appear in `PrivateUsage`.** The JIT arena's cost is therefore
invisible to per-process commit-charge measurement. This invalidates no existing measurement, since
every commit-charge figure recorded here concerns private memory — but it means the budgeting
diagnostic cannot see its fastest-growing consumer, given D5 measured 20-35 MiB of code cache **per
guest thread**. The arena's mapped size is therefore reported as a first-class figure alongside
private usage.

**`release` must walk an allocation's real extent.** Passing a zero size released only the first
piece of a split reservation and returned success — a latent trap, harmless only because the one
existing caller happened to coalesce first.

### Further corrections from building the loader on it (Task 5)

**A view mapped `Read` out of an executable section *can* be dropped to `ReadWrite` and raised back.**
The correction above established that `Read → ReadExecute` fails with error 87; it did not say whether
`Read → ReadWrite → Read` works, and the loader depends on it, because a writable segment must be
mapped `Read` (a copy-on-write view is charged its full size the moment it is mapped) and raised only
in windows. Measured: it works, a 64 KiB window of an 8 MiB view costs **+0.062 MiB** while open and
refunds to **+0.004 MiB** on restore with one page written, and the written bytes survive the restore.

**`PT_GNU_RELRO` routinely runs past the end of its own segment's memory image.**
`libdatastore_shared_counter.so` has a writable `PT_LOAD` ending at `0x5428` and a relro segment
ending at `0x6000` — the page boundary above it. A loader that validates the relro range against
`p_vaddr + p_memsz` of a containing `PT_LOAD` rejects that library outright. The check has to be
against the **mapped pages**. Found by loading all eleven libraries, not by the main one.

**The loader's own scratch memory was the largest single consumer of commit charge.** Measured before
it was fixed: **+36.8 MiB** for the `Vec<Rela>` holding 568,272 relocations (13.6 MB of data, peaking
at ~2.7× that because the `Vec` grows by doubling), against **+16.7 MiB** for everything the guest
actually gets. Streaming the packed blob straight into the relocation pass removed it, bringing the
peak down to equal the steady state. The general lesson: when the design question is "how much commit
charge does one instance cost", the loader's own transient allocations are in the same budget as the
guest's, and they are not automatically smaller.

**Windowing bounds the worst case, not this case.** All 568,806 of `libroblox.so`'s relocations land
in the 5.5 MB writable part of its image, which becomes private regardless, so the measured peak is
almost independent of window size (16.79 MiB at 64 KiB against 16.79 MiB at a whole 5.2 MB segment).
The window is what keeps a `DT_TEXTREL` binary — or a tampered one aiming relocations into the 103 MB
`r-x` segment — from charging 99 MiB per instance. That is still worth the measured 1.4 ms, but the
justification is the adversarial case and not the stock one, and this entry previously implied
otherwise.

**Measured end to end, for the record.** Loading `libroblox.so` costs **about +16.7 MiB** of commit charge,
peak and steady, of which 11.039 MiB is `.bss` committed eagerly, 4.965 MiB is the relro region after
relocation and 0.328 MiB is `.data`. The 103,649,280 bytes of text and rodata cost nothing and are
shared. Load wall-time is 11.8 ms in release. Four load/unload cycles return commit charge to baseline
each time.

### Confirmed prediction

The same tests confirmed the multi-instance premise of this decision: a 4 MiB shared read-only view
cost **+0.008 MiB** of commit charge, and that figure was **unchanged after reading every byte of
it**. File-backed pages genuinely do not consume commit charge, which is what allows instances to
share `libroblox.so`'s ~109 MB of text at near-zero marginal cost. Committed private memory
behaved as D10 predicted: 64 MiB committed cost +64.125 MiB, i.e. the size plus size/512 of page
tables, and decommit returned exactly that.

---

## D12 — JIT code memory: dual-mapped section, not `VirtualProtect` flipping
**Decided from measurements.** Emitting and then executing code via a dual-mapped pagefile-backed
section (one RW view, one RX view of the same pages) costs **162 ns** per emit+execute cycle with
**0 mismatches across 200,000 trials** and needs no instruction-cache flush on x86-64. The
conventional `VirtualProtect` RW to RX and back cycle costs **2259 ns** — about 14 times worse.

Why it matters: the translator writes code constantly, so this is a hot path, and the dual mapping
also avoids ever holding a page that is simultaneously writable and executable. It is both the
faster and the safer option, which is a rare combination.

Cost if wrong: the approach is Windows-specific in its mechanics. The same pattern is available on
Linux (`memfd_create` plus two `mmap`s) and macOS (`MAP_JIT` with
`pthread_jit_write_protect_np`, which behaves differently and will need its own measurement), so
this sits behind the platform abstraction rather than in shared code.

---

## D4 (resolved) — Identity mapping confirmed: guest VA == host VA, at zero cost
**Was provisional, now verified.** The spike configured dynarmic with `fastmem_pointer = Some(0)`
and `fastmem_address_space_bits = 64`, which emits `mov reg, [r13 + vaddr]` with `r13 = 0` — a
single instruction with the base folded into the SIB byte. Verified executing at host VA
**0x7F00_0000_0000** (bit 46) with **zero** slow-path callbacks taken.

So the central architectural bet in `ARCHITECTURE.md` section 1 holds: there is no address
translation on the memory path, and it costs not even a register add. Measured consequence: the
memory-heavy loop runs at **5,207 Mguest-insn/s** on the fastmem path versus **396 Mguest-insn/s**
through memory callbacks — a 13.2x difference *as measured by the spike*. **See the Task 3 correction
below: the real cost is 30-49x, and 13.2x is a floor.** Either way, this single configuration choice is
the difference between a viable runtime and an unusable one.

**Two footguns to guard against in code, not comments:**
- The default `fastmem_address_space_bits` is **36**, and a high guest VA silently degrades to the
  callback path rather than erroring. A 13x performance loss that produces correct results is the
  worst possible failure mode, so Omnidroid must assert this configuration at startup.
- Guest PC is truncated to a sign-extended **56 bits**. Harmless for Windows and Android user-space
  addresses, but it is a real cap and is recorded here so nobody is surprised by it later.

**Correction (Task 3): the cost of losing identity mapping is larger than first recorded, and the
original figure measured the wrong thing.** This decision recorded 13.2x, from 5,207 versus 396
Mguest-insn/s. Re-measured through Omnidroid's *real* callback path (n=31, release, two loop shapes,
both degraded mechanisms): **30-49x**. Specifically 32.62x and 29.90x on a 40%-memory-dense loop, and
49.06x and 44.45x on the 50%-dense loop the original figure used.

The fast path reproduces the original within 3% (5,055 versus 5,207); it is the **callback** path that
differs, by 3.5x (114 versus 396), because the original spike's callbacks were bare stubs while the
runtime's do a `catch_unwind` plus a `GuestSpace` lookup per access. So **13.2x measured a floor, not
the runtime's cost**, and the loss *rises* with memory density rather than falling.

The practical consequence is that the startup assertion defends against a larger loss than this
decision first claimed, which makes it more valuable, not less.

Windows fault handling inside dynarmic is **frame-based SEH scoped to its code cache**, which means
an Omnidroid-installed **vectored** exception handler runs first. Verified: Omnidroid's VEH took the
fault (`veh_hits=1`) and dynarmic's slow path was never entered. Omnidroid therefore keeps ownership
of guest demand paging, which D10 requires.

---

## D5 (resolved) — CPU core: adopt dynarmic as a pinned fork, plan to replace the x64 backend
**Ruling.** Adopt dynarmic now, pinned as a fork we carry patches against, behind the `GuestCpu`
trait. Plan for eventual replacement of its x64 backend rather than treating it as permanent.

`merryhime/dynarmic` **404s**; the spike used mirror `yuzu-mirror/dynarmic@9d45823` (v6.7.0,
2024-03-05, ISC/0BSD, externals vendored as git subtrees rather than submodules). That is the pin.

**It clears the bar.** Builds clean in 49 s with `-j24`, and **all 202,200 test assertions pass**.
A64 execution verified correct across 37 hand-encoded checks covering shifted ALU operands,
load/store including pair and register-offset forms, loops, `BL`+`RET`, NEON, FP, and `LDXR`/`STXR`.
Rust FFI demonstrated end to end — 18 `extern "C"` entry points plus 17 callback pointers, with
guest code writing directly into a Rust `Vec<u64>` at 5,566 Mguest-insn/s.

**Build requirements the prior-art survey missed:** an **undeclared Boost dependency** (icl and
variant), `-DCMAKE_POLICY_VERSION_MINIMUM=3.5` (robin-map still declares `VERSION 3.1`, which
CMake 4.x rejects), and short build paths (MSVC `C1083` otherwise). Worth recording because the
survey listed the dependency set as fmt/mcl/xbyak/zydis/robin-map and Boost was not in it.

**Why not a custom translator now:** estimated ~45,000 LOC and 2-4 engineer-years for A64 integer
plus NEON plus FP, against a component that already passes 202,200 assertions. That trade is not
close today.

**Why "plan to replace the backend":** measured performance is uneven.

| Workload | Throughput | vs native |
|---|---|---|
| Memory-heavy (fastmem) | 5,207 Mguest-insn/s | ~2.0x |
| NEON / FP | 1,626 Mguest-insn/s | ~2.2x |
| Register-bound integer | 603 Mguest-insn/s | **~33x** |

The 33x case is not inherent to translation — the host dump shows per-block register allocation
spilling every guest register to `JitState` each iteration, plus `lahf`/`sahf` round-trips for NZCV.
That is a backend quality problem with a known cause, which is exactly the kind of thing a
replacement backend fixes while keeping the frontend.

**Risks we now carry, with the mitigation each needs:**

1. **Cold translation is the weak spot.** 0.15-0.31 Mguest-insn/s, producing 12-30 bytes of host
   code per guest instruction, implying **7-25 s** to warm a Roblox-sized working set. Mitigations
   to build: parallel translation across the 24 available cores, and a persistent on-disk code
   cache keyed by library content hash so the cost is paid once rather than per launch.
2. **Per-thread memory conflicts with the memory goal.** One `Jit` per guest thread with **fully
   duplicated** code caches, committing **20-35 MiB per thread regardless of code volume** —
   0.6-1.1 GiB for 32 threads. Roblox is heavily multithreaded, so this directly threatens D10.
   Needs investigation into cache sharing; if dynarmic cannot share, this becomes the strongest
   argument for a replacement backend.
3. **`ExclusiveMonitor` anti-scales 21x from 1 to 16 threads** — one global spinlock per
   `LDXR`/`STXR`. `fastmem_exclusive_access` is the untested mitigation. This compounds with risk 4.
4. **231 of 874 decoder entries are unimplemented**: LSE atomics (`CAS`/`LDADD`/`SWP`), FP16
   arithmetic, BF16, i8mm, `FJCVTZS`, `CNTVCT_EL0`, and `MIDR_EL1`/`ID_AA64*` system registers.
   They surface cleanly via `InterpreterFallback` at ~87 ns per trap, so they are correct but slow.
   Note the interaction: because Omnidroid implements bionic, it controls `getauxval(AT_HWCAP)` and
   can decline to advertise LSE — but that steers the engine onto `LDXR`/`STXR`, straight into
   risk 3's global spinlock. The two must be resolved together, not separately.

**Good news worth recording:** `TPIDR_EL0`/`TPIDRRO_EL0` are fully supported, which is essential
given Android TLS depends on the thread pointer. PAC/BTI hint-space forms no-op correctly, crypto,
CRC32 and SDOT work, unaligned access just works, and `SVC` reaches a clean `CallSVC` hook.

**A real dynarmic bug found:** `hook_hint_instructions` is never plumbed into the A64 frontend, so
every `YIELD` exits the JIT. One-line fix, and a patch we carry on the fork.

Cost if wrong: if the per-thread memory cost or the exclusive-monitor contention proves
unfixable under real Roblox thread counts, the x64 backend replacement moves from "later" to
"required", which is a large but bounded piece of work. The `GuestCpu` trait is what keeps that
change from touching the rest of the runtime.

Evidence: `research/dynarmic-spike.md`.

---

## D7 (resolved) — No JVM, no ART, and no dex interpreter. Confirmed.
**Verified.** Every mechanism that would have forced real dex execution was searched for in
`libroblox.so` and is **absent**: zero `dalvik/system/*`, zero `java/lang/reflect`, zero
`Class.forName`/`getDeclaredMethod`/`defineClass`, zero `java/lang/invoke`, zero Java-side HTTP
(`java/net/*`, `okhttp3`), zero `java/io/File`, zero `android/webkit`, zero
`android/database/sqlite`. `JNIEnv::DefineClass` is never dereferenced. Only 2 `RegisterNatives`
sites exist and both are native-driven (AGDK's own `GameActivity_register`), not driven from Java
static initializers.

The one reflective-looking item — `ClassLoader.loadClass`/`findClass` reached via
`com/snapchat/djinni/NativeObjectManager.getClassLoader()` — is the standard "cache the app
ClassLoader so `FindClass` works on native threads" pattern, about 20 lines to satisfy.

**A dex interpreter would be strictly worse, not merely unnecessary.** Running
`MainGameActivity.onCreate` for real drags in the full 26,620-class closure: AndroidX lifecycle,
Kotlin coroutines, OkHttp, Dagger, Play Services. `NativeHelper.n0` alone takes
`(int, android/view/SurfaceView, com/roblox/client/RbxKeyboard, vk/e$f)`.

**How the JNI surface was actually measured**, since the binary is stripped: function boundaries
came from `.eh_frame_hdr` (**245,117 exact function starts** across 18,153,537 instructions, no
symbols needed), then `JNIEnv` was identified by interprocedural taint from the 539 `Java_*`
exports plus `JNI_OnLoad` plus the 26 `RegisterNatives` function pointers. A JNI call has the shape
`ldr Xb,[ENV]; ldr Xt,[Xb,#imm]; blr Xt`, which is what distinguishes it from the roughly one
million identically-shaped C++ vtable calls. The offset table self-validated: every literal-string
call site landed on exactly one of the six lookup offsets with the right argument shape.

**The surface is small and the shape is favourable:**

| Fact | Value |
|---|---|
| `JNINativeInterface` slots actually dereferenced | **59 of 233** (943 call sites); 170 never used |
| `JavaVM` slots used | **2** — `GetEnv` and `AttachCurrentThread` only |
| Distinct Java members referenced | **409** (296 methods + 113 fields) across **104** classes |
| Of those, lazy Djinni bridges no first frame touches | 109 members / 26 classes |
| Real surface | **300 members / 78 classes** |
| **Needed for a first frame** | **~120 distinct Java members** |

Three implementation details that matter more than their size suggests:
- **Every `CallXxxMethod` funnels through the `...MethodV` slot** — exactly one site each, zero
  non-`V` sites, because the C++ `jni.h` inline wrappers got ICF-merged. So the **`va_list` forms
  must be correct**; the convenience forms are never called and need not exist.
- **The engine only ever reads Java fields, never writes them.** All 18 `Set*Field` and
  `SetStatic*Field` slots are unused, as are `DefineClass`, `IsInstanceOf`, `GetSuperclass`,
  `AllocObject`, `NewObject`, local frame management, monitors, and the reflection converters.
- **5 referenced Java members do not exist in this APK's dex** (`DeviceUtils.
  getScreenPhysicalSizeInMillimeters`, `signalVideo*`). The shim must therefore return `NULL` plus a
  pending exception on a failed lookup, **not abort**.

**The real cost is different from what was feared.** `libroblox.so` does not bootstrap itself: flags,
client settings, base URLs, directories, device parameters and `InitParams` all arrive *from Java*,
and `NativeEngine` waits for them. Omnidroid must therefore write a native shell that issues that
whole sequence. That is **orchestration, not interpretation** — a long, ordered, verifiable script
rather than an interpreter. It is recorded step by step in `research/jni-surface.md`.

AGDK is **statically linked into `libroblox.so`** (verified: a 24-entry `JNINativeMethod` table at
`.data.rel.ro 0x062dc1c8`, `!gGameActivityClassInfo.*` assert strings, `android_native_app_glue` and
`GameTextInput` strings, and no separate `.so`). The contract was recovered field by field: the
`GameActivity` struct, a **21-slot `GameActivityCallbacks`** map with 19 of 21 individually verified,
a 632-byte `NativeCode`, a 384-byte `android_app`, `android_main` at `0x2bcc6a4` leading to
`new NativeEngine` and `GameLoop()` at `0x2bcd5d0`. The AGDK version cannot be pinned exactly
(Roblox vendored and modified it) but is bounded to **game-activity 2.0.x or later**.

Two abort traps to respect: `initializeNativeCode` **aborts** unless
`GameActivity.{finish,setWindowFlags,getWindowInsets,getWaterfallInsets,setImeEditorInfoFields}`,
`Insets.{l,t,r,b}` and 9 `WindowInsetsCompat$Type` statics all resolve; and `ALooper_forThread` must
not return NULL or it returns 0 and dies.

Cost if wrong: if some later Roblox version adds Java-side logic the engine depends on, the shim
grows. The measurement method is repeatable against a new APK, so this is detectable rather than
surprising.

Evidence: `research/jni-surface.md`, `research/jni-surface-lists.txt`.

---

## D13 — `TPIDR_EL0` must be programmed with a bionic TLS block before **any** guest code runs
**Hard prerequisite, discovered late and easy to miss entirely.**

`libroblox.so` contains **1,282 `MRS Xt, TPIDR_EL0` instructions, and 1,276 of them read
`[Xt, #0x28]`** — which is bionic's `TLS_SLOT_STACK_GUARD` (slot 5, at offset 5 × 8 = 0x28). Every
stack-protected function in the engine reads the bionic thread pointer *directly*, without going
through libc.

The consequence is ordering, and it is severe: this happens **before JNI matters, before
`JNI_OnLoad`, and before the first of the 3,594 static initializers**. If `TPIDR_EL0` does not point
at a valid bionic-layout TLS block with a stack guard at +0x28, the very first stack-protected
function crashes. No amount of correct loading, relocation, or symbol resolution gets past it.

So the guest-thread bring-up sequence is: allocate a bionic-layout TLS block per guest thread,
populate at minimum slot 5 with a stack-guard value, set `TPIDR_EL0` to it, **and only then** run any
guest code. This applies to every guest thread, not just the first.

This is implementable: the dynarmic spike separately confirmed that `TPIDR_EL0` and `TPIDRRO_EL0`
are **fully supported** by the A64 frontend (D5), so the register is real and writable rather than
trapped.

Cost if wrong: an immediate, near-inexplicable crash at the first initializer, with a symptom
(faulting on a load from a small offset off a zero register) that looks like a loader bug rather than
a missing thread pointer. Recording it here is what prevents a long debugging session at M3.

Evidence: `research/jni-surface.md` finding 14.

---

## D14 — `.bss` is committed eagerly: a deliberate, temporary exception to "never commit speculatively"
**Ruling.** The loader commits `.bss` eagerly (11.6 MB for `libroblox.so`), which is the majority of the
+16.7 MiB per-instance commit charge. This knowingly contradicts D10's "never commit speculatively"
constraint, so it is recorded as an exception rather than left to be discovered later as an
inconsistency.

Why eager, for now:
- **Lazy `.bss` would be a latent fault with nothing driving it.** Committing on demand requires
  something to catch the first touch of a page, and D10 already rejected fault-driven paging on
  measurement: a VEH fault costs **2053 ns** against **3 ns/page** for bulk commit. Building a
  fault-driven path here would reintroduce exactly the mechanism that measurement ruled out.
- **Nothing yet drives lazy commit.** Until the CPU backend runs guest code, there is no execution to
  hang demand-commit off. Committing eagerly now and adding a driver later is reversible; building a
  speculative demand-commit path first is not obviously so.
- **Eager `.bss` is pagefile charge, not resident pages.** The working-set cost stays demand-driven
  because untouched committed pages are never faulted in; only the commit *charge* is taken up front.
  That is the less harmful half of the cost.

The lazy path is implemented and measures **around +5.4 MiB** — roughly 11 MiB per instance cheaper — and
is one field away.

**This exception expires when a commit driver lands.** Once the CPU backend is executing guest code
there is a natural place to hang demand-commit, and the measured 6 MiB per instance is worth taking
back, especially multiplied across instances.

Cost if wrong: about 6 MiB of unnecessary commit charge per instance. It is visible — the per-instance
figure is now pinned by an assertion rather than merely printed — and reversible by flipping one field.

Evidence: `research/` measurements recorded in D10; loader figures in the Task 5 report.

---

## D15 — The commit ceiling is two bounds, and it covers anonymous commit only
**Decided from measurement, after the whole-branch review found a tampered `p_memsz` producing
+3406.664 MiB of eager commit from an 8-byte edit, with the load reporting success.**

Why one number could not work: the attack committed 3.3 GiB, while D10's validated scenario and the
project goal both require an instance to reach **several GB during startup**. Any single ceiling
loose enough to permit the latter also permits the former. The two cases differ in **shape**, not
size — one absurd request versus many ordinary ones — so the bound must too.

| Bound | Default | Role |
|---|---|---|
| `max_commit_request` | **128 MiB** | The tight one. This is what refuses the attack. |
| `max_committed` | **3.5 GiB** | The loose one. Stops an instance spending its whole address space; deliberately weak. |

**The tight bound is bracketed by measurement, not chosen.** The largest single private anonymous
request any real library makes is `libroblox.so`'s `.bss` at **11,575,296 bytes**; the next largest
across all eleven `.so` is **61,440 bytes**, 188x smaller. Nothing in the workspace can issue a single
request between 64 MiB and 1 GiB. So 128 MiB sits 11.6x above the largest real segment and 27x below
the demonstrated 3.3 GiB tamper, with nothing measured in between.

One honest qualification, from the re-review: the 64 MiB lower endpoint is a test's chunk size rather
than a requirement, so that endpoint is **soft** — but it is soft in the conservative direction, since
the hard floor is 11.04 MiB. It means 128 MiB could be lower, never that it is too low.

**The loose bound is floored by the requirement it must not break**: D10's 3 GB of live use plus its
measured `size/512` page-table charge, about 3078 MiB. It is capped below `DEFAULT_SPACE_SIZE`,
because a 4 GiB ceiling inside a 4 GiB space would bound nothing.

**Both halves are tested, and the second half is the one usually forgotten.** The tampered `p_memsz`
is asserted to be refused by `CommitRequestTooLarge` **specifically**, with the exact requested size,
so it cannot pass for the wrong reason via the total ceiling. And a legitimate 3 GB growth is asserted
to still succeed at the defaults — measured at +3078.020 MiB, i.e. size plus exactly `size/512`,
pinning D10's model rather than loosening a tolerance. Mutation rows in both directions exist:
tightening the per-request bound to 32 MiB and lowering the total to 2 GiB both fail the growth test.

**What these ceilings do not cover.** Copy-on-write charge raised by `protect`, and page-table charge,
sit outside **both**. Neither is an amplification vector today, because copy-on-write charge is
bounded by the size of the file being mapped — but a tampered binary can still provoke roughly
127 MiB per segment, up to the loader's own 256 MiB cap. That is bounded rather than unbounded, and it
is recorded here rather than left for someone to rediscover.

Cost if wrong: a per-request bound set too tight refuses a legitimate segment, which fails loudly and
immediately with both the requested and permitted sizes named — the opposite of the silent failure it
replaced.

---

## D16 — Stopping a runaway guest: which flag set, and what it costs
**Measured, then independently reproduced.** Guest code is untrusted by construction (D6) and
`libroblox.so` is a 109 MB binary we do not control, so a guest thread that will not stop itself must
be stoppable from outside. That is not automatic, and dynarmic's default configuration leaves guest
shapes that **no** mechanism can stop.

A runaway guest is stopped by one of two host mechanisms: a **cycle budget**, or a **cross-thread
halt**. Which one works depends on the terminal the guest's loop leaves its block through, and each
terminal is governed by an optimization flag:

- `LinkBlock` ends a **direct** branch. With `BlockLinking` **clear** it emits `ReturnFromRunCode` and
  stops there. With it set, it compares the cycle counter when cycle counting is on, and the halt flag
  when it is off — **one or the other, never both**.
- `PopRSBHint` and `FastDispatchHint` end an **indirect** branch (`BR`, `BLR`, `RET`). Their handlers
  compute a location descriptor and jump straight to the next block, reading **neither** the cycle
  counter nor the halt flag. Clearing `ReturnStackBuffer` and `FastDispatch` routes them to
  `ReturnFromRunCode` instead.
- `ReturnFromRunCode` is the **only** path that checks both.

Measured across three guest shapes, three flag sets and three escape configurations — **27 cells, each
in its own process**:

| Flags | direct-branch loop | indirect-branch loop | budget and halt both armed |
|---|---|---|---|
| `0x0000_FFFF` — dynarmic's default | one escape, chosen by cycle counting | **neither** | no |
| `0x0000_FFF9` — no RSB, no FastDispatch | one escape, chosen by cycle counting | both | indirect shapes only |
| `0x0000_FFF8` — also no `BlockLinking` | both | both | **yes, every shape** |

**A configuration in which every runaway guest is stoppable does exist: `0x0000_FFF8`.** It costs a
dispatcher round trip at every block boundary.

Cost, **n=31**, release, on the D2 host:

| Workload | `0xFFFF` | `0xFFF9` | `0xFFF8` |
|---|---|---|---|
| no indirect branches, 4-instruction blocks | 0.079 ms | 0.079 ms (1.00x) | 0.561 ms (**7.08x**) |
| 2 indirect transfers per 12 instructions | 0.217 ms | 0.988 ms (4.56x) | 1.541 ms (7.11x) |
| 2 indirect transfers per 4 instructions | 0.213 ms | 1.003 ms (4.71x) | 1.581 ms (7.43x) |

**The two costs scale differently, and that is the part to carry forward.** `0xFFF9`'s cost is **per
indirect transfer** (about 3.9 ns), so it tracks branch mix and is **zero** for a guest with no
indirect branches. `0xFFF8`'s additional cost is **per basic block**, so it tracks block *length*.

**Correction (Task 3).** This decision originally called 7x an upper bound, reasoning that the
benchmarks used 4-instruction blocks while "real code has longer blocks". A static scan of
`libroblox.so` shows **one control transfer every 4.30 words** — Roblox's blocks are *not* longer, they
are the benchmark's length. So **7x is the expected cost for this guest, not a loose upper bound**, and
the grounds for discounting it are withdrawn. That strengthens the case for `0xFFF9` over `0xFFF8`
wherever `0xFFF9` suffices.

For the other axis, `libroblox.so` measures **2.27% indirect, one indirect transfer every 44 words**.
Both figures are **static instruction mixes**, while the cost model is per transfer *executed*, so they
are proxies rather than measurements of the real cost: a static average cannot see the hot loop, which
for Roblox means Luau dispatch and C++ virtual calls. Treat them as informative, not as bounds.

**The practical consequence for the runtime:** under `0xFFFF` or `0xFFF9` with cycle counting on, a
cross-thread halt of a direct-branch loop is not ignored forever — it is honoured **when the budget
expires**. So a watchdog built on **short budget windows works at every flag set**, while one built on
a cross-thread halt alone does not. Build the watchdog from budgets.

**A footgun in the same code, reachable by accident:** the cycle comparison is **signed**, while the
budget is a `u64`. Any budget above `i64::MAX` — including the obvious `u64::MAX` for "no limit" —
reads as already spent, so every block returns to the dispatcher. Results stay correct; throughput
falls by roughly two orders of magnitude.

Cost if wrong: too permissive a flag set leaves a guest shape that can spin a host thread forever,
which for a multi-instance runtime is a denial of service. Choosing `0xFFF8` unnecessarily costs up to
7x on short-block code. The flag set is therefore configuration, not a constant.

Evidence: `crates/dynarmic-sys` tests and report; reproduced independently twice, cost figures agreeing
to within 7%.

---

## D12 (exception) — W^X does not hold for the translator's code cache
**Recorded because D12 claims something no longer true everywhere.** D12 states Omnidroid never holds a
page simultaneously writable and executable, enforced by the dual-mapped JIT arena. That remains true of
**our** arena. It is **not** true of dynarmic's own code cache.

That cache is a `VirtualAlloc` region in the runtime's address space, committed
`PAGE_EXECUTE_READWRITE`. Building the pin with `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT=ON` **makes
upstream's own test suite segfault** on the first A64 test, while `OFF` passes all 202,200 assertions —
verified by rebuilding upstream's suite both ways. The cost of W^X here is therefore not throughput; it
is that the component does not run.

**The exposure, stated honestly rather than comfortingly:**

> Under identity mapping there is no guest/host address separation to fall back on — by construction
> there is one address space — so **the W+X code cache is guest-writable in principle**, with nothing
> between a guest and it but not knowing where it is. That is ASLR, and ASLR is the only thing.
>
> **W^X is a mitigation the identity-mapping bet gives up**, not a property that survives because the
> guest is boxed in.

This is a previously unrecorded consequence of **D4**, the project's central architectural decision, and
belongs beside it rather than buried in a build flag.

Mitigations in place, none of which is W^X: the effective configuration reports the state; a test
asserts it and instructs withdrawal of this exception when it flips; a feature flag lets the next re-pin
retest; and the build script refuses the option with a reproduction rather than silently producing a
binary that crashes.

Cost if wrong: a guest able to write the code cache can execute arbitrary host code. The honest position
is that this is gated by ASLR alone, and fixing it needs either an upstream fix to the no-execute path
or a backend that never holds W+X pages.

---

## D3 (amendment) — the permissive set also includes BSL-1.0 and PSF-2.0
D3 required MIT / BSD / Apache-2.0 / ISC / 0BSD. Vendoring dynarmic brought in two licences D3 did not
name, both permissive, and they arrived because Boost was an **undeclared** dependency.

A full enumeration of the vendored tree found **376 files 0BSD, 214 BSL-1.0, 12 MIT** by SPDX tag, with
licence files covering ISC/0BSD, MIT, BSD-3-Clause and BSL-1.0, plus a **PSF-2.0** in fmt's
documentation that the crate's own licence inventory had missed. **No reciprocal licence appears
anywhere in the tree.**

**BSL-1.0** (Boost, Catch2) is permissive and explicitly **waives the notice requirement for object
code**, so it asks strictly less of us than MIT. **PSF-2.0** is likewise permissive. Both join the
permitted set.

The wider lesson: the dependency that brought them in was not declared by the component we adopted, so
the licence surface was wider than the adoption decision assumed. **Licence enumeration belongs in the
vendoring step, not in the decision that precedes it.**

---

## D5 (amendment) — risk 3 is worse than recorded, and has a working fix
D5 recorded that `ExclusiveMonitor` anti-scales 21x from 1 to 16 threads on a global spinlock, with
`fastmem_exclusive_access` as an **untested** mitigation.

Both halves are now verified on our pin. `LDXR` **leaves the fast path even with fastmem enabled**:
with `fastmem_exclusive_access` off, a single exclusive load costs 1 slow-path read plus 1 exclusive
callback — 2 callbacks. With it on, **0**. The counters are deterministic, so a sample size is not
meaningful here.

The risk therefore compounds exactly as D5 feared, and slightly worse: the engine's atomics leave the
fast path before contention is even considered. The mitigation works and should be enabled.

The interaction D5 already flagged still stands: declining to advertise LSE atomics via
`getauxval(AT_HWCAP)` steers the engine onto `LDXR`/`STXR` — straight into this path — so the two must
be decided together, not separately.

**The scale, measured (M2).** A static scan of `libroblox.so` over 17,485,957 words, pinned by a
self-checking tool:

| Class | Sites | Exclusive monitor? |
|---|---|---|
| `LDXR`/`STXR`/`LDAXR`/`STLXR` | 108 | yes |
| `LDXP`/`STXP`/`LDAXP`/`STLXP` | 20 | yes |
| `CAS`/`CASP` | 16 | LSE |
| `LDADD`/`SWP`/… | 37 | LSE |
| `LDAR`/`STLR`/`LDLAR`/`STLLR` | 15,516 | **no** — ordered, not exclusive |
| `LDAPR` | 0 | no |

So **128 exclusive-monitor sites against 53 LSE**, with LSE making up **29.3% of atomic
read-modify-write sites**. (Two words first classified as `LDXP` are `CASPA`/`CASPL`: `CASP` shares
`o2=0, o1=1` with the exclusive-pair encoding and is discriminated by bit 31 alone. Verified by an
independent decoder plus a capstone cross-check over all 15,697 classified words.)

This corrects a figure that briefly claimed 15,646 exclusive sites by counting the acquire/release
class as exclusives. The conclusions that followed from it were all wrong and are withdrawn: risk 4
is **not** moot — each of the 51 LSE sites is a hard halt into the interpreter — risk 3 is **not**
backed by tens of thousands of call sites, the `AT_HWCAP` question is **live** rather than empty, and
`fastmem_exclusive_access`'s benefit was overstated by two orders of magnitude.

The wrong version never entered this file, because a figure is recorded here only after someone other
than its author reproduces it. This is the fourth time that rule has caught a wrong number in two
milestones, and the most clear-cut: the mistaken figure was the *convenient* one, since it closed an
open question rather than keeping it open.

Separately measured, and worth keeping because it was nearly asserted instead: the 15,516 ordered
accesses **do** stay on the fastmem path — measured at n = 1,000 per class, `LDR`/`STR`, `LDAR`/`STLR`
and `LDXR`/`STXR` all take **0** callback entries, with a mutation row that turns
`fastmem_exclusive_access` off and is caught by it.

### The finding that resolves the coupling: `AT_HWCAP` is not merely live, it is the control

All 53 LSE sites reference **one byte** — `0x683ba58`, compiler-rt's `__aarch64_have_lse_atomics` —
through **outline atomics**, and **106 of the 128 exclusive sites are those same helpers' fallback
arms**. So Roblox does not contain two independent populations of atomic code. It contains one
population behind a runtime branch on a single flag, and **Omnidroid owns that flag**, because it
implements `getauxval(AT_HWCAP)`.

That turns D5's "these two risks must be decided together" from a coupling into a **switch**:

- **Advertise `HWCAP_ATOMICS`** → the 53 LSE sites are taken, and each is a hard halt into the
  interpreter on this pin (risk 4).
- **Decline it** → the 106 fallback arms are taken instead, into the global exclusive-monitor
  spinlock that anti-scales 21x from 1 to 16 threads (risk 3).

Neither is free, but the choice is now a single bit we set, with both arms measured rather than
guessed, and it is mechanically pinned by a test. The earlier claim that "each LSE site is a hard
halt" is therefore true **only if we advertise the capability** — which is exactly the decision this
amendment leaves open for M3, now with the evidence to make it.

---

## D4 (amendment 2) — the startup assertion defends the configuration; a second check defends the behaviour

**Added at M2, from a defect class Task 3 found and could not close.**

D4's startup assertion reads back dynarmic's live `UserConfig` once per context, before any guest
code runs, and refuses anything that is not identity mapping. That is the right check and it is
worth what D4 says it is worth. But it is a check on a **configuration**, and Task 3 found two ways
the memory path degrades *after* it has passed:

1. two guest threads faulting on pages of one 64 KiB commit granule made the second decline, which
   handed the fault to dynarmic's frame-based handler, which recompiled the block with fastmem off
   **permanently**;
2. a panic inside the fault handler declined a resolvable fault while incrementing no counter at
   all.

Both leave correct results and a runtime 30-49x slower on the affected blocks. The startup
assertion structurally cannot see either, because nothing about the configuration changed.

**The class-level answer, which is now implemented:**

> Per run slice, the callback-path counter's delta must be zero unless that slice ended in a
> memory-fault exit.

It cannot be phrased as "the counter stays at zero". A legitimate typed `MemoryFault` *arrives*
through that same callback and increments the same counter, so that phrasing would fire on the
normal case and be turned off within a day. The exemption is exactly one exit kind, and it has its
own test so that it cannot silently become dead code.

**What it costs.** `dynarmic-sys` gained `od_jit_slow_path_total`, which is one load rather than
`od_jit_stats`'s 72-byte struct copy, and the run loop reads it twice per slice. Measured on the D2
host, release:

| Quantity | Figure |
|---|---|
| one counter read | **0.396-0.430 ns** (two runs, median of n = 31 runs of 10,000,000 reads each) |
| per slice (two reads) | 0.79-0.86 ns |
| a 5,000,000-instruction workload, armed vs disarmed | **0.9906x** and **0.9989x** (two runs, n = 31 per configuration each) |

A slice is 1,000,000 guest instructions by default, so the whole check costs about 4 nanoseconds
across a five-million-instruction run. The end-to-end ratio is below the noise floor, which is why
the per-read figure is given as well: a ratio of 1.00x on its own would not distinguish "free" from
"not measured".

**Where it is disarmed, and why that is reported rather than assumed.** The check is off when the
backend does not own guest paging, because the callback path is then the *designed* route for a
first touch rather than a degradation. `DynarmicBackend::slice_invariant_armed()` reports what is
really in force, so a test cannot claim a check that is switched off.

---

## D10 (correction) — a demand pager that could not be installed was silently accepted

**Found at M2, and it is the same shape as the defect above.**

`DynarmicBackend::new` installed the demand pager with `.ok()`, which flattened two different
failures into one. `FaultError::Unsupported` means *this platform has no vectored-handler
implementation* — a documented state the backend still works in. `FaultError::HandlerTableFull`
means the platform has one and could not give us a slot, and a backend that carries on from there
sends every guest fault to dynarmic's own frame-based handler, which recompiles the block with
fastmem off for good: 30-49x, correct results, no error anywhere.

It was not hypothetical. `MAX_HANDLERS` was **8**, and its own documentation said the slack existed
for tests that build several address spaces in one process — but `omni-cpu`'s suites build one per
test and `libtest` runs them in parallel, so a binary with ten tests could exhaust the table. The
symptom was intermittent and depended on how libtest happened to schedule.

Two changes: `MAX_HANDLERS` is **32**, and `DynarmicBackend::new` refuses anything but
`Unsupported`, with a message naming both the resource that ran out and the 30-49x that carrying on
would have cost. `crates/omni-cpu/tests/pager_exhaustion.rs` fills the table on purpose — in its own
process, because otherwise it would starve its neighbours — and pins the refusal.

---

## D5 (amendment 2) — cold translation and per-thread cost, measured on real Roblox code

Every CPU figure in this document before M2 came from synthetic loops of four to eight instructions
written to isolate one effect. These are from **870 real `libroblox.so` leaf functions**, selected
by `omni-elf`'s `leaf-scan` out of the 245,117 that `.eh_frame_hdr` names, executed through the
backend with a bionic TLS block and a guest stack.

| Quantity | Synthetic (D5) | Real Roblox leaves |
|---|---|---|
| cold translation | 0.15-0.31 Mguest-insn/s | **0.516 Mguest-insn/s** (n = 11 passes, median, a fresh context each; 870 functions, 8,679 guest instructions) |
| warm | — | 156.7 Mguest-insn/s (n = 31 passes, median) |
| per-thread commit | 20-35 MiB | **24.5 MiB** (n = 8 threads, serialized) |

**Cold translation is 1.7-3.4x *better* than the synthetic figure, not worse.** That much is
measured. The *explanation* — that the synthetic benchmark translated a tight loop, where nearly
every translated instruction is a loop body dynarmic's IR optimizer works over repeatedly, while the
real leaves are short straight-line-plus-branch functions averaging 9.98 executed instructions where
the optimizer has much less to chew on — is a **hypothesis the figures are consistent with, not one
they establish**. The one testable half is measured: per-instruction cold cost **rises** with
function length, 0.698 Mguest-insn/s for the shortest third of the leaves against 0.494 for the
longest, which is the opposite of a per-function-overhead model and the direction the hypothesis
needs. It is still not a test of it; the same curve would appear if any optimizer pass were
superlinear in block size for unrelated reasons. Settling it means instrumenting those passes, or
translating the same instruction count once as a loop and once as straight-line code.

So D5's 0.15-0.31 remains the right figure for *loop-shaped* code and the 7-25 s warm-up estimate
that follows from it is not improved; what is new is that the long tail of small functions costs
less than the estimate assumed.

**The warm figure is not a steady-state throughput and must not be read as one.** A warm pass is 870
entries to and exits from `od_jit_run` around 8,679 instructions of work, so it measures the *call*
and not translated code. And the per-call figure is itself a **ceiling** rather than the boundary:
of 63.7 ns per timed iteration, 10.7 ns is the harness's own eight `set_x` calls plus `rearm`
(measured directly, by running the same loop with the `run` removed), and the remaining **53.0 ns**
still contains about ten guest instructions of real work. So the call boundary costs **under 53 ns**,
and that is the number M3 should plan against — every imported symbol becomes a thunk exit and a
re-entry. Isolating it exactly would need a guest function of zero instructions. The steady-state
comparison point remains D5's table.

**Where the per-thread cost comes from.** It tracks `code_cache_size` plus a fixed term and not
translated volume: 24.5 MiB at this backend's 8 MiB cache, of which 16 MiB is
`A64EmitX64`'s `std::array<FastDispatchEntry, 0x100000>`, constructed and zeroed by the constructor
whether or not the FastDispatch optimization is enabled — and this backend disables it (D16). The
figure is now **asserted against a 32 MiB ceiling** rather than merely printed — a ceiling that is
fitted, with the measurement plus about 30% of headroom, and that sits below the top of D5's band so
the 128 MiB-cache configuration would fail it.

`GuestCpu::cost()` reports **16.004 MiB** of the 24.525, from two *derived* terms: the guest's TLS
page, and the 16 MiB `FastDispatchEntry` table this pin allocates per jit whether or not the
optimization that uses it is enabled. What it still misses is the code cache's committed high-water
mark, a private member of `BlockOfCode` that `A64::Jit` does not expose, and that omission is
**bounded and asserted** rather than admitted: the gate checks that `cost()` never exceeds what was
measured and that the gap stays under `code_cache_size` plus a named 2 MiB allowance. Writing that
bound as an assertion is what showed that `code_cache_size` alone does *not* bound it — the gap is
8.521 MiB against an 8 MiB cache, so about 536 KiB per jit is `JitState`, the block-range map,
xbyak's labels and the two shim allocations.

---

## D13 (confirmed) — verified on real engine code, in both directions

D13 was inferred from a static count: 1,282 `MRS Xt, TPIDR_EL0` instructions, 1,276 of them loading
`[Xt, #0x28]`. M2 ran one of them.

`libroblox.so + 0x2872aac` is one of **45** stack-guard-protected leaves the scan found. With a
bionic TLS block programmed it reads the guard, stores the canary on its frame, reads the guard
again, compares, and returns `0x20000`. Three directions are asserted, because the positive one
alone would pass on a runtime that never executed the comparison:

* with the guard matching, it returns — and the `BL __stack_chk_fail` in its failure tail is
  registered as a **thunk**, so a taken call would be a typed exit rather than something invisible;
* with the guard changed in guest memory **between the two reads** — through a breakpoint on the
  reload — real engine code calls `__stack_chk_fail`, which is what proves the value it compared
  came from `[TPIDR_EL0, #0x28]`;
* with `TPIDR_EL0` pointed at an unmapped address, the fault is at **exactly** thread pointer plus
  `0x28`, which pins the offset itself.

One correction to the phrasing D13 uses: the guard is read through a register the function keeps,
not re-read from `TPIDR_EL0`, so a guest that re-points its thread pointer mid-function still
compares the old block's guard. That is bionic's own behaviour and not a runtime concern; it is
recorded because the obvious test — change `TPIDR_EL0` between the reads — does not work, and the
next person will try it.

---

## D9 (correction) — `.eh_frame_hdr` is an exact function map, and nothing relocates into the text

Two facts M2 needed and measured, recorded here because both were assumptions before.

**The function map is exact and complete enough to select from.** `PT_GNU_EH_FRAME`'s binary-search
table names **245,117** functions with their exact lengths, read back and cross-checked against the
FDEs they point at. Exactly **one** describes an empty range, at `0x364f404`; it is kept rather than
refused, because rejecting a 245,117-entry map over one entry would be the wrong trade and dropping
it silently would make the count disagree with `fde_count`.

**Not one of the 568,806 relocations lands in an executable segment.** D9 already recorded that
there is no `DT_TEXTREL`; this is the stronger statement, measured rather than inferred, and it is
what makes "no relocation-bearing loads" checkable rather than a hope — the bytes in the file at a
function's address really are the bytes that execute.

**And a bound on the map that is derived rather than chosen.** Functions do not overlap, so the sum
of their lengths cannot exceed the executable segments they live in: **69,943,828 against
103,645,584**, 48% of headroom for a real object. The scan refuses a map that breaks it. The reason
is cost rather than correctness — a corrupted `.eh_frame` whose lengths decode to a few kilobytes
each turns a quarter-second scan of 245,117 entries into gigabytes of decoding that produces
nothing, which is a denial of service on an analysis tool from exactly the input D6 says to expect.
---

## D10 (correction 2) — the fault-handler contract was insufficient, not violated

**Found by the whole-branch review at M2, at the seam between `omni-platform` and `omni-mem`.**

`fault::install` asked callers to keep the handler's `context` valid "until the returned
registration is dropped", and `release` cleared the slot with a release store and returned.
`omni-mem`'s `DemandPager` satisfied that exactly: it declares its registration field before the
state the handler reads, so the slot is cleared first. The contract was still not enough, and
neither side alone could show it.

A vectored handler is **process-wide**. It runs on whichever thread faulted, for whatever reason —
another guest instance, the host allocator, a stack probe — and it can be preempted between loading
the handler pointer and calling it. Clearing a slot stops calls that have not read it yet; it cannot
stop one already in flight. So `handle_fault` could read `base`/`end` out of a freed `Box`, inside
the OS exception dispatcher. Slot reuse compounded it: a stale in-flight call could be paired with a
**new** registrant's context.

Nothing a caller writes closes that window, so it is closed at the seam. `release` is now a
**quiescence point**: a per-slot in-flight count is taken before the handler pointer is read and
released after the handler returns, and `release` unpublishes the slot and then waits for the count
to drain. All four accesses are `SeqCst`, deliberately — each side writes one location and reads the
other, which is the store-buffer shape that acquire/release does not order, and the argument is
written out in `fault/windows.rs`. Quiescence subsumes a generation tag: a stale pairing is not
detected, it is unrepresentable, so slots are still reused and `MAX_HANDLERS` still means a capacity.

**The race was reachable in-tree, and is measured.** `omni-platform/tests/fault_teardown_race.rs`
tears a handler down under load — n = 24 rounds x 4 threads x 48 pages — and **6 of the 24 releases
had to wait for a dispatch that was already inside the handler**. Zero handler frames observed their
context after the release returned; under the previous contract the same test reports non-zero.

The general lesson, and it is the third of this shape in the project: **a contract that every caller
satisfies can still be the defect.** The two per-task reviews each saw one side and each concluded
correctly about it.

---

## D5 (amendment 3) — a processor id must outlive the jit that holds its monitor entry

`DynarmicCpu::drop` released the processor id and then freed the jit. In that window another
thread's `build` can take the id and construct a jit against the **same entry of the shared
`ExclusiveMonitor`** as a jit that is still alive. Two guest threads on one entry makes `STXR`
succeed where the architecture requires it to fail — a silent wrong answer in the subsystem this
decision already lists as risk 3 of 4, with no error anywhere and no test that would notice.

The statements are swapped. Because the failure has no symptom of its own, the order is pinned by a
witness rather than by a comment: `release_processor_id` is told whether the jit is already gone, and
`DynarmicBackend::processor_ids_released_early` counts the times that claim was false. It must stay
0, and `tests/lifecycle.rs` says so over ordinary create/drop churn and over the construction
failures that release an id without a jit ever existing.

---

## D5 (amendment 4) — `CNTPCT_EL0` was the instruction counter, and the guest reads it as a clock

The `GetCNTPCT` callback returned the backend's per-slice guest-instruction count. `run` zeroes that
at the top of **every slice** — a million instructions by default — and again at the start of every
run, so the counter a guest reads as a monotonic clock sawtooths. Two reads and a subtraction give a
negative interval, and a spin-until-deadline loop never terminates; it is eventually stopped by the
step budget and reported as `StepLimitReached`, which points at the budget. The comment above the
callback said "monotonic". The units were wrong in the same place: one tick per guest instruction
against the 600 MHz `CNTFRQ_EL0` dynarmic advertises by default.

It is now the host's monotonic clock scaled to `CNTFRQ_EL0`, from one process-wide epoch, and
`cntfrq_el0` is programmed from the same constant that scales it rather than left at 0 to pick up
dynarmic's default — two defaults that happen to agree is not one constant used twice. The value is
unchanged at **600 MHz**.

The alternative the review offered — an accumulator of guest instructions that slices do not reset —
was rejected for a reason worth recording: it cannot be given honest units. Guest instructions per
second is not a constant, so no value of `CNTFRQ_EL0` makes `ticks / CNTFRQ` a time, and this is the
counter behind `clock_gettime(CLOCK_MONOTONIC)` on AArch64 Android. Determinism was the thing given
up, and it was not the thing a guest clock is for.

Measured: **12,006,840 ticks over 20.0175 ms** of wall clock at 600 MHz (n = 1 interval, busy-waited
rather than slept because Windows' sleep granularity is ~15 ms), and monotonic over n = 100,000
consecutive reads. Bounded in both directions on purpose — the upper bound is what pins the units,
and it needed a warm-up run before the window to be tight enough to catch "the counter returns
nanoseconds".

**Carried into M3, unchanged by this:** `ARCHITECTURE.md` section 6 records that `CNTVCT_EL0` is not
implemented on this pin and surfaces through the interpreter fallback, and Android's `clock_gettime`
vDSO reads `CNTVCT_EL0`, not `CNTPCT_EL0`. So the engine's real clock path still traps. Fixing
`CNTPCT_EL0` was necessary and is not sufficient.

