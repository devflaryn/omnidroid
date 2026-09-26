# APK analysis: stock `Roblox-2.738.1397.apk`

Measured 2026-09-26 from the bytes of the stock file (git-ignored, repo root) with Python:
`zipfile` and raw EOCD parsing, `pyelftools`, `cryptography`, a small AXML decoder, and the dex
reader in `crates/omni-android/tools/gen_dex_surface.py`. The runtime picks its APK at run time
(`omni_apk::choose_apk`: `--apk`, else `OMNI_APK`, else the root's `*.apk` with the highest `versionCode`).

An earlier fixture of the same name was a third-party re-signed build (trojanised `libzstd-jni`,
injected `classes4.dex`, `assets/gloop/`); it was replaced by this one. The other 10 arm64
libraries, `libroblox.so` included, are byte-identical in both, so `libroblox.so` figures
measured on the old file stay valid.

## 0. Method

Direct reads of the file; nothing inferred. The APS2 decoder was validated by consuming all
2,100,778 bytes of `libroblox.so`'s `DT_ANDROID_RELA` blob and producing exactly the 568,272
relocations its header declares.

## 1. APK structure

### 1.1 Container

| Property | Value |
|---|---|
| Size, sha256 | 229,466,269 B, `bbe00ae306cc251c4ea55b7a932d9c524ecb0d6d9203c2a6161bcf0fae792742` |
| Entries | 2,382: 950 STORED, 1,432 DEFLATED |
| Total uncompressed / compressed | 433,379,038 / 229,037,710 B |
| Central directory | offset 229,240,832, 225,415 B; EOCD at 229,466,247, no comment |
| ZIP64, data descriptors | none; 0 entries with flag bit 3 |

The old fixture had a `PK\x06\x06` false positive inside compressed data, which is why
`omni-apk` (`zip.rs`) looks for a zip64 locator only immediately before the EOCD.

### 1.2 Top level

`lib/` 33 files (§2), `assets/` 594 (§8), `classes.dex`/`classes2.dex`/`classes3.dex` (22.2 MB,
DEFLATED), `resources.arsc` (STORED), `res/` 1,511, `META-INF/` 178, `AndroidManifest.xml`.

### 1.3 Alignment

Payload offsets, exclusive buckets:

| Set | 16 KiB | 4 KiB | 4-byte | unaligned |
|---|---:|---:|---:|---:|
| 950 STORED | 0 | 2 | 948 | 0 |
| 1,432 DEFLATED | 0 | 1 | 378 | 1,053 |

Local-header extra-field lengths `{0: 1665, 1: 244, 2: 235, 3: 238}` (zipalign `-f 4`). Every
`.so` is DEFLATED, so libraries are inflated before mapping (D11), never mapped from the APK.

### 1.4 Signing

No v1/JAR signature. v2 (`0x7109871a`) and v3 (`0xf05368c0`): `CN=Matt Critelli, OU=Mobile,
O=Roblox Corporation, L=San Mateo`, RSA-1024, cert sha256
`44932ea35a17a267372d71b54d1a0cb3da0dca5113e94406ae2fe18090ba1477`. Also present: v3.1
(`0x1b93ad61`, rotated key `CN=Roblox Corporation, OU=GameEngine`, RSA-4096), a Google Play source
stamp (`0x6dff800d`) and verity padding. Omnidroid never rewrites the APK.

## 2. ABIs and libraries

### 2.1 ABIs

`arm64-v8a`, `armeabi-v7a`, `x86_64`, 11 libraries each. Omnidroid loads only `arm64-v8a`
(`Apk::native_libraries_for_abi("arm64-v8a")`) and runs ARM code only, by design.

### 2.2 arm64-v8a

| Library | Uncompressed | Compressed |
|---|---:|---:|
| `libroblox.so` | 109,193,800 | 46,516,719 |
| `libbacktrace-native.so` | 5,339,704 | 2,078,803 |
| `libzstd-jni-1.5.7-6.so` | 603,960 | 261,877 |
| `librenderscript-toolkit.so` | 394,112 | 129,704 |
| `libeigen_blas.so` | 251,784 | 81,495 |
| `libimage_processing_util_jni.so` | 32,544 | 15,630 |
| `libdatastore_shared_counter.so` | 7,112 | 2,630 |
| `libtrampoline.so` | 5,104 | 1,900 |
| `libsurface_util_jni.so` | 4,896 | 1,756 |
| `libeigen_lapack.so` | 4,032 | 1,338 |
| `libyuv_shared.so` | 3,752 | 1,254 |

`libroblox.so` sha256 `1d96e2c7466779e4bbf2f50c3b5f8ce2fa5263bf6ed19e4b1a7d1b119aa717de`.
No `libc++_shared.so` (§4.2).

## 3. ELF facts (arm64)

All 11 are stripped `ET_DYN` AArch64 with no `DT_TEXTREL`, `PT_TLS`, `STT_TLS`, `DT_INIT`/`DT_FINI`,
`DT_RELR`, `DT_ANDROID_REL` or `R_AARCH64_IRELATIVE`, and max `PT_LOAD` `p_align` 0x4000. Only the
stock zstd has `.note.gnu.property` (BTI|PAC bits); none requests MTE.

### 3.1 Summary

| Library | NDK | relocations | init/fini | dynsym def/undef | `JNI_OnLoad` | `Java_*` |
|---|---|---|---|---|---|---:|
| `libroblox.so` | r28c | APS2 568,272 + 534 PLT | 3,594/3 | 543/565 | yes | 539 |
| `libbacktrace-native.so` | r27-beta1 | 22,578 | 5/2 | 10,994/315 | yes | 10 |
| `libzstd-jni-1.5.7-6.so` | r19 | 420 | 0/2 | 728/32 | no | 148 |
| `librenderscript-toolkit.so` | r28c | 2,065 | 2/2 | 730/89 | no | 4 |
| `libeigen_blas.so` | r28c | 1,688 | 2/2 | 300/45 | no | 0 |
| `libimage_processing_util_jni.so` | r27 | 61 | 0/2 | 8/18 | no | 8 |
| `libdatastore_shared_counter.so` | r25c | 18 | 1/2 | 9/10 | no | 4 |
| `libtrampoline.so` | r28c | 11 | 0/0 | 0/7 | no | 0 |
| `libsurface_util_jni.so` | r27 | 12 | 0/2 | 1/9 | no | 1 |
| `libeigen_lapack.so` | r28c | 7 | 0/2 | 0/4 | no | 0 |
| `libyuv_shared.so` | r28c | 6 | 0/2 | 0/3 | no | 0 |

`libtrampoline.so` is Crashpad's handler trampoline, a PIE executable (`PT_INTERP`, no
`DT_SONAME`) that calls `CrashpadHandlerMain`, which `libroblox.so` exports. The stock zstd has no
`PT_GNU_RELRO`, no `DT_FLAGS` and no build-id; the other ten have RELRO and `DF_BIND_NOW`.

### 3.2 `DT_NEEDED`

`libroblox.so`: `libOpenMAXAL.so`, `libmediandk.so`, `libandroid.so`, `libm.so`, `libOpenSLES.so`,
`libGLESv2.so`, `libEGL.so`, `liblog.so`, `libdl.so`, `libc.so`. OpenSLES and OpenMAXAL supply no
imported symbol but must exist. Stock zstd needs `libm.so`, `libdl.so`, `libc.so`; `libz.so` is
needed only by `libbacktrace-native.so`.

### 3.3 Relocations

`libroblox.so` has no `DT_RELA`; everything is APS2 plus `DT_JMPREL`: 568,194
`R_AARCH64_RELATIVE`, 56 `GLOB_DAT`, 22 `ABS64`, of which 78 are symbolic; 534 `JUMP_SLOT`.
Earlier versions of this file called the 22 `ABS32`; the type is 257, `ABS64`
(`omni-elf/tests/libroblox_golden.rs`). The other ten use `DT_RELA` + `DT_JMPREL`:

| Library | RELATIVE | ABS64 | GLOB_DAT | JUMP_SLOT | total |
|---|---:|---:|---:|---:|---:|
| `libbacktrace-native.so` | 11,325 | 5,906 | 746 | 4,601 | 22,578 |
| `librenderscript-toolkit.so` | 1,265 | 533 | 49 | 218 | 2,065 |
| `libeigen_blas.so` | 1,208 | 381 | 22 | 77 | 1,688 |
| `libzstd-jni-1.5.7-6.so` | 23 | 51 | 9 | 337 | 420 |
| `libimage_processing_util_jni.so` | 43 | 0 | 0 | 18 | 61 |
| `libdatastore_shared_counter.so` | 4 | 0 | 0 | 14 | 18 |
| `libsurface_util_jni.so` | 3 | 0 | 0 | 9 | 12 |
| `libtrampoline.so` | 4 | 0 | 0 | 7 | 11 |
| `libeigen_lapack.so` | 3 | 1 | 0 | 3 | 7 |
| `libyuv_shared.so` | 3 | 0 | 0 | 3 | 6 |

### 3.4 TLS

No ELF TLS. Thread-locals use `pthread_key_*`; `__cxa_thread_atexit_impl` is a WEAK import of
`libroblox.so`; `__emutls_get_address` is not imported.

### 3.5 `libroblox.so` constants

RELRO 5,205,568 B at `0x62dc1c0`. `DT_INIT_ARRAY` `0x67c27a8`, 3,594 pointers; `DT_FINI_ARRAY`
`0x67c2790`, 3. `.eh_frame` 11,550,152 B, 245,117 FDEs. Load span `0x7333c3c`.

## 4. Imported symbols

Full lists in `apk-undefined-symbols.txt`, regenerated from this APK with the original group
names so `tools/os_surface.py`, `tools/init_reach.py` and `omni-elf/tests/all_libraries.rs` parse
it unchanged.

### 4.1 Totals

Union over the 11 libraries: **641** (669 with the old trojanised zstd). `libroblox.so`: **565**.
By provider group: libc generic 354, GLES 74, libm 55, bionic-specific 47, libmediandk 33,
libandroid/libnativewindow 31, EGL 17, libz 8, libdl 6, liblog 5, C++ runtime 4,
libjnigraphics 3, zstd's WEAK `ZSTD_trace_*` hooks 4. Nothing is imported from libvulkan,
OpenSLES, OpenMAXAL, aaudio or camera2ndk.

### 4.2 C++ runtime

Imported: `__cxa_atexit`, `__cxa_finalize`, `__cxa_thread_atexit_impl` (WEAK),
`__gxx_personality_v0`. libc++, libc++abi and libunwind are linked into each library, so the
guest unwinds itself from `.eh_frame` and needs true `dl_iterate_phdr` answers.

### 4.3 Bionic-specific

The 47 without a glibc equivalent include `__errno`, `__sF`, `__stack_chk_guard`/`_fail`,
`__system_property_get`, `__libc_init`, `__register_atfork`, `android_set_abort_message`,
`getauxval`, `arc4random_buf`, `gettid`, `__cmsg_nxthdr` and the `_FORTIFY` `__*_chk` family.
`libroblox.so` reads `ro.arch`, `ro.build.fingerprint`, `ro.build.version.sdk`, `ro.hardware`,
`ro.product.board`, `ro.product.manufacturer`, `ro.product.model`, `ro.soc.manufacturer`.

### 4.4 NDK surface

APK-wide `libandroid`/`libnativewindow` imports, 31 (`libroblox.so` imports 27 of them):

| Family | Symbols |
|---|---|
| AAsset (6) | `AAssetManager_fromJava`, `AAssetManager_open`, `AAsset_close`, `_getBuffer`, `_getLength`, `_openFileDescriptor` |
| AConfiguration (9) | `_new`, `_delete`, `_fromAssetManager`, `_getCountry`, `_getLanguage`, `_getNavHidden`, `_getScreenHeightDp`, `_getScreenSize`, `_getScreenWidthDp` |
| ALooper (7) | `_prepare`, `_forThread`, `_acquire`, `_release`, `_addFd`, `_removeFd`, `_pollOnce` |
| ANativeWindow (9) | `_fromSurface`, `_acquire`, `_release`, `_getWidth`, `_getHeight`, `_getFormat`, `_setBuffersGeometry`, `_lock`, `_unlockAndPost` |

"ANativeWindow (9)" is APK-wide: `libroblox.so` imports five (`_acquire`, `_fromSurface`,
`_getHeight`, `_getWidth`, `_release`); the rest belong to `libsurface_util_jni.so` and
`libimage_processing_util_jni.so`. The seven ALooper symbols are exhaustive (no `_wake`,
`_pollAll` or message API). No library imports `AAsset_read` (only the old trojanised zstd did).

Absent APK-wide: `AInputEvent_*`, `AMotionEvent_*`, `AKeyEvent_*`, `AInputQueue_*`,
`AChoreographer_*`, `ASensor*`, `ANativeActivity_*`, `GameActivity_*`, `vk*`. So input is
GameActivity's buffered model: the statically linked glue reads Java `MotionEvent`/`KeyEvent`
through JNI. `libvulkan.so`, `libaaudio.so`, `libcamera2ndk.so` and `AHardwareBuffer_*` appear only
as strings for `dlopen`/`dlsym`. libmediandk: 23 functions and 10 `AMEDIAFORMAT_KEY_*` data objects.

### 4.5 Other groups

`dl_iterate_phdr` is imported by `libroblox.so`, `libbacktrace-native.so` and two others; `dladdr`
by `libroblox.so` only. `os-surface-inventory.md` classifies `libroblox.so`'s 565 by OS resource.

## 5. JNI and the Java side

### 5.1 Exports

714 `Java_*` across the APK: `libroblox.so` 539, zstd 148, `libbacktrace-native.so` 10, four small
libraries 17. `libroblox.so` also exports `JNI_OnLoad`, `CrashpadHandlerMain` and
`__start/__stop_pb_defaults`, but not `GameActivity_onCreate` or `android_main`. Its entry is
`Java_com_google_androidgamesdk_GameActivity_initializeNativeCode`. Audio is FMOD.

### 5.2 Static vs `RegisterNatives`

The dex declares **700** native methods: 657 match a short `Java_*` name, 5 a long (overloaded)
one, 38 none. The unmatched: `GameActivity` 23 (registered by `libroblox.so` from the table in
`jni-surface-lists.txt` Section F), `AppsFlyer2dXConversionCallback` 7 (dead), `zstd.Zstd` 3 (the
stock zstd has no `JNI_OnLoad`), `org.fmod.MediaCodec` 2, and one each on
`NativeAppBridgeInterface`, `org.webrtc.Logging`, `WebRtcAudioManager`.

### 5.3 Java vs native

Dex: 26,615 classes (648 under `com.roblox.*`), 154,108 method ids, 122,029 strings. Game logic,
renderer, Luau and the app UI (Luau, `UniversalApp.rbxm`) are in `libroblox.so`; Java is a shell.
`RobloxApplication extends android.app.Application`; `startup.ActivitySplash` and
`ActivityNativeMain` extend `com.roblox.client.a`; **`startup.MainGameActivity extends
com.google.androidgamesdk.GameActivity`**. No class extends `android.app.NativeActivity`.

## 6. Manifest

Decoded: `apk-AndroidManifest.decoded.xml`. `com.roblox.client` 3092 / 2.738.1397, minSdk 26,
targetSdk 35. 30 permissions. `<application android:name="com.roblox.client.RobloxApplication"
extractNativeLibs="true" largeHeap="false">`; 75 components; extra processes `:isolate` (ads) and
`:personaIsolate` (KYC). Launcher: alias `LauncherAliasMain` -> `startup.ActivitySplash`.
`startup.MainGameActivity` (singleTask, `configChanges="0xfb0"`) holds the only
`android.app.lib_name` (`roblox`). `glEsVersion 0x30000` required; no Vulkan `uses-feature`.

## 7. Graphics API

EGL + GLESv2 are linked (17 + 74 imports); GLES 3 entry points come through `eglGetProcAddress`
(97 more `gl*` name strings). Vulkan is `dlopen`-only: 0 imports, 593 `vk*` name strings,
`VK_KHR_android_surface`, and `Vulkan: Device %s is emulated, skipping` rejects CPU/virtual
devices. Shader packs (STORED, magic `RBXS`): `shaders_vulkan_mobile.pack` 14,724,671 B (1,364
SPIR-V modules), `shaders_glsles3.pack` 3,964,445 B. Host side: D8, `graphics-spike.md`.

## 8. Assets

### 8.1 Tree

594 entries, 81,209,148 B, 391 STORED. Largest groups: `ExtraContent/models` 25.3 MB,
`shaders` 18.7 MB, `content/fonts` 12.2 MB, `android/textures` 8.6 MB.

### 8.2 File types

`.rbxm` 25 (25.4 MB), `.pack` 2, `.ttf` 69, `.otf` 13, `.dds` 28, `.tex` 12 (KTX1 files with ETC1
payloads, the skybox; see `texture-formats.md`), `.png` 273, `.ktx` 26, `.mesh` 43, `.json` 82,
`.dict` 1 (zstd dictionary), `.rbxl` 3 (places), `ssl/cacert.pem`.

### 8.3 Access path

`rbxasset://` paths (3,066 strings) resolve against `assets/content/` and `assets/ExtraContent/`
via `AAssetManager_open` and `AAsset_getBuffer`/`_getLength`/`_openFileDescriptor`; the last
returns fd + offset + length and works only for STORED entries.

## 9. Startup chain

`GameActivity.onCreate` loads `libroblox.so`; APS2 relocation and RELRO; 3,594 initializers;
`JNI_OnLoad` registers the GameActivity natives; `initializeNativeCode` starts the game thread.
The measured sequence is `jni-surface.md` §8.

## 10. Integrity

`libroblox.so` carries root/emulator/anti-cheat kick codes (`AndroidRootedKick`,
`AndroidEmulatorKick`, `AndroidAnticheatKick`), ten `DisconnectRemoteAttestation*` codes and
`NativeMetaInterface.getDeviceAttestationToken`; no `Hyperion`/`Byfron` string, no packing. Whether
this stock APK passes the join-time integrity check has not been tested in a world yet.
