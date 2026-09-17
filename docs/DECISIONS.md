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
**Fact, not a choice.** `libroblox.so` uses `DT_ANDROID_RELA` (APS2, SLEB128-packed) **exclusively**:
568,272 packed relocations (568,194 `R_AARCH64_RELATIVE` + 534 `JUMP_SLOT`), and has **no `DT_RELA`
and no `DT_RELR`**. A loader that handles only standard or `RELR` relocations applies literally zero
relocations to Roblox and cannot work at all.

Other loader requirements measured from the same binary:
- **3,594 `DT_INIT_ARRAY` entries** run before `JNI_OnLoad` — all must succeed.
- `PT_GNU_RELRO` covers 5,205,568 bytes and must be made read-only at the right moment.
- **No `PT_TLS` and no `STT_TLS` symbols anywhere in the APK** — there is no ELF TLS to implement.
  Thread-local storage is `pthread_key_*` only. TLSDESC, `__tls_get_addr`, and DTV modelling are
  **not needed**, which is a meaningful saving; that effort belongs in `pthread_key_*` performance
  instead.
- No ifunc, no BTI/PAC/MTE, no `DT_TEXTREL`.
- The C++ runtime is **statically linked** (no `libc++_shared.so`), so the unwinder lives inside the
  guest and reads 11.5 MB of `.eh_frame`. It resolves frames via **`dl_iterate_phdr`**, so that
  function must be faithful, not a stub — C++ exceptions will not work otherwise.
- All 11 `.so` are **DEFLATED and only 4-byte aligned**, so they cannot be mapped in place from the
  APK. They must be decompressed, which makes `extractNativeLibs` effectively true and rules out
  zero-copy segment mapping for this APK.
- Two `DT_NEEDED` libraries (`libOpenSLES.so`, `libOpenMAXAL.so`) import **zero** symbols but must
  still exist as loadable objects, and 10 `AMEDIAFORMAT_KEY_*` imports are **data** objects rather
  than functions — both are failure modes with no symbol name to guide diagnosis.

Evidence: `research/apk-analysis.md`. The APS2 decoder used was validated byte-exact: it consumed
2,100,778 of 2,100,778 bytes and produced exactly the 568,272 declared relocations.

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

**What is scarce:** commit charge. `MEM_COMMIT` debits the system commit limit **immediately on
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
