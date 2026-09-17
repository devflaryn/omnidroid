# APK Forensic Analysis — `Roblox-2.738.1397.apk`

**Target file:** `C:\Users\berat\Desktop\Omni Apps\omnidroid\Roblox-2.738.1397.apk`
**Size:** 159,853,296 bytes (152.4 MiB) — VERIFIED from filesystem + ZIP EOCD.
**Analysis date:** 2026-09-18
**Extraction dir:** `C:\Users\berat\AppData\Local\Temp\claude\C--Users-berat-Desktop-Omni-Apps-omnidroid\2692e040-d7b4-4250-b114-62e3f26c66c9\scratchpad\apk\`

> Every numbered section below matches the corresponding investigation item.
> **VERIFIED** = read directly out of the bytes of this file. **INFERENCE** = reasoned conclusion, explicitly labelled.

---

## 0. Tooling used

Standard ELF tooling is **absent** on this host. Everything structural was parsed by
purpose-written Python (Python 3.11.9), stored in the scratchpad:

| Tool | Provenance | Used for |
|---|---|---|
| `python 3.11.9` | `C:\Users\berat\AppData\Local\Programs\Python\Python311\python` | all parsers |
| `unzip` (Git Bash) | `/usr/bin/unzip` | bulk extraction only |
| `file`, `xxd`, `head` (Git Bash) | `/usr/bin/…` | magic-byte spot checks |
| **`scratchpad/zipscan.py`** (written here) | — | raw ZIP central directory + local headers, EOCD, APK Signing Block, per-entry compression method / data offsets / alignment |
| **`scratchpad/elf.py`** (~330 lines, written here) | — | ELF64 header, program headers, section headers, `PT_DYNAMIC` walk, `.dynsym`, `.rela.*`, **Android APS2 packed relocations (SLEB128 group decoder)**, `DT_RELR` bitmap decoder, `.note.*`, GNU property parsing |
| **`scratchpad/axml.py`** (written here) | — | Android binary XML (AXML) decoder for `AndroidManifest.xml` |
| **`scratchpad/dex.py`** (written here) | — | DEX header, `class_def` table, `encoded_method` walk, native-method detection, string pool |
| **`scratchpad/cert.py`** (written here) | — | DER/BER walker for X.509 in `META-INF/KEY.RSA` and the APK Signing Block |
| **`scratchpad/strsearch.py`**, `scratchpad/undef.py`, `scratchpad/jnicheck.py` | — | string mining, undefined-symbol classification, JNI cross-check |

**Confidence check on the APS2 decoder:** for `libroblox.so` the decoder consumed
**2,100,778 of 2,100,778** bytes of the `DT_ANDROID_RELA` blob and produced exactly the
**568,272** relocations declared in the blob header. Byte-exact consumption + exact count
match is strong evidence the decode is correct.

`readelf`, `objdump`, `llvm-readobj`, `nm`, `strings`, `7z`, `aapt`, `aapt2` were all
probed and are **not installed** on this machine.

---

## ⚠️ HEADLINE FINDING (read before anything else)

**This is NOT a stock Roblox APK. It is a third-party modified, re-signed build containing an embedded Luau script executor ("Gloop").**

VERIFIED evidence, all four independent:

1. **Signing certificate is not Roblox's.** `META-INF/KEY.RSA` and the v2/v3 blocks in the
   APK Signing Block all carry the same self-signed cert:
   `C=DE, ST=Berlin, L=Berlin, O=Gloop, OU=Gloop, CN=Gloopiest Man`
   SHA-256 of cert DER: `8d57bfbe5c24e25e74e1faca78ced13d314efcdba2a948f66ecf69d8d051876a`
   Validity `2024-07-12 → 2025-07-12` (**expired**).
2. **`assets/gloop/dlt.zip`** (2,053,671 B) — a resource pack of 74 UI images
   (`Execute.png`, `AutoExecNotify.png`, `NewUI.png`, `OldUI.png`, `Poppins.rbxmx`, …),
   plus a macOS `assets/gloop/.DS_Store` (8,196 B) left behind by the modder.
3. **`classes4.dex`** contains exactly 5 classes, all `com.roblox.gloop.Loader*`, with four
   native methods: `nativeStart`, `nativeResize`, `nativeContext`, `getDownloadUrl`.
   `Lcom/roblox/gloop/Loader;` is also referenced from `classes2.dex`, i.e. Roblox's own
   dex was patched to call it.
4. **`lib/arm64-v8a/libzstd-jni-1.5.7-6.so` has been replaced with a trojanised build.**
   It is 18,440,296 bytes (upstream zstd-jni for one ABI is ~1 MB), it still exports the
   148 genuine `Java_com_github_luben_zstd_*` symbols, **and** it additionally contains:
   * Rust build paths for a **Luau decompiler**: `luau-lifter/src/lifter.rs`,
     `luau-lifter/src/deserializer/bytecode.rs`, `ast/src/formatter.rs`,
     `cfg/src/ssa/construct.rs`, `restructure/src/loop.rs`, and the full `LOP_*` Luau opcode
     name table.
   * `Dear ImGui 1.92.9 (19290)` + `imgui_impl_android` + `imgui_impl_opengl3`
     (`imgui.ini`, `imgui_log.txt`).
   * libcurl + OpenSSL, with the modder's build path
     **`/tmp/gloop-deps-build/src/curl-8.21.0/lib/vtls/openssl.c`**.
   * Rust std/toolchain paths from the build machine:
     `/Users/user/.rustup/toolchains/nightly-aarch64-apple-darwin/…`, `/Users/user/.cargo/…`.
   * `DT_NEEDED` on `libEGL.so` **and `libGLESv3.so`** — neither of which real zstd-jni needs —
     and imports of `eglSwapBuffers`, `eglGetCurrentDisplay`, `vkGetInstanceProcAddr`,
     `vkGetDeviceProcAddr`, `ANativeWindow_*`, `mprotect`, `dl_iterate_phdr`, `dlopen`, `dlsym`.
   * Non-standard sections `.adi` (703,748 B), `.rhash` (88 B), `.stack` (88 B), and a
     42,680-byte `.text` against an **11,162,632-byte `.data`** — the classic shape of
     packed/virtualised code.

**Implication for Omnidroid:** the "primary test target" is a hostile-modified binary. See
§10 and the design implications. A stock Roblox APK should be obtained for the canonical
compatibility surface; this file is still useful because `libroblox.so` itself appears
unmodified (see §10), but `libzstd-jni-1.5.7-6.so` must be treated as adversarial code that
hooks EGL/GLES/Vulkan, walks `/proc/self/maps`, and calls `mprotect`.

---

## 1. APK structure

### 1.1 Container facts (VERIFIED, `zipscan.py` on raw bytes)

| Property | Value |
|---|---|
| File size | 159,853,296 B |
| EOCD offset | 159,853,274 |
| Central directory offset / size | 159,629,312 / 223,962 B |
| Entry count | **2,365** |
| Total **uncompressed** size of all entries | **247,032,045 B** (235.6 MiB) |
| Total compressed size of all entries | 159,413,043 B |
| STORED entries | **954** |
| DEFLATED entries | **1,411** |
| Other compression methods | none (only 0 and 8) |
| APK Signing Block | present, offsets 159,625,216 → 159,629,312 (4,096 B) |
| ZIP64 EOCD record | absent (the `PK\x06\x06` byte pattern found by a naive search is inside compressed data, not a real record; the real EOCD at 159,853,274 has non-`0xFFFF` counts and no ZIP64 locator) |
| Archive comment | none (length 0) |

### 1.2 Top-level layout

| Top-level entry | Files | Uncompressed | Compressed | STORED | DEFLATED |
|---|---:|---:|---:|---:|---:|
| `lib/` | 11 | 133,677,136 | 60,552,321 | **0** | **11** |
| `assets/` | 596 | 83,271,015 | 71,100,752 | 391 | 205 |
| `classes.dex` | 1 | 9,105,132 | 9,105,132 | 1 | 0 |
| `classes3.dex` | 1 | 6,998,496 | 6,998,496 | 1 | 0 |
| `classes2.dex` | 1 | 6,178,724 | 6,178,724 | 1 | 0 |
| `resources.arsc` | 1 | 4,068,236 | 4,068,236 | 1 | 0 |
| `res/` | 1,511 | 2,938,559 | 1,135,422 | 467 | 1,044 |
| `META-INF/` | 181 | 643,233 | 226,618 | 91 | 90 |
| `AndroidManifest.xml` | 1 | 56,204 | 10,573 | 0 | 1 |
| `kotlin/` | 8 | 51,125 | 12,053 | 0 | 8 |
| `com/` | 1 | 24,470 | 10,865 | 0 | 1 |
| `classes4.dex` | 1 | **7,348** | 7,348 | 1 | 0 |
| 52 × `*.properties` / `*.proto` / misc root files | 52 | ~9 KB | ~5 KB | 0 | 52 |

**DEX:** 4 files — `classes.dex` 9,105,132 B; `classes2.dex` 6,178,724 B; `classes3.dex`
6,998,496 B; `classes4.dex` 7,348 B. All four are **STORED** (mmap-able; required by ART).

**`resources.arsc`:** present, 4,068,236 B, STORED, at data offset **44** (i.e. the very first
entry in the archive) — the standard AGP layout. Header verified: `02 00 0C 00` =
`RES_TABLE_TYPE`, header size 12, chunk size 0x3E138C, package count 2.

### 1.3 Alignment (matters for mmap-ability) — VERIFIED

| Set | 16 KiB-aligned | 4 KiB-aligned | 4-byte-aligned | unaligned |
|---|---:|---:|---:|---:|
| 954 STORED entries | 0 | 1 | **953** | **0** |
| 1,411 DEFLATED entries | 0 | 0 | 377 | 1,034 |

* **Every STORED entry is 4-byte aligned; none is page aligned.** This is classic
  `zipalign -f 4`, not `zipalign -p -f 4`.
* **All 11 `.so` files are DEFLATED** and their data offsets are arbitrary
  (e.g. `libroblox.so` at 98,795,169, `%4 == 1`).
* Local-file-header extra fields are 0–3 bytes (histogram: 0→1660, 1→242, 2→231, 3→232) —
  padding inserted purely to reach 4-byte alignment.

**Consequence (VERIFIED, not inference):** the native libraries in this APK **cannot be
mmap-ed in place**. They must be inflated to a file or to anonymous memory first. This also
means `android:extractNativeLibs` is effectively **`true`** (see §6).

### 1.4 Signatures

| Scheme | Present | Evidence |
|---|---|---|
| **v1 (JAR)** | ✅ | `META-INF/MANIFEST.MF` (285,909 B, **2,362 `Name:` digest entries**), `META-INF/KEY.SF` (286,036 B), `META-INF/KEY.RSA` (1,363 B) |
| **v2** | ✅ | APK Signing Block pair ID `0x7109871a`, 1,547 B |
| **v3** | ✅ | APK Signing Block pair ID `0xf05368c0`, 1,547 B |
| v3.1 | ❌ | no `0x1b93ad61` / `0x71030202` pair |
| v4 | ❌ | no `.idsig` sidecar in this file; no v4 block |
| Verity padding | ✅ | pair ID `0x42726577`, 946 B |
| **Play source stamp** | ❌ **absent** | manifest declares `com.android.stamp.source=https://play.google.com/store` and `com.android.stamp.type=STAMP_TYPE_STANDALONE_APK`, but **no stamp block** (`0x2b09189e`/`0x6dff800d`) exists in the signing block |

`KEY.SF` contains `X-Android-APK-Signed: 2, 3`.
The `KEY.*` basename (rather than `CERT.*`) plus the missing source stamp plus the `O=Gloop`
certificate together confirm a **re-sign after modification** (see Headline Finding).

---

## 2. ABI / native library layout

### 2.1 ABI directories — VERIFIED

Exactly **one** ABI directory exists:

```
lib/arm64-v8a/     11 files
```

* `arm64-v8a` — **PRESENT**
* `armeabi-v7a` — **ABSENT**
* `x86` — **ABSENT**
* `x86_64` — **ABSENT**
* `riscv64` — **ABSENT**

A substring search over all 2,365 central-directory names for `x86` and `armeabi` returned
**zero** matches. This is a single-ABI, 64-bit-ARM-only APK.

### 2.2 Every `.so` — VERIFIED

| # | Path | Uncompressed | In-APK compressed | Ratio |
|---|---|---:|---:|---:|
| 1 | `lib/arm64-v8a/libroblox.so` | **109,193,800** | 46,516,719 | 42.6% |
| 2 | `lib/arm64-v8a/libzstd-jni-1.5.7-6.so` | **18,440,296** | 11,721,092 | 63.6% |
| 3 | `lib/arm64-v8a/libbacktrace-native.so` | 5,339,704 | 2,078,803 | 38.9% |
| 4 | `lib/arm64-v8a/librenderscript-toolkit.so` | 394,112 | 129,704 | 32.9% |
| 5 | `lib/arm64-v8a/libeigen_blas.so` | 251,784 | 81,495 | 32.4% |
| 6 | `lib/arm64-v8a/libimage_processing_util_jni.so` | 32,544 | 15,630 | 48.0% |
| 7 | `lib/arm64-v8a/libdatastore_shared_counter.so` | 7,112 | 2,630 | 37.0% |
| 8 | `lib/arm64-v8a/libtrampoline.so` | 5,104 | 1,900 | 37.2% |
| 9 | `lib/arm64-v8a/libsurface_util_jni.so` | 4,896 | 1,756 | 35.9% |
| 10 | `lib/arm64-v8a/libeigen_lapack.so` | 4,032 | 1,338 | 33.2% |
| 11 | `lib/arm64-v8a/libyuv_shared.so` | 3,752 | 1,254 | 33.4% |
|  | **Total** | **133,677,136** | **60,552,321** | 45.3% |

**There is no `libc++_shared.so`.** See §4 — the C++ runtime is statically linked.

---

## 3. Per-`.so` ELF analysis (exhaustive, all 11)

All 11 files are, VERIFIED: `EI_CLASS = ELFCLASS64`, `EI_DATA = ELFDATA2LSB`,
`EI_OSABI = 0 (SYSV)`, `e_machine = 183 (EM_AARCH64)`, `e_type = ET_DYN`, `e_flags = 0x0`.
All 11 are **stripped** (no `SHT_SYMTAB`, no `.debug_*`, no `.symtab`/`.strtab`).
None has `DT_TEXTREL`. None has `DT_RPATH`/`DT_RUNPATH`. None has `PT_GNU_PROPERTY`,
`.note.gnu.property`, or a `GNU_PROPERTY_AARCH64_FEATURE_1_AND` record — **no BTI, no PAC,
no GCS, no MTE markers anywhere in this APK**. All 11 have
`PT_GNU_STACK` with flags `RW` (6) — non-executable stack. All 11 have `PT_GNU_RELRO`.
**None has `PT_TLS`**; there are **zero** `STT_TLS` symbols, defined or undefined, in any
library. All 11 have `DT_FLAGS = 0x8 (DF_BIND_NOW)` — note: the `DF_BIND_NOW` *flag* is set
while `DT_BIND_NOW` (tag 24) is **not** present in any of them.

### 3.1 Master table

| Library | `DT_SONAME` | sh / ph | `PT_GNU_RELRO` memsz | reloc scheme | total relocs | `init_array` | `fini_array` | `.eh_frame` | `.ARM.exidx` | `.gcc_except_table` | `.dynsym` (def/undef) | `JNI_OnLoad` | `Java_*` |
|---|---|---|---:|---|---:|---:|---:|---:|---|---|---|---|---:|
| `libroblox.so` | `libroblox.so` | 32 / 9 | 5,205,568 | **`DT_ANDROID_RELA` (APS2)** | **568,806** | **3,594** | 3 | **11,550,152 B** | no | yes | 1109 (543/565) | **yes** | **539** |
| `libzstd-jni-1.5.7-6.so` | `libzstd-jni-1.5.7-6.so` | 28 / 9 | 580,752 | `DT_RELA` | 34,275 | **705** | 2 | 958,500 B | no | yes | 488 (149/338) | **yes** | 148 |
| `libbacktrace-native.so` | `libbacktrace-native.so` | 30 / 9 | 285,056 | `DT_RELA` | 22,578 | 5 | 2 | 237,076 B | no | yes | 11310 (10994/315) | **yes** | 10 |
| `librenderscript-toolkit.so` | `librenderscript-toolkit.so` | 28 / 9 | 20,384 | `DT_RELA` | 2,065 | 2 | 2 | 42,068 B | no | yes | 820 (730/89) | no | 4 |
| `libeigen_blas.so` | `libeigen_blas.so` | 28 / 9 | 16,112 | `DT_RELA` | 1,688 | 2 | 2 | 24,012 B | no | yes | 346 (300/45) | no | 0 |
| `libimage_processing_util_jni.so` | `libimage_processing_util_jni.so` | 25 / 9 | 3,280 | `DT_RELA` | 61 | 0 | 2 | 3,296 B | no | no | 27 (8/18) | no | 8 |
| `libdatastore_shared_counter.so` | `libdatastore_shared_counter.so` | 24 / 9 | 3,648 | `DT_RELA` | 18 | 1 | 2 | 492 B | no | no | 20 (9/10) | no | 4 |
| `libtrampoline.so` | **(none)** | 24 / **11** | 1,456 | `DT_RELA` | 11 | 0 | 0 | 216 B | no | no | 8 (0/7) | no | 0 |
| `libsurface_util_jni.so` | `libsurface_util_jni.so` | 22 / 8 | 1,616 | `DT_RELA` | 12 | 0 | 2 | 200 B | no | no | 11 (1/9) | no | 1 |
| `libeigen_lapack.so` | `libeigen_lapack.so` | 22 / 9 | 2,400 | `DT_RELA` | 7 | 0 | 2 | 180 B | no | no | 5 (0/4) | no | 0 |
| `libyuv_shared.so` | `libyuv_shared.so` | 21 / 8 | 2,592 | `DT_RELA` | 6 | 0 | 2 | 148 B | no | no | 4 (0/3) | no | 0 |

### 3.2 `DT_NEEDED` per library — VERIFIED

| Library | `DT_NEEDED` (in file order) |
|---|---|
| `libroblox.so` | `libOpenMAXAL.so`, `libmediandk.so`, `libandroid.so`, `libm.so`, `libOpenSLES.so`, **`libGLESv2.so`**, **`libEGL.so`**, `liblog.so`, `libdl.so`, `libc.so` |
| `libzstd-jni-1.5.7-6.so` | `liblog.so`, `libandroid.so`, `libdl.so`, `libm.so`, `libc.so`, **`libEGL.so`**, **`libGLESv3.so`** |
| `libbacktrace-native.so` | `liblog.so`, **`libz.so`**, `libdl.so`, `libm.so`, `libc.so` |
| `librenderscript-toolkit.so` | `libjnigraphics.so`, `liblog.so`, `libdl.so`, `libm.so`, `libc.so` |
| `libimage_processing_util_jni.so` | `liblog.so`, `libandroid.so`, `libjnigraphics.so`, `libm.so`, `libdl.so`, `libc.so` |
| `libeigen_blas.so` | `libm.so`, `libdl.so`, `libc.so` |
| `libeigen_lapack.so` | **`libeigen_blas.so`**, `libm.so`, `libdl.so`, `libc.so` |
| `libsurface_util_jni.so` | `libandroid.so`, `libm.so`, `libdl.so`, `libc.so` |
| `libdatastore_shared_counter.so` | `libm.so`, `libdl.so`, `libc.so` |
| `libyuv_shared.so` | `libm.so`, `libdl.so`, `libc.so` |
| `libtrampoline.so` | `liblog.so`, `libdl.so`, `libc.so` |

**Union of `DT_NEEDED`** (count = how many of the 11 need it):
`libdl.so`(11), `libc.so`(11), `libm.so`(10), `liblog.so`(6), `libandroid.so`(4),
`libjnigraphics.so`(2), `libEGL.so`(2), `libz.so`(1), `libeigen_blas.so`(1),
`libOpenMAXAL.so`(1), `libmediandk.so`(1), `libOpenSLES.so`(1), `libGLESv2.so`(1),
`libGLESv3.so`(1).

**Note:** `libGLESv3.so` is not a real NDK stub library name (the NDK ships
`libGLESv1_CM.so`, `libGLESv2.so`, `libGLESv3.so` — `libGLESv3.so` *does* exist in the NDK
sysroot but Android's platform provides GLES3 entry points through `libGLESv2.so`). An
Omnidroid loader must be able to satisfy a `DT_NEEDED` on `libGLESv3.so` or the trojanised
zstd lib will fail to load.

### 3.3 Relocations — VERIFIED

**`libroblox.so` uses `DT_ANDROID_RELA` with the `APS2` packed format.** This is the single
most important loader requirement in the file.

| tag | value |
|---|---|
| `DT_ANDROID_RELA` blob | 2,100,778 B, magic `APS2` |
| declared relocation count | 568,272 |
| decoded relocation count | 568,272 (byte-exact, 2,100,778/2,100,778 consumed) |
| `R_AARCH64_RELATIVE` | **568,194** |
| `R_AARCH64_GLOB_DAT` | 56 |
| `R_AARCH64_ABS32` | 22 |
| symbolic (non-zero `r_sym`) | 78 |
| separate `.rela.plt` (`DT_JMPREL`) | 534 × `R_AARCH64_JUMP_SLOT` |
| `DT_RELA`/`DT_REL` | **absent** |
| `DT_RELR` | **absent** |
| `DT_RELACOUNT`/`DT_RELCOUNT` | absent |

No library in the APK uses `DT_RELR` or `DT_ANDROID_REL`. The other ten use plain
`DT_RELA` + `DT_JMPREL`:

| Library | `.rela.dyn` RELATIVE | ABS32 | GLOB_DAT | `.rela.plt` JUMP_SLOT | total |
|---|---:|---:|---:|---:|---:|
| `libzstd-jni-1.5.7-6.so` | 33,919 | 5 | 18 | 333 | 34,275 |
| `libbacktrace-native.so` | 11,325 | 5,906 | 746 | 4,601 | 22,578 |
| `librenderscript-toolkit.so` | 1,265 | 533 | 49 | 218 | 2,065 |
| `libeigen_blas.so` | 1,208 | 381 | 22 | 77 | 1,688 |
| `libimage_processing_util_jni.so` | 43 | 0 | 0 | 18 | 61 |
| `libdatastore_shared_counter.so` | 4 | 0 | 0 | 14 | 18 |
| `libsurface_util_jni.so` | 3 | 0 | 0 | 9 | 12 |
| `libtrampoline.so` | 4 | 0 | 0 | 7 | 11 |
| `libeigen_lapack.so` | 3 | 1 | 0 | 3 | 7 |
| `libyuv_shared.so` | 3 | 0 | 0 | 3 | 6 |

Zero `R_AARCH64_IRELATIVE` and zero `STT_GNU_IFUNC` symbols in any library —
**no ifunc resolution needed**.

### 3.4 TLS — VERIFIED

* **No `PT_TLS` segment in any of the 11 libraries.**
* **Zero `STT_TLS` symbols**, defined or undefined, anywhere.
* `__cxa_thread_atexit_impl` is imported as a **WEAK undefined** symbol by `libroblox.so`
  and `libzstd-jni-1.5.7-6.so` (so thread_local destructors are supported-if-present).
* `__emutls_get_address` is **not** imported by anything.
* Thread-local storage is therefore done entirely through `pthread_key_create` /
  `pthread_getspecific` / `pthread_setspecific` (all three are imported) — i.e. the
  emulated-TLS / pthread-key model, **not** ELF TLS, **not** `-ftls-model=…` initial-exec
  segments.

**INFERENCE:** Omnidroid does **not** need an AArch64 ELF TLS implementation
(no `TPIDR_EL0` / `__tls_get_addr` / TLSDESC / DTV) for this APK. It does need a correct,
fast `pthread_key_*` implementation. Rust code in `libzstd-jni` emits "out of TLS keys,
aborting" — so the pthread-key limit must be generous (bionic's is 128 total with ~
`PTHREAD_KEYS_MAX`); make it larger rather than smaller.

### 3.5 RELRO / init / fini — VERIFIED

* All 11 have `PT_GNU_RELRO`. `libroblox.so`'s RELRO region is **5,205,568 bytes** at
  vaddr `0x62dc1c0` with `p_align = 1`; there is an explicit `.relro_padding` `SHT_NOBITS`
  section in 9 of 11 libraries (LLD's 16 KiB-page padding scheme).
* **No library has `DT_INIT` or `DT_FINI`** — only array forms.
* `libroblox.so`: `DT_INIT_ARRAY = 0x67c27a8`, `DT_INIT_ARRAYSZ = 28,752` →
  **3,594 initializer function pointers**. `DT_FINI_ARRAY = 0x67c2790`, size 24 → 3 entries.
* `libzstd-jni-1.5.7-6.so`: **705** init_array entries.
* No library has `DT_PREINIT_ARRAY`.

**INFERENCE:** running 3,594 C++ static constructors before `JNI_OnLoad` is the first real
stress test of the compatibility layer, and any one of them touching an unimplemented libc
function will abort the process before a single line of Roblox logic runs.

### 3.6 Max `PT_LOAD` alignment (16 KiB page readiness) — VERIFIED

| Library | max `p_align` of `PT_LOAD` |
|---|---|
| 10 of 11 libraries | **`0x4000` (16 KiB)** |
| **`libzstd-jni-1.5.7-6.so`** | **`0x1000` (4 KiB)** |

The trojanised zstd library is the **only** one not built 16 KiB-page-compatible — another
independent signal that it was produced by a different toolchain than the rest of the APK.

### 3.7 Notes — VERIFIED

`.note.android.ident` is present in all 11 (`n_name = "Android"`, `n_type = 1`, 132 bytes).
Layout is `{uint32 android_api; char ndk_version[64]; char ndk_build_number[64];}`:

| Library | `android_api` in note | NDK version | NDK build |
|---|---:|---|---|
| `libroblox.so` | **26** | **r28c** | 13676358 |
| `libeigen_blas.so` | 26 | r28c | 13676358 |
| `libeigen_lapack.so` | 26 | r28c | 13676358 |
| `libyuv_shared.so` | 26 | r28c | 13676358 |
| `libtrampoline.so` | 21 | r28c | 13676358 |
| `librenderscript-toolkit.so` | 23 | r28c | 13676358 |
| `libzstd-jni-1.5.7-6.so` | 26 | **r26d** | 11579264 |
| `libimage_processing_util_jni.so` | 23 | r27 | 12077973 |
| `libsurface_util_jni.so` | 23 | r27 | 12077973 |
| `libbacktrace-native.so` | 21 | r27-beta1 | 11718014 |
| `libdatastore_shared_counter.so` | 21 | r25c | 9519653 |

Other notes:
* `.note.gnu.build-id` (`n_type = 3`, 20 B = SHA-1 style) in 10 of 11 —
  **`libtrampoline.so` has no build-id**, and `libzstd-jni-1.5.7-6.so` has **no build-id**
  either (its only note is `.note.android.ident`).
* `.note.crashpad.info` (`n_name = "Crashpad"`, 8 B) in `libroblox.so` and
  `libbacktrace-native.so` only.
* **`.note.gnu.property` is absent from all 11** ⇒ no `GNU_PROPERTY_AARCH64_FEATURE_1_AND`,
  so **BTI = off, PAC = off, GCS = off** for every library. No
  `PT_AARCH64_MEMTAG_MTE` segment either ⇒ **MTE not requested**.

### 3.8 Exception handling — VERIFIED

* **`.eh_frame` + `.eh_frame_hdr` + `PT_GNU_EH_FRAME` present in all 11.** `libroblox.so`'s
  `.eh_frame` is **11,550,152 bytes** (10.6% of the whole library).
* `.gcc_except_table` present in 5 (`libroblox`, `libzstd-jni`, `libbacktrace-native`,
  `librenderscript-toolkit`, `libeigen_blas`) ⇒ real C++ `try`/`catch` with landing pads.
* **`.ARM.exidx` absent everywhere** (as expected — that is a 32-bit ARM section).
* `__gxx_personality_v0` and `__cxa_*` are imported (see §4) but `_Unwind_*` is **not** in
  the undefined set ⇒ the unwinder is **statically linked** into each library.

### 3.9 Unusual / non-standard sections — VERIFIED

| Library | Non-standard sections |
|---|---|
| `libroblox.so` | `malloc_hook`, `pb_defaults`, `protodesc_cold` |
| `libbacktrace-native.so` | `BACKTRACE_IO_BCD` |
| `libeigen_blas.so`, `librenderscript-toolkit.so` | `__lcxx_override` (libc++ `--lto`/override section) |
| **`libzstd-jni-1.5.7-6.so`** | **`.adi` (703,748 B), `.rhash` (88 B), `.stack` (88 B)**, plus two separate `.data` sections and no `.relro_padding` |
| `libtrampoline.so` | `.interp` (!) — see below |

**`libtrampoline.so` is not a shared library — it is a PIE executable named `.so`.**
VERIFIED: it has `PT_INTERP` containing `/system/bin/linker64`, `e_entry = 0x47a8`,
**no `DT_SONAME`**, **zero defined dynamic symbols**, `DT_FLAGS_1 = 0x8000001`
(`DF_1_NOW | DF_1_PIE`), and 11 program headers. Its full string table is:
`crashpad_trampoline`, `dlopen: %s`, `dlsym: %s`, `usage: %s <path>`,
**`CrashpadHandlerMain`**, `libdl.so`, `libc.so`, `liblog.so`, and a clang 19.0.1 /
LLD 19.0.1 comment. Its imports are `__libc_init`, `__cxa_atexit`, `__android_log_print`,
`dlopen`, `dlsym`, `dlerror`, `__stack_chk_fail`.
It is Crashpad's "handler trampoline": Android forbids executables in `lib/`, so Crashpad
ships the handler as `lib*.so` and `exec()`s it; it then `dlopen`s the path given in `argv[1]`
and calls the exported `CrashpadHandlerMain` (which `libroblox.so` does export — see §5).

---

## 4. Union of undefined symbols — the Omnidroid API surface

**Method (VERIFIED):** every `.dynsym` entry across all 11 libraries with
`st_shndx == SHN_UNDEF` and a non-empty name, unioned by name.
Full lists (with binding, type, and which libraries reference each symbol) are in
**`docs/research/apk-undefined-symbols.txt`** (2,918 lines).

### 4.1 Totals

**Union: 669 distinct undefined symbols.**

| Group | Count | Notes |
|---|---:|---|
| **libc / bionic (POSIX-shaped, glibc-compatible)** | **365** | ordinary libc; a glibc-compatible shim covers these |
| **libGLESv2 / libGLESv3** | **88** | see §7 |
| **libm** | **55** | |
| **libc / bionic (BIONIC-SPECIFIC — no glibc equivalent)** | **50** | see 4.2 |
| **libmediandk** | **33** | 23 functions + 10 `AMEDIAFORMAT_KEY_*` **data objects** |
| **libandroid / libnativewindow** | **32** | see 4.3 |
| **libEGL** | **20** | see §7 |
| **libz** | **8** | `deflate`, `deflateEnd`, `deflateInit2_`, `deflateInit_`, `inflate`, `inflateEnd`, `inflateInit_`, `zError` (only `libbacktrace-native.so`) |
| **libdl** | **6** | `dlopen`, `dlsym`, `dlclose`, `dlerror`, `dladdr`, `dl_iterate_phdr` |
| **liblog** | **5** | `__android_log_print`, `__android_log_write`, `__android_log_vprint`, `__android_log_assert`, `__android_log_buf_write` |
| **C++ runtime** | **4** | `__cxa_atexit`, `__cxa_finalize`, `__cxa_thread_atexit_impl` (WEAK), `__gxx_personality_v0` |
| **libjnigraphics** | **3** | `AndroidBitmap_getInfo`, `AndroidBitmap_lockPixels`, `AndroidBitmap_unlockPixels` |
| **libvulkan** | **0** | ⚠️ nothing is statically linked against Vulkan — see §7 |
| **libcamera2ndk** | **0** | dlopen'd — see 4.4 |
| **libOpenSLES / libOpenMAXAL** | **0** | ⚠️ `DT_NEEDED` but **zero imported symbols** — see 4.4 |
| **libaaudio** | **0** | dlopen'd — see 4.4 |

Per-library counts: `libroblox.so` 565, `libzstd-jni-1.5.7-6.so` 338,
`libbacktrace-native.so` 315, `librenderscript-toolkit.so` 89, `libeigen_blas.so` 45,
`libimage_processing_util_jni.so` 18, `libdatastore_shared_counter.so` 10,
`libsurface_util_jni.so` 9, `libtrampoline.so` 7, `libeigen_lapack.so` 4,
`libyuv_shared.so` 3.

### 4.2 C++ runtime: STATICALLY LINKED (critical)

**Only 4 C++-runtime symbols are imported across the entire APK**, and none is a mangled
`_Z*` name. There is no `libc++_shared.so` in `lib/`. `libroblox.so`'s `.rodata` is full of
`NSt6__ndk1…` mangled names — the `__ndk1` inline namespace of the NDK's libc++.

**Conclusion (VERIFIED):** every library statically links `libc++_static.a` +
`libc++abi.a` + `libunwind.a`. Omnidroid therefore does **not** need to supply a C++ ABI
library; it needs only `__cxa_atexit`, `__cxa_finalize`, `__gxx_personality_v0`, and
(optionally) `__cxa_thread_atexit_impl`. But it **does** need the AArch64 unwinder inside the
guest to work, which means the `.eh_frame` data must be readable at its mapped addresses and
any host-side signal/exception bridging must not corrupt the guest stack.

### 4.3 Bionic-specific symbols with no glibc equivalent (50)

These are the ones that need hand-written implementations, not a glibc alias:

```
__assert            __assert2           __errno             __libc_init
__register_atfork   __stack_chk_fail    __stack_chk_guard   __system_property_get
android_set_abort_message                arc4random_buf     getauxval
gettid              mallinfo            prctl               pthread_setname_np
__gcov_dump (WEAK)  __gcov_flush (WEAK)  __sF               _ctype_
__ctype_get_mb_cur_max                   __gnu_strerror_r   __cmsg_nxthdr
environ
-- _FORTIFY_SOURCE helpers (bionic spelling) --
__memcpy_chk  __memmove_chk  __memset_chk  __strcpy_chk  __strcat_chk
__strlcpy_chk __strncpy_chk  __strncpy_chk2 __strncat_chk __strlen_chk
__strchr_chk  __vsnprintf_chk __vsprintf_chk __umask_chk  __read_chk
__open_2      __fread_chk    __fwrite_chk   __readlink_chk __sendto_chk
__write_chk   __FD_SET_chk   __FD_CLR_chk   __FD_ISSET_chk
```

Also worth calling out as bionic-flavoured but present in glibc under different semantics:
`getrandom` (WEAK, `libzstd-jni`), `getentropy` (WEAK, three libraries),
`__errno` (bionic's `errno` accessor — glibc uses `__errno_location`).

`__system_property_get` is imported by `libroblox.so` **and** `libzstd-jni-1.5.7-6.so`.
The properties actually read by `libroblox.so` (VERIFIED as string literals) are:
`ro.arch`, `ro.build.fingerprint`, `ro.build.version.sdk`, `ro.hardware`,
`ro.product.board`, `ro.product.manufacturer`, `ro.product.model`, `ro.soc.manufacturer`.

### 4.4 Android NDK API surface

**`libandroid.so` / `libnativewindow.so` — 32 imported symbols (VERIFIED):**

| Family | Symbols |
|---|---|
| `AAssetManager` / `AAsset` (7) | `AAssetManager_fromJava`, `AAssetManager_open`, `AAsset_close`, `AAsset_getBuffer`, `AAsset_getLength`, `AAsset_openFileDescriptor`, `AAsset_read` |
| `AConfiguration` (9) | `AConfiguration_new`, `_delete`, `_fromAssetManager`, `_getCountry`, `_getLanguage`, `_getNavHidden`, `_getScreenHeightDp`, `_getScreenSize`, `_getScreenWidthDp` |
| `ALooper` (7) | `ALooper_prepare`, `_forThread`, `_acquire`, `_release`, `_addFd`, `_removeFd`, `_pollOnce` |
| `ANativeWindow` (9) | `ANativeWindow_fromSurface`, `_acquire`, `_release`, `_getWidth`, `_getHeight`, `_getFormat`, `_setBuffersGeometry`, `_lock`, `_unlockAndPost` |

`ALooper_pollOnce` (not `ALooper_pollAll`) — this is the newer NDK spelling and the one the
GameActivity glue uses.

**Notably ABSENT from the entire APK's undefined set (VERIFIED by explicit search):**
`AInputEvent_*`, `AMotionEvent_*`, `AKeyEvent_*`, `AInputQueue_*`, `AChoreographer_*`,
`ASensor*`, `ATrace_*`, `AHardwareBuffer_*`, `ACamera*`, `AAudio*`, `slCreateEngine`,
`XA*`, `vk*`, `ANativeActivity_*`, `GameActivity_*`.
The absence of the input-event families is independent confirmation that GameActivity's
**buffered** input model is used (events are copied into `GameActivityMotionEvent` /
`GameActivityKeyEvent` structs by the statically-linked glue, never read through
`AInputQueue`). The absence of `AChoreographer_*` means frame pacing is **not** vsync-driven
through the NDK Choreographer — it is driven by the render thread and `eglSwapInterval`.

**Additionally dlopen'd / `dlsym`'d from `libandroid.so` (string evidence in
`libroblox.so`, *not* in `.dynsym`):** `AHardwareBuffer_acquire`, `AHardwareBuffer_describe`,
`AHardwareBuffer_release`, `AImage_getHardwareBuffer`.

**`libmediandk.so` — 33 (VERIFIED):** 23 `AMediaCodec_*` / `AMediaFormat_*` functions plus
**10 data objects** that are `const char*` globals and must be *exported data symbols*, not
functions: `AMEDIAFORMAT_KEY_MIME`, `_WIDTH`, `_HEIGHT`, `_COLOR_FORMAT`, `_STRIDE`,
`_BIT_RATE`, `_FRAME_RATE`, `_I_FRAME_INTERVAL`, `_CHANNEL_COUNT`, `_SAMPLE_RATE`.

**`libjnigraphics.so` — 3:** `AndroidBitmap_getInfo`, `AndroidBitmap_lockPixels`,
`AndroidBitmap_unlockPixels` (from `librenderscript-toolkit.so` and
`libimage_processing_util_jni.so`).

**`libOpenSLES.so` and `libOpenMAXAL.so`: `DT_NEEDED` by `libroblox.so` but ZERO symbols
imported from either.** No `slCreateEngine`, no `SL_IID_*`, no `XA*` in the undefined set.
**INFERENCE:** the libraries are linked only so that the dynamic loader guarantees they are
present; the actual entry points are obtained via `dlsym`. Omnidroid must therefore *provide
loadable stub objects* for both names even though nothing resolves against them at load time.

**dlopen'd-only libraries (names appear as string literals in `libroblox.so` but are not
`DT_NEEDED` and contribute no undefined symbols):**
`libvulkan.so`, `libvulkan.so.1`, `libcamera2ndk.so`, `libaaudio.so`, `libtrampoline.so`,
`libroblox.so` (self-reference).
There are **no** `ACamera*` or `AAudio*` undefined symbols, confirming both are resolved at
runtime via `dlsym`.

### 4.5 libc groups worth planning around (VERIFIED, from the 365 + 50)

* **Threads:** `pthread_create/join/detach/self/equal/exit`, mutex + rwlock + cond + attr
  families, `pthread_key_create/delete/getspecific/setspecific`, `pthread_once`,
  `pthread_sigmask`, `pthread_setname_np`, `pthread_getschedparam/setschedparam`,
  `pthread_condattr_setclock`, `sem_*`.
* **Sync primitives beyond pthreads:** `eventfd`, `epoll_create/create1/ctl/wait`,
  `poll`, `ppoll`, `select`, `pipe`, `pipe2`, `socketpair`.
* **Memory:** `malloc/calloc/realloc/free/memalign/posix_memalign/malloc_usable_size`,
  `mmap`, `mmap64`, `munmap`, `mprotect`, `madvise`, `mremap`, `msync`, `mlock`.
* **Files:** the full `stdio` set including `__sF` (bionic's `stdin/stdout/stderr` array),
  `open/openat/__open_2`, `pread/pwrite`, `preadv/pwritev`, `readv/writev`, `fstatat`,
  `statvfs`, `fallocate`, `sendfile`, `inotify_*`, `ftruncate64`, `lseek64`.
* **Process / signals:** `fork`, `vfork`, `execv/execve/execvp/execvpe`, `waitpid`, `wait4`,
  `kill`, `raise`, `sigaction`, `sigaltstack`, `sigprocmask`, `abort`, `_exit`, `_Exit`,
  `atexit`, `prctl`, `gettid`, `getpid`, `getppid`, `sched_getaffinity`, `sched_yield`,
  `setpriority`, `getrlimit`, `setrlimit`, `getrusage`.
* **Networking:** BSD sockets (`socket/bind/listen/accept/accept4/connect/send/recv/
  sendto/recvfrom/sendmsg/recvmsg/setsockopt/getsockopt/shutdown`), `getaddrinfo`,
  `freeaddrinfo`, `gai_strerror`, `getnameinfo`, `if_nametoindex`, `inet_ntop`, `inet_pton`,
  `res_*` is **absent** (no direct resolver use).
* **Time:** `clock_gettime`, `clock_getres`, `gettimeofday`, `nanosleep`, `clock_nanosleep`,
  `localtime_r`, `gmtime_r`, `mktime`, `timegm`, `strftime`, `tzset`, `daylight`.
* **Dynamic linking introspection:** `dl_iterate_phdr` and `dladdr` are imported by
  `libroblox.so`, `libbacktrace-native.so` **and** `libzstd-jni-1.5.7-6.so`. Crashpad,
  the Rust backtracer, and the injected code all walk the loaded-module list. Omnidroid
  must supply a `dl_iterate_phdr` that reports **plausible** `dlpi_addr` / `dlpi_phdr` for
  every guest module or the unwinder and the crash handler will both misbehave.

---

## 5. JNI surface and native/Java split

### 5.1 `Java_*` static exports — VERIFIED

**714 distinct `Java_*` symbols** across the APK:

| Library | `Java_*` exports | `JNI_OnLoad` | `JNI_OnUnload` |
|---|---:|---|---|
| `libroblox.so` | **539** | ✅ | ❌ |
| `libzstd-jni-1.5.7-6.so` | 148 | ✅ | ❌ |
| `libbacktrace-native.so` | 10 | ✅ | ❌ |
| `libimage_processing_util_jni.so` | 8 | ❌ | ❌ |
| `libdatastore_shared_counter.so` | 4 | ❌ | ❌ |
| `librenderscript-toolkit.so` | 4 | ❌ | ❌ |
| `libsurface_util_jni.so` | 1 | ❌ | ❌ |
| `libeigen_blas.so`, `libeigen_lapack.so`, `libyuv_shared.so`, `libtrampoline.so` | 0 | ❌ | ❌ |

`libroblox.so`'s **entire** non-`Java_*` export list is just four symbols:
`JNI_OnLoad`, **`CrashpadHandlerMain`**, `__start_pb_defaults`, `__stop_pb_defaults`.
Notably **`GameActivity_onCreate` / `android_main` are NOT exported**.

`libroblox.so`'s 539 `Java_*` exports by Java package:
`com.roblox.engine` 183 · `com.roblox.universalapp` 180 · `com.roblox.protocols` 140 ·
`com.roblox.client` 33 · `com.roblox.audio` 1 · **`com.google.androidgamesdk` 1** ·
**`org.fmod.FMOD` 1**.

The two singletons are the load-bearing ones:
* **`Java_com_google_androidgamesdk_GameActivity_initializeNativeCode`** — the AGDK
  GameActivity native bootstrap.
* `Java_org_fmod_FMOD_OutputAAudioHeadphonesChanged` — **FMOD** is the audio engine
  (corroborated by mangled names `N4FMOD14OutputEmulatedE`, `N4FMOD15ChannelEmulatedE`).

### 5.2 `RegisterNatives` usage — VERIFIED by cross-check

`jnicheck.py` mangled every `native`-flagged DEX method and compared against the 714
exports: **706 native methods declared in DEX, 657 have a matching `Java_*` export,
49 do not** and must therefore be bound via `RegisterNatives`:

| Owning class | Unbound natives | Registered by |
|---|---:|---|
| `com.google.androidgamesdk.GameActivity` | **23** | `libroblox.so` `JNI_OnLoad` / `initializeNativeCode` |
| `com.appsflyer.AppsFlyer2dXConversionCallback` | 7 | no matching lib — likely dead code |
| **`com.roblox.gloop.Loader`** | **4** | `libzstd-jni-1.5.7-6.so` `JNI_OnLoad` |
| `com.github.luben.zstd.Zstd` | 3 | `libzstd-jni-1.5.7-6.so` |
| `backtraceio.library.base.BacktraceBase` | 2 | `libbacktrace-native.so` |
| `com.roblox.engine.jni.NativeAppBridgeInterface` | 2 | `libroblox.so` |
| `com.roblox.universalapp.linking.JNILinkingProtocol` | 2 | `libroblox.so` |
| `org.fmod.MediaCodec` | 2 | `libroblox.so` |
| `com.roblox.engine.jni.NativeQuoteInterface`, `org.webrtc.Logging`, `org.webrtc.voiceengine.WebRtcAudioManager`, `pk.z` | 1 each | `libroblox.so` |

So: **both mechanisms are used**. Static `Java_*` for the bulk (657/706 = 93%) and
`RegisterNatives` for the 24 GameActivity glue methods plus a handful of others.
`org.webrtc.*` natives confirm **WebRTC is statically linked into `libroblox.so`**.

### 5.3 Java/Kotlin vs native split

| Metric | Value |
|---|---:|
| DEX `class_def` count (4 files, all distinct) | **26,620** |
| DEX `method_id` count | 154,176 |
| DEX `string_id` count | 122,157 |
| DEX bytes (uncompressed) | 22,289,700 (9.0% of APK uncompressed) |
| `libroblox.so` bytes | 109,193,800 (44.2% of APK uncompressed) |
| Native methods declared in DEX | 706 |

Java class count by package (top): `com.google.android` 3,973 · **`com.withpersona.sdk2`
3,628** · `com.appsflyer.internal` 384 · **`com.roblox.client` 350** · `com.google.mlkit` 260 ·
`androidx.credentials.playservices` 212 · `com.google.gson` 168 · … ·
**`com.roblox.engine` 112** · **`com.roblox.protocols` 103** · **`com.roblox.universalapp` 71**.

**Roblox's own Java/Kotlin is only ~636 classes out of 26,620 (2.4%).** The remaining 97.6%
is third-party SDK (Google Play Services, Persona KYC, MLKit, AppsFlyer, AndroidX, OkHttp,
Firebase, Dagger, Coil, Zstd, Backtrace, WebRTC bindings, Billing).

**INFERENCE (well-supported):** essentially **all** game logic — engine, renderer, Luau VM,
physics, networking, asset pipeline, and even the entire in-game *and* app UI (the
`ExtraContent/LuaPackages` and `UniversalApp.rbxm` are Luau/Roblox UI, not Android views) —
lives in `libroblox.so`. The Java layer is a **thin shell**: lifecycle, surface plumbing,
input forwarding, IAP, push notifications, and third-party SDK glue. Omnidroid does not need
a full ART; it needs a JNI-shaped facade whose ~130 Roblox-specific Java callbacks are
answered by native host code.

Key classes (VERIFIED superclasses from `class_def`):
| Class | Superclass |
|---|---|
| `com.roblox.client.RobloxApplication` | `android.app.Application` |
| `com.roblox.client.startup.ActivitySplash` | `com.roblox.client.a` (obfuscated base Activity) |
| **`com.roblox.client.startup.MainGameActivity`** | **`com.google.androidgamesdk.GameActivity`** |
| `com.roblox.client.ActivityNativeMain` | `com.roblox.client.a` |
| `com.roblox.engine.jni.NativeGLInterface` | `java.lang.Object` |
| `com.roblox.gloop.Loader` | `java.lang.Object` |

**No class in the APK extends `android.app.NativeActivity`.** (A search over all 26,620
`class_def` superclass names for `NativeActivity` returned zero hits.)

---

## 6. `AndroidManifest.xml` (decoded from binary AXML)

Fully decoded copy: **`docs/research/apk-AndroidManifest.decoded.xml`** (1,483 lines).

### 6.1 `<manifest>` / `<uses-sdk>` — VERIFIED

| Attribute | Value |
|---|---|
| `package` | **`com.roblox.client`** |
| `android:versionCode` | **3092** |
| `android:versionName` | **`2.738.1397`** ✅ confirmed |
| `android:compileSdkVersion` | **36** |
| `android:compileSdkVersionCodename` | `16` |
| `platformBuildVersionCode` / `Name` | 36 / 16 |
| **`android:minSdkVersion`** | **26** (Android 8.0 Oreo) |
| **`android:targetSdkVersion`** | **35** (Android 15) |

`<supports-screens>`: `smallScreens=false`, `normalScreens=true`, `largeScreens=true`,
`xlargeScreens=true`, `requiresSmallestWidthDp=300`.

### 6.2 `<uses-permission>` — 32 entries, VERIFIED, in manifest order

| Permission | `maxSdkVersion` |
|---|---|
| `android.permission.BLUETOOTH` | 30 |
| `android.permission.POST_NOTIFICATIONS` | |
| `android.permission.VIBRATE` | |
| `com.android.vending.BILLING` | |
| `android.permission.INTERNET` | |
| `android.permission.ACCESS_NETWORK_STATE` | |
| `android.permission.ACCESS_WIFI_STATE` | |
| `android.permission.MODIFY_AUDIO_SETTINGS` | |
| `android.permission.READ_CONTACTS` | |
| `android.permission.USE_FULL_SCREEN_INTENT` | |
| `android.permission.DISABLE_KEYGUARD` | |
| `android.permission.WRITE_EXTERNAL_STORAGE` | 28 |
| `android.permission.READ_EXTERNAL_STORAGE` | 32 |
| **`android.permission.MANAGE_EXTERNAL_STORAGE`** | |
| `android.permission.RECORD_AUDIO` | |
| `android.permission.CAMERA` | |
| `com.google.android.gms.permission.AD_ID` | |
| **`android.permission.DETECT_SCREEN_CAPTURE`** | |
| `android.permission.USE_BIOMETRIC` | |
| `android.permission.USE_FINGERPRINT` | |
| `android.permission.ACCESS_ADSERVICES_ATTRIBUTION` | |
| `com.samsung.android.mapsagent.permission.READ_APP_INFO` | |
| `com.huawei.appmarket.service.commondata.permission.GET_COMMON_DATA` | |
| `android.permission.WAKE_LOCK` | |
| `com.google.android.c2dm.permission.RECEIVE` | |
| `com.google.android.finsky.permission.BIND_GET_INSTALL_REFERRER_SERVICE` | |
| `android.permission.RECEIVE_BOOT_COMPLETED` | |
| `android.permission.ACCESS_ADSERVICES_AD_ID` | |
| `com.roblox.client.DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION` | |
| `android.permission.READ_BASIC_PHONE_STATE` | |
| `android.permission.ACCESS_ADSERVICES_TOPICS` | |

`MANAGE_EXTERNAL_STORAGE` is not a stock-Roblox permission pattern — **INFERENCE:** it was
almost certainly added by the Gloop modder (its `Loader` class references
`android.settings.MANAGE_ALL_FILES_ACCESS_PERMISSION` and
`android.settings.MANAGE_APP_ALL_FILES_ACCESS_PERMISSION` to write user scripts to
external storage).

Also declared: `<permission android:name="com.roblox.client.permission.CONFIGURATION"
android:protectionLevel="0x2"/>` (signature-level).
One `<uses-library android:name="android.ext.adservices" android:required="false"/>`.
A `<queries>` block is present.

### 6.3 `<uses-feature>` — 9 entries, VERIFIED

| `android:name` | `glEsVersion` | `required` |
|---|---|---|
| `android.hardware.camera` | — | false |
| `android.hardware.camera.autofocus` | — | false |
| `android.hardware.bluetooth` | — | false |
| **(no name — GLES declaration)** | **`0x30000`** | **true** |
| `android.hardware.type.pc` | — | false |
| `android.hardware.sensor.accelerometer` | — | false |
| `android.hardware.sensor.gyroscope` | — | false |
| `android.hardware.touchscreen` | — | false |
| `android.hardware.microphone` | — | false |

**`android:glEsVersion = 0x00030000` ⇒ OpenGL ES 3.0 is a hard requirement.**

**There is NO `android.hardware.vulkan.level` and NO `android.hardware.vulkan.version`
`<uses-feature>`.** Vulkan is not declared as a requirement at all. This is consistent with
§7: Vulkan is optional and runtime-probed.

`android.hardware.type.pc` (not required) is present — the app is Chromebook/desktop aware.

### 6.4 `<application>` attributes — VERIFIED, complete

| Attribute | Value |
|---|---|
| `android:name` | **`com.roblox.client.RobloxApplication`** |
| `android:theme` | `@ref/0x7f1302b6` |
| `android:label` | `@ref/0x7f120729` |
| `android:icon` | `@ref/0x7f0f0000` |
| `android:allowBackup` | **false** |
| **`android:hardwareAccelerated`** | **true** |
| **`android:largeHeap`** | **false** |
| `android:fullBackupContent` | `@ref/0x7f150000` |
| `android:resizeableActivity` | **false** |
| `android:networkSecurityConfig` | `@ref/0x7f150006` |
| `android:appCategory` | 0 (game) |
| `android:appComponentFactory` | `androidx.core.app.CoreComponentFactory` |
| `android:dataExtractionRules` | `@ref/0x7f150001` |
| **`android:extractNativeLibs`** | **ABSENT** |
| **`android:hasCode`** | **ABSENT** (⇒ default `true`) |
| **`android:debuggable`** | **ABSENT** (⇒ default `false`) |

**`extractNativeLibs` resolution (important, and evidence-based):** the attribute is not in
the manifest, so the platform default (`true`) applies. This is *corroborated*, not merely
assumed, by §1.3: all 11 `.so` entries are DEFLATED and 4-byte (not page) aligned, which is
only legal when `extractNativeLibs=true`. **Omnidroid must therefore model the
"libraries have been unpacked to `<app>/lib/arm64/`" world**, not the "libraries are mapped
from inside the APK" world.

### 6.5 Components

**75 components** total (activities, activity-aliases, services, receivers, providers).
Full tree in the decoded manifest file. Highlights:

**Launcher entry (VERIFIED):** there is **no `<activity>` with the LAUNCHER category**.
It is an `<activity-alias>`:
```xml
<activity-alias android:name="com.roblox.client.startup.LauncherAliasMain"
                android:enabled="true" android:exported="true"
                android:targetActivity="com.roblox.client.startup.ActivitySplash">
  <intent-filter>
    <action android:name="android.intent.action.MAIN"/>
    <category android:name="android.intent.category.LAUNCHER"/>
    <category android:name="android.intent.category.DEFAULT"/>
```
A second, `enabled="false"` alias `LauncherAliasClassic` targets the same activity
(icon variant).

**Activities that matter for native startup:**

| Activity | Attributes |
|---|---|
| `com.roblox.client.startup.ActivitySplash` | `exported=true`, `launchMode=1` (singleTop) — the actual launch target |
| **`com.roblox.client.startup.MainGameActivity`** | `exported=false`, `launchMode=2` (singleTask), `configChanges=0xfb0`, `alwaysRetainTaskState=true`, `windowSoftInputMode=0x10` (adjustResize), **`<meta-data android:name="android.app.lib_name" android:value="roblox"/>`** |
| `com.roblox.client.ActivityNativeMain` | `launchMode=2`, `configChanges=0xfb0`, `alwaysRetainTaskState=true`, `windowSoftInputMode=0x10` — **no `lib_name` meta-data** |
| `com.roblox.client.ActivityProtocolLaunch` | `exported=true`, `noHistory=true` (deep links) |
| `com.roblox.client.RobloxWebActivity` | `configChanges=0x480` |
| `com.roblox.client.captcha.ActivityFunCaptcha` | |
| `com.roblox.client.IncomingCallActivity` | `taskAffinity=com.roblox.client.calling`, `launchMode=3` (singleInstance), `showOnLockScreen`, `showForAllUsers` |

`configChanges=0xfb0` decodes to
`keyboard|keyboardHidden|navigation|orientation|screenLayout|uiMode|screenSize|
smallestScreenSize` — i.e. the activity handles nearly every configuration change itself and
will **not** be recreated on rotate/resize. Native code sees `onConfigurationChanged` +
`ANativeWindow` resize instead.

**The only `android.app.lib_name` meta-data in the whole manifest is on
`MainGameActivity`, value `"roblox"`.** Combined with §5.1 this is decisive: the native
entry is AGDK `GameActivity` loading `libroblox.so` and calling
`Java_com_google_androidgamesdk_GameActivity_initializeNativeCode`.

**Providers (4):**
`com.roblox.client.provider.ShellConfigurationContentProvider`
(`authorities=com.roblox.client.ShellConfigurationProvider`, `exported=true`, guarded by
`com.roblox.client.permission.CONFIGURATION`);
**`com.roblox.client.provider.AppAssetsContentProvider`** (`authorities=com.roblox.client`,
`grantUriPermissions=true`) — serves APK assets to other components;
`androidx.core.content.FileProvider`;
`com.withpersona.sdk2.inquiry.DocumentFileProvider` (`process=:personaIsolate`).
Plus `androidx.startup.InitializationProvider` entries with `androidx.startup` initializers:
`EmojiCompatInitializer`, `ProcessLifecycleInitializer`, `ProfileInstallerInitializer`,
`okhttp3.internal.platform.PlatformInitializer`.

**Services / receivers:** `com.roblox.client.realtime.RealtimeService`, push-notification
receivers, `com.roblox.client.widgets.RecentlyPlayedWidgetProvider` (`enabled=false`),
AppsFlyer install-referrer receiver, MLKit `MlKitComponentDiscoveryService`
(`directBootAware=true`), Firebase/GMS plumbing.

**Extra processes:** `:isolate` (5 components — GMA ad sandbox) and `:personaIsolate`
(3 components — Persona KYC SDK). **INFERENCE:** these are ad/KYC only; the game never runs
in them, so Omnidroid can ignore multi-process for a first-frame goal.

**Application-level `<meta-data>` affecting startup:**
`com.google.android.gms.games.APP_ID`, `com.google.android.gms.version`,
`com.google.android.gms.ads.APPLICATION_ID=ca-app-pub-6934957019599250~2990093015`,
`android.max_aspect=2.4`, `com.google.mlkit.vision.DEPENDENCIES=ocr,face,barcode`,
`com.android.dynamic.apk.fused.modules=base,personasdk`,
`com.android.stamp.source=https://play.google.com/store`,
`com.android.stamp.type=STAMP_TYPE_STANDALONE_APK`,
`com.android.vending.derived.apk.id=4`,
`com.google.android.play.billingclient.version=9.0.0`.

---

## 7. Graphics API determination

### 7.1 What is statically linked — VERIFIED

| Evidence | Finding |
|---|---|
| `libroblox.so` `DT_NEEDED` | **`libEGL.so` ✅, `libGLESv2.so` ✅, `libvulkan.so` ❌** |
| `libroblox.so` imported `egl*` symbols | **17** |
| `libroblox.so` imported `gl*` symbols | **74** |
| `libroblox.so` imported `vk*` symbols | **0** |
| APK-wide imported `vk*` symbols | **0** |
| `libzstd-jni-1.5.7-6.so` `DT_NEEDED` | `libEGL.so`, `libGLESv3.so` |
| `<uses-feature android:glEsVersion>` | **`0x30000`, required=true** |
| `<uses-feature android:name="android.hardware.vulkan.*">` | **absent** |

### 7.2 GLES path — VERIFIED

**EGL (17 imported, all from `libEGL.so`):**
`eglGetDisplay`, `eglInitialize`, `eglTerminate`, `eglGetError`, `eglChooseConfig`,
`eglGetConfigAttrib`, `eglCreateContext`, `eglDestroyContext`, `eglCreateWindowSurface`,
`eglCreatePbufferSurface`, `eglDestroySurface`, `eglMakeCurrent`, `eglQuerySurface`,
`eglSwapBuffers`, `eglSwapInterval`, `eglGetCurrentContext`, **`eglGetProcAddress`**.

**GLES 2.0-level functions are hard-linked (74).** The full list is in the appendix; it is a
textbook GLES2 core set: `glDrawElements`, `glDrawArrays`, `glTexImage2D`,
`glCompressedTexImage2D`, `glCreateShader`/`glShaderSource`/`glCompileShader`/
`glLinkProgram`, `glGenFramebuffers`/`glFramebufferTexture2D`, `glBindBuffer`/`glBufferData`,
etc.

**GLES 3.x is reached through `eglGetProcAddress`, not through `DT_NEEDED`.** VERIFIED:
`libroblox.so`'s `.rodata` contains **171 distinct `gl[A-Z]*` function-name strings**, of
which **97 are NOT in the import table** — exactly the GLES3/extension set, each with both
the core and the `EXT`/`OES` spelling so it can fall back:

```
glMapBufferRange/EXT      glUnmapBuffer/OES        glMapBuffer/OES
glBindVertexArray/OES     glGenVertexArrays/OES    glDeleteVertexArrays/OES
glBindBufferBase/EXT      glBindBufferRange/EXT    glUniformBlockBinding/EXT
glGetUniformBlockIndex/EXT glGetActiveUniformBlockiv/EXT
glTexStorage2D/EXT        glTexStorage3D/EXT       glTexImage3D/OES
glTexSubImage3D/OES       glCompressedTexImage3D/OES glCompressedTexSubImage3D/OES
glDrawArraysInstanced/EXT glDrawElementsInstanced/EXT glDrawBuffers/EXT
glBlitFramebuffer/EXT     glRenderbufferStorageMultisample/EXT
glInvalidateFramebuffer/EXT glFramebufferTextureLayer/EXT
glClearBufferfv/iv/fi (+EXT)                       glCopyImageSubData/EXT/OES
glFenceSync/EXT glWaitSync/EXT glClientWaitSync/EXT glDeleteSync/EXT
glGenQueries/EXT glBeginQuery/EXT glEndQuery/EXT glDeleteQueries/EXT
glGetQueryiv/EXT glGetQueryObjectiv/EXT glGetQueryObjectui64v/EXT glQueryCounter/EXT
glGetProgramBinary/OES glProgramBinary/OES glProgramParameteri/OES
glBufferStorage/EXT      glGetInteger64v/EXT
glPushGroupMarker/EXT glPopGroupMarker/EXT glObjectLabelKHR
```

GL extension strings it queries: `GL_KHR_texture_compression_astc_ldr`,
`_astc_hdr`, `_astc_sliced_3d`, `GL_OES_texture_compression_astc`,
`GL_IMG_texture_compression_pvrtc`, `GL_OES_compressed_ETC1_RGB8_texture`,
`GL_EXT_color_buffer_float`, `GL_EXT_buffer_storage`, `GL_EXT_copy_image`,
`GL_OES_copy_image`, `GL_OES_texture_border_clamp`, `GL_EXT_disjoint_timer_query`,
`GL_EXT_timer_query`, `GL_KHR_debug`, `GL_ARB_pixel_buffer_object`,
`GL_ARB_shader_storage_buffer_object`.

### 7.3 Vulkan path — VERIFIED as dlopen-only

`libroblox.so` contains the strings, **in `.rodata`**:
```
libvulkan.so
libvulkan.so.1
vkGetInstanceProcAddr
vkGetDeviceProcAddr
vkCreateInstance
vkCreateDevice
"Unable to load Vulkan API: vkCreateInstance is NULL"
```
plus **593 distinct `vk[A-Z]*` entry-point name strings** — the complete Vulkan 1.3 + vendor
extension table, i.e. a **volk-style generated loader**. Since `vk*` count in `.dynsym` is
**0**, every one of those is resolved at runtime through
`dlopen("libvulkan.so") → dlsym("vkGetInstanceProcAddr") → vkGetInstanceProcAddr(...)`.

Instance/device extensions actually named as strings (VERIFIED, 57 `VK_*` strings total):
`VK_KHR_surface`, **`VK_KHR_android_surface`**, `VK_KHR_swapchain`,
`VK_KHR_get_physical_device_properties2`, `VK_KHR_get_memory_requirements2`,
`VK_KHR_bind_memory2`, `VK_KHR_maintenance1`, `VK_KHR_maintenance2`, `VK_KHR_multiview`,
`VK_KHR_sampler_ycbcr_conversion`,
`VK_ANDROID_external_memory_android_hardware_buffer`,
`VK_EXT_queue_family_foreign`, `VK_EXT_extended_dynamic_state`,
`VK_EXT_texture_compression_astc_hdr`, `VK_EXT_device_fault`,
`VK_EXT_debug_utils`, `VK_EXT_debug_report`, `VK_EXT_debug_marker`,
`VK_EXT_validation_features`.
Layers it knows about: `VK_LAYER_KHRONOS_validation`, `VK_LAYER_RENDERDOC_Capture`,
`VK_LAYER_ADRENO_debug`.
AHB interop entry points: `vkGetAndroidHardwareBufferPropertiesANDROID`,
`vkGetMemoryAndroidHardwareBufferANDROID`.

Renderer-selection logic visible as strings:
```
[FLog::Graphics] Vulkan Android Device: %s
[FLog::Graphics] Vulkan Device: API %d.%d.%d
[FLog::Graphics] Vulkan Device: Driver %d.%d.%d (%d)
[FLog::Graphics] Vulkan Device: Vendor %04x Device %04x
[FLog::Graphics] Vulkan: Device %s is blacklisted, skipping
[FLog::Graphics] Vulkan: Device %s is emulated, skipping      <-- !!
[FLog::Graphics] Vulkan: Device %s is incompatible, skipping
Unable to pick Vulkan device
Unable to create Vulkan device
Device %s is blacklisted for Vulkan
Vendor: 0x%X Device 0x%X Driver 0x%X is blacklisted for Vulkan
GraphicsVulkanBlacklistDevicePattern
GraphicsVulkanBlacklistVendorDeviceDriverIDs
DebugGraphicsPreferVulkan
StudioDisableVulkanGraphicsMode
SupportHeadlessDeviceVulkan
```
There is a `RBX::Enums::GraphicsMode` reflected enum with `Vulkan`, `OpenGL`, `Metal`,
`Direct3D11` members (mangled names `N3RBX10Reflection18EnumPropDescriptorI19CRenderSettingsItemNS_5Enums12GraphicsModeEEE`).

### 7.4 Shader assets — VERIFIED

| Asset | Size | Compression | Container magic | Contents |
|---|---:|---|---|---|
| `assets/shaders/shaders_vulkan_mobile.pack` | **14,724,671 B** | **STORED** | `RBXS` v0x0b, variant `default` | **1,364 SPIR-V modules** (little-endian magic `03 02 23 07` = `0x07230203`) |
| `assets/shaders/shaders_glsles3.pack` | 3,964,445 B | **STORED** | `RBXS` v0x0b, variant `default` | GLSL ES source/binary; **0** SPIR-V magics |

Both packs contain the same named shader entries (`ProfilerFS`, `ShadowEvsm*`, `Blur3FS`,
`SunRays12FS`, …). The Vulkan pack additionally contains compute shaders that have no GLES
counterpart: `LightGridCullCountOffsetCS`, `ClearOneChannelCS`,
`CompressAstc6x6RgUncorrelatedCS`, `motionBuffer*` (temporal AA / motion vectors),
`ASWDepthDownsize`, `motionBufferDepthASWHack`. `libroblox.so` logs
`[FLog::Graphics] Loaded {} shaders from pack {} variant {} ({} bytes)` and
`%d instanced compute shaders failed to load.`

### 7.5 Verdict

| Question | Answer | Basis |
|---|---|---|
| Is Vulkan linked directly? | **No.** `libvulkan.so` is not `DT_NEEDED`; zero `vk*` imports. | VERIFIED |
| Is Vulkan `dlopen`'d? | **Yes**, via `libvulkan.so` then `libvulkan.so.1`, volk-style. | VERIFIED |
| Is GLES linked directly? | **Yes** — `libEGL.so` + `libGLESv2.so` `DT_NEEDED`, 17 EGL + 74 GL imports. | VERIFIED |
| Are GLES3 entry points linked? | **No** — resolved via `eglGetProcAddress` (97 extra name strings). | VERIFIED |
| Which is the likely default? | **Vulkan is tried first and preferred; GLES 3 is the guaranteed fallback.** | INFERENCE |

**Reasoning for the verdict (INFERENCE, but strongly supported):**
* The Vulkan shader pack is **3.7× larger** than the GLES pack (14.7 MB vs 4.0 MB) and
  contains compute shaders and motion-vector/temporal passes the GLES pack lacks — you do
  not ship a 14.7 MB secondary path.
* `<uses-feature glEsVersion=0x30000 required=true>` and **no** Vulkan `uses-feature` means
  GLES3 is the *floor* the app guarantees it can run on — i.e. the fallback — while Vulkan
  is opportunistic (hence dlopen + blacklist + "is incompatible, skipping" logic).
* The presence of `GraphicsVulkanBlacklistDevicePattern` /
  `GraphicsVulkanBlacklistVendorDeviceDriverIDs` FFlags implies Vulkan is the default that
  gets *turned off* for bad drivers, not an opt-in.

**Critical for Omnidroid:** the string `Vulkan: Device %s is emulated, skipping` means that
if the host exposes a software/virtual Vulkan device (lavapipe, SwiftShader, a virtio-gpu
device, or anything reporting `VK_PHYSICAL_DEVICE_TYPE_CPU` / `_VIRTUAL_GPU`), Roblox will
reject it and fall back to GLES 3. Presenting a real host GPU through a Vulkan
pass-through — with a `deviceType` of `DISCRETE_GPU`/`INTEGRATED_GPU` and a plausible
vendor/device ID not on Roblox's blacklist — is required to get the Vulkan path.

---

## 8. Assets

`assets/` = **596 entries, 83,271,015 B uncompressed** (391 STORED / 205 DEFLATED).

### 8.1 Tree summary

| Directory | Files | Uncompressed | Compressed | STORED |
|---|---:|---:|---:|---:|
| `assets/ExtraContent/` | 132 | 33,113,162 | — | — |
| ├ `models/` | 8 | 25,281,969 | 25,281,884 | 6 |
| ├ `LuaPackages/` | 60 | 3,130,106 | 3,129,823 | 59 |
| ├ `textures/` | 59 | 2,451,204 | 2,451,204 | 59 |
| ├ `translations/` | 2 | 1,919,259 | 610,347 | 0 |
| └ `places/` | 3 | 330,624 | 173,239 | 0 |
| **`assets/shaders/`** | **2** | **18,689,116** | 18,689,116 | **2** |
| `assets/content/` | 402 | 18,223,485 | — | — |
| ├ `fonts/` | 118 | 12,168,593 | 12,150,706 | 75 |
| ├ `sky/` | 10 | 2,955,861 | 1,314,449 | 2 |
| ├ `textures/` | 174 | 1,105,097 | 746,455 | 158 |
| ├ `configs/` | 34 | 819,883 | 220,926 | 0 |
| ├ `avatar/` | 58 | 701,075 | 234,056 | 17 |
| ├ `models/` | 4 | 440,097 | 171,877 | 2 |
| ├ `guac/` | 3 | 29,772 | 8,862 | 0 |
| └ `localization/` | 1 | 3,107 | 352 | 0 |
| `assets/android/` | 45 | 10,036,534 | — | — |
| ├ `textures/` | 42 | 8,644,782 | 2,492,965 | **0** |
| ├ `shared_compression_dictionaries/` | 1 | 1,377,228 | 410,796 | 0 |
| ├ `terrain/` | 1 | 8,915 | 1,389 | 0 |
| └ `fonts/` | 1 | 5,609 | 892 | 0 |
| **`assets/gloop/`** ⚠️ | **2** | **2,061,867** | 2,053,090 | 0 |
| `assets/fonts/` | 5 | 746,688 | 746,688 | 5 |
| `assets/ssl/` | 1 | 228,725 | 130,451 | 0 |
| `assets/com/appsflyer/` | 4 | 30,882 | 30,882 | 4 |
| `assets/dexopt/` | 2 | 7,819 | 7,819 | 2 |
| `assets/PublicSuffixDatabase.list` | 1 | 132,737 | 42,484 | 0 |

### 8.2 File-type census

| Ext | Count | Bytes | Identified magic |
|---|---:|---:|---|
| `.rbxm` | 25 | 25,409,206 | `3c 72 6f 62 6c 6f 78 21 89 ff 0d 0a 1a 0a` = **`<roblox!\x89\xff\r\n\x1a\n`** — Roblox binary model |
| `.pack` | 2 | 18,689,116 | `RBXS` — Roblox shader pack (§7.4) |
| `.ttf` | 69 | 11,624,176 | TrueType |
| `.dds` | 28 | 5,700,493 | `DDS \x7c` — DirectDraw Surface |
| `.tex` | 12 | 5,244,312 | Roblox texture container |
| `.png` | 273 | 5,059,547 | `\x89PNG\r\n\x1a\n` |
| `.zip` | 1 | 2,053,671 | **`assets/gloop/dlt.zip`** ⚠️ |
| `.csv` | 2 | 1,919,259 | localization tables |
| `.otf` | 13 | 1,743,804 | OpenType |
| `.dict` | 1 | 1,377,228 | **JSON** (`{"applicationSet…`) despite `.dict` name — Zstd shared-dictionary manifest |
| `.ktx` | 26 | 1,269,980 | Khronos texture |
| `.mesh` | 43 | 1,013,679 | Roblox mesh |
| `.js` | 2 | 777,771 | `rofiler.js`, `rofiler.tools.js` — profiler web UI |
| `.jpg` | 3 | 533,679 | |
| `.rbxl` | 3 | 330,624 | Roblox place files (`ExtraContent/places/`) |
| `.pem` | 1 | 228,725 | `assets/ssl/cacert.pem` — **CA bundle** |
| `.list` | 1 | 132,737 | OkHttp public-suffix DB |
| `.json` | 82 | 115,855 | |
| (none) | 7 | 39,334 | |
| `.prof`/`.profm` | 2 | 7,819 | ART baseline profile |

**Biggest single assets:** `shaders_vulkan_mobile.pack` 14,724,671 ·
`ExtraContent/models/InExperience/InExperience.rbxm` 12,786,640 ·
`ExtraContent/models/UniversalApp/UniversalApp.rbxm` 12,081,231 ·
`shaders_glsles3.pack` 3,964,445 · `content/sky/clouds.dds` 2,396,905 ·
`gloop/dlt.zip` 2,053,671 · `CoreScriptLocalization.csv` 1,512,425 ·
`android/textures/plastic/normal.dds` 1,398,268 ·
`android/shared_compression_dictionaries/67d516…03cf.dict` 1,377,228 ·
`content/fonts/TwemojiMozilla.ttf` 1,324,332.

### 8.3 Roblox's own VFS / content system — VERIFIED

`libroblox.so` contains **3,066 distinct `rbxasset://` URI strings**, including the scheme
prefixes `rbxasset` / `rbxasset://` and fully-qualified paths such as
`rbxasset://LuaPackages/Packages/_Index/BuilderIcons/BuilderIcons/Font/BuilderIcons-Regular.ttf`
and `rbxasset://textures/DeviceEmulator/emulator.png`, plus bare mount-point strings
`ExtraContent`, `ExtraContent/`, `content/`, `content/fonts/`, `shaders`, `shaders_`, `.pack`,
`../shaders`.

**This confirms a virtual-filesystem layer inside the engine**: `rbxasset://<path>` is
resolved against the APK's `assets/content/` and `assets/ExtraContent/` trees.
The bridge to the OS is `AAssetManager_fromJava` → `AAssetManager_open` →
`AAsset_getBuffer` / `AAsset_read` / `AAsset_getLength` / `AAsset_openFileDescriptor` (all
six imported), and the Java side supplies the base path via
`Java_com_roblox_client_startup_MainGameActivity_nativeSetAssetPath`.

**Things the runtime must serve from the APK at runtime:**
1. `assets/shaders/shaders_vulkan_mobile.pack` **or** `assets/shaders/shaders_glsles3.pack`
   (whichever renderer wins) — no first frame without one.
2. `assets/ExtraContent/models/UniversalApp/UniversalApp.rbxm` — the entire app UI.
3. `assets/ExtraContent/LuaPackages/**` — Luau CoreScripts/UI packages.
4. `assets/content/fonts/**` (118 files) — text rendering.
5. `assets/content/configs/**` — `ClientAppSettings`-style JSON.
6. `assets/android/textures/**`, `assets/content/textures/**`, `assets/content/sky/**`.
7. `assets/ssl/cacert.pem` — TLS trust store for the engine's HTTP stack.
8. `assets/android/shared_compression_dictionaries/*.dict` — Zstd shared dictionary.
9. `assets/ExtraContent/translations/*.csv`, `assets/content/localization/*`.

**`AAsset_openFileDescriptor` is imported.** That API returns an fd into the APK **plus an
offset and length**, and it only works for **uncompressed (STORED)** entries. 391 of the 596
asset entries are STORED, including both shader packs and the big `.rbxm` files — this is
deliberate. Omnidroid's asset layer must support the fd+offset+length form, not just
`AAsset_getBuffer`.

---

## 9. Startup requirements inferred from the above

### 9.1 The chain (INFERENCE from VERIFIED facts, each step cited)

1. **Process start / `Application`.** `RobloxApplication extends android.app.Application`
   (§5.3). `androidx.startup.InitializationProvider` runs four initializers before
   `Application.onCreate` returns (§6.5). No native code yet.
2. **`ActivitySplash`** launches (the LAUNCHER alias targets it — §6.5). Java-only.
3. **`MainGameActivity extends com.google.androidgamesdk.GameActivity`** starts (§5.3).
4. **`GameActivity.onCreate` reads `android.app.lib_name = "roblox"`** (§6.5) and calls
   `System.loadLibrary("roblox")`.
5. **The dynamic loader must now handle `libroblox.so`:**
   * Inflate 109,193,800 bytes from a DEFLATE stream (§1.3) — libs are **not** mmap-able
     in place.
   * Map 9 program headers, honouring 16 KiB `p_align` (§3.6).
   * Resolve `DT_NEEDED`: `libOpenMAXAL.so`, `libmediandk.so`, `libandroid.so`, `libm.so`,
     `libOpenSLES.so`, `libGLESv2.so`, `libEGL.so`, `liblog.so`, `libdl.so`, `libc.so`
     (§3.2). Two of those (`libOpenSLES`, `libOpenMAXAL`) need only to *exist* (§4.4).
   * **Apply 568,272 `APS2`-packed relocations** (§3.3) — 568,194 of them `R_AARCH64_RELATIVE`
     — plus 534 `R_AARCH64_JUMP_SLOT`, plus bind 565 undefined symbols.
   * Apply `PT_GNU_RELRO` to a **5,205,568-byte** region (§3.5).
6. **Run 3,594 `DT_INIT_ARRAY` entries** (§3.5). This is where malloc, pthread keys, stdio
   (`__sF`), `__cxa_atexit`, `__system_property_get`, `getauxval`, `dl_iterate_phdr`,
   locale/ctype (`_ctype_`, `__ctype_get_mb_cur_max`) and the stack-protector
   (`__stack_chk_guard`, `__stack_chk_fail`) all get exercised for the first time.
7. **`JNI_OnLoad(vm, reserved)`** runs (§5.1). It must at minimum:
   * `GetEnv(JNI_VERSION_1_6)`;
   * `FindClass` + `RegisterNatives` for `com/google/androidgamesdk/GameActivity`
     (23 methods — §5.2) and the ~10 other `RegisterNatives` classes;
   * cache `jclass`/`jmethodID`/`jfieldID` for the Roblox bridge classes
     (`com/roblox/engine/jni/NativeGLInterface`, `NativeInputInterface`,
     `NativeSettingsInterface`, `com/roblox/universalapp/messagebus/MessageBus`, …);
   * store the `JavaVM*` for later `AttachCurrentThread`.
8. **`Java_com_google_androidgamesdk_GameActivity_initializeNativeCode`** is called (§5.1).
   The AGDK glue then creates a native thread running the app's `android_main`-equivalent,
   builds an `ALooper` (`ALooper_prepare`, `ALooper_addFd`, `ALooper_pollOnce` — §4.4), and
   wires the `GameActivity` callback table.
9. **Java-side configuration calls** (all 539 `Java_*` symbols are available; the ones that
   must succeed before rendering, from §5.1):
   `MainGameActivity_nativeSetAssetPath`, `MainGameActivity_nativeAppBridgeSetInitParams`,
   `MainGameActivity_nativePreloadFlagOverrides`,
   `NativeSettingsInterface_nativeSetBaseDataDirectories` / `nativeSetFilesDirectory` /
   `nativeSetCacheDirectory` / `nativeSetExternalDirectory` / `nativeSetPreferencesFile` /
   `nativeSetRobloxVersion` / `nativeSetRobloxChannel` / `nativeSetDeviceInfo` /
   `nativeSetBaseUrl` / `nativeInitFastLog`,
   `FlagJniInterface_nativeInitializeNativeFlags`,
   `NativeGLInterface_nativeInitClientSettings*`, `NativeGLInterface_nativeGameGlobalInit`,
   `NativeGLInterface_nativeAppBridgeV2InitWithParams`.
   `NativeSettingsInterface_getRunningArchitecture`, `nativeCPUSupportsNEON` and
   `nativeGetCpuFamilyAndFeatures` will probe the CPU — these read `/proc/cpuinfo` and
   `getauxval(AT_HWCAP)` and must return a coherent AArch64 answer.
10. **Surface.** Java delivers a `Surface`; native calls `ANativeWindow_fromSurface`,
    `ANativeWindow_getWidth/getHeight/getFormat`, `ANativeWindow_setBuffersGeometry`, then
    `NativeGLInterface_nativeAppBridgeV2UpdateSurfaceAppWithPlatformParams` (§4.4, §5.1).
11. **Renderer selection.** `dlopen("libvulkan.so")` → probe → if it fails, is blacklisted,
    or reports "emulated", fall back to EGL/GLES3 (§7.3, §7.5).
12. **Asset mount + shader load.** `AAssetManager_fromJava` → open
    `shaders/shaders_vulkan_mobile.pack` or `shaders/shaders_glsles3.pack`, then
    `UniversalApp.rbxm` and `LuaPackages` (§8.3).
13. **First frame** via `eglSwapBuffers` or `vkQueuePresentKHR`.

### 9.2 Minimum viable stub/implementation set to reach `JNI_OnLoad`

**Blocking, must be real:**
| Component | Why |
|---|---|
| AArch64 ELF64 `ET_DYN` loader with **`DT_ANDROID_RELA`/APS2** decoding | §3.3 — without it `libroblox.so` cannot be relocated at all |
| DEFLATE inflate of ZIP entries (or a pre-extraction step) | §1.3 — libs are compressed and unaligned |
| 16 KiB-page-aware mapping + `PT_GNU_RELRO` + `mprotect` | §3.5, §3.6 |
| `libc.so` with the **365 generic + 50 bionic-specific** symbols | §4.1, §4.2, §4.3 |
| `libm.so` with 55 symbols | §4.1 |
| `libdl.so`: `dlopen`, `dlsym`, `dlclose`, `dlerror`, `dladdr`, **`dl_iterate_phdr`** | §4.1, §4.5 |
| `liblog.so`: 5 symbols | §4.1 |
| `pthread_key_*` with a generous key limit | §3.4 |
| `__cxa_atexit`, `__cxa_finalize`, `__gxx_personality_v0`, `__cxa_thread_atexit_impl` (weak) | §4.2 |
| Working AArch64 `.eh_frame` unwinding at mapped addresses | §3.8 |
| `getauxval` returning credible `AT_HWCAP`/`AT_HWCAP2`/`AT_PAGESZ`/`AT_PLATFORM` | §4.3 |
| `__system_property_get` for the 8 `ro.*` keys | §4.3 |
| JNI `JNIEnv`/`JavaVM` facade: `GetEnv`, `FindClass`, `GetMethodID`, `GetStaticMethodID`, `GetFieldID`, `RegisterNatives`, `NewStringUTF`, `GetStringUTFChars`, `NewGlobalRef`, `AttachCurrentThread`, array + `Call*Method` families | §5.2 |
| `libandroid.so` with 32 symbols, `libjnigraphics.so` with 3 | §4.4 |
| `libEGL.so` + `libGLESv2.so` with 17 + 74 symbols **and a working `eglGetProcAddress` for the 97 GLES3 names** | §7.2 |
| **Loadable stubs for `libOpenSLES.so`, `libOpenMAXAL.so`, `libGLESv3.so`, `libz.so`** | §3.2, §4.4 — needed for `DT_NEEDED` resolution even with zero imports |

**Can be stubbed to fail gracefully at first:**
`libmediandk.so` (33 — video/audio codec; return errors),
`libcamera2ndk.so` / `libaaudio.so` (dlopen — let `dlopen` return NULL),
`libvulkan.so` (let `dlopen` fail → forces the GLES path, which is the simpler bring-up),
`libtrampoline.so` / Crashpad (`nativeInitCrashpad` can be made a no-op),
`librenderscript-toolkit.so`, `libeigen_*`, `libyuv_shared.so`,
`libimage_processing_util_jni.so`, `libsurface_util_jni.so`,
`libdatastore_shared_counter.so`, `libbacktrace-native.so` — none is on the
first-frame path.

**Recommended first milestone:** load `libroblox.so` only, stub `libEGL`/`libGLESv2` to a
real desktop GL 4.x context via a GLES3→GL translation layer, make `dlopen("libvulkan.so")`
fail, and target "3,594 initializers complete + `JNI_OnLoad` returns `JNI_VERSION_1_6`".
That single milestone exercises ~470 of the 669 undefined symbols.

---

## 10. Anti-tamper / integrity

### 10.1 In `libroblox.so` (Roblox's own; VERIFIED string evidence)

| Mechanism | Evidence |
|---|---|
| **Root detection with user-visible message** | `"Roblox cannot be used in a rooted environment. Please run Roblox on a supported device."` |
| **Root kick codes (server-side telemetry/disconnect)** | `AndroidRootedKick`, `DisconnectAndroidRootedKick` |
| **Emulator detection kick codes** | `AndroidEmulatorKick`, `DisconnectAndroidEmulatorKick` |
| **Generic anti-cheat kick code** | **`AndroidAnticheatKick`**, `DisconnectAndroidAnticheatKick` |
| **Remote device attestation** | `Java_com_roblox_engine_jni_meta_NativeMetaInterface_getDeviceAttestationToken`, and kick codes `DisconnectRemoteAttestationFailureGeneral`, `…FailureBootValidation`, `…FailureOsOutOfDate`, `…FailureTimeout`, `…FailureUnsupported`, `…GeneralFailure`, `…BootValidationFailure`, `…OSOutOfDate`, `…Timeout`, `…Unsupported` |
| **Play Integrity plumbing (Java side)** | `integrity.properties` at APK root; `com.google.android.play.core.*` in dex |
| **GPU-level emulator rejection** | `[FLog::Graphics] Vulkan: Device %s is emulated, skipping` |
| `/proc` introspection | `/proc/self/maps`, `/proc/self/status`, `/proc/self/auxv`, `/proc/self/exe`, `/proc/self/fd`, `/proc/%d/mem`, `/proc/%d/task`, `/proc/sys/kernel/yama/ptrace_scope`, `/proc/cpuinfo`, `/proc/meminfo` — note most of these are Crashpad's, not necessarily anti-cheat |
| Device fingerprinting properties | `ro.build.fingerprint`, `ro.product.model`, `ro.product.manufacturer`, `ro.product.board`, `ro.hardware`, `ro.soc.manufacturer`, `ro.arch`, `ro.build.version.sdk` |
| Crash reporting (not anti-tamper, but process-level) | Crashpad + `.note.crashpad.info` + `CrashpadHandlerMain` + `libtrampoline.so` (§3.9) + Backtrace.io (`libbacktrace-native.so`, `CrashpadUploadToBacktraceUrl`) |
| **Appdome ThreatEvents SDK** | `META-INF/appdome-threatevents_release.kotlin_module` |

**No `Hyperion` or `Byfron` string appears anywhere in `libroblox.so`.** (Both patterns were
searched, case-insensitively, over the full 109 MB: zero hits.) The Android client's
anti-cheat is evidently server-attestation + root/emulator heuristics rather than the
desktop Hyperion packer.

**Packing/obfuscation of `libroblox.so`:** none detected. It is a plain, stripped LLVM/NDK
r28c build: standard sections, readable `.rodata` (3,066 `rbxasset://` strings, full Itanium
C++ mangled names like `N3RBX8Graphics12DeviceVulkanE`, readable `FLog::` format strings),
no self-modifying `.text` section, no encrypted blob, `.text`/`.rodata` proportions normal.
`DT_FLAGS_1 = 0x1 (DF_1_NOW)`, no `DT_TEXTREL`.
**INFERENCE: `libroblox.so` in this APK appears to be the unmodified stock library.**

### 10.2 In `libzstd-jni-1.5.7-6.so` (the injected payload; VERIFIED)

| Observation | Evidence |
|---|---|
| Size/shape anomaly | 18,440,296 B; `.text` 42,680 B vs `.data` 11,162,632 B + `.rodata` 3,743,972 B |
| Non-standard sections | `.adi` (703,748 B), `.rhash` (88 B), `.stack` (88 B) |
| Only 16 KiB-incompatible library | max `PT_LOAD` `p_align = 0x1000` (§3.6) |
| No build-id | only note is `.note.android.ident` (§3.7) |
| Built with a *different* NDK | r26d/11579264 vs r28c for `libroblox.so` (§3.7) |
| **Graphics hooking** | `DT_NEEDED libEGL.so` + `libGLESv3.so`; imports `eglSwapBuffers`, `eglMakeCurrent`, `eglGetCurrentDisplay`, `eglGetCurrentContext`, `eglGetCurrentSurface`, `eglQueryContext`, `eglDestroyContext`, `eglTerminate`, 53 `gl*`, and holds the strings `vkGetInstanceProcAddr` / `vkGetDeviceProcAddr` |
| **Code patching primitives** | imports `mprotect`, `dlopen`, `dlsym`, `dladdr`, `dl_iterate_phdr`, `pthread_create`, `signal` |
| **Process introspection** | string `/proc/self/maps` |
| **Overlay UI** | `Dear ImGui 1.92.9 (19290)`, `imgui_impl_android`, `imgui_impl_opengl3`, `imgui.ini`, `imgui_log.txt` |
| **Luau decompiler** | `luau-lifter/src/lifter.rs`, `luau-lifter/src/deserializer/bytecode.rs`, `ast/src/formatter.rs`, `cfg/src/ssa/{construct,destruct,inline,upvalues,structuring}.rs`, `restructure/src/{loop,conditional,jump}.rs`, full `LOP_*` opcode table, `"-- failed to decompile"` |
| **Bundled network + TLS** | libcurl 8.21.0 + OpenSSL/BoringSSL (QUIC, X.509, SCT) |
| **Modder's build path** | **`/tmp/gloop-deps-build/src/curl-8.21.0/lib/vtls/openssl.c`** |
| **Modder's machine** | `/Users/user/.rustup/toolchains/nightly-aarch64-apple-darwin/…`, `/Users/user/.cargo/registry/…`, `/Users/user/.cargo/git/checkouts/petgraph-…` |
| Rust runtime | `library/std/src/panicking.rs`, `gimli-0.32.3`, `addr2line-0.25.1`, `rustc-demangle-0.1.27`, `miniz_oxide-0.8.9`, `hashbrown`, `indexmap-1.9.3`, `parking_lot_core-0.9.12`, `petgraph` |
| Loads/uses assets | `assets/gloop/dlt.zip`, `getDownloadUrl` native |

**Documented, not defeated.** The compatibility requirement this creates for Omnidroid is
mechanical: if this APK is loaded as-is, the runtime will be asked to
`dlopen` `libEGL.so`/`libGLESv3.so`, `mprotect` guest text pages to `RWX`, walk
`/proc/self/maps`, iterate `dl_iterate_phdr`, spawn threads, install signal handlers, and
render an ImGui overlay through the same GL context Roblox uses. A runtime that only
implements the "well-behaved app" subset will crash here.

### 10.3 Signature verification the runtime must be compatible with

* `MANIFEST.MF` carries **2,362** `SHA-256-Digest` entries (one per archive member, v1).
* v2 and v3 blocks cover the whole file; the verity padding block is present.
* **If Omnidroid ever re-packs, re-compresses, or page-aligns entries, all three signature
  schemes break.** Whether that matters depends on whether anything in the guest verifies
  its own APK — nothing in `libroblox.so`'s strings suggests it computes its own APK digest,
  but `getDeviceAttestationToken` + Play Integrity on the *server* side will see the
  non-Roblox signing certificate regardless (§ Headline Finding).

---

## Design implications for Omnidroid

1. **Implement the Android packed-relocation format (`DT_ANDROID_RELA`, `APS2`) before
   anything else.** `libroblox.so` has no `DT_RELA` at all — 568,272 relocations are only
   reachable through the SLEB128 group decoder. A loader that handles only `DT_RELA`/`DT_RELR`
   loads exactly zero of Roblox's code. (Also implement `DT_ANDROID_REL` and `DT_RELR` for
   future Roblox builds; they are absent today but cheap to add.)

2. **Plan for 16 KiB pages, not 4 KiB.** Ten of eleven libraries have `PT_LOAD p_align =
   0x4000`, and `libroblox.so` has an explicit `.relro_padding` section. On Windows x86-64
   the allocation granularity is 64 KiB and the page size 4 KiB, so segment mapping needs a
   reserve-then-commit scheme that satisfies a 16 KiB alignment contract without wasting
   address space across 109 MB of image.

3. **No ELF TLS implementation is needed; a fast `pthread_key_*` is.** Zero `PT_TLS`
   segments and zero `STT_TLS` symbols in the whole APK. Skip TLSDESC/`__tls_get_addr`/DTV
   entirely and spend that effort on `pthread_getspecific` being a couple of instructions —
   Roblox's engine and the Rust code in the payload both hit it on hot paths. Make the key
   limit much larger than bionic's.

4. **No ifunc, no BTI/PAC/MTE, no `DT_TEXTREL`.** Zero `R_AARCH64_IRELATIVE`, zero
   `STT_GNU_IFUNC`, no `.note.gnu.property` anywhere. The ARM→x86 translation layer does not
   need to model branch-target-identification landing pads or pointer authentication for
   this APK, and can assume all executable pages are relocated-then-sealed.

5. **The C++ runtime is static, so the unwinder lives inside the guest.** Only 4 C++-ABI
   symbols are imported. That means `libroblox.so`'s own copy of libunwind will read
   `.eh_frame` (11.5 MB of it) at guest virtual addresses and walk guest frames. Any
   host-side exception, signal, or stack-switching mechanism must leave the guest AArch64
   stack layout and frame pointers intact, and `dl_iterate_phdr` must report accurate
   `dlpi_addr`/`dlpi_phdr`/`dlpi_phnum` or unwinding silently fails.

6. **Budget for 3,594 static initializers as the first real milestone.** They run before any
   Roblox logic and before `JNI_OnLoad`. Instrument them: the first crash will be in one of
   them, and the failing symbol will identify the missing libc piece. 705 more run in the
   zstd payload if it is loaded.

7. **Build the JNI facade around `RegisterNatives`, not just `Java_*` resolution.** 93% of
   natives are statically named, but the 24 `com.google.androidgamesdk.GameActivity` methods
   — the ones that drive the whole native lifecycle and input pump — are registered
   dynamically from `JNI_OnLoad`. `FindClass` + `RegisterNatives` + `GetMethodID` must work
   against synthetic class objects for at least `GameActivity`, `NativeGLInterface`,
   `NativeInputInterface`, `NativeSettingsInterface`, `MessageBus`, and `FlagJniInterface`.

8. **The startup shape is AGDK `GameActivity`, not `NativeActivity`.** No class extends
   `android.app.NativeActivity`; `MainGameActivity extends
   com.google.androidgamesdk.GameActivity`, and the native entry is
   `Java_com_google_androidgamesdk_GameActivity_initializeNativeCode` — **not**
   `ANativeActivity_onCreate` and **not** `android_main`. Omnidroid's shim must reproduce
   GameActivity's contract: a native thread, an `ALooper` with `ALooper_pollOnce` and
   `ALooper_addFd`, the `GameActivityCallbacks` table, and GameActivity's own input-event
   queue (motion/key events arrive as `GameActivityMotionEvent`, not raw `AInputEvent` —
   note that **no `AInputEvent_*`/`AMotionEvent_*`/`AKeyEvent_*` symbols are imported at
   all**, which independently confirms GameActivity's buffered-input model).

9. **Target GLES 3 first, Vulkan second.** `libEGL.so` + `libGLESv2.so` are `DT_NEEDED` with
   91 hard-linked entry points and GLES3 comes through `eglGetProcAddress`; Vulkan is
   `dlopen`-only with zero link-time dependency. Making `dlopen("libvulkan.so")` return NULL
   is a legitimate, supported configuration that forces the simpler path — and
   `shaders_glsles3.pack` (4.0 MB) exists precisely for it. Get a frame on GLES3, then chase
   Vulkan.

10. **But the Vulkan path is the one Roblox wants, and it will refuse a fake GPU.**
    The 14.7 MB Vulkan shader pack with 1,364 SPIR-V modules and compute/temporal-AA passes
    that the GLES pack lacks makes Vulkan the quality path. To get it, Omnidroid must expose
    `vkGetInstanceProcAddr` from a pass-through to a real host Vulkan driver reporting
    `VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU` or `_INTEGRATED_GPU` (not `_CPU`/`_VIRTUAL_GPU`),
    a vendor/device ID not on Roblox's blacklist, and `VK_KHR_android_surface` +
    `VK_KHR_swapchain` + the nine other `VK_KHR_*` extensions. `Vulkan: Device %s is
    emulated, skipping` is a live code path.

11. **`extractNativeLibs` is effectively `true`, and that is good news.** All 11 `.so`
    entries are DEFLATED and 4-byte aligned, so the guest already expects libraries to exist
    as ordinary files under a writable `lib/arm64/` directory. Omnidroid can inflate them
    once to disk on first run, cache them, and mmap from there — no need for an
    inflate-into-anonymous-memory loader, and no need to preserve the APK's byte layout.

12. **Assets must be served through the `AAsset*` API including
    `AAsset_openFileDescriptor`.** 391 of 596 asset entries are STORED specifically so the
    engine can get an `(fd, offset, length)` triple. A `AAsset_getBuffer`-only
    implementation will break shader-pack and `.rbxm` loading. Also implement
    `AConfiguration_*` (9 symbols) — the engine reads screen size/DPI/locale from it during
    init.

13. **Provide loadable stubs for libraries with zero imported symbols.**
    `libOpenSLES.so` and `libOpenMAXAL.so` are `DT_NEEDED` by `libroblox.so` but contribute
    **no** undefined symbols (they are `dlsym`'d later); `libGLESv3.so` is `DT_NEEDED` by the
    zstd payload; `libz.so` by `libbacktrace-native.so`. If `dlopen` of a `DT_NEEDED` name
    fails, the whole library fails to load — so these must exist as objects even when empty.

14. **Audio is FMOD over OpenSL ES / AAudio, both resolved by `dlsym`.** `libaaudio.so`
    appears only as a string, `libOpenSLES.so` only as `DT_NEEDED`, and the sole FMOD JNI
    export is `Java_org_fmod_FMOD_OutputAAudioHeadphonesChanged`. Audio can be deferred
    entirely for a first frame by making both `dlopen`s fail and hoping FMOD degrades to its
    `OutputEmulated` backend (`N4FMOD14OutputEmulatedE` is present) — verify this rather than
    assume it.

15. **Treat this specific APK as a hostile test target, and get a stock one.**
    The signing cert is `O=Gloop / CN=Gloopiest Man` (expired 2025-07-12), `classes4.dex` is
    a `com.roblox.gloop.Loader`, and `libzstd-jni-1.5.7-6.so` is a trojanised 18 MB blob with
    a Luau decompiler, Dear ImGui, libcurl/OpenSSL, and GL/Vulkan hooks. `libroblox.so`
    itself looks stock and is still a valid subject for §1–§9, but any conclusion drawn from
    the *set* of libraries, the permission list, or the dex graph is contaminated. Obtain a
    Play-signed `com.roblox.client` 2.738.x and re-run this analysis before freezing the API
    surface.

16. **Budget for root/emulator/attestation rejection even after the frame renders.**
    `AndroidRootedKick`, `AndroidEmulatorKick`, `AndroidAnticheatKick`, the ten
    `DisconnectRemoteAttestation*` codes, and `getDeviceAttestationToken` mean that a
    technically perfect compatibility layer will still be disconnected by Roblox's servers.
    Plan the project's success criterion around local rendering and engine execution
    (Studio-style/offline place files, of which three ship in
    `assets/ExtraContent/places/`), not around joining live games.

---

## Unknowns / needs runtime testing

1. **Does anything in `libroblox.so` verify its own APK signature or `sourceDir`?** No
   string evidence of an in-process APK digest, but the Java layer (26,620 classes, heavily
   obfuscated in places — e.g. `com.roblox.client.a`, `pk.z`) was not decompiled. Needs a
   dex-level control-flow audit or a runtime test.
2. **Exactly which of the 3,594 initializers touch the OS, and in what order.** Cannot be
   determined statically without disassembly; instrument at runtime.
3. **Whether `libroblox.so` tolerates `dlopen("libvulkan.so") == NULL` cleanly** or whether
   some FFlag combination makes Vulkan mandatory. `DebugGraphicsPreferVulkan` and
   `SupportHeadlessDeviceVulkan` exist as flag names; their defaults are set from
   `assets/content/configs/` + server-delivered `ClientAppSettings` and were not resolved.
4. **What `GetIsEmulationEnabled` / `Vulkan: Device %s is emulated` actually test.** The
   comparison is in compiled code; whether it keys off `VkPhysicalDeviceProperties::deviceType`,
   the device name string, or a driver-ID list is unknown.
5. **The precise `GameActivity` version and therefore the exact `RegisterNatives` signature
   list.** 23 method names were identified by DEX cross-check but their JNI descriptors and
   the AGDK ABI version were not extracted.
6. **Whether the `pthread_key` count actually exceeds bionic's limit at runtime** ("out of
   TLS keys, aborting" is a Rust string in the payload, not proof it triggers).
7. **Contents of `libzstd-jni-1.5.7-6.so`'s `.adi` section (703,748 B) and whether it is
   bytecode for a custom VM.** The 42,680-byte `.text` vs 11 MB `.data` ratio suggests yes,
   but this was not disassembled.
8. **Whether `resources.arsc` lookups are needed by native code.** No `AConfiguration`-driven
   resource resolution appears in the native imports, and the UI is Luau, so probably not —
   but the Java shell certainly needs it, and the 1,511 `res/` entries have to go somewhere.
9. **Real memory footprint.** `libroblox.so` maps ~109 MB of file plus a 5.2 MB RELRO region
   plus `.bss`; `largeHeap=false` means the Java heap is standard. Peak RSS with assets
   resident is unmeasured.
10. **Whether the engine's HTTP stack uses `assets/ssl/cacert.pem` exclusively** or also
    consults a system trust store (which would need a Windows-side bridge).
11. **Behaviour of `AAsset_openFileDescriptor` expectations** — whether the engine assumes
    the fd is seekable and shareable across threads, and whether it ever `mmap`s it directly.
12. **Multi-process.** `:isolate` and `:personaIsolate` are ads/KYC only *by static reading*;
    if `RobloxApplication.onCreate` unconditionally initialises the GMA SDK, a first-run
    attempt may try to fork a second process.
