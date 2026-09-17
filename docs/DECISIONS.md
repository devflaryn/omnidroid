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
