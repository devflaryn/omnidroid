# Status

The honest capability record. A thing is **Verified** only if it was run and observed. Nothing is
claimed for Linux or macOS, because nothing has been tested there.

Last updated: 2026-09-18

## Platforms

| Target | Status |
|---|---|
| Windows x86-64 | **Active development.** Host capabilities verified; runtime not yet built |
| Linux x86-64 | Not tested. Structural portability only |
| Linux ARM64 | Not tested. Structural portability only |
| macOS ARM64 | Not tested. Structural portability only |
| macOS x86-64 | Not tested. Structural portability only |

## Verified by measurement on this host

| Area | Finding |
|---|---|
| Toolchain | Rust 1.89 msvc compiles and links; MSVC 14.44 present but off `PATH` |
| Host CPU | AVX2, BMI2, FMA, F16C, AES, SHA, CMPXCHG16B; **no AVX-512** |
| Vulkan | Loader 1.4.321; RTX 4060 device API 1.4.325; `VK_KHR_win32_surface` available |
| Native window | Resizable desktop window rendering a Vulkan triangle, with correct swapchain recreation on resize |
| Texture formats | ETC2 and ASTC **unsupported**; BC1/BC3/BC7 supported. Transcoding is mandatory |
| Validation layers | **Absent.** A real use-after-free crashed the driver with no diagnostic |
| Memory: reservation | `MEM_RESERVE` costs 0 bytes of commit, verified to 97.7 TB |
| Memory: commit | Debited immediately on commit, not on touch. 4 GB guest space = 37.25 MB commit |
| Memory: reclamation | `MEM_DECOMMIT` is the only primitive that returns commit charge |
| Memory: placeholders | `MapViewOfFile3` gives real `MAP_FIXED` at **4 KB** base and file-offset granularity (512/512 verified) |
| JIT memory | Dual-mapped section: 162 ns per emit-and-execute cycle versus 2259 ns for `VirtualProtect` |
| Multi-instance | 4 concurrent Vulkan instances independent, about 52 MiB VRAM and 110 to 160 MB host RAM each |
| CPU: identity mapping | Guest VA == host VA verified at a 47-bit address with zero slow-path callbacks; costs one folded SIB base |
| CPU: correctness | dynarmic builds in 49 s; all 202,200 of its test assertions pass; 37/37 hand-encoded A64 checks correct |
| CPU: throughput | About 2.0x native on memory-heavy code, 2.2x on NEON/FP, about 33x on register-bound integer code |
| CPU: fastmem gain | 5,207 Mguest-insn/s fastmem versus 396 through callbacks, a 13.2x difference |
| CPU: cold translation | 0.15 to 0.31 Mguest-insn/s, implying 7 to 25 s to warm a Roblox-sized working set |
| CPU: per-thread cost | 20 to 35 MiB committed per guest thread, code caches not shared between threads |

## Verified about the test APK

`Roblox-2.738.1397.apk` — see `research/apk-analysis.md`. **Note: this APK is cheat-injected and is
not a stock Roblox build** (D6). A stock Play-signed APK is needed before the API surface is frozen.

| Area | Finding |
|---|---|
| ABI | `lib/arm64-v8a/` only, 11 `.so`, all DEFLATED and 4-byte aligned |
| Relocations | `DT_ANDROID_RELA` (APS2) **exclusively**: 568,272 relocations, no `DT_RELA`, no `DT_RELR` |
| TLS | **None.** No `PT_TLS`, no `STT_TLS` anywhere. `pthread_key_*` only |
| Hardening | No ifunc, no BTI/PAC/MTE, no `DT_TEXTREL` |
| Initializers | 3,594 `init_array` entries before `JNI_OnLoad`; RELRO covers 5,205,568 bytes |
| C++ runtime | Statically linked; in-guest unwinder over 11.5 MB `.eh_frame`, needs real `dl_iterate_phdr` |
| Imports | 669 distinct undefined symbols, enumerated in `research/apk-undefined-symbols.txt` |
| Startup | AGDK `GameActivity`, not `NativeActivity` |
| Graphics | Vulkan `dlopen`-only with 1,364 shipped SPIR-V modules; EGL/GLESv2 hard-linked |

## Runtime implementation

Nothing is implemented yet. Research and architecture are complete enough to begin; the boot
milestone ladder in `ARCHITECTURE.md` section 9 is the progress measure.

| Milestone | Status |
|---|---|
| M0 APK parsed, libraries extracted to aligned cache | Not started |
| M1 ELF loaded, all 568,272 relocations applied, symbols resolved | Not started |
| M2 ARM64 function from `libroblox.so` executes | Not started |
| M3 All 3,594 initializers complete | Not started |
| M4 `JNI_OnLoad` succeeds | Not started |
| M5 `initializeNativeCode` runs, surface requested | Not started |
| M6 Vulkan device created through the forwarding layer | Not started |
| M7 First frame presented | Not started |
| M8 Interactive | Not started |

## Open decisions

| # | Decision | Blocked on |
|---|---|---|
| D7 | JNI without a JVM, or is a dex interpreter unavoidable | JNI surface extraction, running |
