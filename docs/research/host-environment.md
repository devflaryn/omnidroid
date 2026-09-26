# Host environment

The Windows development machine, measured 2026-09-18. The dynarmic-sys and omni-cpu benchmarks
quoted elsewhere ran here unless they say otherwise. Other hosts (the 4-core Linux box, the Mac)
are described in `docs/ports/`.

## Machine

| property | value | source |
|---|---|---|
| OS | Windows 11 Pro 10.0.26200 | session |
| CPU | Intel Core i7-13700F (Raptor Lake), 16 cores / 24 threads | `Win32_Processor` |
| RAM | 31.8 GB; commit limit 46.8 GB | `Win32_OperatingSystem` |
| GPU | NVIDIA GeForce RTX 4060, driver 591.86 | `Win32_VideoController`, `vulkaninfo` |
| second adapter | Parsec Virtual Display Adapter 0.45.0.0 (no Vulkan ICD) | `Win32_VideoController` |

The machine is often driven over Parsec; frame pacing measured here may differ from a local display
(see `graphics-spike.md` §4).

## CPU features (for ARM64 -> x86-64 translation)

Measured with `is_x86_feature_detected!` in a release build.

| present | absent |
|---|---|
| SSE4.2, AVX, AVX2, BMI1, BMI2, LZCNT, TZCNT, POPCNT, ADX, F16C, FMA, AES-NI, SHA, PCLMULQDQ, MOVBE, CMPXCHG16B | AVX-512 (F/VL/BW) |

Consequences: x86-64-v3 is the usable baseline; AVX-512 must never be required; `CMPXCHG16B`
(part of x86-64-v2) gives lock-free 128-bit guest atomics (`LDXP`/`STXP`, `CASP`).

## Toolchain (as measured; versions drift)

| tool | version |
|---|---|
| Rust | 1.89.0 stable, `x86_64-pc-windows-msvc` (workspace `rust-version = "1.89"`) |
| MSVC | 14.44.35207, not on `PATH`; the `cc`/`cmake` crates find it |
| Windows SDK | 10.0.19041.0, 10.0.26100.0 |
| CMake / Ninja | 4.4.3 / 1.13.2 |
| Python | 3.11.9 (`python`), 3.14.6 (`python3`, `py`) |
| not installed | clang, gcc, Vulkan SDK |

## Vulkan

Loader `vulkan-1.dll`, instance version 1.4.321; one physical device (RTX 4060, apiVersion 1.4.325,
`DRIVER_ID_NVIDIA_PROPRIETARY`); `VK_KHR_surface` and `VK_KHR_win32_surface` present. No validation
layers are installed. The loader is opened at run time, so building needs no Vulkan SDK.

## Language

These facts back D1 (Rust, with C++ via FFI: Rust and MSVC built and linked with no extra
installs) and D2 (x86-64-v3 baseline, AVX-512 optional).
