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
and not translated code. Note this **is** an entry-and-exit figure; a later brief of mine described it
as "an entry ceiling excluding the exit", which is wrong and was propagated into an M3 report and three
code comments before being caught. And the per-call figure is itself a **ceiling** rather than the boundary:
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


---

## D17 — The thunk boundary dispatches inside the run loop
**Measured before building, which is the point.** M3 needs every imported symbol to cross from guest
ARM64 into host Rust, and the two candidate designs differ by more than the difference between
convenient and inconvenient.

| Design | Cost per call | Notes |
|---|---|---|
| **A — exit to Rust per call** | **≈ 80-105 ns** (81.2 via a PLT stub, 102.8 direct) | **Unstable by about 2x**; see the open question below |
| **B — dispatch inside the run loop** | **≈ 33 ns**, including the floating-point guard | Stable across every cell measured |
| Ratio to plan against | **3x** | Measured 2.5-3.1x here, 3.00x independently |

Method: 15 processes × 3 code placements, n = 31 per cell per process, committed as
`tools/thunk_sweep.py` — which **refuses to print when a load-bearing cell is unstable**, judging on
the interquartile ratio, because one preempted round and a genuine second mode look identical to a
max/min check.

**Decision: dispatch inside the run loop, per symbol**, keeping the exit path for unresolved imports
and for anything that must call back into guest code. Verified not to weaken the runaway-guest
defence: the halt check lands on `ReturnFromRunCode`, which D16 established is the only path that
tests both the halt flag and the cycle counter.

**A silent-corruption class the faster design brings with it.** An inline handler runs with the
**guest's MXCSR**, because dynarmic's supervisor-call emitter omits the control-word switch that the
exit path performs — verified line by line in the pin. Host floating-point code would then execute
under guest rounding and denormal settings, and `exp`, `log`, `powf` and `sincosf` are all among the
reachable imports, so the corruption would be numerical and silent.

The guard costs **1.1 ns**, about 3% of design B, and belongs in the **dispatcher rather than each
handler** — a forgotten guard is invisible — and must restore the *guest's* word on the way out, not
only set the host's on the way in. It is asserted **from inside the guest**: a denormal multiply after
the call must flush to +0 under `FPCR.FZ`, in both directions, with both halves ablated and pinned by
mutation rows. Worth recording why it is nearly free: `switched` is false by default, so it reduces to
a single `stmxcsr` that stalls only on in-flight SSE and otherwise hides behind independent work.

### Open question, recorded rather than resolved

Two figures from the same tree do not reconcile. An `od_jit_run` entry-and-exit measures **≈ 41 ns**,
and a design-A round trip **is** one entry and one exit — yet it measures **81-103 ns**. During review
the entry/exit path was directly observed to be **bimodal** on this host: the same context and cell
gave 41.8 / 89.0 / 91.7 ns across three rounds, with host frequency, thermal drift, live contexts,
code placement, entry count and exit reason all ruled out, and design B immune at 24.9 / 25.2 / 25.2.

A later attempt to make that bimodality a committed measurement — two tests named to sort at opposite
ends of the run order — **did not reproduce it**: 41.38 ns first, 41.27 ns last, ratio 1.00 across 15
processes. The original observation was direct, so it stands, and design A is therefore quoted as a
band on its strength rather than as a point. But the discrepancy is unexplained, and the sweep tool
now prints both positions and their ratio on every run so that the next sighting arrives with numbers
instead of a recollection.

This is recorded as an open question because design B is chosen under every regime measured, so the
ambiguity changes no decision — but it would change the cost of ever going back to A.

### Scope this sets for the rest of M3

Of `libroblox.so`'s 565 imports, **188 are statically reachable** from the 3,594 `init_array` roots
(band 113 / 188 / 246 / 565; closure 15,779 of 245,117 functions). Of those, 18 are `STT_OBJECT` data
and 2 are `STT_NOTYPE`, so the thunk surface is **170 functions plus 18 data objects**.

188 is a **lower bound**, and honestly so: the closure contains **17,698 unresolvable indirect call
sites**, and the function map has a **2,670,684-byte region with no unwind information** that hides
one initializer entry point — recovering it is worth exactly 67 of the 188. Treat the figure as a
superset prediction to scope work against, never as a completion criterion.

---

## D18 — The thunk boundary's shape, and the four places the ARM64 path nearly closed

D17 decided *where* a thunk is dispatched. This is what the crossing itself is, built in M3 task 2.

### The two paths, and why the type system separates them

| Path | Handler | May run guest code | Cost |
|---|---|---|---|
| inside the run loop | `ImportFn` over `ImportCall` | **no — structurally** | **26.7-31.0 ns** |
| out through `ExitReason::Thunk` | `ReentrantFn` over `ReentrantCall` | yes | 80-102 ns |

Re-measured after the mechanism changed, with `tools/thunk_sweep.py` unchanged: **15 rounds × 3 code
placements = 45 processes, n = 31 per cell per process.** The inline figure is a band across three
cells (26.69 / 28.73 / 30.96 ns medians; bare, with an eight-argument marshal, and through a real PLT
stub) rather than a point, and it supersedes D17's "≈33 ns" without contradicting it. Design A in the
same processes: 81.36 / 91.31 / 100.31 / 100.53 ns. **The 3x ratio D17 planned against is unchanged**,
and D17's open question about A's bimodality is untouched — every A cell came out stable again, and
the entry/exit probe still measures 41.4 ns for what a design-A round trip *is*.

**An inline handler cannot reach a `GuestCpu`**, because `ImportCall` does not contain one. That is
not a convention: an inline handler runs inside one of the translating backend's own callbacks, where
a `&mut CpuCtx` is live, so re-entering `od_jit_run` from there would form a second one — undefined
behaviour. A handler that needs guest code run, or that has a typed error and no return channel, calls
`ThunkCall::defer_to_caller()` and the call becomes an ordinary `ExitReason::Thunk` at the same
address with the guest `PC` left on the thunk. One mechanism for both escalations.

### The region: two areas, and a slot that is four times wider than it needs to be here

The **function** area is mapped `Protection::Read` — deliberately not executable — and lazily
committed, so it costs no commit charge and a branch into the middle of a slot is refused on
protection *before* `admit`'s commit rule. `Boundary::run` then re-describes that execute fault
through the symbol table, so the error names the symbol whose slot it was inside and the offset. The
**data** area is `ReadWrite`, because the 18 `STT_OBJECT` imports are loaded from and two of them are
written.

A slot is **16 bytes**, derived from `VENEER_INSTRUCTIONS = 4` rather than chosen. On the translating
backend one word would do, or none; on an ARM64 host the backend plants a veneer and the guest's `BL`
really executes it, and the smallest veneer that reaches an arbitrary 64-bit host address is
`LDR X16, #8` / `BR X16` / `.quad host_entry`. The slot size is baked into every address the loader
writes into a relocated `GOT` slot, so a 4-byte layout would have had to change after 568,806
relocations already referenced it. The cost is 2,720 bytes of address space that is never read.

### Four places the ARM64-native path nearly got foreclosed, two of which already had

1. **The handler signature was dynarmic-only.** Task 1's probe took `fn(&mut InlineThunkCall)`, and
   that type lives in `omni-cpu::dynarmic`, which does not exist on an ARM64 host at all. `ThunkRegs`
   is a trait now and `ThunkCall` is backend-neutral.
2. **`set_return_sentinel` was on `DynarmicCpu`, not `GuestCpu`.** Section 5 promises the host-to-guest
   direction; the mechanism that makes a call *into* guest code finish was reachable only through one
   backend's concrete type, and the compatibility layer holds `&mut dyn GuestCpu`. On the trait now,
   with `return_sentinel()` beside it because a nested call must restore what the outer one armed.
3. **A 4-byte slot.** See above.
4. **`Capabilities::inline_thunks`.** A backend answering `false` gets every call through the exit
   path — three times slower and identical in behaviour — and `add_inline_thunk` *refuses* rather than
   registering a handler that would never run and return a fabricated zero for every import.

**None of this is evidence the ARM64 path works, and it is not claimed to.** What is established is
that no type in `omni-android` names a backend, that the crate builds with `--no-default-features`
with no `dynarmic-sys` in its normal dependency tree, and that the slot fits the veneer.

### Variadics: thirteen of the reachable 188, and three ways to be silently wrong

**Nine true variadic** — `fprintf`, `fscanf`, `snprintf`, `sscanf`, `syslog`, `open`, `prctl`,
`syscall`, `__android_log_print` — and **four `va_list` consumers** — `vsnprintf`, `vfprintf`,
`vasprintf`, `__vsnprintf_chk`. Note that `printf` itself is **not** reachable and `fprintf` is;
`vsscanf` is not and `sscanf` is; and `__open_2` looks variadic and is not.

AAPCS64 passes variadic floating point in **`V0`-`V7`**, exactly as it passes named arguments. Apple's
arm64 puts *all* variadic arguments on the stack and Windows on ARM64 puts variadic floating point in
the general-purpose registers, so an implementation written from either rule reads the wrong bytes and
returns a plausible number. The evidence for the first is bionic's own `va_list`, which has `__vr_top`
and `__vr_offs`: a structure with nowhere to record a floating-point save area would describe a
platform that does not have one.

Three silent-wrong-number classes, each pinned by a mutation row:

* a variadic `float` has been **promoted to `double`** by the caller, so reading it back as a `float`
  reads the low 32 bits of a `double`'s pattern — for `1.0` that is exactly `0.0`;
* the **SIMD save area's slot is 16 bytes**, because it holds `Q` registers, and a walker stepping by
  8 lands in the previous argument's zeroed upper lanes and also returns `0.0`;
* `va_list` is **32 bytes**, so AAPCS64 passes it *indirectly* — `X3` for `vsnprintf` holds a pointer
  to the record, and a marshaller reading 32 bytes out of `X3`-`X6` reads something else entirely.

A `va_list` is guest-written state, so both offsets are range-checked *and* every read goes through
`GuestMem`. Neither check is redundant: the first says which field was wrong, the second is what
catches a wild `__gr_top`. A **positive** offset is legal and means the registers are spent, and is
normalised rather than refused.

**A fourth register the boundary needs.** `mallinfo` returns 80 bytes, so AAPCS64 returns it
**indirectly through `X8`** — neither an argument register nor a return register. One import of 188,
and a marshaller built to "returns in `X0`/`X1`/`V0`" would have had no way to reach it.

### Re-entrancy, and a bound that was not one

Depth is capped at **8** (`AbiError::TooDeep`) because the alternative bound is the host's stack, which
is an abort reachable from guest data. `call_guest` saves and restores the whole architectural state;
the callee-saved half is load-bearing and pinned, and the caller-saved half is **defence in depth with
no test that can distinguish it**, recorded as a watch rather than a detector. The outer call's
arguments are snapshotted before the handler runs, so a handler that reads its third argument after
invoking a comparator gets the argument rather than the comparator's leftovers.

**A defect the mutation harness found by hanging rather than failing.** The driver passed the caller's
`RunLimit` to *every* `cpu.run`, so a counted budget bounded each exit-path segment rather than the
run: a guest crossing N times got N times the allowance it was given. `GuestCpu::last_run_instructions`
is on the trait now and the budget is spent down. The exit path also has its own crossing cap
(`AbiError::CrossingLimit`), because a guest looping through it returns to Rust before any block
finishes and the backend's budget never expires — which is *not* true of the inline path, where every
iteration costs counted guest instructions.

### Scope note that reconciles two figures

The M3 plan says 23 imports are `STT_OBJECT`; D17 says 18. **Both are right and they count different
sets: 23 of the 565 across the library, 18 of the 188 the initializers reach.** Measured, not argued —
`crates/omni-android/tests/libroblox.rs` declares exactly D17's eighteen and asserts that the five
`STT_OBJECT` imports left unresolved are the difference. Every unresolved import is `STT_OBJECT`; no
function is left bound to null.

---

## D19 — `omni-bionic` stays a separate crate, because "no OS access" should be checkable

**Decision.** The bionic libc/libm implementation stays in its own crate, `omni-bionic`, rather than
folding into `omni-android`. `omni-android` depends on it. ARCHITECTURE §2 is amended to list it.

**Why this was open.** The crate was created only to isolate 12,543 lines of unreviewed work during
review, and §2 assigns "bionic libc/libm" to `omni-android`. With the review finished, the reason to
keep it separate had expired and the question was whether to fold it back.

**The argument that decided it, and it is structural rather than aesthetic.** `omni-bionic` has
**zero dependencies** — verified, `cargo tree -p omni-bionic -e normal` is one line. It is the only
crate in the workspace of which that is true. Its own design rules claim "No OS access. The crate has
zero dependencies, no `cfg(target_os)`, and compiles unchanged on every host", and today that claim
is not a convention anyone has to remember: the crate **cannot** reach an OS primitive, because it has
nothing to call.

`omni-android` pulls `omni-cpu` → `omni-mem` → `omni-platform`, and `omni-platform` is where
`windows-sys` lives. Folding bionic in would put 12,543 lines of pure computation inside a crate that
transitively links the OS bindings, and the portability guarantee would downgrade from *impossible*
to *against the rules*. Given the five-target requirement, and that this project has twice come close
to foreclosing the ARM64 hosts by accident (D18 records four such places, two of which had already
happened), a guarantee that `cargo tree` can check beats one a reviewer has to notice.

**An argument that was considered and is WRONG, recorded so it is not made again.** "Folding it in
would couple the libc tests to the C++ translator build." It would not: `cargo tree -p omni-android
-e normal` contains **no `dynarmic-sys`**, which is the `--no-default-features` property HANDOFF
already records as verified. `omni-android` is dynarmic-free. The build-time argument does not exist;
the dependency-surface argument above is the whole case.

**Cost if wrong.** Low and reversible. One extra crate in the workspace and one more line in the
dependency graph. If a later task needs bionic and the thunk boundary to share a private type, the
fold is a mechanical move — the dependency direction is already `omni-android` → `omni-bionic`, which
is downward, so nothing about D-record's "strictly downward" rule has to change to undo this.

**What this does not decide.** Where the *adapter* lives. The adapter binds bionic's functions to the
thunk boundary and needs both, so it belongs in `omni-android` — that is where the boundary is, and it
is the crate §2 already names for the compatibility layer.

---

## D20 — The bionic adapter's shape, and a count that was wrong by nine in the convenient direction

D19 decided that `omni-bionic` stays a zero-dependency crate and that the **adapter** binding it to
the thunk boundary belongs in `omni-android`. This is that adapter, built in M3 task 3 phase 1.

### The count first, because it was wrong

HANDOFF said **"of the 188 reachable imports, 88 are already implemented in `omni-bionic`"**, with
the method stated as "a symbol counts as implemented when a doc comment naming it sits above a
`pub fn`", and with a warning that a naive grep over-counts because `pthread_sigmask` appears in a
comment that *excludes* it.

Re-measured by running that method rather than approximating it:

| method | count |
|---|---|
| any mention of the symbol anywhere in `crates/omni-bionic/src/**.rs` | **89** |
| the same, minus the one known excluding comment (`pthread_sigmask`) — **how 88 was produced** | 88 |
| a doc comment naming the symbol directly above a `pub fn` — **the documented method, run** | **82** |
| the same, minus five where the "symbol" is an ordinary English word in unrelated prose | **77** |
| plus two implemented under a name that does not spell the C symbol | **79** |

The five prose false positives are `abort`, `access`, `clock`, `read` and `time`: each matched a doc
comment above a `pub fn` that has nothing to do with it (`access` alone matched five different
functions' prose). The two under-counts are `strerror`, which is `string::strerror_message` plus an
adapter-supplied buffer, and `__vsnprintf_chk`, which is `printf::format` plus the boundary's
`va_list` walk — the same shape as `pthread_cond_timedwait`, which HANDOFF already records as
implemented-but-unspelled.

**79, not 88.** The error is nine symbols and it makes the remaining work look smaller, which is the
direction this project has been wrong in before (D5's exclusive-monitor miscount, twice). The
documented method is itself the weak link: matching a symbol name inside prose is not distinguishable
from matching a declaration, and no mechanical rule will be, which is why the adapter now carries a
**test** that parses `init-reachable-imports.txt` and asserts every bound symbol is in it.

### What phase 1 binds

**86 of the 188**, asserted exactly by `the_bound_count_is_exactly_what_this_phase_claims`:

| | count | what |
|---|---|---|
| serviced | **81** | 79 backed by `omni-bionic`, plus `__errno` and `vsnprintf` which the adapter composes |
| refused by name | **5** | `fprintf`, `vfprintf`, `vasprintf`, `sscanf`, `fscanf` |
| left `Unbound` | 102 | files, clocks, process info, sockets, logging, thread lifecycle, `dl*`, the data symbols |

84 are serviced **inside** the run loop; two — `pthread_once` and `qsort` — are on the exit path,
because both call guest code and D18 makes that a type property rather than a rule.

The five refusals are **bound rather than left `Unbound`** deliberately. `Unbound` says "nothing
implements this"; a refusal says *which missing piece*, and three thousand initializers deep that is
the difference between a lead and a shrug. `fprintf` names the guest `FILE *` it was handed and says
`omni-platform` is virtual memory and faults only; `vasprintf` says the result must come from the
guest's heap and that `libroblox.so` imports no allocator; `sscanf` says there is no scanning engine
and that every partial answer would write a wrong value through the guest's output pointers.

### Three things the adapter had to decide

**1. A handler is a bare `fn`, so the per-instance state lives in a thread-local.** `ImportCall`
carries no user data. A process-wide `static` would be wrong rather than merely ugly: the runtime is
designed for three concurrent guest instances (D10's measured ~50 MiB for three), and one static
would give them one `pthread_key` table and one mutex owner table. So `Bionic` is per-instance and a
caller holds an `Activation` across `Boundary::run`. A handler that finds none returns
`AbiError::BionicNotActive` naming the symbol; it does **not** construct a default, because a
per-call default would give two guest threads their own private copy of the same mutex, and two
threads that each believe they hold it is the failure no later test can see.

**2. `errno` is guest-visible storage, so the adapter maps an arena — before any CPU exists.**
`__errno()` returns a pointer the guest dereferences and `strerror` returns a pointer to a per-thread
buffer, so both need memory the guest can read. One 17 KiB mapping, one 272-byte block per thread, 64
blocks, and a 65th thread is a refusal rather than two threads sharing an `errno` slot. The mapping
happens in `Bionic::new`. That is **task 2 review F9's constraint**: `ImportCall::mem()` reaches the
whole `GuestSpace`, so a handler *could* map while generated code is live, and nothing in this phase
does.

**3. The futex ignores `expected`, and that is measured rather than convenient.** Linux's
`FUTEX_WAIT` compares `*addr` with `expected` atomically with the decision to block. This one does
not, because **`omni-bionic`'s own callers do not all pass a meaningful `expected`**: `mutex::lock`
passes `LOCKED_WITH_WAITERS`, which is right, but `rwlock`'s reader and writer waits both pass `0`
(`rwlock.rs:281`, `rwlock.rs:360`) while the word they wait on is, by construction, not zero — a
rwlock with waiters is held. A futex honouring `expected` would return `WouldBlock` to every rwlock
waiter and the caller's `continue` would turn blocking contention into a busy spin.

**Recorded as a finding about `omni-bionic`, not patched there.** Those constants are in a reviewed
crate with its own mutation harness and changing them changes what `wait` means for every caller at
once. The lost-wake window is closed the way that crate's callers already close it: every waiter
re-checks its predicate after every return, and every `wake` is issued after the state change the
waiter will observe. The implementation is `parking_lot_core`'s parking lot — a wait queue keyed by
an integer, which is what a futex is — rather than a hand-rolled `HashMap<u64, Condvar>`, because the
registration/sleep window is exactly where the one defect already found in this layer (`sem_post`
consuming the waiter flag, **1.0104 s** measured) lived.

### A Critical defect this work reached, in code that had never seen guest input

`printf::emit_padded` pads with `repeat_n(' ', width - body.len())`. A width is guest-controlled —
`%999999999d` in the format string, or a `*` width taken from an argument — and the digits were
accumulated without saturating, so thirty nines produced `usize::MAX` and the pad became an
allocation. Global Constraint 11 calls an abort reachable from untrusted input Critical.

It had never been reachable: nothing had ever handed that engine a guest format string, and the
adapter is the first thing that does. Two bounds now, both typed refusals: `MAX_FIELD_WIDTH` (64 KiB,
the same number and the same reasoning as the boundary's `GuestMem::STRING_LIMIT`) per conversion,
checked **after** the `*` arguments are fetched because no scan of the format string can see one; and
`MAX_OUTPUT` (1 MiB) per call, because capping one field leaves a format string free to repeat a wide
conversion. Peak allocation is therefore bounded at `MAX_OUTPUT + MAX_FIELD_WIDTH`, and the test
asserts the output really stopped near the cap rather than being built and then rejected.

### One walk that is bounded only by the address space, stated rather than fixed

`omni-bionic`'s `strlen` reads one byte at a time until it finds a NUL or faults, exactly as the real
one does. The first attempt at a hostile test for it **walked out of the data region into the
adjacent read-only page and returned 4096** — correct behaviour, and the reason the test now maps an
island with free space after it.

The consequence: a guest that passes an unterminated pointer into a large mapped region makes one
handler scan that whole region, one `admit` per byte. It terminates and it cannot abort, so it is not
Critical, but it is unbounded work the guest chooses. Not capped, because a cap would give a wrong
answer for a legitimately long string and bionic's own `strlen` has none. The bounded forms exist and
are used where they can be: `__strlen_chk` takes the object size, and `GuestMem::cstr` — which the
whole `printf` family goes through — carries the boundary's 64 KiB `STRING_LIMIT`.

### What is deliberately not decided here

Where the OS-dependent remainder goes. Files, directories, clocks, process information, sockets,
logging, thread lifecycle and `dl*` all need surface `omni-platform` does not have, and the plan's
guidance is that adding it comes with the Linux and macOS signatures as honest `unsupported` returns
at the same time. This phase extended `omni-platform` not at all.

**Cost if wrong.** The thread-local is the one piece that would be expensive to change, because every
handler reads it. It is one function (`bionic::active`) and one type, so a move to a
`ThunkContext`-style user-data channel — if the boundary ever grows one — is mechanical.

---

## D21 — Phase 2: `dl*`, guest memory and the data symbols, and a data list wrong by two in each direction

D20 is the adapter and phase 1. This is phase 2 of M3 task 3: the three groups that needed no new
`omni-platform` surface. It extends D20 rather than replacing it, and it extends `omni-platform`
not at all.

### What is bound now

| | count | what |
|---|---|---|
| serviced inside the run loop | **88** | phase 1's 84, plus `dlopen`, `dlsym`, `dlclose`, `dlerror` |
| serviced on the exit path | **8** | `pthread_once`, `qsort`, `dl_iterate_phdr`, and the five guest-memory calls |
| `STT_OBJECT` data objects placed and filled | **18** | all of them |
| **of the 188 reachable imports** | **114** | 96 thunk functions and the 18 data objects |
| left `Unbound` | 74 | files, directories, clocks, process info, sockets, logging, thread lifecycle |

Pinned exactly by `the_bound_count_is_exactly_what_this_phase_claims`, which now asserts the two
tables separately, and by `dispatch_paths_are_what_f9_requires`, which asserts membership of each.

### The count was right and the membership was wrong, in both directions at once

D17 scopes the reachable `STT_OBJECT` imports at **18**. That number is correct and is re-derived
here. The *list* that stood beside it — `crates/omni-android/tests/libroblox.rs`'s `DATA_SYMBOLS` —
was not:

| | |
|---|---|
| Named but **not reachable** | `timezone`, `tzname` — `init-reachable-imports.txt` puts both in its "never referenced from the Tier C closure at all" section |
| Reachable but **omitted** | `AMEDIAFORMAT_KEY_STRIDE`, `AMEDIAFORMAT_KEY_WIDTH` — both in the reachable `libmediandk` group |

Two wrong and two missing, so the count stayed at eighteen. **Every assertion around it still
passed**, and that is the interesting part rather than the error: `timezone` and `tzname` really are
`STT_OBJECT` imports of `libroblox.so`, so declaring them produced eighteen resolved data symbols and
five unresolved ones exactly as the reconciliation of 23-against-18 required. A count-based test
cannot see a substitution.

It is derived now, from the real `.dynsym` intersected with the reachable list, by
`the_eighteen_data_symbols_are_derived_from_the_real_library_and_not_from_a_list`. That is the sixth
wrong number this project has recorded, and the first whose error was in membership rather than in
magnitude.

### Measured: nothing reaches a data import with a non-zero addend

`BoundaryBuilder::declare_data` requires a size and has no default, justified in its own
documentation by "`__sF` is an array of three `FILE` structures that the guest reaches as
`__sF + addend`, so a pointer-sized cell would be silently too small".

**Measured against the real library, and the evidence is wrong.** Each of the eighteen has exactly
**one** relocation against it, every one `R_AARCH64_GLOB_DAT` (type 1025), and every addend is
**zero**. The `GOT` holds the object's base; any subscripting is an instruction the guest executes.
`every_data_import_is_referenced_with_a_zero_addend` asserts it and will fail if that ever changes.

The *conclusion* survives intact — `&__sF[2]` is `__sF + 2 * sizeof(FILE)` computed at run time, so
the object still has to be three `FILE`s wide — and the requirement to state a size is still right.
Only the reason given for it was not true of this binary. Recorded rather than quietly fixed,
because a correct conclusion resting on a wrong measurement is how the next one gets believed.

### `sizeof(FILE) = 152`: derived, not verified, and why that is acceptable here

`__sF` is `FILE __sF[3]`, so it needs three times whatever a bionic LP64 `FILE` is. There is no NDK
on this machine — the same gap `omni-bionic`'s `layouts.rs` records for `pthread_mutex_t` — so the
number comes from `struct __sFILE`'s fields laid out by hand, and the arithmetic is written out in
`bionic::data::FILE_BYTES`.

**ASSUMED, and it is safe to assume only because nothing can read a field out of it.** A `FILE` is
opaque: every function that would interpret one is `Unbound` or refuses by name — `fopen`, `fclose`,
`fread`, `fwrite`, `fdopen`, `fileno` are unbound, and `fprintf` refuses while naming the guest
`FILE *` it was handed. The single observable a wrong stride can reach is the arithmetic
`stdout == &__sF[1]`, and that arithmetic is wrong **consistently**: this module places `stdout` at
`__sF + FILE_BYTES` with the same number guest code would use, so the two agree with each other
whatever the truth is. **The phase that implements stdio must confirm it against a real header
before reading a field**, and that obligation is written in the constant's own documentation.

### The contents, and the two that are facts rather than placeholders

* **`__stack_chk_guard`** carries D13's canary — the same value programmed into `TPIDR_EL0 + 0x28`
  for every thread of this guest, taken from the backend's TLS arena rather than chosen here. A
  function that loads the global form and one that loads the TLS form must see the same number, and
  1,276 of `libroblox.so`'s 1,282 thread-pointer reads are the second kind. **A zero is refused**,
  not stored: a zero canary compares equal to a zeroed stack slot, so a stack overflow that wrote
  zeroes would pass every check, and `omni-cpu` already refuses to *generate* one for that reason.
* **`environ`** points at a vector of one terminating null — an empty environment. This is a fact
  about a process started with none, and it is the answer that is *not* a stub: `environ = NULL`
  would be wrong, because POSIX-shaped code walks the vector without checking the pointer first, so
  a null there is a crash in guest code rather than a refusal here.
* `in6addr_any` is sixteen zero bytes and `in6addr_loopback` is `::1`, both fixed by RFC 4291.
* The ten `AMEDIAFORMAT_KEY_*` point at the published `android.media.MediaFormat` key strings. Every
  `AMediaFormat_*` function is `Unbound`, so the *use* fails by name; the strings still have to be
  right and non-null, because a `strcmp` or a hash of one fails silently.
* **A data symbol that is *called* is still `DataSymbolCalled`**, now that there are contents to
  execute. Asserted from real guest code with `BLR` into `__sF`, `environ` and a media key.

### `dl_iterate_phdr` is faithful, and the refusal is the load-bearing part

The C++ runtime in `libroblox.so` is statically linked, so the in-guest unwinder walks 11.5 MB of
`.eh_frame` through this call. It enumerates the images a host registered with
`Bionic::register_image`, in registration order, filling a real `struct dl_phdr_info` in guest memory
and calling the guest callback once per object, stopping at the first non-zero answer — which is the
contract the unwinder depends on, since it answers non-zero the moment it finds the object holding
the address it wants.

**An adapter with no image registered refuses, and that decision is the whole of why this is not a
stub.** Reporting an empty process is a *success*: the call returns zero, which is exactly what it
returns when every callback declined. Every C++ `throw` in the engine would then fail to find a
landing pad, thousands of initializers from the mistake. The refusal names `Bionic::register_image`
and says why.

`dlpi_adds` is the number of registered objects and `dlpi_subs` is zero, and both are facts rather
than placeholders: nothing in this layer can `dlopen` or `dlclose`, so the pair the unwinder caches
on is valid for the life of the process. `dlpi_tls_modid` and `dlpi_tls_data` are zero because D9
established there is no `PT_TLS` anywhere in this APK and `ElfImage::parse` refuses one.

The struct is **64 bytes**, derived from bionic's `link.h` and, like `FILE_BYTES`, not verified
against an NDK. Two things make that safe. The last four fields were added in Android R and nothing
has been added since, so 64 is the largest this structure has ever been and a guest built against an
older header reads a prefix. And the `size` argument passed to the callback *is* that number, so a
callback that checks before reading is told exactly how much is there.

### `dlopen`, `dlsym`, `dlclose` refuse; `dlerror` answers

Bound rather than left `Unbound`, for D20's reason: `Unbound` says "nothing implements this" and a
refusal says *which missing piece*, with the guest's own argument quoted. `dlopen` names the library
path it was handed, `dlsym` names the symbol and the handle.

The instruction this follows is blunt and correct: **returning a plausible handle you cannot honour
is worse than refusing.** A guest given a non-null `dlopen` result will `dlsym` it, store what comes
back and call it thousands of initializers later. `dlsym` returning null is worse still, because null
is `dlsym`'s ordinary "not found" and the guest would treat a missing capability as an absent
optional one.

`dlerror` returns null and that is **true**, not convenient: null means "no error since the last
call", and the three calls that could leave one refuse instead of returning. The common idiom —
`dlerror(); p = dlsym(...); if (dlerror())` — reaches the first call legitimately and never reaches
the second.

### The guest-memory group, and F9 honoured

`libroblox.so` imports **no allocator at all**. It carries its own and reaches the host through guest
`mmap`, so these five are where the engine's heap comes from.

**All five are `bind_reentrant`, and task 2's review finding F9 is why.** `ImportCall::mem()` reaches
`GuestMem::space()` and therefore the whole `GuestSpace`, and an inline handler runs inside one of
the translating backend's own callbacks with generated code live and a `&mut CpuCtx` on the stack.
Two things go wrong there, neither a type error: the pager's documented "the thread running guest
code must not hold this space's lock" invariant becomes reachable, and unmapping or reprotecting a
range invalidates memory the live translations reference from inside the callback executing them.
Phase 1 avoided it by mapping exactly once in `Bionic::new`; phase 2 cannot, because `mmap` is a
guest call.

**Nothing in the types says so**, and the naive move is caught only by accident — `ImportFn` and
`ReentrantFn` have different signatures, so moving a row between the tables does not compile, but
rewriting a handler against `ImportCall` would. So it is asserted instead, in both directions, by
`dispatch_paths_are_what_f9_requires`.

The exit path is also the only one that can reach a CPU, which is what makes the new
`ReentrantCall::invalidate_code` possible. `munmap` and `mprotect` must discard translations of the
memory they are about to change, or a guest that unmaps code and maps different code at the same
address runs the old one. **It is per context, and that is a narrowing of the window rather than a
closing of it**: a second guest thread that had already translated the same range keeps its
translation, and closing that needs a registry of live contexts the boundary does not have. Labelled
as a narrowing rather than described as complete.

### The split between a refusal and a `-1`, which is the whole risk in this group

* **Cannot be carried out correctly → `AbiError::Refused`**, naming the symbol and the argument:
  a file-backed `mmap`, `MAP_FIXED`, an unimplemented flag, a protection AArch64 can express and
  `omni_mem::Protection` cannot, `MADV_DONTNEED`, `mlock`. `MAP_FAILED` is the *believable* answer
  here — the guest's allocator handles it by trying something else, and the real failure would
  surface later as an allocation pattern with no explanation.
* **Well-formed and legitimately failed → what Linux returns, with `errno` set.** A length of zero
  is `EINVAL`, a length that cannot be page-rounded is `ENOMEM`, an occupied `MAP_FIXED_NOREPLACE`
  address is `ENOMEM`. That is the contract, not a stub: an allocator that cannot handle a failing
  `mmap` is broken on a real device too.

Four decisions inside that split are worth recording:

1. **`MAP_FIXED` is refused and `MAP_FIXED_NOREPLACE` is implemented.** Linux's `MAP_FIXED` silently
   unmaps whatever is already there; `Placement::Fixed` deliberately refuses an occupied range. The
   two spellings are kept apart and only the one with the checkable meaning is honoured.
2. **`MADV_FREE` is implemented and `MADV_DONTNEED` is refused**, and the difference is their
   contracts rather than their difficulty. `MADV_FREE` promises "the old contents or zeroes", which
   is exactly `advise_idle` plus a later `reclaim_idle`. `MADV_DONTNEED` promises **zero,
   immediately**, and meeting that would mean writing zeroes across the range — which commits every
   lazy granule the call was asking to release, the opposite of the point. `MADV_REMOVE` is refused
   for the same reason. The purely advisory advices return 0, because every one of them leaves the
   contents of the range untouched by definition, which is what makes ignoring them conforming
   rather than convenient. An advice nobody defined is `EINVAL`, as on Linux.
3. **`mlock` is refused, and `-1`/`ENOMEM` was considered and rejected.** It is the most tempting
   wrong answer in the group: a failing `mlock` is ordinary on a real device, `RLIMIT_MEMLOCK` is
   small, and well-written code handles it — which is precisely the problem. The guest would record
   a refusal by policy for a request nobody made. Nothing in `omni-platform` can promise residency
   that *stays*, and committing through the pager gives the first half of the contract only.
4. **`MAP_SHARED` on anonymous memory is accepted.** It differs from `MAP_PRIVATE` only across a
   `fork`, and there is none — `fork` is not in the reachable set and there is no process surface to
   build one on. Refusing it would be an over-correction that fails a correct program, which is what
   mutation row `guestmem-B1` exists for.

A guest `mmap` is `CommitPolicy::Lazy`, so the demand pager stays the heap seam (D10: never commit
speculatively). MEASURED, n=1 per side and structural rather than statistical: a 16 MiB guest `mmap`
adds 16 MiB to `SpaceStats::mapped` and **zero** to `SpaceStats::committed`, and one guest store then
adds exactly one 64 KiB granule.

### Verification

* `cargo test --workspace --release`: **959 passed, 0 failed, 12 ignored**, from 926.
* `tools/mutate.py`: **155 → 175 rows**, twenty new, **20/20 caught** after one MISS was closed.
* Clippy clean on `--all-targets`, `cargo doc` clean, `cargo tree -p omni-bionic -e normal` still one
  line (D19).

**The MISS is the finding worth keeping.** `guestmem-A1` — removing the code invalidation from
`munmap`/`mprotect` — came back NOT CAUGHT. The test was structurally incapable of seeing it: it
took a fresh guest thread for each step through the `value_of` helper, and the translating backend's
code cache is **per context** (D5: unshared per-thread code caches), so every step translated afresh
and the test could not tell an invalidated cache from an empty one. One context across the whole
sequence — map, write, protect, call, unmap, remap, write, protect, call — and the row is caught. A
test that runs the code and cannot fail is the thing Global Constraint 13 is about, and the mutation
harness is the only thing that found it.

### Two open items carried forward

* `sizeof(FILE) = 152` and `sizeof(struct dl_phdr_info) = 64` are both derived from bionic's headers
  and **not verified against an NDK**, which this machine does not have. Both are stated where they
  are used, both are safe for the reasons above, and the first must be confirmed by whichever phase
  first reads a field out of a `FILE`.
* `ReentrantCall::invalidate_code` reaches one context. The thread-lifecycle phase is where a
  registry of live contexts would go.

**Cost if wrong.** The data objects' contents are one function and are cheap to change. The refusals
are cheap to turn into implementations when the surface exists. The one expensive thing to get wrong
is `FILE_BYTES`, and it is expensive only from the phase that first interprets a `FILE` — which is
why the obligation is recorded in the constant rather than in a report.

---

## D22 — Phase 3a: `omni-platform` grows past `vm` and `fault`, and `AT_HWCAP` stays open on purpose

D20 is the adapter, D21 is phase 2. This is phase 3a of M3 task 3, and it is the first time
`omni-platform` has gained a module since it was written. The shape established here is the one
phases 3b (files), 3c (sockets) and 3d (thread lifecycle) copy, so the shape is the decision.

### What is bound now

| | count | what |
|---|---|---|
| serviced inside the run loop | **111** | phase 2's 88, plus all 23 of phase 3a |
| serviced on the exit path | **8** | unchanged: nothing in this phase calls guest code or touches `GuestSpace` |
| `STT_OBJECT` data objects | **18** | unchanged |
| **of the 188 reachable imports** | **137** | 119 thunk functions and the 18 data objects |
| left `Unbound` | **51** | files and directories (3b), sockets and polling (3c), thread lifecycle (3d), and `mallinfo` and the two `__gcov_*`, which belong to no group |

The 23: five clocks (`clock_gettime`, `gettimeofday`, `gmtime_r`, `nanosleep`, `usleep`), fourteen
process-and-environment (`getpid`, `sched_getcpu`, `arc4random_buf`, `getauxval`, `getenv`,
`__system_property_get`, `abort`, `__stack_chk_fail`, `_exit`, `android_set_abort_message`,
`sysconf`, `sysinfo`, `prctl`, `syscall`), four logging (`__android_log_print`, `syslog`, `openlog`,
`closelog`).

`the_bound_count_is_exactly_what_this_phase_claims` now asserts **membership** as well as totals —
all 23 named one by one, plus a complement check that eight symbols belonging to 3b/3c/3d are still
`Unbound`. D21 records why: a count cannot see a substitution, and this project has had a list whose
count stayed right while two members were wrong and two were missing.

### `AT_HWCAP` IS STILL OPEN, and the code is built so it cannot be closed by accident

The LSE question is unresolved and both arms are measured: advertising `HWCAP_ATOMICS` gives **53
hard interpreter halts**; declining gives **106 fallback arms** into a global spinlock that
**anti-scales 21x**. `getauxval(AT_HWCAP)` is where the answer would be delivered.

`bionic::HwcapPolicy` has three values — `Undecided`, `Advertise { hwcap, hwcap2 }`, `Decline` — and
**no `Default` implementation**. An instance starts `Undecided`, spelled out at the construction
site rather than reached by a derive, and under it `getauxval(AT_HWCAP)` and `getauxval(AT_HWCAP2)`
**refuse by name, with both measurements in the refusal text**. A host that has made the decision
calls `Bionic::set_hwcap_policy`, and the fact that it had to call something is the point.

**Defaulting to `Decline` was considered and rejected**, and the reasoning is the part worth keeping.
It reads as the safe arm, because declining a feature cannot halt the interpreter. It is also the
arm that costs 21x on a machine with cores — so whoever ran the engine next would measure it, report
"the runtime is slow", and nothing anywhere would say that a decision had been made. A refusal
naming both arms cannot be mistaken for anything. `Decline` and `Undecided` are deliberately
distinct values of the same type, and `declining_and_being_undecided_are_not_the_same_value` asserts
it; a type in which they were the same could not refuse.

Mutation row `procenv-A1` is that failure injected directly — the default changed to `Decline` — and
it is caught.

### The shape the later phases copy: not every primitive needs a `cfg`

`omni-platform` gains `clock`, `process` and `log`. The five-target rule says a new platform
primitive gets its Linux and macOS signatures at the same time, as honest `Unsupported` returns
naming the POSIX call they intend to make. **That applies to two of the new primitives and not to
the other five**, and saying which is part of the decision:

| primitive | how | Linux / macOS |
|---|---|---|
| `clock::monotonic_now`, `realtime_now`, `sleep` | `Instant`, `SystemTime`, `thread::sleep` | **implemented** — portable `std`, no backend |
| `process::pid`, `cpu_count` | `std::process::id`, `available_parallelism` | **implemented** — portable `std`, no backend |
| `log::emit`, `format_line`, the priority scales | `std::io::stderr` | **implemented** — portable `std`, no backend |
| `process::random_bytes` | `BCryptGenRandom` on Windows | **`Unsupported`**, naming `getrandom(2)` / `arc4random_buf(3)` |
| `process::current_cpu` | `GetCurrentProcessorNumber` on Windows | **`Unsupported`**, naming `sched_getcpu(3)` |

`vm` and `fault` are OS APIs end to end, which is why both have a structural unix half. The five
portable entries call no OS API at all. **A fabricated `Unsupported` for something `std` already
does correctly on all five targets would be a false claim in the other direction** — it would assert
that a clock this process can read cannot be read, and it would make the non-Windows bring-up harder
rather than easier. The rule is *never claim a platform works*; `std` working on Linux is not a
claim of ours. `omni-cpu`'s `CNTPCT_EL0` (D5 amendment 4) is already built on exactly this.

What stays unclaimed is what has been **run**, which is Windows x86-64 only. `process/linux.rs` and
`process/macos.rs` exist now as separate files, **though as written they are identical one-line
re-exports of `super::unix` and do not differ at all** — a review checked. They are separate because
the two implementations *will* differ once written: `getrandom` can block and return short and needs
a loop, `arc4random_buf` cannot fail and cannot return short — and **macOS has no `sched_getcpu` and no
supported equivalent**, so that symbol is expected to stay a refusal there permanently. That is a
real, permanent difference between the two unix targets and it is written where it will be read.

### Recorded and not worked around: Windows' sleep granularity is guest-visible

`std::thread::sleep` on Windows is bound by the scheduler's ~15.6 ms timer tick, so a guest
`usleep(1000)` sleeps for something closer to 15 ms than to 1 ms. D5 (amendment 4) already records
the same number from the other side — its interval measurement busy-waits "because Windows' sleep
granularity is ~15 ms". Raising it needs either `timeBeginPeriod`, which is process-wide and raises
power draw for every thread, or a `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer per sleeping thread.
Neither is free, neither has been measured, and the choice belongs to whoever first has a guest that
cares. `clock::sleep` therefore guarantees only what `nanosleep` guarantees: **at least** the
requested duration, with no bound in the other direction.

### `abort` and `_exit` are reported, never performed

Two new `AbiError` variants: `GuestAborted { symbol, address, why, message }` and
`GuestExited { symbol, address, status }`. `abort` and `__stack_chk_fail` produce the first,
`_exit` the second.

**A host `abort()` cannot be contained by any caller**, and hosting several isolated guest instances
in one process is non-negotiable — one instance's `abort` must not take the other two with it, or
the host, or the test runner. Reporting it as a value leaves the embedder every option an
`abort()` removes: log it, restart the instance, turn it into its own exit.

`_exit` is an `Err` rather than a successful `ExitReason` for a narrower reason: every caller of
`Boundary::run` treats `Ok` as "the guest returned and may be resumed", and a guest that has called
`_exit` may not be. Making that a type-level difference rather than a flag is the same reasoning D18
applies to re-entrancy.

`android_set_abort_message` is captured per instance and travels with the abort, because that string
is where bionic's crash reporter gets the only human-readable account of why a process died.

A test cannot assert "the host did not abort" — there would be nothing left to assert it with — so
`abort_and_exit_become_typed_outcomes_rather_than_ending_the_host` asserts the shape that makes
aborting impossible: a value, with the reason in it. Rows `procenv-A7`/`A8`/`A9` inject the three
ways to lose it.

### `sysconf` refuses two names it could answer, and that is the interesting refusal

Four symbols are bound and **refuse by name**: `sysconf`, `sysinfo`, `prctl`, `syscall`. Three of
them refuse because answering means modelling something that is not here — a `struct sysinfo`'s
uptime and free memory, a `prctl` option's effect, a raw syscall's contract, each with a believable
wrong answer sitting next to it (a zeroed struct, a `0`, a `-1`/`ENOSYS` that callers route around).

`sysconf` is different. Two of its names — the page size and the processor count — are things this
layer knows, and it still refuses. **Bionic's `_SC_*` numbering is bionic's own**: it is not glibc's,
and `_SC_PAGESIZE` and `_SC_PAGE_SIZE` are two different values there rather than one macro. There
is no NDK on this machine. A constant derived from memory has exactly the wrong failure mode here:
if it is wrong, the real page-size query arrives as an unmodelled number and is refused **loudly**,
while some other `_SC_` name silently receives a page size. One half of that is safe and the other
is the plausible-wrong-answer class.

The refusal names the value it was given and what that value is *believed* to be, flagged
`UNVERIFIED` — diagnostic without being load-bearing, because nothing branches on it. **Confirming
four constants against a real header turns this into four lines**, and the cheapest way to learn
which names the engine actually passes is to disassemble its `sysconf` call sites, or simply to run
task 4 and read the refusals.

The `AT_*` values are the opposite case and are answered: they are Linux UAPI
(`include/uapi/linux/auxvec.h`), the same source `omni-bionic`'s errno numbers come from, stable
across every architecture. So are the `clockid_t` values, the `prctl` option numbers and the arm64
syscall numbers used in the refusal messages.

### The environment and the property table are empty facts, and the host's own is unreachable

`getenv` answers `NULL` and `__system_property_get` answers 0 with an empty string, because this
guest process was started with no environment and no Android property service. That is the same fact
`environ` already states as one of the eighteen data objects (D21: it points at a vector of one
null), and it is a fact rather than a stub.

Both are host-settable — `Bionic::set_env`, `Bionic::set_system_property` — so that "empty" is a
configuration rather than an absence and the path that *finds* a value is exercised rather than dead.
A value longer than `PROP_VALUE_MAX` (92, the published Android constant) is refused **when the host
sets it** rather than truncated when the guest reads it, because the guest sizes its own buffer from
that constant.

**`omni-platform`'s process seam has no `host_environment()`, deliberately.** Handing the guest the
variables this process was started with would be a wrong answer — a desktop shell's environment is
not an Android app's — and an information leak of everything in it, credentials included.
`getenv_answers_null_until_the_host_gives_the_guest_a_variable` asks for `PATH` first, precisely
because a `getenv` that reached the host would answer it.

### Logging is the one group that must not refuse

Everything else in this phase that cannot be modelled refuses by name. The four log symbols are
serviced, and the asymmetry is the decision: a log call has **no return value the guest acts on**
(`syslog` returns `void`, `__android_log_print`'s byte count is universally ignored), so there is no
believable wrong answer available to give. What there is instead is the engine's own account of what
it is doing, arriving in order, during the run of 3,594 initializers this milestone has to get
through. Refusing `__android_log_print` would halt the run at the first thing the engine wanted to
report.

Formatting goes through the **real `printf` engine** — `format::render` became `pub(super)` rather
than being duplicated — because a line reading `%s at %p` with its arguments dropped is worse than no
line, and because a second copy of the AAPCS64 variadic walk is two places for those rules to drift.

Every record goes to a per-instance bounded ring as well as to stderr. The ring is the
recording-mock shape this project's working agreements prefer to an output-capture assertion, and it
is bounded because how much the engine logs has not been measured and an unbounded one is a host
allocation a guest can drive in a loop. `LOG_CAPTURE_MAX` is 256, records past it are dropped
oldest-first, and `Bionic::log_dropped` says how many — so a wrap is visible rather than silent.

A priority outside `android_LogPriority` is a refusal naming the value rather than a mapping to its
nearest neighbour: it is the one argument of these four with a wrong answer available, and a guest
passing 42 has either a miscompiled call or a corrupted stack.

### `gmtime_r` is in the pure crate, and the algorithm choice is about hostile input

`gmtime` is UTC by definition, so it needs no timezone database, no `TZ` and no host locale; and it
is handed a `time_t` rather than reading one, so it needs no clock. What is left is integer
arithmetic, which belongs in `omni-bionic` — `cargo tree -p omni-bionic -e normal` is still one line
(D19).

`civil_from_days` (Hinnant; the derivation C++20's `<chrono>` is specified against) rather than a
loop from 1970, **and the reason is hostile input rather than elegance**: a year-stepping loop turns
`gmtime_r(INT64_MAX)` into a hundred-billion-iteration spin inside a thunk handler. This version is
**loop-free — O(1)** over the whole `i64` range. (Recorded as *branch-free* until a review pointed
out that is literally false: `floor_div` alone has an `if`/`else`. The absence of a loop is the
property the argument needs; the stronger word would invite a constant-time claim it cannot support.)

A year that will not fit `int tm_year` is `NULL` with `EOVERFLOW`, which is C's own answer.
Wrapping would produce a *date*: plausible, printable, and wrong by billions of years.

`TM_BYTES = 56` and its field offsets are derived from bionic's `<time.h>` field by field and are
**not verified against an NDK** — the same provenance `layouts.rs` and `FILE_BYTES` record, stated
the same way. Unlike the `pthread_*` sizes this one has an independent check available: every field
is an `int` except the last two, so the layout is forced by the C rules once the field *order* is
right, and the order is POSIX plus two BSD extensions bionic inherits.

### Two defects found by running the harness, one of them in the harness

**1. `gmtime(i64::MIN)` overflowed.** The time of day was `timestamp - days * SECONDS_PER_DAY`, and
for timestamps near `i64::MIN` the floor pushes that product past `i64::MIN`: a **panic in a debug
build**, a silent **wrap in a release build**. `time_t` is a number the guest chooses, so this is a
panic reachable from guest input, which Global Constraint 11 calls Critical. `rem_euclid` gives the
same value and cannot overflow.

The suite could not see it. `cargo test --workspace --release` wraps rather than panicking, and the
wrapped value still produced the `YearOutOfRange` the test asserted — so the test passed for the
wrong reason. It appeared the first time the mutation harness, which builds **debug**, ran the
module. `no_timestamp_at_all_can_make_this_panic_or_wrap` enumerates the boundaries rather than
sampling them, because the overflow is a property of the multiplication and not of any date.

**2. The harness reported 8/8 rows caught while its command was already failing.** A command that
does not pass on the unmutated tree reports every row using it as `caught`, because "the suite
failed" is the whole of what caught means. Those eight results were worth nothing until the defect
above was fixed. `mutate.py` now runs each distinct command once on the pristine tree before
mutating anything and refuses if any fails, naming the tests — one run per command, not per row.

**3. Six new rows collided with existing ids**, and nothing checked. `plat-A1`..`plat-A4` already
belonged to the fault handler. A full run still touched every row so the totals stayed right, which
is the count-cannot-see-a-substitution failure again, this time in the harness that exists to catch
it. The six are `seam-*` now and `mutate.py` refuses to run when two rows share an id.

### Hostile input

`nanosleep` and `usleep` are capped at `MAX_SLEEP_SECONDS` (60) and **refuse** past it. A sleeping
thread executes no guest instructions, so D16's runaway-guest defence — which is built from short
step budgets — cannot end one, and `nanosleep({INT64_MAX, 0})` is a permanent hang of the host
thread that serviced it. A cap and not a clamp: clamping would return success from a call that slept
for a minute when it was asked for a year.

The decision is a predicate, `clocks::capped`, with a unit test, **because the end-to-end form of
that test cannot fail safely**: a version that did not refuse would sleep for the `i64::MAX` seconds
the test asked for and hang the harness rather than fail it, which is the failure mode `mutate.py`'s
own docstring records from M3 task 2. Row `clocks-A5` is scoped to a library-only command for the
same reason.

`usleep` reads only the low 32 bits of `X0`, because `useconds_t` is `unsigned int` and AAPCS64 does
not require a caller to clear the high half. Reading all 64 would turn an ordinary 100 µs sleep into
a request for 584,000 years.

`arc4random_buf` validates the **whole** destination before generating a byte, and generates
host-side in 4 KiB chunks rather than in one allocation the guest chose the size of. Without the
first, a destination writable for its first page and not its second would receive real entropy in
that page behind a reported failure, and the caller would have no way to know which half it got.

37 hostile argument shapes across the group, in
`hostile_arguments_to_the_clock_and_process_group_are_typed_errors_and_not_panics`. Every one either
completes with a defined `0`/`-1`/`NULL` or refuses by name.

### A wrong number in this phase's own record, corrected

The commit message for `21b712d` says "16 tests pass in `omni-platform` (was 11)". **`was 11` is
wrong: it was 7** — two in `vm::windows` and five in `fault::windows` — so the nine new tests took
it from 7 to 16, not from 11 to 16. The 16 was counted; the 11 was remembered, and this project's
own rule is that a remembered figure is not a figure. Recorded here rather than left in a commit
message nobody re-reads, because that is the seventh wrong number in this record and the shortest
possible example of how they get in.

### Verification

* `cargo test --workspace --release`: **1,004 passed, 0 failed, 12 ignored**, from 959. The 45 new
  tests are 9 in `omni-platform`'s lib (7 → 16), 10 in `omni-bionic`'s new `time` module, 10 in
  `omni-android`'s lib (81 → 91) and 16 in `omni-android`'s `bionic` target (60 → 76).
* `tools/mutate.py`: **175 → 212 rows**, 37 new — 29 direction A and 8 direction B — and a **full
  run of the whole table is 212/212 caught**, with `pre-flight: 11/11 commands pass on the
  unmutated tree`.
* Clippy clean on `--all-targets`, `cargo doc` clean, `cargo build --workspace --release
  --no-default-features` builds.
* `cargo tree -p omni-bionic -e normal` is still one line (D19), and `cargo tree -p omni-android -e
  normal` still has no `dynarmic-sys` — so moving `omni-platform` from a dev-dependency to an
  ordinary one did not touch that guarantee.
* The portability invariant is re-verified: every `cfg(target_os)` mention outside `omni-platform`
  is still a doc comment stating the rule, not an escape from it.

**Nothing here is a claim about Linux or macOS.** Neither has been built for, let alone run.

**Cost if wrong.** The refusals are cheap to turn into implementations once the surface exists. The
`_SC_*` constants are four lines behind one header. The expensive one to get wrong is `AT_HWCAP`,
and it is not decided here.

---
## D23 — Phase 3b: files, and a guest that cannot name a host file

D20 is the adapter, D21 is phase 2, D22 is phase 3a. This is phase 3b of M3 task 3: the 29
`file-io` symbols, the `omni-platform` seam under them, and the two design questions the brief
said had to be answered explicitly rather than by accident.

### What is bound now

| | count | what |
|---|---|---|
| serviced inside the run loop | **140** | phase 3a's 111, plus all 29 of phase 3b |
| serviced on the exit path | **8** | unchanged: nothing in this phase calls guest code or touches `GuestSpace` |
| `STT_OBJECT` data objects | **18** | unchanged |
| **of the 188 reachable imports** | **166** | 148 thunk functions and the 18 data objects |
| left `Unbound` | **22** | eight sockets and polling, eight threads and signals, and the six nothing else claims |

The 29: eighteen descriptor symbols (`open`, `__open_2`, `close`, `read`, `pread`, `__write_chk`,
`access`, `stat`, `fstat`, `lstat`, `statvfs`, `rename`, `unlink`, `mkdir`, `rmdir`, `opendir`,
`readdir`, `closedir`) and eleven `FILE *` ones (`fopen`, `fdopen`, `fclose`, `feof`, `fflush`,
`fgets`, `fileno`, `fputc`, `fputs`, `fread`, `fwrite`).

**Derived, not taken from the plan.** The plan's `3b` row was re-derived twice before anything was
written: the 188 of `init-reachable-imports.txt` minus every symbol named in `bionic/handlers.rs`
and `bionic/data.rs` gives the 51-symbol remainder, and intersecting that with
`tools/os_surface.py`'s `file-io` bucket gives exactly these 29 — set equality in both directions,
not a count. `the_bound_count_is_exactly_what_this_phase_claims` now names all 29 **and** asserts
that the unbound remainder is exactly the 22 named ones, as a set difference against the reachable
file rather than as a total. D21 records why: a count cannot see a substitution.

All 29 are **inline**. None calls guest code and none reaches `GuestSpace` — they read and write
guest memory, which `memcpy` already does from the fast path, and the arena a `FILE` or a `struct
dirent` lands in is mapped once in `Bionic::new`. That is F9's constraint honoured by the same
means phase 1 used, rather than by the exit path phase 2 needed.

### Question 1: a guest path is an Android path and none of them exists here

`/data/data/…`, `/system/lib64/…`, `/proc/self/maps`. **None may be allowed to mean what it
says**: `open("/etc/passwd")` resolved against the host's own root hands the guest a host file. D6
records that the APK under test is cheat-injected and that guest code is treated as hostile, and
"several isolated instances in one process" is non-negotiable — two instances that can reach each
other's files are not isolated.

**The policy: every guest path is resolved to a host path inside one host directory, by rules
applied before any host call, and a path that cannot be is refused by name.**

**Confinement is a property of the type rather than a check.** A `Filesystem` *is* a root plus a
descriptor table and there is no constructor without a root; an instance whose embedding has not
called `Bionic::set_filesystem_root` has **no filesystem at all**, and every path-taking symbol
refuses naming that method. That default is the same shape as `dl_iterate_phdr` refusing with no
image registered (D21) and `HwcapPolicy::Undecided` refusing `getauxval` (D22), and for the same
reason: a default root would have to be *somewhere* — the process's working directory, or a
temporary directory — and either hands untrusted guest code host files nobody decided to expose.
The root may be set **once**: allowing it to move would let a descriptor opened under one root be
read under another.

Six rules, in `omni_platform::fs::path`, four of them before any host call:

1. **Length** — `PATH_MAX` (4096) and `NAME_MAX` (255), the guest's own limits, reported as
   `ENAMETOOLONG` because that is what a real device answers. It is also what stops a guest's
   64 KiB string becoming a 64 KiB host path.
2. **Encoding** — UTF-8, because a lossy conversion can make two different guest paths name one
   host file.
3. **Lexical resolution with no host call** — split on `/`, drop `.` and empty components, *pop* on
   `..`. A `..` at the top stays at the top, which is POSIX's own `/.. == /`. **After this step no
   `..` exists**, so none ever reaches the host. This is the whole traversal defence and it is
   arithmetic rather than a check made afterwards on a path the host already saw.
4. **Component hygiene**, which is where the host-specific hazards die: a separator (`\` is one on
   Windows, so `a\..\..\b` is a traversal the `/` split never sees), a drive or alternate-data-stream
   marker (`:`), a wildcard, a control character, a **Windows character device** (`NUL`, `CON`,
   `COM1`… name a device in *every* directory, with any extension), and a trailing dot or space
   (Win32 strips them, so `secret.` and `secret` are one host file — two guest paths, one file).
   Applied on all five targets rather than under a `cfg`, so the confinement property does not
   depend on the host.
5. **Symlinks** — every component is checked and a symlink anywhere in the path is refused, except
   as the final component of an `lstat`, which is the one call whose job is to describe a link
   without following it.
6. **A final containment assertion** — the built path must still start with the root. Rules 3 and 4
   already guarantee it; this costs one comparison and is what notices if they ever stop.

**What it defends against and what it does not, stated rather than implied.** It defends against
everything the *guest* can do, which is the threat D6 names: `symlink`, `symlinkat` and `link` are
not in the reachable set and are not implemented, and `open(O_CREAT)` creates a regular file — so
the set of symlinks inside the root is fixed by whoever populated it, and rule 5 refuses those. It
is **not** race-free against an adversary who can create a symlink inside the root *while* the
guest runs, because the check and the open are two calls. Closing that needs `openat(2)` with
`O_NOFOLLOW` per component, which Windows has no equivalent of and `std` exposes on no target; the
Linux backend's notes record it as a real improvement available on that target rather than a
like-for-like port.

**The guest has no working directory, and that is a fact rather than a simplification.** `chdir`,
`fchdir` and `getcwd` are not among the 188, so nothing this milestone runs can move or observe
one. A relative path therefore resolves against the root, which is what a zygote-forked Android
app that never called `chdir` sees.

The end-to-end test creates a bait file **outside** the root and hands the guest eleven shapes of
path that would reach it, then asserts afterwards that the bait is unread and unmodified. A
traversal that worked would show up as content rather than as a missing refusal.

### Question 2: `sizeof(FILE)` is still ASSUMED, and this phase does not need it to be right

D21 recorded `FILE_BYTES = 152` as derived from bionic's `struct __sFILE` and **not verified
against an NDK**, with the obligation that "the phase that implements stdio must confirm it
against a real header before reading a field". There is still no NDK on this machine. **The
obligation is not discharged; it is narrowed, and the narrowing is the answer.**

**No field of a guest `FILE` is ever read or written.**

* A `FILE *` is a **key**, not a structure. The descriptor and the two sticky flags live in a
  host-side table on `Bionic`, keyed by the guest address. `feof`, `fileno` and `fflush` answer
  from that table.
* The bytes at a `FILE *` are written exactly once — to **zero**, when the object is handed out —
  and are never read. A zeroed bionic `FILE` has `_flags == 0`, which that library's own `__sfp`
  calls a free slot, so the bytes say "not an open stream": true, and safe.
* So a wrong `FILE_BYTES` cannot produce a wrong **answer**. It can only produce a wrong
  **address** — a translation unit compiled against an old header where `stdout` was the macro
  `(&__sF[1])` would compute a different one — and that address is not in the table, so every
  function **refuses by name**, naming the address and the number this layer used. A loud failure,
  not a silent one.

**What remains open, precisely.** Anything that makes a `FILE` field observable to the guest:
`ferror`, `clearerr`, `fseek` and `setvbuf` are the four that would, and **none is among the 188**.
If a later phase binds one, that is the paragraph it invalidates. One residual edge is recorded
rather than argued away: a translation unit that *inlines* a field access instead of calling the
function reads our zeroes, which for `_flags` reads as "closed stream" and makes an inlined
`feof`/`ferror` macro answer false — the safe direction, and it needs a pre-Lollipop NDK header to
happen at all.

The three guest structures this phase *does* write are the opposite case and are handled the
opposite way: see below.

### The five-target rule, applied with a sharper test than "does it call the OS"

D22's rule is that a primitive implemented purely on `std` works on all five and must **not** get a
fabricated `Unsupported` arm, because that is a false claim in the other direction. Files are where
that rule has to be applied operation by operation rather than module by module, and the test that
does it is: **is there one `std` call that serves all five targets?**

| primitive | how | Linux / macOS |
|---|---|---|
| `open`, `close`, `read`, `write`, `flush` | `std::fs::File`, `Read`, `Write` | **implemented** — portable `std`, no backend |
| `stat`, `lstat`, `fstat` | `fs::metadata`, `symlink_metadata`, `File::metadata` | **implemented** — portable `std` |
| `rename`, `unlink`, `mkdir`, `rmdir` | `std::fs`'s four of the same name | **implemented** — portable `std` |
| `opendir`, `readdir`, `closedir` | `std::fs::read_dir` | **implemented** — portable `std` |
| `access` | metadata plus an open probe | **implemented** — portable `std` |
| **`pread`** | **backend**: `FileExt::seek_read` on Windows | **`Unsupported`**, naming `pread(2)` |
| **`statvfs`** | **backend**: `GetDiskFreeSpaceExW` + `GetDiskFreeSpaceW` + `GetVolumeInformationW` | **`Unsupported`**, naming `statvfs(3)` |

Fifteen of the seventeen are one portable call and are written once. `pread` is
`FileExt::seek_read` on Windows and `FileExt::read_at` on unix — two traits in two modules, no
single call — and `statvfs` has no `std` spelling at all, so those two get the Windows
implementation and a structural unix half, exactly as `process::random_bytes` does (D22).

**Nothing here has been run on Linux or macOS.** The portable half is expected to work there and
has not been built for either, let alone tested; an implementation existing is not a claim.

### MEASURED: `FileExt::seek_read` is not `pread`, and the first version of this seam was wrong

Windows' `ReadFile` with an `OVERLAPPED` offset updates the file pointer for a synchronous handle,
and `std` does not undo it. Measured on a ten-byte file, n=1 per row and structural rather than
statistical:

| step | expected of `pread` | `seek_read` alone |
|---|---|---|
| `read(4)` | `0123`, cursor 4 | `0123`, cursor 4 |
| `pread(3, offset 7)` | `789`, cursor still 4 | `789`, **cursor 10** |
| `read(3)` | `456` | **0 bytes: end of file** |

Every call returns `Ok`, nothing is reported, and a guest's *sequential* reads silently jump to the
end of the file the first time anything `pread`s. That is the exact silent-wrong-answer shape this
seam exists to avoid, and it is why `pread` is a primitive here rather than a seek and a read in
the caller. The position is saved and restored now, **including when the read itself fails**.

It was caught by the seam's own test, written before the implementation was believed. Row `fs-A3`
is that defect injected.

### `statvfs` invents nothing, and the three fields it does not answer are an answer

Three Win32 queries fill every field: `GetDiskFreeSpaceW` for the cluster geometry,
`GetDiskFreeSpaceExW` for the 64-bit byte counts (the older call's cluster counts are 32-bit and
saturate near 8 TB; the newer one has no cluster size in it, so both are needed), and
`GetVolumeInformationW` for `f_namemax`, `f_fsid` and the read-only flag.

`f_files`, `f_ffree` and `f_favail` are **zero**, and that is what Linux reports for a filesystem
with no fixed inode table — FAT and exFAT do exactly this, and NTFS has none either because its MFT
grows. Any other number would be a count of something that does not exist, and a guest computing
"inodes remaining" from an invented `f_files` would refuse to write a file for a reason nobody
could find.

### The three guest structures are the layout trap, and every field carries its provenance

Android arm64 is LP64 with a 64-bit `time_t`, a 16-byte `struct timespec` and the kernel's own
field order. Unlike `FILE`, these are **transparent** — the guest reads their fields directly — so
a wrong offset is a wrong answer rather than a wrong address.

| structure | bytes | source | state |
|---|---|---|---|
| `struct stat` | **128** | Linux UAPI `include/uapi/asm-generic/stat.h`, which arm64 uses unmodified and which bionic's `<sys/stat.h>` matches field for field on LP64 | **ASSUMED** |
| `struct statvfs` | **112** | bionic `<sys/statvfs.h>`, LP64 | **ASSUMED** |
| `struct dirent` | **280** | bionic `<dirent.h>`, LP64 (`dirent` and `dirent64` are the same structure there) | **ASSUMED** |

`statvfs` has the strongest safety argument of the three, the same one `omni_bionic::time::TM_BYTES`
has: on LP64 every one of `fsblkcnt_t`, `fsfilcnt_t` and `unsigned long` is eight bytes, so the
layout is *forced* once the field order is right.

Field by field, `struct stat`:

* **Exact**: the `S_IF*` type bits, `st_size`, the three timestamps.
* **`st_blksize`** is `IO_BLOCK` (4096) and is a **fact**: every chunked transfer in this seam and
  in the `FILE *` layer moves at most that much, so a guest sizing its buffers from the field is
  sizing them to what happens.
* **`st_mode`'s permission bits are DERIVED** from the host's read-only attribute, which is the
  only permission `std` exposes on all five targets, and the code says so rather than implying an
  ACL evaluation. A symlink is `0o777`, which is not a derivation — Linux reports exactly that for
  every symlink. Refusing `stat` outright because Windows has no mode word was considered and
  rejected: `stat` is mostly used to ask "is this a directory" and "how big is it", both exact
  here, and refusing all of it to avoid approximating one field would fail a correct program over a
  field it is not reading. `access(W_OK)` and `st_mode & S_IWUSR` are derived from the *same* fact,
  so they cannot disagree.
* **`st_ino` is a hash of the path, never zero**, and this is the field where the choice matters
  most. Windows' real file identity is reachable only from an open handle and opening a *directory*
  needs a flag `std::fs` does not expose. **Zero was rejected outright**: real code compares
  `(st_dev, st_ino)` pairs to ask "are these the same file", and a constant makes the answer always
  *yes* — every file in the guest's world would be one file. A path hash answers "different" for
  different names; its one inaccuracy is that a hard link reports as two files, which is the
  conservative direction, and nothing the guest can call creates one. Row `fs-A4` is the zero.
* **`st_nlink` is 1 for everything**, including directories. Not 2: **btrfs reports 1 for
  directories**, so the `st_nlink - 2` subdirectory-count optimisation has had to tolerate it for a
  decade, and 1 is *true* here because nothing can create a hard link.
* **`st_uid` and `st_gid` are 0.** There is no user here. An invented app uid would be a number with
  nothing behind it; 0 is the only value that is not one.
* **`st_blocks`** is `size.div_ceil(512)` — the 512-byte units the field is defined in, derived
  from the size rather than from allocation, so a sparse file over-reports. `div_ceil` rather than
  `(n + 511) / 512`, because the second overflows for a size near `u64::MAX` and a release build
  wraps.

### The `FILE *` layer is in `omni-bionic`, over a trait, and that is where the interesting failures are

D19's guarantee holds: `cargo tree -p omni-bionic -e normal` is still one line. The layer belongs
there because **none of it is an OS call** — `fread(p, 3, 7, f)` is "multiply, with the overflow
checked; read that many bytes; report how many whole items arrived", and only the middle clause
reaches the OS. The adapter implements `stdio::Descriptors` over the filesystem seam; the tests
implement it over a `Vec<u8>`.

**Unbuffered, and that is a decision.** C says a stream may be unbuffered and `setvbuf` is not
reachable. It is what makes `fgets` correct: an unbuffered `fgets` reads one byte at a time and
stops **on** the newline, so the descriptor is left exactly where C says it is, where a buffered one
reads ahead and must put back what it did not use — and a read-ahead that is not put back is a
silently lost byte on a shared descriptor. It is also what makes `fflush` honest: every write
reaches the descriptor before the call returns, so there is nothing of this layer's to flush and
succeeding is the contract being *satisfied*. Calling `sync_all` instead would be a **stronger**
guarantee than `fflush` makes, bought with a disk round trip per call. The test counts the reads —
six for `"first\n"`, not seven.

**`size * nmemb` is `checked_mul`, and the detector is the errno rather than the count.** Two
guest numbers: a debug build panics on the overflow, which Global Constraint 11 calls Critical, and
a **release build wraps** to zero — which still satisfies "fewer items than asked for", so a test
that only checked the return would pass against the broken version. That is `gmtime(i64::MIN)`
again (D22). Six boundary pairs are enumerated rather than sampled, and each asserts `EINVAL`.

**Every host allocation is bounded by a constant, not by a guest argument.** Transfers move through
a 4 KiB buffer however large the request is — the shape `arc4random_buf` already uses — so
`fread(p, 1, 1 << 40, f)` runs through 4 KiB and returns what was there.

### The split between `-1` with `errno` and a refusal, which is this group's whole risk

* **Well-formed and legitimately failed → what Linux returns, with `errno` set.** `ENOENT`,
  `EEXIST`, `ENOTEMPTY`, `EISDIR`, `ENOTDIR`, `EBADF`, `EMFILE`, `EINVAL`. The contract, not a stub:
  guest code has a branch for every one and a real device produces them.
* **Cannot be carried out correctly → `AbiError::Refused`, naming the symbol and the argument.**

The refusals, each with the believable wrong answer it declines to give:

1. **No filesystem root.** Answering `ENOENT` would hide "this runtime was not configured" inside
   the ordinary noise of a guest probing for files.
2. **A path that tries to leave the root.** A traversal, a Windows device name, a drive-relative
   path. Reporting one as `ENOENT` would hide a hostile input in that same noise.
3. **`access(X_OK)`.** Windows has no execute permission on a file and the read-only attribute says
   nothing about one, so *both* answers are believable and wrong: `0` tells the guest it may execute
   a file this runtime cannot execute at all, and `-1`/`EACCES` reports a policy decision nobody
   made. `F_OK`, `R_OK` and `W_OK` are answered by **probing** — asking the host to open the file
   the way the guest asks about — which is the only answer that is not a guess.
4. **`O_SYNC`, `O_DSYNC`, `O_DIRECT`, `O_PATH`, `O_TMPFILE`, `FASYNC`.** Each promises something
   this layer does not do, and each has "accept it and do nothing" sitting next to it — which is
   `mlock`'s decision (D21) applied to flags. The flags that are *accepted and do nothing* are a
   separate list and each is accepted because what it asks for is already true: `O_NOCTTY` (no
   controlling terminal exists), `O_NONBLOCK` (a no-op on a regular file on Linux too),
   `O_LARGEFILE` (always in effect on LP64), `O_NOFOLLOW` (no path component may be a symlink, which
   is stronger), `O_NOATIME`, `O_CLOEXEC` (nothing execs). A bit nobody has defined is refused with
   the bits named rather than masked away.
5. **`__open_2` with `O_CREAT`.** That form takes no `mode`, so the file would be created with
   whatever was in the register — which is why bionic's FORTIFY build calls `__fortify_fatal` here.
   `__write_chk` with `count > buf_size` is the same shape: a **detected buffer overrun in guest
   code**, reported as one rather than as a short write.
6. **A wild `FILE *` or `DIR *`.** `NULL` is `readdir`'s own end-of-directory answer and `EBADF` is
   `fileno`'s own invalid-stream answer, so either would let guest code route around a wild pointer,
   a use-after-`fclose`, or the `&__sF[n]` arithmetic. Every `FILE *` and `DIR *` in this guest's
   world came out of this layer. (`closedir` is the exception and answers `EBADF`: its whole job is
   to release a handle, and `EBADF` is what a double `closedir` gets on a device.)
7. **A host failure `std::io::ErrorKind` did not classify.** It is **not** given `EIO`. `EIO` is a
   real answer guest code retries and reports; an error nobody identified deserves the refusal that
   names it. The trait between the layers carries an errno, so the adapter *stashes* the
   unclassified failure and refuses after the stream logic unwinds — the same shape `GuestView`
   already uses to carry a rich `AbiError` through `omni-bionic`'s thin `Fault`.

### Recorded and not worked around: `mkdir`'s mode is not applied

Windows has no POSIX permission bits, so a directory the guest asks to create with `0700` is
created with what it inherits from the instance's root. It is stated in the open rather than turned
into a refusal, because refusing every `mkdir` would stop the engine creating any directory at all,
and because **the security boundary this design rests on is the root** — which the host operator
supplies and protects — rather than the permissions of one directory inside it. Row `files-B7`
injects the over-correction.

### Two facts about descriptors that are answers rather than gaps

`stdin` reads **end of file immediately**: this guest was started with no terminal and no pipe, so
there is nothing to read. Blocking would be the wrong answer and inventing input a worse one.
`stdout` and `stderr` go to the host's own standard streams — not to `omni-platform::log`, which is
where a *log record* with a priority and a tag goes; a `write(1, …)` is bytes with neither.

The three are registered as streams over descriptors 0, 1 and 2 when the data objects are placed,
so `fileno(stdout)` is 1. That one assertion proves the registration, the `FILE_BYTES` spacing and
the declaration order in `DATA_OBJECTS` all agree with each other.

### A correction to a refusal that this phase made false

`fprintf` and `vfprintf` refused with "writing to the guest `FILE *` needs host file surface, and
`omni-platform` has none yet: it is virtual memory and faults only". That was true until this phase
and is not any more. Both refusals now name what is **actually** missing — the binding of the
`printf` family onto the stream layer, both halves of which now exist — and say that phase 3b
deliberately left it out of its scope of 29. A diagnostic that states something false is a defect in
the diagnostic, and this project's record already carries enough of those.

### A defect this phase's own test found in this phase's own work, and it was mine

`Bionic::new` commits the adapter's arena **eagerly**, and justifies that exception to D10's
"never commit speculatively" with one sentence: the arena is under a commit granule, so lazy and
eager cost the same charge, and eager buys that the first `errno` write on a new thread cannot
fail inside a handler.

Phase 3b added two tables to that arena — the `FILE` objects `fopen` hands out and the `struct
dirent` slot each directory stream owns — and with `MAX_GUEST_FILES = 32` it came to **67,712
bytes**, against `omni_mem::DEFAULT_COMMIT_GRANULE`. Over it by 2,176 bytes, so the eager commit
would silently have cost a **second** granule per guest instance and the sentence justifying it
would have been false.

**Nothing else would have noticed.** The extra charge is real but small, no test measured it, and
every other assertion in the workspace still passed. `the_arena_fits_in_one_commit_granule` is the
assertion that does, and it pins the total against the granule and the four tables against the
order the accessors assume; the two ceiling relations moved to `const` assertions beside the
constants they relate, because both sides are constants and a build that violated one could not
produce a binary to run a test with.

`MAX_GUEST_FILES` is **16** now, and the number is chosen by the granule budget rather than by
preference: 64 × 848 + 4096 + 16 × 152 + 16 × 280 = **65,280**.

**The test compares against `omni_mem::DEFAULT_COMMIT_GRANULE` rather than a literal**, because the
granule is a *measured* quantity — D10 set it by measurement, having found 4 KiB worse than the VEH
fault it rejected — and this project's own rule is that a measured quantity appears once with its n
and everything else links to it. A literal would have been a fourth copy, and it would have left
the relation silently wrong if the granule were ever re-measured.

**A second figure fell out of this, and it had been wrong for two phases.** The comment that
justified the eager commit said the arena was "17 KiB". That was right when D20 wrote it — 64
blocks of 272 bytes is 17,408 — and stopped being right in **phase 2**, which widened the
per-thread block to 848 bytes for the `dl_phdr_info` slots and took the arena to **58,368** without
anyone updating the sentence. Nothing asserted it, so nothing noticed. That is the eighth wrong
number in this record and the second whose whole cause was a figure living only in prose.

### Verification

* `cargo test --workspace --release`: **1,059 passed, 0 failed, 12 ignored**, from 1,004. The 55
  new tests are 21 in `omni-platform`'s lib (**16 → 37**), 10 in `omni-bionic`'s new `stdio`
  module (**98 → 108**), 9 in `omni-android`'s lib (**91 → 100**) and 15 in `omni-android`'s
  `bionic` target (**76 → 91**). Every "before" here was **re-measured** in a throwaway worktree
  at `e5abf1b`, not carried over: a first draft of this paragraph said "11 in `omni-bionic`
  (97 → 108)", the four deltas then summed to 56 against a workspace movement of 55, and rather
  than reconcile that by arithmetic the endpoints were measured. `omni-bionic`'s lib was 98.
  `omni-bionic` and `omni-android`'s libs were also run in **debug**, per the working agreement
  about overflow, and pass there.
* `tools/mutate.py`: **213 → 239 rows**, 26 new — 19 direction A and 7 direction B. The new rows
  are **26/26 caught**, and **a full run of the whole table on the committed tree is 239/239
  caught**, with `pre-flight: 239/239 patterns match exactly once` and `pre-flight: 11/11 commands
  pass on the unmutated tree`. 239 rows reported, 239 distinct ids, no MISS.

  **That full run is the second one; the first was discarded rather than reported.** It also said
  239/239, and it was worthless: `cargo` was run against the tree while it was going and
  `bionic/mod.rs` — which three rows mutate — was edited underneath it. A concurrent build makes a
  row read as "caught" for the wrong reason, because a compile failure from a half-written file is
  indistinguishable from the suite failing. The harness's own docstring warns about exactly this.
  A contaminated pass is worse than no number, because it is believable.

  **Two stale rows were found by the pre-flight rather than by a MISS**, which is what that gate
  is for:

  * `stdio-A2`'s pattern is Python source *and* Rust source at once, and an unescaped `\n` in it
    is a newline in the pattern rather than the two characters the file holds. It matched nothing.
  * `data-A1` had been mutating the `stdin`/`stdout`/`stderr` loop, which phase 3b gave a
    `register_stream` call. Re-targeted at the line that computes the spacing, and it is now
    caught by two tests: the data-object one it always had, and phase 3b's `fileno(stdout) == 1`.

  Both refused the *whole run* rather than reporting a row as MISS, which is the difference
  between a gate and a report.
* Clippy clean on `--all-targets --release`, `cargo doc --workspace --no-deps` clean,
  `cargo build --workspace --release --no-default-features` builds.
* `cargo tree -p omni-bionic -e normal` is still one line (D19), and `cargo tree -p omni-android -e
  normal` still has no `dynarmic-sys`.
* The portability invariant is re-verified: every `cfg(target_os)` mention outside `omni-platform`
  is still a doc comment stating the rule or a `#![cfg(target_os = "windows")]` gate on a
  Windows-only *test*, which the two `windows_only.rs` files already assert. This phase added none.

**Nothing here is a claim about Linux or macOS.** Neither has been built for, let alone run.

### Cost if wrong

The confinement rules are the expensive thing to get wrong and they are the most heavily tested:
one property test against a bait file outside the root, the lexical rules enumerated rather than
sampled, and four mutation rows in both directions. The three ASSUMED layouts are the next: each is
one table and is cheap to change, and each is asserted from real guest code against a value the
test chose, so an offset that moved fails rather than drifts. `MAX_OPEN_FILES`, `MAX_OPEN_DIRS`,
`MAX_GUEST_FILES` and `MAX_DIR_ENTRIES` are policy numbers and are stated as such.

---

## D24 — Phase 3c: guest threads, and the three signal symbols that stay refusals

D20 is the adapter, D21 is phase 2, D22 is phase 3a, D23 is phase 3b. This is phase 3c of M3
task 3: the eight symbols of the plan's `3c` row — `pthread_create`, `pthread_join`,
`pthread_detach`, `pthread_getschedparam`, `pthread_sigmask`, `raise`, `sigaction`,
`sigfillset` — and it is the phase where the runtime can create a guest thread for the first
time.

### What is bound now

| | count | what |
|---|---|---|
| serviced inside the run loop | **145** | phase 3b's 140, plus four signal symbols and `pthread_getschedparam` |
| serviced on the exit path | **11** | phase 3b's 8, plus `pthread_create`, `pthread_join`, `pthread_detach` |
| `STT_OBJECT` data objects | **18** | unchanged |
| **of the 188 reachable imports** | **174** | 156 thunk functions and the 18 data objects |
| left `Unbound` | **14** | eight sockets and polling, and the six nothing else claims |

**Derived, not taken from the plan.** The 188 of `init-reachable-imports.txt` minus every symbol
named in `bionic/handlers.rs` and `bionic/data.rs` is a 22-symbol remainder; intersecting it with
the plan's `3c` row gives exactly these eight, and the remaining 14 are asserted as a **set
difference** against the reachable file rather than as a total.

**One correction to the plan's own text, and it is a swap rather than an error of substance.**
`HANDOFF.md`'s "Next action" called phase 3c *sockets and polling* and 3d *thread lifecycle*,
while the plan's phase-3 table has `3c` as threads + signals and `3d` as network. The table is
what was followed; HANDOFF has been corrected to agree with it. Nothing depended on the order.

### `pthread_create` is where three constraints meet, and none is traded against another

**1. D13 is satisfied structurally rather than by remembering it.** A guest thread's `TPIDR_EL0`
must point at a populated bionic TLS block with a stack guard at `+0x28` **before it executes one
instruction**. The context comes from a new `GuestCpuBackend::create_guest_thread`, which
allocates the block from the backend's own arena and builds a `GuestThreadConfig` — a type that
cannot be constructed without a usable thread pointer. A backend with no arena refuses that call
by name rather than inventing one.

The arena must be **the backend's**, and this is the part that would have been easy to get wrong
in a way nothing would have noticed for a long time. Bionic reads its stack guard once per
process and copies **one** value into every thread's slot 5; a second `TlsArena` beside the
backend's would put a second guard value into one address space, and a canary stored on a frame
in one thread and checked in another would then fail `__stack_chk_fail` — which is a
*termination*, arriving in a thread that did nothing wrong, on a timing-dependent schedule.
`a_created_guest_thread_has_its_thread_pointer_and_the_process_stack_guard` asserts it from real
guest code: the start routine executes `MRS X0, TPIDR_EL0` and `LDR X1, [X0, #0x28]` itself,
exactly as 1,276 of `libroblox.so`'s own instructions do, and the value it reads is compared
against the arena's.

**2. Host → guest re-entry stays a type property** (D18, task 2's finding F9). `ImportCall` still
holds no CPU, so an inline handler still cannot run guest code. What `pthread_create` needed was
not a second CPU on the calling thread but the **boundary**, so that it could install the thunk
table on a context it had just created and start a run loop there: `ReentrantCall::boundary()`,
which exists only on the exit path. It cannot be used to re-enter the calling thread's guest
either — `Boundary::run` needs a `&mut dyn GuestCpu`, the call already holds the only one for
this thread, and the borrow checker will not produce a second. The capability it hands out is
exactly "drive a CPU you have just made", which is what `pthread_create` is.

`pthread_create` also maps the new thread's stack, which is F9's other half and the reason `mmap`
is on the exit path too.

**`pthread_getschedparam` is inline**, unlike its three siblings: it runs no guest code and
touches no mapping, and putting it on the exit path for tidiness would cost it 3x per call (D17)
for nothing. `dispatch_paths_are_what_f9_requires` pins that in both directions.

**3. The 16 MiB-per-guest-thread blocker is live now, and is measured below.**

### D16's shape for a thread that never returns

A created thread runs in **short budget windows** and re-reads the instance's stop switch between
them, rather than in one unlimited run. That is D16's prescription rather than a preference: the
halt flag is checked at terminals that a counted budget makes exclusive
(`crates/dynarmic-sys/patches/README.md` item 2b), so an asynchronous halt is not a mechanism
that can be relied on here, while a window boundary is a decision point that exists whatever the
guest is doing. `Bionic::stop_guest_threads` is the switch, and its documentation says what it is
not: it is not an interrupt, and a thread blocked in a guest mutex or inside `pthread_join` stops
only once that returns.

`a_runaway_guest_thread_stops_at_a_window_boundary` runs an unconditional guest loop and stops it.

### The split between a POSIX return and a refusal

The pthread functions return their error **as the return value**, not through `errno`.

* **Well-formed and legitimately failed → what POSIX says**, and each is a branch guest code has:
  `EAGAIN` for a resource that ran out (the live-thread limit, the backend's thread count, a
  stack that could not be mapped), `ESRCH` for a `pthread_t` no live thread answers to, `EINVAL`
  for a thread that is not joinable and for an attribute object whose detach state is neither
  value, `EDEADLK` for a join that would deadlock.
* **Cannot be carried out correctly → `AbiError::Refused` naming the symbol.** Three of them:

  1. **No thread host.** `EAGAIN` would say the runtime ran out of resources when it was never
     configured, and a correct guest would retry for ever.
  2. **A NULL start routine.** POSIX defines no error for it and bionic simply branches to it, so
     there is no correct number: `EAGAIN` claims a shortage that did not happen and `0` claims a
     thread was started. It is a detected defect in guest code and is reported as one — the same
     treatment `__write_chk` gives a detected buffer overrun (D23).
  3. **Joining a thread that stopped without returning.** There is no `void *` for a thread that
     never produced one, and `0` with an untouched `retval` is indistinguishable from a thread
     that returned `NULL`.

**A detached thread's failure has nobody to report to**, so it goes to
`Bionic::guest_thread_failures` — which is the only place it can surface — and a joinable one is
reported both ways.

### The hostile surface a guest that spawns threads adds, and what each case answers

| the guest does | the answer |
|---|---|
| joins itself | `EDEADLK`, which POSIX names |
| two threads join each other | `EDEADLK` — the check walks the wait chain, so a cycle of any length is caught, not only the self-join |
| joins a thread nobody handed out | `ESRCH` |
| joins a detached thread | `EINVAL` |
| detaches twice | `EINVAL`; succeeding twice would make a double detach indistinguishable from a single one, and in a real implementation it is a use-after-free of the thread's descriptor |
| asks for a stack of `SIZE_MAX` | `EAGAIN`. The round-up is `checked_add`; the masked form wraps to **zero** and a release build does it silently, so the thread would get a stack made entirely of its guard page |
| asks for a stack below the floor | `EINVAL`, which is POSIX's answer for a stacksize under `PTHREAD_STACK_MIN` — never a silent round-up, which would leave `pthread_attr_getstacksize` and the real stack disagreeing |
| writes a detach state of 99 into its attr | `EINVAL` |
| passes a start routine at an unmapped address | the thread is created, faults, and is **recorded**; the join refuses naming the failure |
| creates more threads than the limit | `EAGAIN` |
| panics the runner | a recorded failure rather than a `pthread_join` that blocks for ever — the panic is caught, because the record would otherwise stay `Running` |

### MEASURED: what a guest thread costs, and what comes back

n = **4 runs of 8 threads**, release, `crates/omni-android/tests/thread_memory.rs`, each run in
its own process because `process_commit_charge` is process-global. The threads are created by
real `pthread_create` calls from translated ARM64 code and joined again.

| | |
|---|---|
| per guest thread, running | **24.76 - 24.84 MiB** |
| instance + boundary + the first context | 25.14 - 25.16 MiB |
| residual after all eight were joined | **2.79 - 3.18 MiB in total, 0.35 - 0.40 MiB per thread** |

**The adapter adds almost nothing per thread.** 24.8 MiB against `omni-cpu`'s **24.56 MiB** for a
raw context with no adapter and no guest stack (n = 1 run of 8 contexts) is agreement to within
1%. What this layer adds is a 1 MiB guest stack that is lazily committed and of which a spinning
thread touches one page, and an arena block that was already committed when the instance was
built.

**98.6% of it comes back**, and that is the figure the multi-instance requirement turns on. The
cost is of *concurrent* guest threads rather than of threads ever created, so a guest that creates
and joins in a loop does not drift upwards. An instance whose guest runs N threads costs the
~16.7 MiB of a loaded `libroblox.so` plus about 24.8 MiB per concurrent thread — three instances
with four threads each is roughly 50 + 12 × 24.8 ≈ 348 MiB. `ThreadHost::with_limit` is what an
embedding bounds that with; the default is `MAX_GUEST_THREADS`, the arena's own capacity, so this
layer adds no second invented ceiling.

**The fork patch is still NOT applied.** 16 MiB of the per-thread figure is `A64EmitX64`'s fixed
fast-dispatch table, held by value and **written by the constructor** for a feature D16 runs
disabled; `crates/dynarmic-sys/patches/README.md` item 4 has the patch and the argument for it.
Applying it gives up D5's byte-for-byte-unmodified claim about the vendored tree, which is a
decision to record rather than a side effect of needing the memory, and it needs upstream's
202,200 assertions run against it with `FastDispatch` both on and off first.

### `omni-platform` did not have to grow, and the plan predicted it would

The plan's phase-3 table lists "**Threads** — spawn, join, detach, attributes, scheduling" among
the things `omni-platform` must grow for. **It did not have to.** Everything here is portable
`std` (`std::thread`, `std::panic::catch_unwind`), `omni-mem` (the stack mapping) and `omni-cpu`
(the context and its TLS block). No new platform primitive exists, so there is **no
`unsupported` arm to write** — and per D22's other half, fabricating one would be a false claim
in the other direction: it would assert that a thread this process can spawn cannot be spawned.

This is the second phase running whose five-target prediction was wrong in the direction of
over-estimating the OS surface; phase 3b predicted files would "almost all" need a unix half and
fifteen of seventeen needed none (D23). The sharper test D23 proposed — *is there one `std` call
that serves all five targets?* — answers yes for the whole of this phase.

**Nothing here has been built for Linux or macOS, let alone run there.**

### The registry of live contexts, decided explicitly

Phase 2 recorded `ReentrantCall::invalidate_code` as reaching **one** context and labelled that
"a narrowing of the window, not a closing of it", with the note that the registry which would
close the rest belonged with thread lifecycle. This is that phase, so the decision is made rather
than carried forward.

Every `Boundary::run` registers the context it drives; `invalidate_code` applies the range to the
calling context synchronously and **queues it for every other live one**, which applies it at the
top of its next run segment — the only place a `&mut dyn GuestCpu` for that context exists. A
queue that fills collapses to the whole address space, which is *more* invalidation rather than
less: over-invalidating costs translation, and dropping a range leaves a context executing bytes
that are not there. `Boundary::watch_context` is the long-lived form, and the thread runner holds
one — without it a guest thread's registration would be taken off and put back between every run
window, and a range invalidated in that gap would be queued for a context that no longer existed.

**What is still open is stated rather than implied.** A context drains at a run-segment boundary,
so a guest thread that neither crosses the exit path nor returns from `cpu.run` keeps a stale
translation until it does. For a created guest thread that is bounded by one step window; under
`RunLimit::Unlimited` with a loop that never leaves generated code it is unbounded. Closing it
completely needs either a cross-context invalidation the backend does not offer or an
asynchronous halt honoured under a counted budget, which the patches README measures as not being
the case on this pin.

`CodeInvalidations` is a **detector**, not a watch: both counters stay at zero under exactly the
workload that raises them if the broadcast is removed.

### Signals: one is answered exactly, three are refused by name

A POSIX signal is three mechanisms — a disposition table, a per-thread mask, and delivery that
can interrupt a thread at an arbitrary instruction — and Omnidroid has none of them. Inventing
one is a design of its own, with its own interactions with the demand pager (D10), the halt flag
(D16) and the boundary's re-entrancy rules (D18).

| symbol | here | the believable wrong answer it declines |
|---|---|---|
| `sigfillset` | **implemented**, in `omni-bionic` | — |
| `sigaction` | **refused** | `0`: the guest believes it will be told about `SIGSEGV`, `SIGPIPE` or `SIGABRT`, and the code that would have recovered is never reached |
| `raise` | **refused** | `0`: `raise(SIGABRT)` is the tail of `assert` and of most C++ runtimes' `std::terminate`, so the guest **runs on past the point it expected to die**, carrying the invariant it had just found broken |
| `pthread_sigmask` | **refused** | `0` with an empty old mask: the guest believes signals are blocked across a critical section |

`pthread_sigmask` is the symbol `omni-bionic` had already **excluded by name** for this exact
reason (D19), so the other two are the same family getting the same answer.

**`sigfillset` is not a concession.** It is `memset(set, 0xff, sizeof(sigset_t))`: a total
function of its one argument with no table, no mask and no delivery behind it. Refusing it as
well would be the over-correction, and row `signals-B1` injects exactly that. What it is *for* is
still refused, which is the point — a guest that calls `sigfillset(&set)` and then
`pthread_sigmask(SIG_BLOCK, &set, &old)` gets a correctly filled set and then a refusal naming
the symbol, rather than a correctly filled set and a lie.

**Mapping `raise(SIGABRT)` onto `abort`'s reported termination was considered and rejected**, and
the refusal text says so. `abort` *is* bound and reports a termination rather than performing one
(D22). Routing `raise` onto it would be this layer deciding that `SIGABRT`'s disposition is
`SIG_DFL` — a fact about a table that does not exist — and it would answer for one signal number
out of 64 while every other still needed a decision.

Two things recorded rather than assumed. Bionic's `sigfillset` fills the **whole** word where
glibc leaves signals 32 and 33 clear for NPTL; this follows bionic, which is the same
follow-bionic-not-glibc convention `guestcmp` already carries for `strcmp`'s byte difference. And
`sizeof(sigset_t) = 8` joins the ASSUMED layouts in `layouts.rs` — forced by `_KERNEL__NSIG = 64`
rather than chosen, and the only write to a `sigset_t` in the whole runtime is bounded by it.

### `pthread_getschedparam` answers, and the argument is that the answer is forced

It reports `SCHED_OTHER` with a priority of **0** for a thread this instance created, and `ESRCH`
for any other `pthread_t`. That is an answer rather than a plausible stub because of three facts,
and the conclusion follows from them rather than from a preference:

1. **Nothing in the reachable 188 can set a scheduling policy.** The set contains
   `pthread_getschedparam`, `sched_getcpu` and `sched_yield`, and no `pthread_setschedparam`, no
   `pthread_attr_setschedpolicy`, no `pthread_attr_setschedparam`, no `sched_setscheduler`, no
   `setpriority`.
2. So every thread in this guest's world has the policy it was created with, which is the
   default, because nothing here changes a host thread's priority either. On Linux and Android
   that default is `SCHED_OTHER` — `SCHED_NORMAL`, which is 0.
3. `sched_priority` is not a choice under `SCHED_OTHER`: Linux's `sched_get_priority_min` and
   `_max` for it are both 0, so the field has exactly one legal value.

**The paragraph to invalidate is fact 1.** If a later phase binds a setter, this stops being an
answer and becomes a lie, and it has to grow a real per-thread policy or become a refusal. Row
`threads-B2` injects the over-correction — refusing it along with the signal family — because
refusing it would stop a correct guest over a field it is only reading.

### What a guest thread deliberately does not do, recorded rather than hidden

**It does not run its `pthread_key` destructors when it exits.** Bionic does. Closing it needs a
host → guest call from *outside* a thunk crossing, which is a boundary API that does not exist —
every call into guest code today is made from inside a `ReentrantCall`, and a thread finishing
its start routine is not inside one. The cost is a leak of whatever a guest frees from a key
destructor, once per thread exit. It does not affect M3's gate, where the 3,594 initializers run
on a thread that does not exit, and `pthread_exit` is not among the 188 at all.
`CallThreads::detach_and_take_destructors` still returns an empty list and still has no caller.

### A defect an independent review found in phase 3b's own test, and it is a correction to D23

`the_arena_fits_in_one_commit_granule` ended with an `assert_eq!` restating `ARENA_BYTES`'s own
definition character for character. **It cannot fail.** D23 claims that test "pins the total
against the granule **and the four tables against the order the accessors assume**"; the first
half is true and the second half was asserted nowhere. Dropping `POOL_BYTES` from `files_base()`,
or swapping it with `dirents_base()`, left the whole workspace suite green while `fopen` handed
out `FILE` objects on top of the pool's interned strings.

`the_arena_tables_are_where_the_accessors_say_they_are` is the replacement: it walks the four
bases **out of the accessors themselves**, checks each region starts exactly where the previous
one ends and that the last ends exactly at the end of the arena, then allocates one `FILE` and
one `dirent` through the only two allocators over those tables and checks each lands inside its
own. Nothing in it restates the sum — extending the tautology to a fifth term would have
reproduced the defect one term wider. Row `arena-A1`.

This is the second time in this project a *total* has been consistent while its *membership* was
not; the data-symbol list wrong by two in each direction (D21) was the first. It is why this
phase derives its own symbol set as a set difference rather than trusting a count.

**This phase added no per-thread arena state**, so the 256 bytes of granule headroom phase 3b left
are untouched: the thread registry is host-side, and `pthread_create`, `pthread_join` and
`pthread_getschedparam` write only into buffers the guest supplied.

### A defect in the table this phase created, found by writing the test that produces it

`ThreadTable` took the next block index from the map's **length**. That is exact for a table
nothing is ever removed from, and until this phase nothing was — thread lifecycle is what removes
one. Remove the entry holding index 1 from a table of three and the length is 2, so the next
thread is handed index 2, **which is live**. Two guest threads would then share one `errno` cell
and one `strerror` buffer, and the symptom would be an occasional wrong error number in a thread
that did nothing wrong.

Blocks come from a free list now, and the index is carried on the slot.
`a_thread_that_exits_does_not_give_its_block_to_a_live_thread` is the detector, and it produces
the collision rather than merely creating threads: two threads start, the **first** exits, a
third starts while the second is still running, and the third's `errno` cell is compared with the
second's. Verified as a detector before the row was written. Row `threads-A2`.

### Three defects in this phase's own code, found by re-reading it before reporting

Recorded because the *method* is the reusable part: each was found by reading the code again with
the question "what does a guest-chosen number do here", not by a test failing.

1. **`pthread_create` added to a guest-chosen address unchecked.** `at + ATTR_GUARD_SIZE` wraps
   for an `attr` near the top of the address space and a **debug build panics** on it, which
   Global Constraint 11 calls Critical. It was not reachable — the first field read
   short-circuits for `attr == usize::MAX` — but that is an accident of ordering rather than a
   defence, and the ordering is one edit away from changing. `checked_add` and a refusal now.
2. **A failed stack unmap was swallowed** by a `let _ =`. It leaks the stack's address space and
   commit charge for the life of the instance, and the thread that leaked it is the only thing
   that knows. Recorded as a thread failure beside the outcome rather than instead of it.
3. **`pthread_getschedparam` answered `ESRCH` for the main thread**, because the registry held
   only threads `pthread_create` made. The wrong answer: the main thread is a thread of this
   process with the same default policy, so facts 1-3 above apply to it identically, and a guest
   asking about *itself* during initialisation — which is the ordinary use — would have taken an
   error branch for no reason. It answers for any thread this instance has an identity for, and
   the test asks about `pthread_self()` as well now.

### Verification

* `cargo test --workspace --release`: **1,092 passed, 0 failed, 13 ignored**, from 1,059 and 12.
  The 33 new tests are **4** in `omni-bionic`'s lib (108 → 112), **3** in `omni-android`'s lib
  (100 → 103), **1** in `omni-cpu`'s `seam` target (7 → 8) and **23** in `omni-android`'s
  `bionic` target (91 → 114), plus the new `thread_memory` target, which contributes the
  thirteenth **ignored** test and 2 passing ones that are the shared harness's own encoding
  checks. `omni-android` and `omni-bionic` were also run in **debug**, per the working agreement
  about overflow, and pass there — which is where `round_up`'s guest-supplied arithmetic is
  checked, and where `pthread_create`'s newly-checked `attr` offset arithmetic would panic if it
  were left as a bare `+`.
* `tools/mutate.py`: **239 → 255 rows**, 16 new — 12 direction A and 4 direction B, and the new
  rows are **16/16 caught** (signals 4/4, threads 9/9, watch 2/2, arena 1/1). **A full run of the whole table on the committed tree is 255/255 caught**, with
  `pre-flight: 255/255 patterns match exactly once` and `pre-flight: 11/11 commands pass on the
  unmutated tree`. 255 rows reported, 255 distinct ids, no MISS, and the tree byte-for-byte
  restored afterwards. It was run on a committed tree with nothing else touching it, which is the
  condition phase 3b's first full run failed and had to be discarded for.

  **One row HUNG rather than failing, and that was a defect in the test.** `threads-B1` refuses
  the first `pthread_detach`, which leaves the thread joinable — and
  `detaching_twice_is_einval_and_joining_a_detached_thread_is_einval` opened its release gate
  from the guest program *after* its own `pthread_join`. With the join blocking instead of
  returning `EINVAL`, the program deadlocked against a gate it had not reached, and the harness
  sat on that row for over half an hour. This project has recorded the same shape once before
  ("two mutation rows hung instead of failing", task 2). A test that deadlocks under the defect
  it exists to detect reports nothing at all. The gate is opened from the **host** now, once both
  detaches have happened, and an `OpenOnDrop` guard releases every gate a test's guest threads
  spin on **while a panic unwinds** — so a failing assertion no longer leaves a guest thread
  spinning for the rest of the binary either. Every row now finishes in about eleven seconds.
* Clippy clean on `--all-targets --release`, `cargo doc --workspace --no-deps` clean,
  `cargo build --workspace --release --no-default-features` builds.
* `cargo tree -p omni-bionic -e normal` is still one line (D19) — this phase put `sigfillset`
  there and added no dependency — and `cargo tree -p omni-android -e normal` still has no
  `dynarmic-sys`.
* The portability invariant is re-verified: every `cfg(target_os)` mention outside `omni-platform`
  is still a doc comment stating the rule or a `#![cfg(target_os = "windows")]` gate on a
  Windows-only *test*. **This phase added none, and added no `omni-platform` surface at all.**

**Nothing here is a claim about Linux or macOS.** Neither has been built for, let alone run.

### Cost if wrong

The expensive thing to get wrong is D13, because its symptom is a stack-check *termination* in a
thread that did nothing wrong, on a schedule nobody controls — so it is asserted from real guest
code reading its own thread pointer, and the guard is compared against the arena's rather than
merely checked for being non-zero. The block-index collision is the next: it is silent by
construction, and its detector produces the collision rather than exercising the code. The
per-thread memory figure is the one that constrains the product rather than the code, and it is
measured through the path the guest takes rather than one layer down.

---

## D25 — Phase 3d + 3e: the last fourteen imports, and two symbols that must resolve to nothing

D20 is the adapter, D21 phase 2, D22 phase 3a, D23 phase 3b, D24 phase 3c. This is the last
import phase of M3 task 3: the plan's `3d` row (**8** network symbols) and its `3e` row (**6**
that no other phase claimed), run together because they are the remainder and because the
membership test that accounts for all 188 has to be written once.

**After this phase every one of the 188 statically-reachable imports is accounted for**, and the
adapter's own test asserts that as a set difference against `init-reachable-imports.txt` rather
than as a total.

### The final coverage of the 188

| | count | what |
|---|---|---|
| **answered** | **143** | a real value, computed or read from a real source |
| **refused by name** | **22** | `AbiError::Refused`, naming the symbol, the guest address and the missing piece |
| **a guest termination, reported** | **3** | `abort`, `__stack_chk_fail`, `_exit` — a third outcome, not a refusal (D22) |
| **bound**, therefore | **168** | 157 inline and 11 on the exit path |
| `STT_OBJECT` data objects | **18** | unchanged since phase 2 |
| deliberately **absent** | **2** | `__gcov_dump`, `__gcov_flush` — see below |
| **total** | **188** | 143 + 22 + 3 + 18 + 2 |

**That split is asserted by *calling* every symbol, not by counting a table.**
`the_final_split_of_the_reachable_set_is_what_the_record_claims` calls each of the 22 with zeroed
arguments and requires `AbiError::Refused` — not `Unbound`, which would mean nothing implements
it — and calls the three terminations and requires neither. This project's most repeated mistake
is a total that stays right while its membership drifts, and a count in a document is exactly
where that happens.

A **conditional** refusal is an answer, and the distinction is load-bearing: `getauxval` refuses
only while the `AT_HWCAP` decision is open, `sched_getcpu` only on a target whose process backend
is structural, `mmap` only for a shape it cannot honour, and `dlerror` answers `NULL` — which is
true, not a stub.

**Derived, not taken from the plan.** The 188 of `init-reachable-imports.txt` minus every symbol
named in `bionic/handlers.rs` and `bionic/data.rs` was a 14-symbol remainder before this phase and
is the **two absent symbols** after it, which is what
`the_bound_count_is_exactly_what_this_phase_claims` now asserts. A count cannot see a
substitution; this project has had a list whose count stayed right while two members were wrong
and two were missing (D21), and a *total* stay consistent while its membership did not (D24).

The twenty-two, by the phase that decided each: `fprintf`, `vfprintf`, `vasprintf`, `sscanf`,
`fscanf` (D20); `dlopen`, `dlsym`, `dlclose`, `mlock` (D21); `sysconf`, `sysinfo`, `prctl`,
`syscall` (D22); `sigaction`, `raise`, `pthread_sigmask` (D24); and this phase's `socket`,
`eventfd`, `getaddrinfo`, `freeaddrinfo`, `mallinfo`, `longjmp`. Each names the missing piece
rather than the fact that something is missing, and each declines a *believable* wrong answer that
is written down beside it.

### `omni-platform` grew by exactly one primitive, and not the one the plan named

The plan's phase-3 table lists "**Sockets and polling** — socket, poll/select, getaddrinfo" among
the things `omni-platform` must grow for. **It did not have to.** No socket seam exists, and
therefore no `unsupported` arm was fabricated for Linux or macOS either — D22's other half: a
primitive that calls no OS API must not be given one, because that is a false claim in the other
direction.

**This is the third phase running whose five-target prediction over-estimated the OS surface.**
Phase 3b predicted files would "almost all" need a unix half and fifteen of seventeen needed none
(D23); phase 3c predicted thread lifecycle would need new platform surface and it needed none at
all (D24). The sharper test D23 proposed — *is there one `std` call that serves all five targets?*
— answers a **third** thing for this group: there is no OS call to make.

What *did* arrive is `process::cpu_time`, for the guest's `clock()`. That one genuinely fails
D23's test: `Instant` is wall time and nothing in the standard library reports consumed processor
time, so it has a Windows backend (`GetProcessTimes` on the current-process pseudo-handle) and the
Linux and macOS signatures written at the same time as honest `Unsupported` returns naming
`clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`. `ProcessError::LastError` is a new variant rather than
a reuse of `Status`, for the reason `Status`'s own documentation gives: an `NTSTATUS` and a
`GetLastError` code are different number spaces with the same digits.

**Nothing in this phase has been built for Linux or macOS, let alone run there.**

### Why `poll` and `select` need no operating system, as a closed argument

Not "they are easy", and not "nothing polls during static initialisation". The argument is that
**the descriptor space they observe is entirely this runtime's own, and POSIX fixes the answer for
every kind in it**:

1. The only bound symbols that produce a descriptor are `open`, `__open_2` and `opendir`, plus
   `fileno` handing back one of those or one of the three standard streams. `socket` and
   `eventfd` — the two symbols in the reachable 188 that would introduce a descriptor which can
   *block* — refuse. `pipe`, `socketpair`, `epoll_create`, `timerfd_create`, `signalfd`,
   `inotify_init` and `dup` are not among the 188 at all.
2. So every descriptor that exists is a regular file, a directory, or one of stdin, stdout and
   stderr, and none of them can block: `omni-platform`'s `read` on a standard stream is an
   immediate end of file and its `write` to one is an immediate host write.
3. Linux answers exactly `POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM` for a regular file — its
   `DEFAULT_POLLMASK` — **regardless of the descriptor's access mode**, which is why a read-only
   file answers `POLLOUT` there and here.

The consistency criterion is the one that matters, and it is not "what would a device do": it is
that **`poll`'s answer predicts what `read` and `write` on that descriptor actually do in this
runtime**.

**The paragraph to invalidate is a test rather than a sentence.**
`the_descriptor_space_poll_answers_over_is_closed` intersects `Bionic::bound_symbols()` with every
POSIX symbol that hands out a descriptor and asserts the result is exactly
`{eventfd, open, __open_2, opendir, socket}`, then calls the two refusals to confirm they refuse.
The day a phase binds `socket` for real, that test fails and this module has to grow a real
readiness source with it. Row `net-A7` injects the always-ready rule applied to a descriptor that
is not open.

**A wait that nothing can end is refused by name**, which is the same argument `MAX_SLEEP_SECONDS`
makes for `nanosleep`: a sleeping thread executes no guest instructions, so D16's runaway-guest
defence — built from step budgets — cannot end one. `poll(fds, n, -1)` and `select(.., NULL)` with
nothing ready are therefore refusals, and a *finite* timeout past the cap is refused rather than
clamped, because a clamp returns `0` from a call that waited a minute when it was asked to wait a
year. Row `net-B1` is the over-correction: every bounded wait refused.

### The two `__gcov_*` symbols must resolve to NOTHING, and the guest's own code says so

This is the phase's most consequential decision and the brief asked for it explicitly.

[`Binding::Unbound`] is the design for every other import: a symbol nothing implements gets a real
address whose call produces a typed error naming it, which beats a branch to address zero with no
symbol attached. **That argument assumes the guest calls the symbol either way.** For a *weak*
undefined symbol it does not, because the reference the compiler emits is a null test and the
address is what the test reads.

**VERIFIED by decoding the single site in `libroblox.so` that references them**, at `0x6194be8`:

```text
0x6194bec: LDR  X8, [X8, #0x788]   ; the __gcov_dump GOT slot
0x6194bf0: CBZ  X8, 0x6194bfc      ; if it is null, skip
0x6194bf4: BL   0x62d7c80          ; __gcov_dump's PLT stub
0x6194bf8: BL   0x62d6760          ; abort's PLT stub
0x6194c00: LDR  X8, [X8, #0x790]   ; the __gcov_flush GOT slot
0x6194c04: CBZ  X8, 0x6194c10      ; if it is null, skip
0x6194c08: BL   0x62d7c90          ; __gcov_flush's PLT stub
```

The guest null-tests both before calling either, and no Android libc exports `__gcov_*` — they
belong to `libgcov`, which is linked only into a coverage-instrumented binary, and this one merely
kept the guarded reference. So a null GOT slot is what a real device produces and what the guest
expects.

Give them an address instead and the `CBZ` falls through. With the symbol left `Unbound` the run
**fails** on a path a device never takes. With the symbol bound to a no-op that "flushed" coverage
data nothing ever collected, the guest goes on to **`BL abort`** — the next instruction. **A
plausible stub here does not merely lie, it terminates the process, and the terminating
instruction is four bytes past the call the stub answered.** Nothing short of decoding the call
site would have shown that.

`BoundaryBuilder::declare_absent` is the mechanism, and it is **not** "weak symbols resolve to
nothing". `libroblox.so` has five weak undefined imports — `__cxa_thread_atexit_impl`, `gettid`,
`getentropy` and these two — and a real bionic supplies the first three, so a guest on a device
calls them. The declaration is therefore per symbol and the provider requires **both** the name
and the weakness: a *strong* reference to a declared-absent symbol still gets a named slot,
because a strong reference has no null test in front of it. Rows `gcov-A1` (the list ignored) and
`gcov-B1` (the weakness ignored) pin both directions, and `declare_absent` refuses a symbol that
already has a slot rather than letting this layer give the loader two answers about one name.

`the_two_gcov_imports_are_weak_null_tested_and_left_unresolved` asserts all four facts against the
real library rather than quoting them: both are `WEAK NOTYPE` in `.dynsym`; each has one
`GLOB_DAT` **and** one `JUMP_SLOT`, so the address is taken as well as called; the instruction
after the `LDR` of each GOT slot is a `CBZ` on the register the `LDR` wrote; and after a real load
the slot holds zero.

**This is the one place in the runtime where "nothing" is the answer**, and it costs the loader
two of its previously-565 function slots: `the_data_symbols_land_in_the_data_area...` now expects
540 rather than 542.

### `inet_ntop`: the standard library is not an oracle for bionic, and a differential run proved it

The brief's guidance was that `inet_ntop` is formatting and `Ipv4Addr`/`Ipv6Addr` already
`Display`. They do, and **`Display` is the wrong answer.**

MEASURED: a differential run of **200,000** pseudo-random 128-bit addresses (half of each draw's
bytes zeroed by a second draw, so the compression paths are reached) against
`std::net::Ipv6Addr`'s `Display` disagrees **43 times**, and all 43 are one class — the
*IPv4-compatible* address, where the first six groups are zero and the seventh is not. Rust
deliberately stopped printing that deprecated form in dotted notation and writes `::77:0`; BIND's
`inet_ntop6`, which bionic ships essentially unchanged, writes `::0.119.0.0`, because its
condition is `best.len == 6` and says nothing about deprecation.

So the algorithm is written out in `omni_bionic::net` and the differential test is **committed
with that class asserted from the other side**, which makes delegating to `Display` later a
failure rather than a silent change of a guest-observable value. The same
follow-bionic-not-something-else convention `guestcmp` records for `strcmp`'s byte difference —
and the same shape: the convenient implementation was *nearly* right.

Two further BIND behaviours a from-scratch RFC 5952 formatter gets wrong are asserted directly: a
run of **one** zero group is not compressed (`1:0:2:3:4:5:6:7`, not `1::2:3:4:5:6:7`), and `::`
and `::1` do **not** grow a dotted tail while `::1.2.3.4` and `::ffff:1.2.3.4` do.

A `size` that cannot hold the result **and its NUL** is `ENOSPC` with **nothing written**, which
is BIND's own behaviour: it formats into a local buffer and only then compares. A truncated
address is still a printable string naming a different host.

`gai_strerror`'s table is fifteen rows **ASSUMED** from bionic's `ai_errlist` with no NDK on this
machine to check them against — the same gap `layouts.rs`, `FILE_BYTES` and `TM_BYTES` record.
What is safe about it is the shape rather than the letters: a wrong message is a wrong
*diagnostic*, the caller is `printf`, and nothing branches on the text. The one thing that would
not be safe — a pointer to storage that does not outlive the call — is the adapter's problem, and
it interns the whole table in the instance's **pool** in `Bionic::new`. The per-thread scratch
`strerror` uses would have been the believable wrong answer: the next `strerror` on that thread
overwrites a message the guest may have stored a pointer to.

### The four network refusals, and the believable wrong answer each declines

| symbol | the believable wrong answer | what it would cost |
|---|---|---|
| `socket` | `-1` with `EAFNOSUPPORT` or `EACCES` | a **legitimate POSIX outcome** a networked program branches on quietly. The engine switches its own networking off during initialisation, the run completes, and nothing anywhere records that Omnidroid rather than the device made that choice |
| `eventfd` | `-1` with `ENOSYS` | says *this kernel* has no eventfd, which is a fact about a kernel and not about this layer — and a guest that believes it falls back to a pipe, which is not bound either |
| `getaddrinfo` | `EAI_NONAME` or `EAI_FAIL` | a caller retries `EAI_AGAIN` and reports `EAI_FAIL` as a real DNS failure; either way it believes it asked a resolver |
| `freeaddrinfo` | doing nothing | it returns **`void`**, so there is no value to be wrong: a silent no-op is indistinguishable from a correct free, and would still be indistinguishable on the day `getaddrinfo` starts returning real lists and it starts leaking them |

`socket` is refused for three reasons and any one would do. `omni-platform` has no socket seam. An
embedding has no way to say which network a guest may reach, the way `Bionic::set_filesystem_root`
says which directory it may reach — and phase 3b's whole confinement argument was that a default
would have to be *somewhere*. And Global Constraint 8 forbids network access at run time, with D6
recording that the APK under test is cheat-injected and carries a Luau executor.

`eventfd` is refused because **a descriptor the guest can obtain and then cannot use is worse than
one it cannot obtain**: every descriptor in this runtime belongs to `omni-platform`'s rooted
filesystem table, which `read`, `__write_chk`, `close`, `fstat` and `poll` are all written
against, and an eventfd is a counter with blocking reads that none of the five could carry. The
failure would move from this call, where it names what is missing, to whichever call the guest
reached next — reporting `EBADF` about a descriptor this layer issued itself.

`getaddrinfo` is refused for two independent reasons. **There is nowhere to build the answer**: a
`struct addrinfo` list lives in *guest* memory and must be freeable, and this layer has no guest
allocator — the arena is a fixed set of tables sized at construction (65,280 bytes of a 65,536-byte
granule, 256 spare), the pool is a bump allocator that never frees, and F9 forbids a handler
mapping guest memory at all. The guest's own allocator is not reachable either: `libroblox.so`
imports no allocator (D17). **And the resolution needs a network**, which is `socket`'s argument.
Recorded for whoever implements it later: `sizeof(struct addrinfo)` on LP64 bionic would be **48
bytes**, and bionic orders `ai_canonname` before `ai_addr` where glibc does the reverse — so a
glibc-derived layout puts the canonical name where the address belongs. **ASSUMED; there is no NDK
on this machine.**

### `mallinfo` and `longjmp`, and why neither is a marshalling problem

**`mallinfo` is the only reachable import that returns through `X8`**, and that is *not* why it is
refused. `Args::indirect_result` exists, task 2 built it for this symbol, and eighty bytes of
zeroes would be three lines. The reason is that **there is no heap for the answer to describe**:
`libroblox.so` imports no allocator at all, carries its own, and reaches the host through guest
`mmap`. Eighty zeroed bytes is the believable wrong answer *precisely because it is
arithmetically true* of a libc heap nothing has allocated from — zero arena, zero free blocks,
zero in use — so a guest logging its memory usage prints a consistent, self-consistent, fictional
zero at both ends of the run. The other available lie is worse: reporting this process's commit
charge as the arena would be a real number, from the right process, describing the wrong
allocator. The refusal reports the `X8` the guest passed, so a reader can see the marshalling is
not what failed.

**`longjmp` needs no operating system**, which is exactly why it fell through every phase of this
task's OS-surface plan and had to be collected by the last one. It needs two things this layer
does not have. Restoring a `jmp_buf` means writing `X19`-`X28`, `X29`, `X30`, `SP` and the low
halves of `D8`-`D15` of the **calling thread's own guest context** and then not returning; the
boundary gives a handler the AAPCS64 argument registers and one return value and deliberately no
way to write guest state, because D18 makes "cannot reach the CPU" a *type* property and that is
what stops an inline handler re-entering the guest. And nothing here can have **filled** a
`jmp_buf`: `setjmp` is not among the 188 — it is in the reachable file's Tier C section, reached
only through an address-taken edge — so it is not bound, and bionic mangles the saved `SP` and
`LR` with a per-process cookie besides. The believable wrong answer is to **return**: `longjmp` is
`noreturn`, and a handler that quietly returned would resume the guest in the frame it was trying
to escape, carrying whatever condition made it jump. That is `raise`'s failure one frame further
in, which is why `longjmp` is documented and refused *with* the signal family rather than beside
it.

### A correction to D22: `CLOCK_PROCESS_CPUTIME_ID` is answered now

Phase 3a refused it on the argument that this layer had no process CPU accounting. This phase
added `omni_platform::process::cpu_time` for the guest's `clock()`, and **the refusal's stated
reason became false**. Leaving it would have had this layer answer one question two ways —
`clock()` reporting a real figure while `clock_gettime` said the figure could not be had. The same
shape as `fprintf`'s refusal text claiming `omni-platform` had no file surface after phase 3b gave
it one (D23).

`CLOCK_THREAD_CPUTIME_ID` stays refused and the distinction is real rather than tidy: a per-thread
figure is `GetThreadTimes`, a primitive that does not exist, and answering it with the *process*
figure would report every thread as having consumed the whole program's CPU. The refusal names
that primitive now. `CLOCK_BOOTTIME` stays refused because it counts time spent suspended.

`the_process_cpu_clock_is_answered_and_the_thread_cpu_clock_is_still_refused` asserts the two
against each other, and the tolerance is what makes it a unit check: **50 ms**, one accounting
quantum over the drift between the two calls, against a `CLOCKS_PER_SEC` of a million. A first
version allowed 5 seconds, which a **thousand-fold** unit error passes.

### A defect in this phase's own code, found by re-reading it before reporting

**`select` zeroed the guest's sets before it validated the timeout.** POSIX is explicit that "on
failure, the objects pointed to by the readfds, writefds, and errorfds arguments are not
modified", and three failures come *after* the sets have been read: a malformed `struct timeval`,
a `timeout` pointer that is not readable, and a wait past the cap. In the first version all three
answered `-1`/`EINVAL` or a refusal having already emptied the sets, so a guest that retried the
call would have retried it with nothing.

It is the same shape as review finding **M1** — a side effect kept behind a reported failure — one
call along from where M1 found it, and it is worth recording that this phase's `poll` had been
written the *right* way round (read whole, decide, write whole) while its `select` had not. The
fix is an ordering: read and validate the timeout, take the wait decision, and only then clear and
write back.

**Found by reading the code again with the question "what does a guest-chosen number do here",
not by a test failing** — the same method that produced phase 3c's three (D24), and the third time
in this project it has been the thing that worked. `a_failed_select_does_not_modify_the_guests_sets`
asserts all three failure paths against sentinel bits, plus the fourth arm that must still zero
them; row `net-A9` restores the old ordering and is caught by it.

### Three test defects this phase found in its own tests, and the method that found them

Recorded because the *method* is the reusable part: two were found by the whole-workspace run
rather than by a filtered one, and the third by a mutation row having nothing to catch it.

1. **A `cpu_time` test asserted "a sleep charges no CPU".** That is true of a *thread* clock and
   false of the process clock this reports — and the whole-workspace run, where libtest has other
   tests executing, is where it shows. MEASURED on the run that caught it: a 50 ms sleep was
   charged **93.75 ms** of process CPU time. The discrimination is made the other way now, and it
   is one a wall clock cannot fake: several threads burning one interval of wall time advance a
   process CPU clock by **more** than that interval, and interference from other threads only
   makes the assertion easier — the direction a shared-process measurement has to be robust in.
   Row `plat-A9`.
2. **The same test read the CPU clock before starting the wall clock**, so the CPU interval was
   the wider of the two and a wall-clock impostor satisfied it by accident. The wall interval
   contains the CPU interval now, with a 1.5x margin.
3. **`poll_with_nothing_ready_sleeps_for_its_timeout_and_returns_zero` never polled an array**, so
   the over-correction row `net-B3` — `poll` demanding a filesystem root for a call that names no
   descriptor — had nothing to catch it. It polls two *disabled* slots now, which is the idiom
   POSIX defines a negative `fd` for.

### What a guest thread costs is unchanged, and so is the arena

This phase added **no per-thread arena state**: `poll`, `select` and `inet_ntop` write only into
buffers the guest supplied, and `gai_strerror`'s table is in the pool, which is inside
`ARENA_BYTES` already. The 256 bytes of granule headroom phase 3b left are untouched and
`the_arena_fits_in_one_commit_granule` still holds.

### Verification

* `cargo test --workspace --release`: **1,134 passed, 0 failed, 13 ignored**, from 1,093 and 13.
  The 41 new tests are **2** in `omni-platform`'s lib (38 → 40), **9** in `omni-bionic`'s lib
  (112 → 121), **5** in `omni-android`'s lib (103 → 108), **24** in `omni-android`'s `bionic`
  target (114 → 138) and **1** in its `libroblox` target (6 → 7). `omni-android`, `omni-bionic`
  and `omni-platform` were also run in **debug**, per the working agreement about overflow, and
  pass there — which is where `poll`'s guest-supplied `nfds` arithmetic and `select`'s
  guest-supplied `timeval` arithmetic are checked.
* `tools/mutate.py`: **258 → 284 rows**, 26 new — 22 direction A and 4 direction B — and the new
  rows are **26/26 caught** (net 19/19, gcov 2/2, clocks 2/2, guestmem 1/1, signals 1/1, plat
  1/1). `net-A8` was verified as a *detector* before its row was written, by injecting the
  per-entry implementation and watching the sentinel change; `net-A9` is the row for the `select`
  ordering defect above and restores it exactly.
* Clippy clean on `--all-targets --release`, `cargo doc --workspace --no-deps` clean,
  `cargo build --workspace --release --no-default-features` builds.
* `cargo tree -p omni-bionic -e normal` is still one line (D19) — this phase put `inet_ntop` and
  `gai_strerror`'s table there and added no dependency — and `cargo tree -p omni-android -e
  normal` still has no `dynarmic-sys`.
* The portability invariant is re-verified: every `cfg(target_os)` mention outside `omni-platform`
  is still a doc comment stating the rule or a `#![cfg(target_os = "windows")]` gate on a
  Windows-only *test*. This phase added none.

### Cost if wrong

The `select` ordering defect is the cheapest thing here to have got wrong and the one most worth
noting, because it was **not** caught by anything: the suite was green, every mutation row was
caught, and the defect sat behind a failure path that reports `-1` either way. Only re-reading
found it. The expensive thing to get wrong is the `__gcov_*` decision, and it is expensive in a direction
that would have been very hard to find: a no-op stub there is followed four bytes later by the
guest's own `BL abort`, so the symptom is a *termination* during initialisation with no
relationship to the symbol that caused it. It is settled by decoding the guest's own instructions
rather than by an argument about weak symbols, and the decoding is a committed test against the
real library.

`poll`'s always-ready rule is the next: it is correct only while the descriptor space stays
closed, and the thing that keeps that honest is a test over the bound-symbol table rather than a
paragraph. `inet_ntop`'s divergence from the standard library is third — it is one address class
out of a hundred and twenty-eight bits, it would never have been found by a hand-written case, and
a guest that logs an address would have logged a different one.

---

## D26 — `AT_HWCAP` decides to **decline**, for the startup path, and says what would reverse it

**Decision.** The M3 gate and everything up to a first frame run with `HwcapPolicy::Decline` —
`AT_HWCAP` and `AT_HWCAP2` both zero, which is what a real ARMv8.0 device reports. `Undecided`
remains the state an instance *starts* in; the gate constructs `Decline` explicitly, so this is a
choice made at a call site and not a default anybody can drift into.

**Why this was open, and what made it decidable.** D5's amendment framed the two arms as
*advertise → 53 hard halts into the interpreter* against *decline → 106 fallback arms into a
spinlock that anti-scales 21x*. Read quickly, "hard halt" sounds fatal, which would have made
declining mandatory rather than chosen. It is not. **D5 risk 4 states the opposite in its own
words:** the 231 unimplemented decoder entries, LSE among them, *"surface cleanly via
`InterpreterFallback` at ~87 ns per trap, so they are correct but slow."*

So **both arms execute correctly** and the question is purely cost:

| | cost per site | behaviour as threads rise |
|---|---|---|
| **Advertise** | ~87 ns interpreter trap, 53 sites | per-context, so it **scales** |
| **Decline** | an uncontended `LDXR`/`STXR`, 106 sites | one **global** spinlock — **21x anti-scaling, 1 → 16 threads** |

An uncontended exclusive pair is a few nanoseconds against 87 for a trap, so declining is roughly an
order of magnitude cheaper *while thread counts are low* and materially worse once they are not.

**Why that resolves it for now.** The 3,594 initializers and the path to a first frame are not
thread-heavy: the engine has not started its worker pools while its statics are still constructing.
Declining takes the cheap arm exactly where the cheap arm applies, and pays nothing for the
scalability it is not yet using. Advertising would put 53 interpreter traps into the hottest,
least-parallel part of the run to buy scaling that nothing is asking for.

**What would reverse it, stated now so it is not rediscovered.** Two triggers:

1. **M8 (interactive), measured under real thread load.** Roblox is heavily multithreaded and D5
   names the spinlock as compounding with risk 4. The moment worker pools are live, the 21x figure
   is the one that matters and this decision must be re-measured rather than assumed.
2. **`fastmem_exclusive_access`, which D5 records as the untested mitigation.** If it removes the
   global spinlock, declining becomes correct permanently and the trigger above disappears. Testing
   it is worth more than re-arguing this decision, because it is the only path where *neither* arm
   costs anything.

**Cost if wrong: low, and reversible in one bit.** It is one enum value at one call site, with no
code shaped around it — which is why the type keeps `Advertise`, `Decline` and `Undecided` distinct
rather than collapsing the last two.

**What this does not decide.** Whether to advertise anything *else* in `AT_HWCAP`. `Decline` is all
zeroes; a later phase that needs a different capability advertised is making a new decision, not
extending this one.


---

## D27 — Texture transcoding is ETC1 only, decoded to RGBA8, in a crate that cannot reach the OS

M6 groundwork, built ahead of the renderer because it is the one piece of M6 that is pure
computation: no guest, no boundary, no JNI, no OS. Full census and method in
`docs/research/texture-formats.md`; the tool is `tools/texture_census.py --check`.

### The census came first, and it changed the scope

HANDOFF records the constraint: the host GPU samples **neither ETC2 nor ASTC**, BC1/BC3/BC7 yes
(`graphics-spike.md` §3). It does **not** say which of those two families the APK actually uses, and
"ETC2 or ASTC" is eleven GL formats and twenty-eight respectively. Measuring first turned weeks of
speculative work into one format.

| | |
|---|---|
| Compressed mobile-format containers in the APK | **38**, every one `GL_ETC1_RGB8_OES` (`0x8D64`) |
| ASTC / ETC2 / EAC / PVRTC / KTX2 bytes anywhere in the APK | **0** |
| 4×4 ETC blocks walked | **813,802** |
| of those, in ETC2's T, H or planar modes | **0** |
| Everything else block-compressed | DXT1/DXT3/DXT5 and `DXGI_FORMAT_R8_UNORM` — natively sampled |
| The engine's streamed-asset compression vocabulary | `dxt`, `etc`, `etc2`, `uncompressed`. `"astc"` does not occur in `libroblox.so` at all |

**Two things in that table are load-bearing and neither was obvious.**

First, the blocks were walked rather than trusted. `GL_ETC1_RGB8_OES` and
`GL_COMPRESSED_RGB8_ETC2` share a container and a bit layout and differ only in what an
out-of-range base-plus-delta *means* — ETC1 forbids it, ETC2 reuses it as T, H and planar mode. A
header saying ETC1 does not prove the payload is; 813,802 blocks with none of the escapes does.

Second, the census classifies by leading bytes, never by extension. `apk-analysis.md` §8.2's
file-type table lists `.ktx` 26 and `.tex` 12 as separate rows, and the twelve `.tex` files **are
KTX1 files** — they are the skybox. Scoping from the extension gives 26 ETC textures and no skybox.

### Decision 1 — implement ETC1 and refuse the rest by name

`GL_ETC1_RGB8_OES` → RGBA8 is implemented. Every ETC2, EAC, ASTC and S3TC enum is refused with its
**specification name** in the message, and a block that escapes into one of ETC2's three modes
fails naming the mode and the block index. Nothing approximates: there is no "unknown mode, use the
average colour" arm, because "close enough" colour is the believable wrong answer this project
refuses everywhere else.

**Why the streamed half does not force ETC2.** Omnidroid answers the capability queries the engine
asks, because it *is* the GLES/Vulkan surface. Advertise DXT — which the host genuinely has — and
decline ETC2 and ASTC, and the engine asks the CDN for `dxt`. The APK's 38 baked files are fixed
whatever we advertise, which is exactly why ETC1 is mandatory and ETC2 is not. That half is
INFERENCE from strings and is not verified until M6 runs; the vocabulary itself is VERIFIED.

### Decision 2 — decode to RGBA8, do not re-encode to BC1, and the reason is testability

ETC1 and BC1 are both 4×4 blocks in 8 bytes, so a transcode would hold the 6:1 compression, and the
difference is real: 6,510,416 B of ETC payload against **52,079,224 B** as RGBA8.

It is still the wrong thing to build first. ETC1 → RGBA8 is **exact** — the specification defines an
integer result for every input, so a known-answer test derived from the specification either passes
or finds a bug. ETC1 → BC1 is an **encode**: two ETC sub-blocks with independent luminance
modulation have to be refitted onto BC1's single endpoint pair, no output is uniquely correct, and
the only available oracle is somebody else's encoder. **This project has already paid for exactly
that**: `Ipv6Addr::Display` was used as the oracle for bionic's `inet_ntop` and disagreed on 43 of
200,000 addresses (D25). A re-encoder, if VRAM ever forces one, is a second stage behind this API,
verified against it, with its quality loss measured rather than assumed.

### Decision 3 — its own crate, for D19's reason, decided rather than inherited

`omni-texture` is a separate crate with **zero dependencies** and `#![no_std]`, and it allocates
nothing — `decode` writes into a caller-supplied buffer. `cargo tree -p omni-texture -e normal` is
one line. `omni-gfx` re-exports it and is the only edge.

D19 kept `omni-bionic` separate because zero dependencies make "no OS access" checkable by
`cargo tree` rather than by review. The same argument applies here and is stronger: `omni-gfx` will
transitively link Vulkan and the windowing system, so folding the transcoder in would downgrade the
guarantee from *impossible* to *against the rules*. `#![no_std]` goes one step past D19's — zero
dependencies means it cannot reach an OS primitive through a crate; `no_std` means it cannot name
one.

**Five targets.** No `cfg(target_os)`, no OS crate, nothing target-specific: integer arithmetic over
a slice. Per D22's distinction that makes it genuinely correct on all five targets in the same sense
`std`'s arithmetic is, and a fabricated `unsupported` arm would be a false claim in the other
direction. What stays unclaimed is what has been **run**: Windows x86-64 only.

### Evidence

29 tests and a doctest, all passing; 20 mutation rows, **20/20 caught**, 15 direction A and 5
direction B. Every expected value is derived from the specification —
`OES_compressed_ETC1_RGB8_texture` and OpenGL ES 3.2 §8.7.3, tables 8.15 and 8.16 — and **no second
decoder was consulted as an oracle**. Single-bit vectors pin the column-first pixel numbering and
the two index bit planes separately, which is the transposition bug that decodes silently wrong. Two
real APK blocks are hand-decoded and committed as bytes, so they assert on a clone with no APK in
it. The APK sweep decodes all 38 textures and every mip level and cross-checks the Python census
from a second implementation — which is how the RGBA8 figure got corrected (see below).

Hostile input: truncation at every length, an undersized destination at every length, zero and
overflowing extents, non-multiple-of-four extents, and all 2^24 base-colour triples in both modes
against an independently written escape predicate. Nothing panics.

**MEASURED cost**, whole baked set (38 textures, 361 mip levels, 813,802 blocks), release, n = 11
runs: median **31.72 ms**, min 31.30, max 32.11 — **39.0 ns/block**, 1,642 MB/s of output,
single-threaded, one core, no SIMD.

**A figure corrected during this work.** The census first reported the decoded size as
52,083,328 B, which is `813,802 blocks × 16 × 4` — whole *blocks*. The *images* are **52,079,224 B**:
the 2×2 and 1×1 mip level of each of the 38 textures still occupies a full 4×4 block, so 27 padded
texels × 38 files = 1,026 texels = 4,104 bytes. Found by the Rust sweep disagreeing with the Python
census, which is the only reason two implementations exist. Both figures are now reported, labelled.

### Cost if wrong

**Low and bounded.** If a later build of Roblox, or a streamed asset, does ship ETC2 or ASTC, the
failure is a **named refusal at load**, not a wrong image — which is the whole point of refusing by
name. The work to add a format is additive: a new `CompressedFormat` variant and a decoder beside
`etc1.rs`, with the format table already carrying every name.

### What this does not decide

- **Whether the engine loads the baked ETC1 assets when ETC1 is not advertised.** It may skip them,
  giving no skybox rather than a wrong one. Not determinable statically; settled by running M6.
- **Where a device limit lives.** `maxImageDimension2D` is 32,768 on this host, and deliberately is
  **not** enforced here — mutation row `texture-B4` injects exactly that over-correction. The limit
  belongs where the image is created.
- **The container layer.** The engine parses KTX itself; this crate takes block data.
- **Anything about the renderer.** `omni-gfx` is still a re-export and a doc comment.

---

## D28 — JNI without a JVM: 409 members is not a JVM, and the one entry that reads like one

**Decision.** `crates/omni-android/src/jni/` implements `jni-surface.md` §8 steps 6-12: a `JavaVM`
and a `JNIEnv` in guest memory, a class and member registry, handles that are checked indices
rather than pointers, and a host-owned startup script. **It does not violate D7**, and the reason
is structural rather than a matter of degree.

### Why 104 classes and 409 members is not the thing D7 forbids

`jni-surface.md` §6 searched for every mechanism that would force dex execution and found **all of
them absent**: zero hits for `java/lang/reflect/*`, `Class.forName`, `getDeclaredMethod`,
`defineClass`, `java/lang/invoke/*` or `dalvik/system/*`, and `JNIEnv::DefineClass` (slot 0x28) is
**never dereferenced**. Nothing loads code. What the engine does is look members up by name and
descriptor and call them, and 90% of what it looks up is Roblox's own thin Kotlin shell — getters
and notification sinks **whose behaviour Omnidroid gets to define**. Defining them is not
interpreting them.

**The one entry that reads as a D7 violation and is not: `java/lang/ClassLoader`.** §6 identifies
`loadClass`/`findClass`/`getClassLoader` as the canonical *"cache the app `ClassLoader` in
`JNI_OnLoad` so `FindClass` resolves app classes on threads attached later"* pattern, reached from
`NativeObjectManager.getClassLoader()` and from
`RBX::Security::Android::Detail::JvmClassLoaderHelper`. It is a **name resolver**, and this layer
implements it as one: `Answer::ResolveClass` looks the argument up in the registry and returns the
`jclass` `FindClass` would. No bytes are read, no class is defined, and `DefineClass` stays a
refusal that names itself. It is a variant of its own so that the reading is unavoidable to
whoever edits it next.

### What it answers, and what it refuses

**59 of 233** `JNINativeInterface` slots and **2 of 8** `JavaVM` slots are ever dereferenced (§0).
All 233 + 8 get a real thunk address; the 174 this layer does not implement produce
`AbiError::JniRefused` **naming themselves**. There are no plausible stubs: §8.1 ranks "`FindClass`
returning `NULL` where the caller does not check" third among the expected failure modes precisely
because a believable answer fails thousands of instructions later.

A member the host has not decided is `Answer::Unanswered`, and **calling** one refuses by name.
`Jni::define` is how an embedding decides — the same shape as `Bionic::set_hwcap_policy` (D26),
where the type refuses until a call site chooses.

### The two-table class registry, and its precedence

| table | what it is | answers |
|---|---|---|
| `classes::DECLARED` | hand-written, from §3.1's tiers and from `classes2.dex` | decided |
| `surface::DEX_SURFACE` | **generated** by `tools/gen_dex_surface.py`: 98 classes, 1,526 members | `Unanswered` |

The generated table exists because a **lookup** that misses and an **answer** that is missing are
different failures and both are real at once. §3.1's Tier 0 members are `CHECK_NOT_NULL` aborts;
§3.1's Tier X classes are absent from the whole APK and the engine tolerates null for them. So a
miss returns null with a pending exception **and is recorded** in `Jni::misses`. What made the
generated table necessary was measured: while a class the engine names was undeclared, the pending
`ClassNotFoundException` that nothing cleared made the engine's own `JNIEnvScope` abort with
`RBXCRASH: JNIException (JNI exception pending when entering JNIEnvScope)`. With the table in,
**0 misses** across the whole startup path.

Precedence is hand-written-wins, and it is held by **two** independent things — `extend_with` adds
only members a class does not already have, and `Registry::method` takes the first match. Two
mutation rows failed to break it with one edit each before `jni-A9` broke both halves at once.

### What this decision is evidence of, measured

`cargo test -p omni-android --release --test jni_startup`, on the real `libroblox.so` after all
3,594 initializers, n = 1 run:

| | |
|---|---|
| `JNI_OnLoad` at `base + 0x2173ff4` | returns **`0x00010006`** |
| step 6a/6b | 23 `FindClass`, 46 `GetStaticMethodID`, 20 `NewGlobalRef`, 18 `ExceptionCheck`, **0 misses** |
| `GetEnv` / `AttachCurrentThread` | 11 / 1 — the thread began detached, so both JavaVM slots ran, and it attached with the name `"Main"` out of `JavaVMAttachArgs` |
| §8 steps 7-12 | **19 of 21** downcalls return |
| imported symbols called across the whole run | **113 distinct** |

**Cost if wrong: high but bounded, and visible.** The bet is that the Java side can be *defined*
rather than executed. The 19 of 21 is what tests it, and the two that do not return name what they
need rather than misbehaving.

---

## D28 (amendment 1) — three corrections M4's gate made to things already written down

Each was found by running the engine, and each is a case where the earlier statement was reasonable
and wrong.

**1. `MADV_DONTNEED` was refused on a false premise (D21).** D21 refused it because meeting its
"a later read returns zero" guarantee would mean *writing* zeroes across the range, committing
every lazy granule the call was asking to release. That assumed the only way to zero a range is to
write to it. It is not: `advise_idle` + `reclaim_idle` **decommits** with `MEM_DECOMMIT` — D10's
only primitive that gives commit charge back — and the demand pager faults the range back in as a
freshly committed, zero-filled page. The guarantee is met by giving the memory back, which is the
direction the call asked for in the first place. It is carried out now, the range is **verified**
afterwards (no entry in it may report committed bytes), and the test reads the bytes back **through
guest code** rather than through a host pointer.

Two things a reader needs: `reclaim_idle` is space-wide, so one `MADV_DONTNEED` makes every
outstanding `MADV_FREE` take effect at once — which `MADV_FREE`'s contract permits. And
`MADV_REMOVE` stays refused: it punches a hole in an *underlying object*, and every mapping this
layer gives the guest is private and anonymous.

**2. `__strncpy_chk2`'s source check was wrong in the refusing direction.** It failed whenever
`n > src_size`. Bionic's loop checks per byte actually read and stops at the source's NUL, so
`strncpy(dst, src, sizeof dst)` with a shorter source — the commonest FORTIFY shape there is — is
legal and copies with NUL padding. `n > src_size` alone now fails only when there is no NUL in the
first `min(n, src_size)` bytes, which is the case where bionic really would read `src[src_size]`.

**3. §8 step 9's order is a list, not a proof.** §8 puts `nativeInitFastLog` first and the two
directory calls fifth and sixth. The engine refuses that order: `nativeInitFastLog` throws
`Cannot initialize fastlog system.  Cache directory not set.` and raises `SIGTRAP`. §4.2 attributes
step 9's eleven downcalls to three different methods (`W0`, `T0`, `X0`) and nothing said which of
the three runs first. **`T0` does.** `script::SEQUENCE` runs the directories first.

**And one defect of this layer's own, for the record.** `GetObjectClass` on a `jclass` answered the
class itself. A `jclass` is an instance of `java.lang.Class`, and `JvmClassLoaderHelper` asks
*that* for `getClassLoader`; the null `jmethodID` went straight into `CallObjectMethodV`. Row
`jni-A1`.

**Three imports outside the 188, all bound now:** `strnlen` (from `JNI_OnLoad`'s registration
helpers), `gmtime` (from `nativeInitFastLog`), `getcwd` (from `nativeSetAssetPath`). D17 calls 188
a lower bound with 17,698 unfollowable indirect call sites behind it; `BEYOND_THE_PREDICTION` is
**eight** symbols now, not five.

**`getcwd`'s answer is a decision.** The guest's working directory is the confinement root, and
from inside it that is `/`. There is no `chdir` here and no per-process directory to change (D23),
so the only directory the guest can be *in* is that root. Answering the host's own working
directory would be a fact about this process rather than about the guest, and it would name a path
outside the confinement boundary.

**`LoggingProtocol.getProcessTimestamp()J` is a decision with one ASSUMED half.** The host owns
"when did this process start"; the **units** are assumed to be milliseconds since the Unix epoch,
which is the overwhelmingly common Android spelling (`System.currentTimeMillis`,
`SystemClock.elapsedRealtime` and `Process.getStartElapsedRealtime` are all milliseconds, though
the last two are measured from boot). If the engine ever subtracts this from its own
`CLOCK_MONOTONIC`, the difference is wrong by the machine's uptime. Nothing on steps 6-12 does —
the value is taken and stored — so the risk is **recorded and not discharged**. The table's default
is `Unanswered`; the gate decides it at its own call site.

**`sizeof(JavaVMAttachArgs) = 24` joins the ASSUMED layouts** (`FILE` 152, `dl_phdr_info` 64,
`struct tm` 56, `stat` 128, `statvfs` 112, `dirent` 280, `sigset_t` 8, `addrinfo` 48). There is
still no NDK on this machine. Its safety argument is the strongest of the family: the only field
read is `name` at offset 8, and every field of the struct is a pointer on LP64 except the leading
`jint` and its padding, so the layout is forced once the field order is right. It is **exercised**
— the engine really does pass one, and the name `"Main"` came out of it.

---

## D29 — M5: `initializeNativeCode` returns, and the two failure modes that fail silently

**Decision.** `jni-surface.md` §8 step 13 is reached by building the four things §5.2 says the
GameActivity constructor and its glue need — a pipe with real readiness, an `ALooper`, an
`AAssetManager` and an `AConfiguration` — and by pointing an **instrument** at each of §8.1's two
*silent* failure modes before either could happen. `crates/omni-android/src/ndk/` is new and is the
third thing in that crate the guest reaches, after the bionic adapter and the JNI tables.

### What it is evidence of, measured

`cargo test -p omni-android --release --test gameactivity`, on the real `libroblox.so` after all
3,594 initializers, `JNI_OnLoad` and §8 steps 7-12. **n = 1 run.**

| | |
|---|---|
| `initializeNativeCode` | returns a **non-zero `jlong`** |
| `activity->callbacks` (+0x00) | `== this + 0x50` |
| `activity->vm` (+0x08), `env` (+0x10) | the `JavaVM` and `JNIEnv` this layer built |
| `activity->sdkVersion` (+0x30) | **33**, which the host decided and `__system_property_get` answered |
| `activity->instance` (+0x38) | non-zero — **this is the assertion that §8 row 14's cond-wait completed**, because `GameActivity_onCreate` writes it only after the game thread signals `app->running` |
| `activity->assetManager` (+0x40) | what `AAssetManager_fromJava` returned |
| `msgread`/`msgwrite` (+0x150/+0x154) | two ends of one real `pipe()`, and the looper watches `msgread` |
| the looper (+0x158) | the one the host prepared on the calling thread |
| the game thread | its own `ALooper_prepare`, `AConfiguration_fromAssetManager` → `en-US 411x731 dp`, `addFd(ident 1, callback 0)` — `LOOPER_ID_MAIN`, exactly §5.2 |
| §8 steps 7-12 | **20 of 21**, up from M4's 19 |
| JNI misses | **0** |
| imported symbols called | **134 distinct**, up from M4's 113 |
| the engine | reaches `[FLog::NativeEngine] initializing.` |

### The two instruments, and that both were needed

§8.1 ranks two failure modes as *silent*, and neither was diagnosed after the fact — both were
made observable first, and both then fired.

**Failure mode 4 — a null `ALooper` makes the call return `0`.** A zero `jlong` is
indistinguishable from a handle the Java side would pass to all 23 other natives.
`Ndk::prepare_looper` is a **host** API precisely so a gate can assert a looper exists *before* the
call; `ALooper_forThread` answers null when there is none, because that null is a real answer the
engine branches on and refusing there would replace a measurable engine behaviour with this
layer's opinion.

**Failure mode 5 — a cond-wait deadlock is indistinguishable from a hang.** It is
indistinguishable *from outside*. `Bionic::parked()` is the inside: every guest thread blocked in
`pthread_cond_wait`, with the condition variable, the mutex it released, the thread and how long.
Maintained by an RAII guard, because every exit from the wait must remove the entry including the
failing ones.

**It fired, and it paid for itself in one use.** A created guest thread carried only the bionic
instance. The game thread `GameActivity_onCreate` spawns called `AConfiguration_new`, found no NDK
instance, refused, and died — so `app->running` was never set and step 13 waited on its condition
variable **for ever**. §8 row 14 and failure mode 5 at once. The watchdog printed
`thread GuestThreadId(1) in pthread_cond_wait on cond 0x…ff0 holding mutex 0x…fc8 for 180.05 s`
beside `last import: AConfiguration_new`, and the diagnosis took three minutes.
`ThreadHost::with_instance` is the fix, and it is the embedding's to state because `Bionic` must
not learn that `Jni` or `Ndk` exist.

`Bionic::parked_peak()` is a **watch** and is labelled one where it is defined: two threads
legitimately waiting and two threads deadlocked are the same number.

### The closed descriptor space, opened on purpose

D25 wrote `the_descriptor_space_poll_answers_over_is_closed` as a detector for one specific future
moment: *the day a phase binds `socket` for real, that test fails and this module has to grow a
real readiness source with it.* **M5 is that day, and the symbol was `pipe` rather than `socket`.**

The test was **replaced, not updated**. `poll` and `select` now answer from
`Filesystem::readiness`, a `match` over the descriptor kinds with **no default arm**, so the space
is still closed — closed under *kinds that have decided what they answer* rather than under *kinds
that cannot block* — and a sixth kind cannot be added without deciding. The successor asserts that
the descriptor-producing symbols bound here are exactly those whose readiness has been decided.

A pipe needs **no operating system**: both ends belong to the same guest, so it is a `VecDeque<u8>`,
two reference counts and a condition variable. Per D22's other half it therefore gets **no
fabricated `unsupported` arm** for Linux or macOS. That is the **fourth** phase running whose
OS-surface prediction was too high — files needed fifteen of seventeen operations to be one `std`
call (D23), threads needed nothing (D24), sockets needed nothing (D25).

**Nothing in `omni-platform`'s pipe ever waits**, and that is deliberate rather than incidental: a
read from an empty *blocking* pipe reports `WouldBlock` exactly as a non-blocking one does, and the
adapter decides what to do about it — bounded by `MAX_SLEEP_SECONDS`, refusal by name at the cap,
the same policy `poll`, `select` and `nanosleep` already apply. D16's runaway-guest defence is built
from step budgets a sleeping thread does not consume, so "how long may a guest block" is not a
question a platform seam should answer.

**The readiness generation is read before each attempt, never after the attempt failed.** The other
order loses a write that lands in between, which is the 1.0104 s lost wakeup of `VERIFICATION.md`
entry 11.

### Thirteen symbols beyond the prediction, and how each was found

`BEYOND_THE_PREDICTION` was eight after M4 and is **thirteen** now. The five M5 added split by
*method*, which is worth keeping:

| symbol | how it was found |
|---|---|
| `pipe`, `fcntl`, `write` | **decoded out of the binary** — §5.2's instruction-by-instruction reading of `initializeNativeCode`. Bound before the call that needed them existed |
| `pthread_attr_setdetachstate` | **ran into as an `Unbound`** by the gate. A pure binding gap: `omni_bionic::metadata::attr_setdetachstate` had existed since phase 3c and nothing called it |
| `strftime` | a **known** gap, recorded by M4 with the reason: there was no implementation to bind. There is now |

`AAsset_read` is bound and `libroblox.so` does **not** import it — only `libzstd-jni` does. It is
named in `NDK_BEYOND_THE_IMPORTS` with the evidence, for `freelocale`'s reason: a layer that hands
out an `AAsset` and cannot read it is worse than one that does neither.

### The M4 access violation, reproduced and fixed

D28's record kept "an early run exited with `STATUS_ACCESS_VIOLATION` **after both tests reported
`ok`**… teardown of a guest with live guest threads is the obvious suspect", unreproduced.

**It reproduces.** M5's gate finishes with the game thread inside `android_main`; the
whole-workspace run then died with `0xc0000005`, the guest's own
`[FLog::NativeMain] [android_main] Create a new NativeEngine:` the last line before it.
`stop_guest_threads` *asked* and nothing waited for the answer.

`Bionic::join_guest_threads(timeout)` is the missing half. **A `Drop` cannot do it**, and that is
structural: every running guest thread holds an `Arc<Bionic>`, so the instance's own `Drop` cannot
run while one is alive — the last reference is dropped *by* the last thread. The gate asserts the
join rather than making a best effort, because a join that timed out and carried on would put the
crash back under a comment claiming it fixed.

**Joining made three silent failures visible.** Nobody joins a detached thread, so
`guest_thread_failures()` is the only place one can surface:

* **two threads died on raw `syscall 98` — arm64 `futex`.** The engine's own workers issue raw
  futex syscalls, bypassing every `pthread_*` symbol this layer binds. The refusal is right — a raw
  syscall asks the kernel directly, and `-1`/`ENOSYS` is the believable wrong answer because
  callers carry an ENOSYS fallback and would route around the gap — but it is now an obstacle with
  a number on it rather than a silence. **This is the largest single thing standing between here
  and M6.**
* **one died on `CallObjectMethodV` with a null `jmethodID`** — §8.1's third failure mode, on the
  game thread.

None of them blocks M5: step 13 returns before any of them happens.

### What the run measured that the research had not

* **`android/view/MotionEvent` and `android/view/KeyEvent` are Tier 0** and §3.1 does not name
  them. `FindClass` missed, and the null went straight to `GetMethodID` — §8.1's third failure mode
  happening for real, one class beyond the prediction. The **22 + 11** member lists in
  `classes.rs` are read out of `Jni::misses` *with the descriptors the engine asked for*, not
  transcribed from the Android API: a transcription would have missed `getClassification` and
  `getActionButton`, both API 29 and later. They are independent confirmation of §4.4's buffered
  input finding — the glue reads the *Java* objects through JNI, which is why `AMotionEvent_*` is
  absent from the whole APK.
* **§4.4's "ANativeWindow (9)" is the APK's count, not `libroblox.so`'s.** That binary imports
  five: `_acquire`, `_fromSurface`, `_getWidth`, `_getHeight`, `_release`. The other four belong to
  `libimage_processing_util_jni` and `libsurface_util_jni`.

  **Reproduced twice since, by two readers who were not its author, and by a different method the
  second time** — attributing every `ANativeWindow_*` line to the per-library section it falls in,
  rather than reading the cited line numbers. Both witnesses agree: **9** distinct names across the
  APK, **5** in `libroblox.so`. The full attribution, which neither earlier reading had:
  `libimage_processing_util_jni.so` 5, `libsurface_util_jni.so` 5, `libzstd-jni-1.5.7-6.so` 4 --
  the four importers overlap, which is why summing them does not give 9 and why "the APK's count"
  is the only reading of §4.4 that is arithmetically possible.

  The same walk confirms M6's other standing figure: **17** `egl*` symbols in `libroblox.so`, as
  §8 row 25 and `HANDOFF.md` both say. That one now has a second witness too.
* **`sched_yield` was called 22,387,975 times** in one run — the AT_HWCAP decline's fallback path
  (D26) spinning. It is not a correctness problem and it is a large number; D26's "revisit at M8
  under real thread load" now has a figure attached to it.

### Cost if wrong

**Bounded and visible, as M4's was.** Everything this milestone added either answers or refuses by
name; the refusals that matter — an indefinite `ALooper_pollOnce`, `AAsset_openFileDescriptor`, a
raw `syscall` — each say what would have to be invented, and each names what would change it. The
bet M5 makes is the same one M4 made and it is now tested one step further: that the Java side can
be *defined* rather than executed, and that the NDK surface under it can be *implemented* rather
than stubbed.

## D30 — Global Constraint 8 is withdrawn: the guest gets a real network

**Decided by the project owner, 2026-09-22, in response to a direct question about this fork.**
The instruction is quoted rather than paraphrased, because it overturns a rule that is reasoned
about in five files and nobody should have to reconstruct it:

> Playable Roblox is the higher-priority requirement. Networking is allowed and required. Do not
> preserve any earlier "no runtime networking" constraint if it prevents login, settings fetches,
> game joining, or normal Roblox operation. Implement the smallest correct networking surface
> Roblox actually exercises, preserving instance isolation and portability. You may temporarily use
> a measured failure path such as EAI_NONAME only as a diagnostic to reach first rendering, but do
> not treat that as the finished behavior.

### What was true, and why it was right until it was not

Global Constraint 8 — "no network access at runtime" — is in every plan document from the
foundation onward. It was never arbitrary. D6 records that the APK under test is **cheat-injected
and carries a Luau executor**, so guest code is hostile by assumption, and a socket handed to it is
an unrestricted host socket. `socket`, `getaddrinfo` and `freeaddrinfo` were therefore refused *by
name*, and D25 recorded the pleasing consequence that `poll` and `select` needed no operating
system at all, because every descriptor in this runtime was a file, a directory or a standard
stream.

The constraint was reached by a runtime that could not yet do anything a network was for. It is
withdrawn by one that can: the engine now asks for
`https://clientsettingscdn.roblox.com/v2/settings/application/android` by name, and everything the
goal actually asks for — login, joining a game, playing it — is on the far side of that request.

### What replaces it, and what does *not*

**The threat D6 named has not gone away, and the answer to it is not a refusal any more — it is a
seam with a policy on it.** The shape is the one `omni-platform`'s filesystem already has and for
the same reason: `Bionic::set_filesystem_root` means a guest path resolves inside one host
directory and nowhere else, and it was never "the guest gets no files". Networking gets the
equivalent, so that "which network may this instance reach" is a question an embedding answers
rather than a question this layer has no way to ask.

Concretely, and these are requirements on the implementation rather than aspirations:

1. **`omni-platform` grows a `net` module**, and it is the only place a socket syscall is made.
   `ARCHITECTURE.md`'s table says "No `net` module was ever added either, and that is a result
   rather than an omission" — that sentence is now wrong and is corrected there, not deleted.
2. **Sockets live in the descriptor table `fs` already owns.** `poll`, `select`, `close` and
   `fcntl` observe **one** descriptor space; two allocators handing out the same number is a defect
   waiting for the guest to find it. `Entry` has no default arm in `readiness`, so adding a socket
   variant forces every decision to be made rather than inherited — keep it that way.
3. **Instance isolation is per-`Filesystem`**, i.e. per `Bionic` instance, exactly as descriptors
   already are. Two instances cannot see each other's sockets because they cannot see each other's
   descriptors.
4. **D25 is superseded in its operative half.** `poll` and `select` answered `ALWAYS` because
   nothing in the descriptor space could block; a socket can, so they now need real readiness and
   therefore a real OS call. D25's own text says "the day a phase binds `socket` for real, that
   test fails and this module has to grow a real readiness source with it". This is that day, and
   it arrived exactly as written.
5. **Portability is not relaxed.** Windows x86-64 stays the only tested host; Linux x86-64, Linux
   ARM64, macOS x86-64 and macOS ARM64 stay required-but-unverified. D23's sharper test — *is there
   one `std` call that serves all five targets?* — answers **partly** here, which is a third answer
   and has to be said out loud: the seam uses `std` where `std` serves and a per-OS backend only
   for the part it does not.

   > **Amended once the seam was built, because the line this paragraph originally drew was in the
   > wrong place.** It said `std::net` carries "connect/send/recv/non-blocking across all five" and
   > that only readiness needs a backend. The connect half is **false**: `std` cannot produce a
   > socket that is not already connected or bound — `TcpStream::connect` blocks,
   > `connect_timeout` documents a zero `Duration` as an error, and there is no `TcpStream::new`.
   > So creation, `bind`, `connect`, four socket options and `poll` are all backend work. The line
   > that does hold, and the one to reason with: **creating a socket is the backend; everything you
   > can do to one once you hold it is `std`, and is written once for all five targets.**

### The smallest correct surface, and how "smallest" is decided

**By what the guest actually calls, measured, and not by what it imports.** `libroblox.so` imports
thirty-odd network symbols (`socketpair`, `listen`, `accept4`, `recvmmsg`, `if_nametoindex` …) and
importing is not calling — D17's whole point. The rule that has held all session holds here: a
symbol is bound when a run has reached it, and `Bionic::guest_thread_failures()` now says so by
name on the first run that does.

The engine's TLS is its **own** — it carries OpenSSL — so this layer owes sockets and DNS and not
one line of cryptography.

### `EAI_NONAME` as a diagnostic, and the trap in it

The owner's instruction permits a measured failure path to reach first rendering and forbids
treating it as finished. That is worth restating as a rule, because it is `VERIFICATION.md` entry
14's shape exactly: **a deliberate failure that gets you past a gate is indistinguishable, a week
later, from an implementation that works.** If a temporary `EAI_NONAME` is used, it must refuse to
be permanent — it belongs behind an explicit, loudly-printed switch, never as the default, and no
test may assert the behaviour it produces as though it were the contract.

## D4 (amendment 1) — identity fastmem does not confine the guest, and the docs said it did

**Found while deciding `vkMapMemory`, and it changed the question.** The premise everyone had been
reasoning from — written in `HANDOFF.md`, and implied by `dynarmic/mod.rs`'s own comment — was that
a host pointer handed to the guest is useless to it because `omni_mem::admit` refuses anything
outside `GuestSpace`. **The first half is true and the second half is false.**

Verified in the source, not inferred: `crates/omni-cpu/src/dynarmic/mod.rs` sets
`fastmem_pointer = 0`, `fastmem_address_space_bits = 64`, `silently_mirror_fastmem = false`,
`fastmem_enabled = true`. A guest `ldr x0, [x1]` therefore compiles to a host load at exactly `x1`,
with **no bounds check in the generated code** — which is the whole of the 30-49x that setting buys
(n = 31, two loop shapes). The only thing that can stop such an access is a **host** page fault.

So the three defences that reject a foreign address — `GuestSpace::contains`, `admit` rule 1 via
`region_at`, and `DemandPager::handle_fault`'s `NotOurs` — all sit on paths the guest's own
instruction stream never takes:

| address the guest dereferences | what happens |
|---|---|
| nothing mapped anywhere in this process | host fault → pager says `NotOurs` → typed `ExitReason::MemoryFault`. **Works as documented.** |
| mapped in this process, outside `GuestSpace` — the code cache, the thunk region, the Rust heap, a DLL, a driver's mapped memory | **no fault; read or written directly** |

`admit` is what stops *this layer* dereferencing a guest-chosen number — Global Constraint 11 — and
that is a narrower claim than guest memory isolation. The two had been conflated.

### What follows, and what does not

* **Omnidroid is a compatibility layer, not a sandbox**, and no document should say otherwise. D6
  records that the APK under test is cheat-injected and carries a Luau executor; under identity
  fastmem, guest code that computes an address reaches whatever is there. That is a property of the
  D4 trade, not a defect introduced by anything since.
* **It changes design decisions, immediately.** Handing the guest a host pointer does not fail
  safely — it *works*, silently, until a shim re-validates the same pointer and refuses it far from
  the cause. That is a worse failure mode than a refusal, and it is the argument for the
  `VK_EXT_external_memory_host` route recorded under the `vkMapMemory` decision: memory that is
  already inside `GuestSpace` keeps one story true on both paths.
* **It is not a patch to apply in passing.** Isolation would mean giving up identity mapping
  (`fastmem_pointer` at a reserved base and `address_space_bits` at the guest's real width, so an
  out-of-range address wraps into the reservation) and paying the 30-49x, or reserving the whole
  range around `GuestSpace` so everything outside it is guard pages. Either is a D4 decision to
  reopen with measurements.

The comment in `dynarmic/mod.rs` said a wild guest address "faults instead of aliasing a valid
page". It is true for the row above where it is true and silent about the row where it is not —
`VERIFICATION.md` entry 13's shape exactly, in the file whose behaviour it describes. Corrected in
place there, under **"What fastmem does not check"**, rather than deleted.
