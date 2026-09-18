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
| Relocations | `DT_ANDROID_RELA` (APS2): 568,272 (568,194 RELATIVE + 56 GLOB_DAT + 22 **ABS64**, type 257 — a **64-bit** store), plus 534 JUMP_SLOT from a separate `DT_JMPREL`, total 568,806. No `DT_RELA`, no `DT_RELR`. All 568,806 applied and verified against mapped memory |
| Imports | 565 undefined symbols in `libroblox.so` — 539 `STT_FUNC`, 23 `STT_OBJECT`, 3 `STT_NOTYPE`, 4 weak; 669 in the union across all 11 libraries. `DT_VERNEED` attributes 407 of the 565 to `libc.so` (345), `libm.so` (56) and `libdl.so` (6); the other 158 are unversioned and the file records no provider for them |
| JNI surface | Only 59 of 233 `JNINativeInterface` slots used; `JavaVM` needs 2; fields are read but never written; all `CallXxxMethod` go via the `...MethodV` slot |
| Dex execution | **Not required.** No reflection, no `dalvik/system/*`, no Java-side HTTP or file I/O; both `RegisterNatives` sites are native-driven |
| Java surface size | 409 members / 104 classes referenced; ~120 members needed for a first frame |
| Thread prerequisite | `TPIDR_EL0` must point at a bionic TLS block with a stack guard at +0x28 before any guest code runs (1,276 of 1,282 reads target that slot) |
| AGDK | Statically linked into `libroblox.so`; 21-slot callback map recovered, 19 of 21 individually verified; `android_main` at 0x2bcc6a4 |
| TLS | **None.** No `PT_TLS`, no `STT_TLS` anywhere. `pthread_key_*` only |
| Hardening | No ifunc, no BTI/PAC/MTE, no `DT_TEXTREL` |
| Initializers | 3,594 `init_array` entries before `JNI_OnLoad`; RELRO covers 5,205,568 bytes |
| C++ runtime | Statically linked; in-guest unwinder over 11.5 MB `.eh_frame`, needs real `dl_iterate_phdr` |
| Startup | AGDK `GameActivity`, not `NativeActivity` |
| Graphics | Vulkan `dlopen`-only with 1,364 shipped SPIR-V modules; EGL/GLESv2 hard-linked |

## Runtime implementation

The boot milestone ladder in `ARCHITECTURE.md` section 9 is the progress measure. Infrastructure
below a milestone is tracked separately, since a milestone only counts when it passes against the
real APK.

**Infrastructure**

"Reviewed" means an independent reviewer verified it and every Critical and Important finding was
fixed and re-verified. "Pending review" means the implementer's tests pass but nothing independent
has confirmed it yet — on this project that distinction has mattered every single time.

| Component | Status |
|---|---|
| Cargo workspace, nine crates | **Done** |
| `omni-platform` virtual-memory seam (Windows) | **Done, reviewed.** Reserve, 4 KB placeholder split, commit, decommit, protect, file-backed map, commit-charge measurement |
| `omni-platform` dual-mapped sections + placeholder coalescing | **Done, reviewed** |
| `omni-platform` Linux / macOS | **Not implemented, and does not pretend to be.** Typed "unsupported on this platform" errors, each naming its intended POSIX call, so a non-Windows build fails immediately rather than misbehaving |
| `omni-apk` — zip reading + 4 KB-aligned extraction cache | **Done, reviewed.** 35 tests. Milestone **M0** |
| `omni-elf` — ELF64 parsing + APS2 packed relocations | **Done, reviewed.** 85 tests |
| `omni-elf` — the loader: map, relocate, resolve, RELRO, `init_array`, `dl_iterate_phdr` state | **Done, pending review.** 129 tests across the crate, 251 workspace-wide. Milestone **M1** |
| `omni-mem` — guest address space + JIT arena | **Done, reviewed.** 87 tests across `omni-mem` and `omni-platform` |
| `omni-cpu`, `omni-android`, `omni-gfx`, `omni-core`, `omni-cli` | Not started |

**Measured, not assumed**

| Property | Measurement |
|---|---|
| Guest address space reservation | 4 GiB costs **0.000 MiB** of commit charge |
| Grown to 1 GiB, written through | **+1026.004 MiB** (+2.004 is page tables at size/512) |
| Everything unmapped, instance closed | back to **+0.000 MiB** — the project's memory requirement, as an assertion |
| Shared read-only file view | 4 MiB costs **+0.008 MiB**, unchanged after reading every byte, so instances share `libroblox.so` text for free |
| Commit granule | 64 KiB. Measured 2414 ns/page at 4 KiB (worse than the 2053 ns VEH fault D10 rejected), 150 ns/page at 64 KiB, against an unavoidable 381 ns first-touch fault |
| JIT arena | Dual-mapped, W+X unrepresentable in the API; a child process storing through the execute pointer dies with `0xC0000005` |
| Partial unmap | Copy-on-write content preserved in survivors, verified in the loader's exact relocation shape; sharing preserved at +0.008 MiB across a partial unmap of a clean 8 MiB view |
| CoW charging | Charged at `protect` time, not write time: +8.020 MiB the instant an 8 MiB view becomes writable, refunded on restore |
| `libroblox.so` extraction | 413 ms once (release), 130 us on a cache hit |
| `libroblox.so` load, end to end | **11.8 ms** release / 77.5 ms debug: map 32 us, bind 1,109 symbols 83 us, relocate 568,806 in 171 windows 11.7 ms, protect 13 us |
| `libroblox.so` loaded, commit charge | **+16.668 MiB** steady **and peak**, against 104.141 MiB mapped file-backed and shared. 11.039 `.bss` + 4.965 RELRO + 0.328 `.data` + page tables. With lazy `.bss`, +5.602 MiB |
| Relocation window | 64 KiB, equal to the commit granule. Transient copy-on-write charge 5.285 MiB, all of it the RELRO region becoming private anyway. 4 KiB costs 9 ms more for no saving; past 64 KiB the curve is flat |
| Loader hostile input | 21 tamper cases each refused with a typed error and zero residue, plus 920 single-byte corruptions of a synthetic library: 447 loaded, 473 refused, 0 panics, 0 leaks, 1.8 s |
| Loader mutation testing | 18 mutations of the loader logic, 15 reverting it and 3 over-correcting it; every one caught by at least one test |
| APS2 decode | 568,272 relocations from 46,184 groups in ~2 ms, consuming 2,100,778 of 2,100,778 bytes |

**Known accounting gap (confirmed):** a pagefile-backed section does not appear in `PrivateUsage`, so
the JIT arena's cost is invisible to per-process commit-charge measurement. It invalidates no recorded
measurement — every commit-charge figure here concerns private memory — but the budgeting diagnostic
cannot see its fastest-growing consumer, since the CPU core commits 20-35 MiB per guest thread. The
arena's mapped size is now reported as a first-class figure alongside private usage.

**Confirmed constraint for the loader:** copy-on-write is charged at `protect` time, not write time.
Protecting an 8 MiB `ReadExecute` view to `ReadWrite` costs +8.020 MiB of commit immediately, before
any byte is written, and is refunded on restore. So relocation must proceed in windows — dropping the
whole 109 MB library to writable would transiently charge 109 MB per instance.

**Known gap:** Windows `unmap` is whole-view-only, so a guest partial `munmap` cannot be serviced by
the platform layer directly. The seam refuses it with a typed error carrying the view extent rather
than over-unmapping, and emulation (unmap the view, re-map the survivors) is owed by `omni-mem`.

| Milestone | Status |
|---|---|
| M0 APK parsed, libraries extracted to aligned cache | **Reached.** All 11 ARM64 libraries extracted into the content-addressed 4 KB-aligned cache and then mapped from it, end to end |
| M1 ELF loaded, all 568,806 relocations applied, symbols resolved | **Reached.** 568,806 relocations applied and read back from mapped memory, RELRO sealed over 5,205,568 bytes, 565 imports enumerated and attributed, 3,594 initializers collected. 16.668 MiB of commit charge per instance |
| M2 ARM64 function from `libroblox.so` executes | Not started |
| M3 All 3,594 initializers complete | Not started |
| M4 `JNI_OnLoad` succeeds | Not started |
| M5 `initializeNativeCode` runs, surface requested | Not started |
| M6 Vulkan device created through the forwarding layer | Not started |
| M7 First frame presented | Not started |
| M8 Interactive | Not started |

## Open decisions

None blocking. Both D5 (CPU backend) and D7 (JNI without a JVM) are resolved.

| # | Decision | Blocked on |
|---|---|---|
