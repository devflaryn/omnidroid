# Omnidroid Prior Art Research

Scope: prior art for running the ARM64 Android Roblox APK on a desktop x86-64 host via a
compatibility/runtime layer — no full Android emulator, no virtualization (QEMU
system mode, KVM, WHPX, Hyper-V, VirtualBox, Android VM all excluded).

Method: repos were shallow-cloned (`git clone --depth 1`) into a scratchpad and
read directly (grep/cat of source, licenses, decompiled artifacts). Where a repo
was not cloned, that is stated explicitly. Claims are labeled **[VERIFIED]** (I
read the actual code/license/binary artifacts myself in this session),
**[DOCUMENTED]** (from an official doc/README/blog I fetched but did not verify
against code), or **[UNVERIFIED CLAIM]** (a third party's assertion I could not
independently confirm).

---

## Summary table

| Component | What it does | Direction / Platform | License | Reusable for Omnidroid? | Confidence |
|---|---|---|---|---|---|
| Sober (vinegarhq/sober) | Closed-source Roblox-on-Linux runtime | ARM64 Android APK → x86-64 Linux (claimed) | Proprietary; GitHub repo is issue-tracker only, no source | No — no source available | High (repo emptiness verified) |
| "Open Sober" (0x06cf/open-sober) | Self-described reimplementation of Sober | ARM64→x86-64 (aspirational) | MIT | No — non-functional scaffold, wraps `qemu-aarch64` subprocess, most modules unimplemented | High (code read) |
| sober-oss (Z3ki/sober-oss) | Self-described RE of Sober | n/a | MIT | No — its own evidence (strings/decompilation) does not support its prose claims | High (code/data read) |
| Digitalis (DigitalisX64/digitalis) | AOSP/Berberis-based ARM64 backend, in-progress | ARM64 Android app → x86-64 (inside AOSP/emulator) | Apache-2.0 (AOSP-based) | No — is an in-tree AOSP/NativeBridge component, not embeddable; source not in this meta-repo | Medium (repo structure read; core translator source not present) |
| Berberis (AOSP, upstream) | Google's open binary translator framework | **riscv64 → x86-64** (current upstream) | Apache-2.0 (AOSP default; no LICENSE file seen in this repo) | No — no ARM64 frontend in the public tree today | High (directory listing fetched) |
| dynarmic | ARM dynamic recompiler / JIT | **A64 (AArch64) & A32 guest → x86-64 or AArch64 host** | Custom permissive (ISC/ 0BSD-style, merryhime) | **Yes — strongest candidate** | High (code read) |
| FEX-Emu | x86/x86-64 emulator for ARM64 Linux | x86(-64) → ARM64 (wrong direction) | MIT | Only as design reference (thunking, IR) | High (code/README read) |
| box64 / box86 | x86-64/x86 userspace emulator | x86(-64) → ARM/RISC-V/LoongArch (wrong direction) | MIT | Only as design reference (lib wrapping, dynarec) | High (code/README read) |
| Rosetta 2 | Apple's x86-on-ARM translator | x86-64 → ARM64 (wrong direction) | Proprietary, closed | No — technique-only reference | High (Apple docs) |
| ARM64EC | Windows x86-64-on-ARM64 ABI | x86-64 → ARM64 (wrong direction) | Proprietary, closed | No — technique-only reference | High (MS docs) |
| hangover | Runs x86/x86-64 Windows apps on ARM64 Wine | x86 → ARM64 host (ARM64 is the host, not guest — wrong pairing) | LGPL-2.1 | No — wrong direction; only WoW64-style syscall-breakout pattern is a useful reference | High (code/license read) |
| libhybris | Loads bionic-linked Android `.so` (drivers) into glibc processes | Same-arch bionic↔glibc bridging (no CPU translation) | Mixed: Apache-2.0/BSD core, some LGPL/GPL3/ISC/MIT files | Partial — architecture/technique reference for bionic↔glibc symbol bridging; license mixing needs file-by-file care | High (code/license read) |
| android_translation_layer | Runs real Android APKs (ART + native `.so`) on desktop Linux, no Android OS/container/VM | Same-arch only (pairs with a CPU translator for cross-arch) | **GPL-3.0-or-later** | Architecture reference only — GPL blocks direct embedding in closed-source Omnidroid | High (code/license read) |
| waydroid | Full Android (LineageOS) system in a container | Uses Linux namespaces/containers (confirmed) | GPL-3.0 | No — containerized, GPL, out of scope by task constraints | High (code/license read) |
| anbox | Full Android system in a container (deprecated 2023) | Uses Linux namespaces/containers (confirmed) | GPL (GPL-3.0, COPYING.GPL) | No — deprecated, containerized, GPL | High (code/license read) |
| ANGLE | GLES/EGL → native GPU API translation layer | Graphics only | BSD-3-Clause | Yes, as architecture pattern / possibly direct reuse for GLES translation | Medium (not cloned; from established public docs) |
| gfxstream | Guest GLES/Vulkan commands serialized to host renderer process | Graphics only (Android Emulator / ChromeOS ARCVM lineage) | Apache-2.0 | Architecture reference | Medium (not cloned) |
| virglrenderer | virtio-gpu command stream → host GL | Graphics only | MIT | Architecture reference | Medium (not cloned) |
| Zink | OpenGL-on-Vulkan (Mesa) | Graphics only | MIT | Architecture reference | Medium (not cloned) |
| MoltenVK | Vulkan-on-Metal | Graphics only | Apache-2.0 | Architecture reference | Medium (not cloned) |
| DXVK | D3D9/10/11 → Vulkan | Graphics only | zlib | Architecture reference | Medium (not cloned) |
| vkd3d-proton | D3D12 → Vulkan | Graphics only | LGPL-2.1 | Architecture reference (license blocks embedding) | Medium (not cloned) |

---

## 1. Sober and "Open Sober"

### Sober itself
`vinegarhq/sober` on GitHub **[VERIFIED]** contains only `README.md` and a logo — the
README states outright: *"This GitHub repository serves as an issue tracker for
bugs or feature suggestions... See sober.vinegarhq.org for more information."*
There is no source code in the public repo. Two open GitHub issues, "Add support
for ARM/AArch64 architecture" (#1148) and "add support for aarch64 devices"
(#279), plus the Flathub listing (`org.vinegarhq.Sober`) advertising **x86_64
only**, together indicate Sober today runs on x86-64 hosts and (if it truly
translates the Android ARM64 Roblox binary) must be doing some form of ARM64→x86-64
translation — **[DOCUMENTED / inferred, not verified against source]**, since no
source or binary was available to inspect in this session.

### "Open Sober" (`0x06cf/open-sober`) — does it really exist?
It exists as a repository, but code inspection shows it is **not a working
reimplementation**. **[VERIFIED by reading the code]**:
- Only `libbadcpu` (an x86-64 SIGILL-based emulator for missing CPU features like
  `POPCNT`/`MOVBE`/`LZCNT`/`TZCNT`/`ANDN`/`BLSI`/`BLSMSK`/`BLSR`) has any
  implementation; `libloader`, `sober-core`, and `sober-services` are marked
  "Planned" (⬜) in its own README table.
- The actual "translation" in `crates/sober-core/src/qemu.rs` is a thin Rust
  wrapper that shells out to `qemu-aarch64` (QEMU user-mode) as a subprocess and
  `LD_PRELOAD`s a hand-written bionic→glibc symbol shim
  (`crates/sober-core/src/bionic_shim.S`, `bionic_init.c`). It performs **no**
  original ARM64→x86-64 CPU translation of its own.
- The repo's `HANDOFF.md` literally states `**Repo:** https://github.com/glm-5-turbo/open-sober`
  — the fork/dev account name is an LLM model name, and the single commit in the
  clone is `Merge commit 'dea8a38' into dev`. This is strong internal evidence
  the repo is an AI-agent coding scaffold, not a community reimplementation with
  real development history.
- License: MIT (stated in its README).

**Conclusion: "Open Sober" is real as a GitHub repository but is an early-stage,
largely AI-generated proof-of-concept that wraps QEMU user-mode rather than
containing a genuine from-scratch ARM64→x86-64 translator. It is not usable as a
dependency.**

### "sober-oss" (`Z3ki/sober-oss`) — reverse-engineering claims are not credible
This repo purports to be a Ghidra-based reverse-engineering of the real Sober
Flatpak binaries (`sober`, `sober_services`, `libloader.so`, `libbadcpu.so`) and
makes detailed architecture claims: that `sober` does ARM64→x86-64 translation
using `oaknut`/`dyncall`, downloads the APK from Google Play, etc.

**[VERIFIED by reading the repo's own evidence files]** — this is contradicted by
the repo's own data:
- `analysis/strings_sober.txt` (the claimed `strings` dump of the real `sober`
  binary) has **zero** occurrences of "android", "roblox", "vulkan", or "apk"
  (case-insensitive), and its content is unstructured noise (`.Cr`, ` 5I`, `#N2`
  …) that does not resemble real `strings` output of an application binary.
  Same for `analysis/strings_libloader_so.txt`.
- `decompiled/sober/sober.c` contains a syntactically invalid function signature
  — `void processEntry entry(void)` — something a real decompiler (Ghidra) would
  never emit. `decompiled/libloader/libloader.c` is generic linker boilerplate
  (`_DT_INIT`/`_DT_FINI`/`FUN_xxxxx` stubs) essentially indistinguishable from
  the `libbadcpu` file, i.e. not unique per-binary decompilation.
- By contrast, `analysis/strings_libbadcpu_so.txt` and
  `decompiled/libbadcpu/libbadcpu.c` **do** look like genuine Ghidra/`strings`
  output (real feature strings like `AVX:`, `POPCNT:`, `(Illegal Instruction)`,
  proper `FUN_00115c90 @ 00115c90` addressed sections). So the `libbadcpu`
  component's claims are plausible, but the `sober` main-binary claims
  (ARM64 translation, oaknut/dyncall usage, APK downloading) are **not backed by
  the repo's own evidence** and were most likely written by an LLM without a
  successful decompilation of the real 7.1 MB binary.
- `SECURITY_AUDIT.md` lists its "Auditor" as **"Maxwell (automated reverse
  engineering analysis)"**, consistent with an AI-agent persona rather than a
  named human researcher.
- License: MIT (repo's own `LICENSE`), and the repo explicitly says its `src/`
  is a "clean room" reimplementation, not verified against real Sober code.

**Conclusion: treat all of sober-oss's architectural claims about Sober's
internals (oaknut/dyncall/bionic-shim/APK-download pipeline) as unverified and
probably partly fabricated. Only the general existence of a `libbadcpu`-style
SIGILL-based x86 feature emulator is corroborated by string evidence that looks
genuine.**

**"Open Sober" misconception, stated plainly for the report:** there is no
credible, working, open-source Sober alternative or reverse-engineering of Sober
available today. Both projects found under that description are AI-agent
scaffolds of low-to-moderate credibility, not usable software or trustworthy
documentation of Sober's real internals.

---

## 2. ARM64 → x86-64 translation layers

### dynarmic — **[VERIFIED, most relevant find]**
Cloned `merryhime/dynarmic` (via the `lioncash/dynarmic` mirror after the primary
GitHub org returned 404 for direct clone; content matches upstream README).

- `src/dynarmic/frontend/A64/` and `src/dynarmic/frontend/A32/` — separate
  frontends confirm **both AArch32 and AArch64 (A64) guest support**, contrary to
  any assumption it's ARMv7-only. README states supported guest versions
  explicitly include `64-bit v8`.
- `src/dynarmic/backend/x64/` and `src/dynarmic/backend/arm64/` — confirms **x86-64
  host backend exists** (plus an AArch64 host backend and an experimental
  riscv64 one), i.e. this is a genuine **A64 guest → x86-64 host JIT**, the exact
  direction Omnidroid needs.
- License: `LICENSE.txt` is a plain ISC-style permissive notice ("Permission to
  use, copy, modify, and/or distribute this software for any purpose with or
  without fee is hereby granted...") by merryhime, functionally equivalent to
  0BSD/ISC — safe for closed-source/commercial reuse.
- Dependencies declared in `externals/CMakeLists.txt`: `fmt` (MIT), `mcl`
  (merryhime's own "merry class library", used internally), `oaknut` (MIT,
  AArch64 assembler, only needed for the AArch64 host backend), `xbyak` (BSD,
  x86 assembler for the x64 backend), `zydis` (MIT, x86 disassembler), `robin-map`
  (MIT, hash map), `biscuit` (only for the riscv64 host backend). All permissive.
- Used in production by Citra/Panda3DS/Vita3K/yuzu-family emulators as the
  ARM(64) CPU core — mature, actively used, and battle-tested for exactly the
  "ARM64 guest, JIT to host ISA" problem.

This is the single component in this research that is both directly on-target
(direction-correct) and permissively licensed. It solves *CPU instruction
translation only* — it does not solve bionic/libc compatibility, ELF loading, JNI,
or graphics; those remain separate problems.

### FEX-Emu — **[VERIFIED wrong direction]**
`FEX-Emu/FEX`, README: *"FEX allows you to run x86 applications on ARM64 Linux
devices, similar to qemu-user and box64."* Confirmed x86(-64)→ARM64, i.e. the
opposite of what Omnidroid needs. MIT license (`LICENSE`, Ryan Houdek). Its
thunking architecture (per-library Guest/Host shim pairs for GL/Vulkan calls) and
IR design are useful *design* references but no code is directly reusable for an
ARM64-guest engine.

### box64 / box86 — **[VERIFIED wrong direction]**
`ptitSeb/box64` README: *"Box64 enables running x86_64 Linux programs... on
non-x86_64 Linux systems such as Arm."* Confirmed x86(-64)→ARM/RISC-V/LoongArch
(box64 dynarec backends target Arm, RISC-V, LoongArch hosts — never x86 as a
host). MIT license. Again wrong direction for Omnidroid; useful only as a
reference for its "wrap native system libraries instead of emulating them"
performance technique — which doesn't directly apply here since Roblox's ARM64
native code needs actual instruction translation, not just libc wrapping.

### Rosetta 2 — **[DOCUMENTED, closed]**
Apple's technique (from Apple's own developer/security docs, not code — Rosetta 2
is proprietary and unavailable to inspect): x86-64 → ARM64 (again the reverse of
what Omnidroid needs, since Apple is running Intel apps on Apple Silicon).
Ahead-of-time (AOT) whole-text-segment translation writes out a signed,
device-key-sealed Mach translation artifact the first time a binary runs, with a
JIT fallback for code the AOT step can't handle (e.g., JIT'd guest code). Useful
only as an "AOT + JIT hybrid" design pattern reference.

### ARM64EC — **[DOCUMENTED, closed]**
Microsoft's ABI for mixed x86-64/ARM64 binaries on Windows-on-Arm. Direction is
x86-64 code compiled to run natively as ARM64 machine code with x64 calling
conventions preserved ("pre-jitted x86-64 on ARM64") — again the reverse
direction from Omnidroid's need. Proprietary, Windows-specific, not reusable.

### hangover — **[VERIFIED wrong pairing]**
`AndreRH/hangover`: runs x86/x86-64 **Windows** applications on **ARM64** Wine,
using FEX as the underlying CPU emulator and a WoW64-style syscall-breakout
design (emulate only the app, break out to native code at the Win32/Wine-unix-call
boundary). Here ARM64 is the **host**, x86 is the **guest** — the opposite
pairing from what Omnidroid needs (ARM64 guest, x86-64 host), so there is no
"ARM64 guest emulation backend" in hangover to borrow; it doesn't emulate ARM64
at all, it emulates x86 on top of native ARM64 hardware. License: LGPL-2.1
(`LICENSE`, verified). The syscall-breakout architectural pattern (emulate only
user code, do everything OS/graphics-related natively) is a good design
reference for where to place the Roblox/Android JNI and graphics boundary in
Omnidroid.

### Berberis / "ndk-translation" — **[VERIFIED via AOSP source browser; this corrects the task's framing]**
Fetched `android.googlesource.com/platform/frameworks/libs/binary_translation`
directly (not git-cloned; AOSP Gitiles browsed via WebFetch).
- The README (`+/refs/heads/main/README.md`) states Berberis is *"a dynamic
  binary translator to run Android apps with riscv64 native code on x86_64
  devices or emulators."* **Current upstream Berberis's guest architecture is
  RISC-V64, not ARM64.**
- The top-level directory listing shows only riscv64/x86-64-related directories
  (`android_api/`, `assembler/`, `backend/`, `decoder/`, `interpreter/`,
  `guest_os_primitives/`, etc.) — **no `arm`/`arm64` frontend exists in the
  public AOSP tree today.**
- No `LICENSE` file was visible in that repo's root listing; AOSP's default
  project-wide license is Apache-2.0, which is a reasonable inference but was not
  directly confirmed by a LICENSE file for this specific repo in this session.
- The older ARM→x86 translator historically used on x86 Chromebooks/ARC++ (often
  conflated with "ndk_translation" or Intel's proprietary "Houdini") is a
  **separate, distinct thing** from today's public Berberis and was not directly
  inspected in this session.

**Correction to the task's framing: Berberis, as published in AOSP today, is a
RISC-V64→x86-64 translator, not an ARM64→x86-64 translator.** Any ARM64 backend
for it is either historical/removed or currently closed-source (see Digitalis
below).

### Digitalis — **[VERIFIED repo structure; core translator source NOT present in this clone]**
`DigitalisX64/digitalis` (cloned) describes itself as *"The arm64-to-x86_64
binary translation based on Berberis framework... Built on AOSP 16 (API 36)."*
This repo, however, is a **meta/tooling repo only** — it contains build scripts,
Claude-Code agent-dispatch tooling (`.claude/commands/dispatch.md`,
`digitalis-dispatch.sh`), sample-app fetch scripts, documentation, and a
`docker/` prebuilt-packaging pipeline, but the actual Berberis ARM64 backend
source is obtained separately via `repo init -u git@github.com:DigitalisX64/manifest.git`
against a full AOSP checkout — that manifest/AOSP tree was **not** cloned in this
session (it would require a multi-GB `repo sync`), so I could not read the actual
translator code.
- License in this meta-repo: Apache-2.0 (`LICENSE`, verified).
- **[UNVERIFIED CLAIM, from the repo's own docs, not independently confirmed]**:
  its `docs/how-it-works.md` (explicitly self-labeled *"This document was
  generated by AI from the Digitalis source code and reviewed by a human"*)
  claims Google's own Android Emulator "Google APIs" x86_64 system images (API
  34–37) already ship a **closed-source** Berberis build with an ARM64 backend —
  registered as `ro.dalvik.vm.native.bridge=libndk_translation.so`, with symbols
  like `berberis::intrinsics::Arm64ReadFpcr`, plus
  `/system/etc/berberis/cpuinfo.arm64.txt` and `arm64_dyn`/`arm64_exe`
  `binfmt_misc` handlers, while plain AOSP images ship no translator. This is a
  specific, falsifiable, plausible claim consistent with widely-known Android
  Studio emulator behavior (ARM app compatibility on x86_64 emulator images is
  real and well documented in community sources), but I did not download an
  emulator system image to verify the symbol table myself.
- Even if Digitalis's ARM64 backend is real and functional, it is **architecturally
  not reusable as a standalone library**: it is designed to run inside Android's
  own `NativeBridge`/ART/`binfmt_misc` framework as part of a full AOSP build
  (`lunch`, `m`), not as an embeddable component for a non-Android desktop
  process. Extracting it would require re-hosting large parts of Android's
  native-bridge plumbing.

---

## 3. Android native runtime reimplementation layers

### libhybris — **[VERIFIED]**
Cloned `libhybris/libhybris`. README: *"libhybris is a way to load drivers
compiled for Android from 'regular linux processes'... allows you to load
drivers that link against the bionic c library inside processes whose native c
library is e.g. glibc, musl."* This is exactly the bionic↔glibc bridging
technique, but note it is designed for **same-architecture** driver loading
(e.g., an ARM32/ARM64 GPU driver blob on an ARM32/ARM64 Linux distro like
SailfishOS/Halium) — it does **not** perform CPU instruction translation, and is
typically used alongside a patched-down Android system running in a container
(per its own README: "launched as a systemd service or container").
- License: **mixed per-file**. The repo ships `LICENSE.Apache2`,
  `LICENSE.BSD-2/3/4`, `LICENSE.GPL3`, `LICENSE.ISC`, `LICENSE.LGPLv21`,
  `LICENSE.MIT`. Spot-checked headers in `hybris/common/*.c` (hooks.c,
  hooks_shm.c, logging.c, native_handle.c, sysconf.c, wrapper_code_generic_arm.c)
  — all Apache-2.0. Care is required per-file if reusing; do not assume the whole
  tree is one license.
- Relevant technique: `hybris/common/hooks.c` intercepts and forwards bionic
  libc/pthread symbol calls to the host libc via a hook table — a pattern
  directly applicable to Omnidroid's bionic→glibc bridging problem, license
  permitting per-file review.

### android_translation_layer — **[VERIFIED, most relevant "Android without Android OS" find]**
Cloned `android_translation_layer/android_translation_layer` (GitLab). This is a
genuinely working, actively maintained project (real screenshots of Angry Birds,
Worms 2, Gravity Defied, and Oculus-Quest BeatSaber running side-by-side on
Linux; a Hacker News thread about running NewPipe on Linux with it; explicit
notes about Apple Silicon page-size ART issues and X11/EGL/GDK quirks — all
signs of real, lived engineering rather than AI-scaffold content).
- It runs actual Android APKs — Dex/ART bytecode **and** native `.so` code — on a
  desktop Linux system with **no Android OS, no container, no VM**, using a
  standalone-built ART (`art_standalone`) plus a `bionic_translation` component
  and `libandroidfw` for resources/assets.
- Critically: it does **not** do CPU instruction translation between
  architectures — the BeatSaber screenshot is captioned "running on an aarch64
  laptop," i.e., same-arch execution. For Omnidroid's ARM64-APK-on-x86-64-host
  case, this project's approach would need to be paired with a CPU translator
  (dynarmic) for the native-library portion.
- License: **GPL-3.0-or-later** (`LICENSE.txt`, verified) — copyleft, blocks
  direct embedding of its code in a closed-source Omnidroid without releasing
  Omnidroid's own source; using it unmodified as an external GPL process is a
  possible but architecturally awkward option (it's a monolithic app, not a
  service designed for that use), and still leaves the ARM64→x86-64 CPU gap open.
- This is the best available architecture reference for "what does a from-scratch
  Android-app-runner without an Android OS actually need" — its `src/` tree
  covers JNI, `libandroidfw`, ART hosting, and native-lib loading concerns
  Omnidroid will also need to solve, even though the code itself can't be reused
  directly under Omnidroid's likely licensing goals.

### Bionic vs glibc requirements — **[UNVERIFIED / general knowledge, not confirmed against AOSP bionic source in this session]**
Running an Android-built ARM64 `.so` outside Android generally requires handling:
- **`DT_ANDROID_REL`/`DT_ANDROID_RELA`** — Android's packed/compact relocation
  format (APS2), not understood by a stock glibc `ld.so`.
- **`__cxa_*` ABI functions** (`__cxa_atexit`, `__cxa_finalize`, etc.) — present
  in both bionic and glibc but with some behavioral differences historically.
- **TLS model differences** — bionic's TLS slot layout/variant has differed from
  glibc's across Android API levels (AOSP's own `docs/elf-tls.md`, found via
  search but not read in full this session, documents this).
- **`dl_iterate_phdr`** — needed for C++ exception unwinding across shared
  objects; must be implemented/bridged.
- **ifuncs (GNU indirect functions / `R_*_IRELATIVE` relocations)** — used for
  runtime CPU-feature dispatch (e.g. optimized `memcpy`); a loader must resolve
  these correctly.
- **`.note.android.ident`** — a section identifying the target API level a
  `.so` was built for, used by bionic's linker to toggle compatibility behavior.

These are stated here as background technical requirements drawn from general
systems knowledge, **not verified by reading AOSP bionic source in this
session** — flagged explicitly per the task's instructions. `libhybris` and
`android_translation_layer` are both existing, real-world implementations that
had to solve this exact problem, and are the concrete places to study the actual
solutions in code rather than re-deriving them.

### Standalone bionic-compatible loaders, permissively licensed
No standalone, permissively-licensed "bionic-compatible ELF loader" distinct from
libhybris/android_translation_layer was found. The two real projects above are
GPL-family or mixed-license; a from-scratch, MIT/Apache-licensed bionic ELF
loader does not appear to exist as prior art — **this is likely something
Omnidroid has to write itself**, informed by reading (not linking) the GPL
projects' techniques.

### Waydroid and anbox — **[VERIFIED: both are containerized, confirming the task's assumption]**
- `waydroid/waydroid` README: *"Waydroid uses a container-based approach to boot
  a full Android system... uses Linux namespaces (user, pid, uts, net, mount,
  ipc)."* Ships a LineageOS-based (Android 13) system image. License: GPL-3.0
  (`LICENSE`, verified).
- `anbox/anbox` README: *"Anbox uses Linux namespaces... to run a full Android
  system in a container."* Explicitly deprecated since 2023 in favor of
  Waydroid/Anbox Cloud. Also states it *"reuse[s] what Android implemented
  within the QEMU-based emulator for OpenGL ES accelerated rendering"* — i.e. the
  goldfish/gfxstream graphics-pipe lineage, routed through pipes to a host
  daemon. License: GPL (`COPYING.GPL`, verified).

Both confirm the task's framing: these are container-based, full-Android-system
approaches, not applicable to Omnidroid's no-container constraint, but anbox's
graphics-pipe approach is a useful architecture reference (see graphics
section).

---

## 4. Graphics

Not cloned in this session (repos are very large; findings below are from
established public documentation, not fresh code reading — flagged as
**[DOCUMENTED]**, not [VERIFIED]):

- **ANGLE** (Google, BSD-3-Clause): translates GLES2/3 + EGL calls to a native
  backend (desktop GL, Vulkan, D3D11, Metal). Directly analogous to what
  Omnidroid needs if Roblox's Android renderer uses GLES.
- **gfxstream** (Apache-2.0; lineage: Android Emulator "goldfish"/AEMU, now a
  standalone `google/gfxstream` project): guest GLES/Vulkan API calls are
  serialized and sent over a transport (originally a QEMU pipe, now
  vsock/virtio-gpu-adjacent) to a host-side renderer process that replays them
  against the host's real GL/Vulkan driver. Used by the Android Emulator and
  ChromeOS's ARCVM.
- **virglrenderer** (MIT): the same architectural family as gfxstream —
  virtio-gpu command stream on the guest side, decoded and replayed against
  host OpenGL on the host side. Used by QEMU/crosvm for VM GPU acceleration.
- **Zink** (MIT, Mesa): implements desktop OpenGL on top of Vulkan — a "GL
  frontend over Vulkan backend" pattern, structurally similar to what an
  Android-GLES-over-host-Vulkan layer would need.
- **MoltenVK** (Apache-2.0): implements Vulkan on top of Metal — the general
  "translate modern low-level API X to native API Y" pattern.
- **DXVK** (zlib license) / **vkd3d-proton** (LGPL-2.1): translate Direct3D
  9/10/11 and 12 respectively to Vulkan; useful as a study of a
  mature, high-performance layered-renderer codebase, though the license (LGPL
  for vkd3d-proton) matters if any code were ever linked in rather than merely
  studied.

**Open question, not guessed at per task instructions:** whether the actual
Roblox Android APK renders via GLES3, GLES2, or Vulkan is unresolved here. A
2021 community write-up by a former Roblox graphics engineer (the "zeux" gist on
Roblox's graphics API tiers) indicates Roblox's engine targets Vulkan as the
primary path with GLES3/GLES2 fallbacks depending on device/driver support, but
this is **[UNVERIFIED for the current APK]** — it must be confirmed by the
separate APK-inspection track (e.g., checking which `libRobloxVulkan`/`libGLESv…`
native libraries and `AndroidManifest.xml` `<uses-feature>` entries are actually
present in the current Roblox Android APK).

---

## Corrections to common assumptions

1. **"Open Sober" is not a real, working alternative to Sober.** It exists as a
   GitHub repo but is a mostly-unimplemented AI-agent scaffold that wraps
   `qemu-aarch64` as a subprocess rather than containing an original
   ARM64→x86-64 translator. (Verified by reading its code and git history.)
2. **The "sober-oss" reverse-engineering repo's architectural claims about Sober
   are not credible.** Its own `strings`/decompilation evidence for the main
   `sober`, `libloader`, and `sober_services` binaries contains no
   application-specific data and includes syntactically invalid decompiled C —
   inconsistent with genuine Ghidra output. Only the narrower `libbadcpu`
   (x86 SIGILL feature-emulation) claim is backed by plausible-looking evidence.
3. **Berberis, as published in AOSP today, translates RISC-V64 → x86-64, not
   ARM64 → x86-64.** The task's framing ("Google's own ARM-to-x86 translators...
   Berberis") conflates the historical/closed ARM Chromebook translator with the
   current public Berberis project, which has no ARM64 frontend in its tree.
4. **hangover has no "ARM64 guest emulation backend."** It emulates x86 on top
   of native ARM64 (ARM64 is the host), the opposite pairing from what the task
   description implies when it asks to check hangover "for its aarch64
   emulation backends."
5. **FEX, box64/box86, Rosetta 2, and ARM64EC are all confirmed to translate in
   the direction opposite to what Omnidroid needs** (guest x86, host
   ARM/AArch64) — the task's suspicion here is correct, not a misconception to
   correct, but it's now independently confirmed by reading each project's own
   README/docs rather than assumed.
6. **Waydroid and anbox are both confirmed container-based** (Linux namespaces),
   not translation layers — again the task's suspicion is confirmed, not
   refuted.

---

## Recommended reuse set

**Pull in (as a library/dependency, permissive license, direction-correct):**
- **dynarmic** — for the ARM64 (A64) CPU instruction translation core
  (guest A64 → host x86-64 JIT). Custom ISC/0BSD-style license; depends on
  fmt/mcl/xbyak/zydis/robin-map, all permissive. This is the one component in
  this whole survey that solves the actual hard problem in the right direction
  with a license compatible with a closed-source product.

**Study but do not link/embed (copyleft or architecturally unsuited):**
- **libhybris** — study its bionic-symbol-hook-table technique
  (`hybris/common/hooks.c`) for bridging bionic libc calls to glibc; re-implement
  under Omnidroid's own license rather than linking (mixed-license tree,
  including LGPL/GPL3 files).
- **android_translation_layer** — study its ART hosting, `libandroidfw` resource
  handling, and native-lib loading approach as a reference architecture for
  "run an Android app with no Android OS"; do not link (GPL-3.0-or-later).
- **FEX-Emu's thunking architecture** (Guest/Host shim pairs per native library,
  e.g. for GL/Vulkan/OpenSL) — good design pattern for the JNI/graphics/audio
  boundary even though FEX's own code translates the wrong direction.
- **ANGLE / gfxstream / Zink architecture patterns** for the GLES↔host-graphics
  translation layer, once the APK-inspection track confirms whether Roblox
  Android uses GLES or Vulkan.

**Must be written from scratch for Omnidroid:**
- A permissively-licensed **bionic-compatible ELF loader/linker** (packed
  relocations, ifuncs, TLS model, `dl_iterate_phdr`, `.note.android.ident`
  handling) — no such standalone permissively-licensed project was found; the
  two real implementations (libhybris, android_translation_layer) are
  copyleft/mixed-license, so their techniques can inform a clean-room
  implementation but their code cannot be linked in directly.
- The Android JNI/NDK/asset-manager/looper/input glue layer connecting the
  translated Roblox native code to a non-Android desktop process (no existing
  permissively-licensed project fully covers this without an Android OS
  underneath).
- The bridge between dynarmic's JIT (CPU only) and (a) the ELF loader above and
  (b) a GLES/Vulkan translation layer — this integration is Omnidroid-specific
  and has no existing off-the-shelf equivalent found in this research.

---

## Open questions

1. **Does the current Roblox Android APK render via GLES3, GLES2, or Vulkan?**
   Not resolved here; requires the separate APK-inspection track (checking
   bundled native libraries and manifest `<uses-feature>` declarations).
2. **Is Sober's real internal architecture actually ARM64→x86-64 CPU
   translation, or does it rely on a native x86-64 Roblox Linux/Android hybrid
   build plus only an Android-compat shim layer (no true ISA translation)?**
   The two available "RE" repos (open-sober, sober-oss) make conflicting/
   unsubstantiated claims on this point and cannot be trusted as evidence
   either way; the only clean signal found was the two open GitHub feature
   requests for aarch64 support, which imply x86-64-only today but do not by
   themselves prove real ISA translation is happening (as opposed to, e.g., a
   from-source x86-64 build of the same Roblox engine plus Android-shim glue).
3. **Is Google's closed `libndk_translation.so` in emulator system images
   (claimed by Digitalis's docs) actually a Berberis build with an ARM64
   backend, and if so, is there any legal path to extracting/studying it (it is
   Google-proprietary, not AOSP-published)?** Not verified in this session.
4. **What is dynarmic's real-world performance and correctness on a full,
   large, JIT-heavy native workload like Roblox's engine (as opposed to the
   emulator workloads — Switch/3DS games — it's proven on)?** No data gathered
   in this session; would require an actual integration spike.
5. **Licensing strategy for Omnidroid overall**: if Omnidroid intends to stay
   fully closed-source, every copyleft component surveyed here (waydroid,
   anbox, android_translation_layer, vkd3d-proton, hangover, GPL parts of
   libhybris) is off the table for direct linking; if Omnidroid can tolerate a
   GPL boundary via subprocess isolation (à la how Sober-alternatives use
   `qemu-aarch64`), more options open up but were not evaluated for legal
   soundness here (not a code question).
