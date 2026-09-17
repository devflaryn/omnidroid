# Omnidroid

A cross-platform desktop runtime that runs ARM64 Android Roblox builds directly, through a
targeted compatibility layer — **not** an Android emulator, and **not** backed by any virtual
machine, container, or hypervisor.

Primary test target: `Roblox-2.738.1397.apk` (arm64-v8a), kept out of version control.

## Status

This project distinguishes **verified** from **planned** everywhere. A feature is only described
as working if it has been run and observed on a real host. Nothing here is marked working on
Linux or macOS until it has actually been tested there; development and testing so far is
Windows x86-64 only.

See `docs/STATUS.md` for the current, honest capability matrix.

## Design constraints (from the project goal)

- No virtualization of any kind: no QEMU, KVM, WHPX, Hyper-V, VirtualBox, VMware, or Android VM.
- Run only the Android surface Roblox actually uses — not a reimplementation of Android.
- Native ARM64 execution on ARM64 hosts; ARM64 → x86-64 binary translation on x86-64 hosts.
- Targets: Windows x86-64, Linux x86-64, Linux ARM64, macOS ARM64, macOS x86-64.
- Vulkan is the primary graphics backend; the renderer is structured so DirectX and Metal
  backends can be added without touching the rest of the runtime.
- A genuinely native, user-resizable desktop window — not a fixed Android resolution.
- Multiple instances run simultaneously with fully isolated process, filesystem, cache, config,
  library, temp, and runtime state.
- Memory is demand-driven. No large fixed per-instance RAM reservation, no dependence on a huge
  pagefile, and unused memory is genuinely reclaimed.
- Performance is a first-class requirement at every layer.

## Documentation

- `docs/ARCHITECTURE.md` — the design (written after research, see below)
- `docs/STATUS.md` — verified vs. planned capability matrix
- `docs/research/` — findings from investigating prior art, the APK, and the host platform
