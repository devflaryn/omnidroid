# Omnidroid Architecture

Omnidroid runs the arm64-v8a Android Roblox engine (`libroblox.so`) as a desktop process: no VM,
no ART, no dex interpreter. Guest code runs through dynarmic; its imports are host Rust. Why:
`DECISIONS.md`. What has run where: `STATUS.md`.

## 1. The central idea

**A guest pointer is a host pointer** (D4). Guest code and data sit in the host process at their
own addresses: no address translation on the memory path, guest `mmap` is a host reservation plus
lazy commit, and host drivers read guest structs in place. The cost: guest code can corrupt the
host (D4 amendment 1), so each instance is its own OS process (§7).

## 2. Module structure

Dependencies point downward. Only `omni-platform` may use `cfg(target_os)` or an OS crate; all
else compiles for the five targets without `cfg`. A primitive that is one portable `std` call is
written once, with no backend and no fake `Unsupported` arm (D22, D23).

| Crate | What it is |
|---|---|
| `omni-platform` | OS seams, backends `windows.rs`/`linux.rs`/`macos.rs` (`unix.rs` shared or structural): `vm`, `fault` (Mach ports on macOS arm64 only), `fs` (rooted), `net` (behind a `NetPolicy`), `process`, `clock`, `log`, `window` (Win32, Xlib, AppKit), `audio` (WASAPI, ALSA, Core Audio), `webview` (WebView2, WKWebView; Linux structural), `sampler`, `hypervisor` |
| `omni-mem` | `GuestSpace` (region map, 64 KiB commit granules, partial-unmap emulation, mapping labels), `CodeArena` (D12), `CommitBudget`, the demand pager, `access` |
| `omni-apk` | zip, the extraction cache (§3), manifest, `choose_apk` |
| `omni-elf` | ELF64 parsing, APS2, `.eh_frame_hdr` function map, the loader (§4) |
| `omni-cpu` | `GuestCpu` trait; `dynarmic` backend; `native` backend (feature `native-hvf`) (§6) |
| `dynarmic-sys` | vendored dynarmic `9d45823`, CMake build, C shim, FFI, `patches/` 0001-0028 (no 0023) |
| `omni-bionic` | pure libc/libm and pthread logic over a `GuestMemory` trait; zero dependencies (D19) |
| `omni-android` | the compatibility layer (§5): thunk crossing (`region`, `abi`, `varargs`, `mem`, `boundary`), `bionic`, `jni`, `ndk`, `vulkan`, `gles`, `aaudio`; instruments `perf`, `memreport`, `waits`, `pacing` |
| `omni-gfx` | host graphics: `GfxVulkanHost`, `GfxGlesHost`, a test `Renderer`, MoltenVK loading |
| `omni-texture` | ETC1 to RGBA8 (D27), `no_std`; used by `omni-gfx`'s renderer, not the guest path |
| `omni-core`, `omni-cli` | empty placeholders |
| `omnidroid` | launcher binary: `play`, `login`, `which` |

**The runtime is assembled in the M5 gate test**, `crates/omni-android/tests/gameactivity.rs`;
`omnidroid play` picks the APK and account and runs it with `cargo test --release`. No library
crate does this yet.

## 3. APK handling and the extraction cache

The APK is chosen at run time: `--apk`, else `OMNI_APK`, else the `*.apk` in the repository root
with the highest `versionCode` (`omni_apk::choose_apk`). Only `lib/arm64-v8a` is used; the other
ABIs are ignored by design.

Libraries are DEFLATED and unaligned in the zip, so each is extracted once to
`<cache>/libs/<sha256>/<name>.so` (content-addressed, read-only) and mapped file-backed
`ReadExecute`: `libroblox.so`'s ~100 MiB of text is shared by all instances; relro, `.data` and
eager `.bss` (~16.4 MiB) are private (D11, D14). Assets are read from the APK on demand.

## 4. ELF loading

A bionic-compatible loader (D3, D9), built on these facts of 2.738.1397's `libroblox.so`:

1. Relocations are APS2: 568,272 (568,194 RELATIVE, 56 GLOB_DAT, 22 ABS64) plus 534 JUMP_SLOT
   via `DT_JMPREL`, 568,806 in all. No `DT_RELA`, no `DT_RELR`.
2. Relocation runs in 64 KiB windows: copy-on-write is charged when a view turns writable (D11).
3. `p_align` is 16 KiB; a library aligned below the host page is refused (`AlignBelowPageSize`).
4. `DT_GNU_HASH` only; 565 imports, `DT_VERNEED` names a provider for 407.
5. RELRO (5,205,568 bytes, holding `DT_PLTGOT`; `DF_BIND_NOW`) is sealed after all relocation,
   its end rounded up as bionic does.
6. All 3,594 `init_array` entries run in order, read from relocated memory.
7. `dl_iterate_phdr` is faithful: the statically linked C++ unwinder finds 11.5 MB of `.eh_frame`
   through it (`bionic/dl.rs`).
8. Absent from the APK, so not implemented: ELF TLS, ifuncs, BTI/PAC/MTE, `DT_TEXTREL`.
9. `dlopen` answers for the libraries this layer provides and loads no other file.

## 5. The Android compatibility layer

**The import list is the specification.** Each of `libroblox.so`'s 565 imports
(`research/apk-undefined-symbols.txt`) gets a 16-byte slot in a reserved thunk region, bound by
the loader through a `SymbolProvider`; an unimplemented slot refuses by name. The engine imports
no allocator (its mimalloc sits on guest `mmap`), so the heap seam is the demand pager.

**The crossing** (D17, D18): a branch to a slot is served inside the run loop (~27-31 ns).
Handlers that call guest code (thread entry, `atexit`, `qsort`, `dl_iterate_phdr`) exit to Rust
(~80-100 ns) and re-enter with a sentinel return address. `abi`/`varargs` implement AAPCS64 and
`va_list`; `mem` checks every guest pointer.

**bionic**: 322 bound symbols (`tests/bionic.rs`); guest threads are host threads, each with its
own `GuestCpu` (D24); rooted files (D23); real sockets (D30); synthesized `/proc`; raw `SVC #0`
routed to the matching import (`sysroute`). `AT_HWCAP` declines LSE (D26). No signal delivery:
`sigaction`, `raise`, `pthread_sigmask` refuse by name.

**JNI without a JVM** (D7, D28): `JavaVM`/`JNIEnv` tables in guest memory; of 233 + 8 slots,
59 + 2 are implemented and the rest refuse by name (`jni/slots.rs`). The Java side is transcribed:
`jni/surface.rs` (98 classes, 1,526 members, generated from the dex) and answers in
`jni/classes.rs`. A missed lookup returns null with a pending exception; an untranscribed call
refuses.

**The Java side is the initiator**: the engine waits for flags, settings, directories and
`InitParams`, so `jni::script` drives that sequence (`jni-surface.md` §8), then GameActivity
(AGDK, statically linked). `jni` also holds input, cursor, the app's cookie store, settings and
the WebView bridge. **NDK** (`ndk`, D29): `ALooper`, `AAssetManager`, `AConfiguration`,
`ANativeWindow`. **Audio**: `aaudio` implements `libaaudio.so` for FMOD on the host output.

## 6. ARM64 execution

- **dynarmic** (D5), on every host. Identity fastmem is asserted at startup and checked per run
  slice (D4 amendment 2); losing it costs 30-49x. Value-compare exclusives (D31); fast dispatch
  and the return-stack buffer kept, with budget/halt checks (D33, D35; fast dispatch off on
  arm64). **One translation cache per instance** is the x86-64 default (D38; 16 MiB regions,
  256 MiB live); arm64 keeps per-thread caches. Only executable ranges invalidate translations.
- **native** (macOS arm64, Hypervisor.framework): guest at EL0, stage 2 at IPA == VA. Faster
  compute, but each import is a VM exit (~1.6 us); not adopted (D34, `ports/macos-hvf.md`).

No native backend exists for Linux ARM64. `TPIDR_EL0` must point at a bionic TLS block with the
stack guard at +0x28 before any guest code runs (D13). Runaway guests stop via `HaltHandle` (D16).
`CNTVCT_EL0` reads are translated (patch 0001); an instruction the pin cannot translate ends the
slice with `ExitReason::UnsupportedInstruction` (`SVC #0` is served there as a syscall).

## 7. Instance isolation

**One OS process per instance**, with its own window, input, guest space and storage; only the
read-only extraction cache is shared. Guest paths (`/data/data/com.roblox.client/...`) resolve
only inside one host data directory (per account under `play --cookie`); an escaping path is
refused (D23).

Memory (D10): a 16 GiB guest reservation, commit in 64 KiB granules on touch, decommit to
reclaim (`MEM_DECOMMIT`; a fresh `MAP_FIXED` mapping on Linux and macOS). The device's RAM, which
is also the commit ceiling, is 60% of host RAM in whole GiB, at most 8 GiB, or
`OMNI_GUEST_MEMORY_MB` (D36). `OMNI_MEM_REPORT` prints memory per owner.

## 8. Graphics

**Vulkan is forwarded** (D8). The engine `dlopen`s `libvulkan.so` and resolves everything via
`vkGetInstanceProcAddr`; `omni_android::vulkan` forwards each call to `omni-gfx`'s
`GfxVulkanHost`, guest structs read in place, guest memory imported as host-visible memory.
`VK_KHR_android_surface` becomes win32, xlib or metal (MoltenVK). The engine refuses a device it
thinks emulated, so a CPU rasterizer (lavapipe) sends it to GLES.

**EGL and GLES are forwarded** (`omni_android::gles`): ES 2.0-3.2 and EGL, signatures generated
from the Khronos registries (`tools/gen_gles_signatures.py`). The only host row is X11 (Mesa or
GLVND); Win32 (ANGLE) and macOS are typed refusals. With no window, `Gles::set_driverless`
answers as Android's `libEGL` does without a driver.

`ANativeWindow` reports the native window's real size. A minimised window answers the surface
query with `VK_ERROR_SURFACE_LOST_KHR`, so the engine skips drawing. `OMNI_FPS_CAP` paces presents
(`pacing`). The APK's compressed textures are ETC1 only (D27).

## 9. Testing strategy

Progress is a ladder of milestones, each passed against the real `libroblox.so`: M0 APK parsed
and libraries cached; M1 ELF loaded, 568,806 relocations applied; M2 a real function returns known
outputs; M3 all 3,594 initializers, asserted by `(index, address)` sequence; M4 `JNI_OnLoad`
returns `0x00010006`; M5 `initializeNativeCode` returns and the game thread runs; M6 the engine
creates its Vulkan device through the forwarding layer; M7 first frame; M8 interactive.

The gate runs the whole sequence and, in a session, the game. Tests use golden data from the
real binary, assert measured memory, fail when the APK is absent, and are checked by mutation
(`tools/mutate.py`). Rules: `VERIFICATION.md`.

## 10. Known risks

| Risk | Why it matters | Mitigation |
|---|---|---|
| No isolation inside a process | guest code can corrupt the host (D4 amendment 1) | one process per instance |
| Untranscribed Java methods | a call to one refuses and kills that thread; the game can freeze | `research/jni-audit-2.739.md`; transcribe from the dex |
| dynarmic arm64 backend | stale-code transfer and freezes on the Mac (m7, m9, m11) | patch 0023 in progress |
| Memory per instance | ~2.5 GiB against a 0.8-0.9 GiB target for many instances | HANDOFF's plan; `OMNI_MEM_REPORT` |
| Untested targets | Linux ARM64 and macOS x86-64 never built or run | no claims until run |

Resolved: the CPU backend (D5), the JNI surface size (D7, D28), APS2 decoding (M1), and the
modified test APK: the repository now holds the stock `Roblox-2.738.1397.apk` (D6).
