# Prior art

Survey of projects that run ARM64 Android code on desktop x86-64 without a VM or emulator, made
2026-09-18 before the design (evidence for D3). Repos were shallow-cloned and read unless marked
"docs only".

## Summary

| project | what / direction | licence | use for Omnidroid |
|---|---|---|---|
| dynarmic | A64 and A32 guest -> x86-64 or arm64 host JIT | ISC / 0BSD | **Used** (vendored, D5) |
| Sober (vinegarhq) | closed Roblox-on-Linux runtime, x86-64 only | proprietary; repo is an issue tracker | none, no source |
| open-sober (0x06cf) | wraps `qemu-aarch64` + a bionic shim; most modules "planned" | MIT | none; an AI-agent scaffold |
| sober-oss (Z3ki) | claimed RE of Sober; its own evidence files do not support it | MIT | none |
| Berberis (AOSP) | riscv64 -> x86-64; no ARM64 frontend in the public tree | Apache-2.0 (AOSP default) | none |
| Digitalis | Berberis-based ARM64 backend inside an AOSP build (meta-repo only) | Apache-2.0 | none, not embeddable |
| FEX, box64/box86, Rosetta 2, ARM64EC, hangover | x86 guest on ARM host (wrong direction) | MIT / closed / LGPL-2.1 | design reference only |
| libhybris | bionic <-> glibc bridging, same arch | mixed per file (Apache, BSD, LGPL, GPL3...) | technique reference (`hybris/common/hooks.c`) |
| android_translation_layer | real APKs (ART + native) on Linux, same arch | GPL-3.0-or-later | architecture reference only |
| waydroid, anbox | full Android in a container | GPL-3.0 / GPL | out of scope |
| ANGLE, gfxstream, virglrenderer, Zink, MoltenVK, DXVK, vkd3d-proton | graphics API translation | BSD / Apache / MIT / zlib / LGPL | pattern reference (docs only) |

## dynarmic

Upstream `merryhime/dynarmic` 404s; the vendored pin is the `yuzu-mirror` copy
(`crates/dynarmic-sys/vendor/PIN.txt`). Frontends `A64/` and `A32/`, backends `x64/` and `arm64/`.
Declared externals: fmt, mcl, oaknut (arm64 host only), xbyak, zydis, robin-map (biscuit for
riscv64). All permissive.

The survey missed that dynarmic also needs **Boost** (icl, variant); see `PIN.txt` and D5. It solves
CPU translation only: ELF loading, bionic, JNI and graphics were ours to write.

## What no prior art covers

No permissively licensed standalone bionic-compatible ELF loader exists (libhybris and
android_translation_layer are copyleft or mixed), nor a JNI/NDK/asset/looper layer usable without an
Android OS. Omnidroid wrote both clean-room (`omni-elf`, `omni-bionic`, `omni-android`).

## Open questions from the survey, since answered

- Roblox Android renders through Vulkan with a GLES path; `libroblox.so` imports `libGLESv2.so` and
  `libEGL.so` (`apk-analysis.md`). Omnidroid forwards Vulkan (D8).
- dynarmic on real Roblox code: measured in D5 and its amendments.
- How Sober works internally remains unknown; the two "RE" repos are not evidence.
