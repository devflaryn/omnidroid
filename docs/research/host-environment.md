# Host Environment — Verified

All facts below were measured on the development machine on 2026-09-18, not assumed.
Method is given for each so they can be re-verified.

## Machine

| Property | Value | How verified |
|---|---|---|
| OS | Windows 11 Pro 10.0.26200 | session environment |
| CPU | Intel Core i7-13700F (Raptor Lake) | `Win32_Processor` |
| Cores / threads | 16 physical / 24 logical | `Win32_Processor` |
| RAM | 31.8 GB total, ~14.2 GB free at probe time | `Win32_OperatingSystem` |
| Commit limit | 46.8 GB (RAM + pagefile) | `TotalVirtualMemorySize` |
| GPU | NVIDIA GeForce RTX 4060, driver 591.86 | `Win32_VideoController`, `vulkaninfo` |
| Secondary display adapter | Parsec Virtual Display Adapter 0.45.0.0 | `Win32_VideoController` |

The Parsec virtual adapter means the machine is likely driven over remote desktop at least
some of the time. It exposes **no** Vulkan ICD, so Vulkan device enumeration returns exactly
one device. Presentation and vsync behaviour may differ from a locally-attached display;
frame-pacing measurements taken here are suspect until confirmed on a local display.

## Host CPU features relevant to ARM64 → x86-64 translation

Measured with `is_x86_feature_detected!` in a release Rust binary.

| Feature | Present | Why it matters for an AArch64 JIT |
|---|---|---|
| SSE4.2 | yes | baseline NEON lowering |
| AVX / AVX2 | yes | 128-bit NEON maps to xmm; VEX encoding avoids false dependencies and gives 3-operand forms |
| AVX-512 (F/VL/BW) | **no** | must not be required; no mask-register tricks for NEON predication |
| BMI1 / BMI2 | yes | `SHLX`/`SHRX`/`SARX`/`RORX` shift without clobbering flags — directly serves AArch64 shifted-register operands, which are extremely common |
| LZCNT / TZCNT | yes | `CLZ` / `RBIT`+`CLZ` lowering |
| POPCNT | yes | `CNT` lowering |
| ADX | yes | multi-precision add chains |
| F16C | yes | AArch64 fp16 conversion |
| FMA | yes | `FMADD`/`FMLA` without double rounding |
| AES-NI, SHA, PCLMULQDQ | yes | ARMv8 crypto extension instructions map near 1:1 |
| MOVBE | yes | byte-swap loads |
| CMPXCHG16B | yes | **required** for AArch64 128-bit atomics (`LDXP`/`STXP`, `CASP`) |

Design consequences:
- Target baseline is **x86-64-v3** (AVX2 + BMI2 + FMA + F16C + LZCNT + MOVBE). AVX-512 is an
  optional fast path only, never a requirement.
- `CMPXCHG16B` being present makes 128-bit guest atomics implementable without a lock; it is
  mandatory on any x86-64 host we support (it is part of x86-64-v2, so this is safe).
- 24 logical cores means background/parallel JIT compilation is worth building in from the start.

## Toolchain

| Tool | Version | Notes |
|---|---|---|
| Rust | 1.89.0 stable, host `x86_64-pc-windows-msvc` | **verified: `cargo build` compiles and links successfully** |
| rustup toolchains | stable + nightly + nightly-2025-02-01 + 1.88.0 (msvc & gnu) | nightly available if ever needed |
| rustup targets installed | `x86_64-pc-windows-msvc`, `x86_64-pc-windows-gnu`, `i686-linux-android`, `x86_64-linux-android` | note: `aarch64-*` targets are **not** installed yet |
| MSVC | 14.44.35207 (`cl.exe`, `link.exe`, `lib.exe`, `dumpbin.exe`) | at `C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207` — **not on `PATH`**; `cl` is unavailable in a plain shell |
| Windows SDK | 10.0.19041.0 and 10.0.26100.0 | |
| CMake | 4.4.3 | |
| Ninja | 1.13.2 | |
| Python | 3.11.9 (`python`) and 3.14.6 (`python3`, `py`) | used for ELF/AXML parsing during APK analysis |
| Java | OpenJDK 21.0.4 LTS | `javap` available for dex-adjacent inspection |
| Node | 24.11.0 | |
| Not installed | `clang`, `gcc`, `zig`, `7z` standalone, Vulkan SDK | |

`cl.exe` exists but is not on `PATH`; anything needing it must source `vcvars64.bat` or be
driven through the `cc`/`cmake` crates, which locate MSVC themselves.

## Vulkan

| Property | Value |
|---|---|
| Loader | `C:\Windows\System32\vulkan-1.dll` present, instance version **1.4.321** |
| `vulkaninfo.exe` | present in System32 |
| Physical devices | exactly 1 — RTX 4060, `apiVersion` 1.4.325, `DRIVER_ID_NVIDIA_PROPRIETARY`, conformance 1.4.3.0 |
| Instance extensions | 20, including `VK_KHR_surface`, `VK_KHR_win32_surface`, `VK_EXT_debug_utils`, `VK_KHR_get_physical_device_properties2` |
| Validation layers | **not installed** — loader reports "Registry lookup failed to get layer manifest files" |

Consequences:
- No Vulkan SDK is needed to *build*: the loader is loaded dynamically at runtime, so a Rust
  Vulkan binding that dlopens `vulkan-1.dll` needs no SDK, headers, or import library.
- `VK_KHR_win32_surface` is present, so real windowed presentation is available.
- **Validation layers are unavailable.** Installing the Vulkan SDK (or just the validation layer
  package) is the single most valuable optional install for graphics debugging. Until then,
  graphics bugs must be diagnosed without validation output — worth flagging early.

## Language choice ruling

**Rust is the implementation language for Omnidroid**, with C/C++ reachable via FFI if a specific
reusable component justifies it.

Rationale:
- It is the only systems toolchain on this machine verified end-to-end (compiles *and* links) with
  zero additional installs.
- The portability requirement (Windows/Linux/macOS × x86-64/ARM64) is served well by Cargo's
  target model and by `cfg(target_os)`/`cfg(target_arch)` at the backend seams.
- Runtime-critical crates exist and are permissively licensed: Vulkan bindings, native windowing,
  memory mapping, ZIP reading.
- The JIT and guest-memory layers need raw pointers and hand-written machine-code emission; Rust
  permits this in localized `unsafe` blocks while keeping the surrounding runtime (loader, object
  model, resource tracking) memory-safe. That containment is valuable in a codebase whose whole
  job is running untrusted-to-us foreign binaries.
- MSVC is present, so a C++ dependency remains possible via the `cc`/`cmake` crates should
  research identify one worth the integration cost.

Cost if wrong: switching languages later is expensive. The mitigation is that the module
boundaries described in the architecture document are language-agnostic, and the components most
likely to be borrowed from C++ (a guest CPU JIT) sit behind a narrow trait boundary.
