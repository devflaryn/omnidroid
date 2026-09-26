# Omnidroid

Runs the ARM64 Android Roblox engine (`libroblox.so` from the stock `Roblox-2.738.1397.apk`) as a
native desktop app. Not an emulator and no virtualization: the APK's arm64 libraries are loaded
by our own ELF loader, executed by dynarmic (ARM64 → x86-64 translation, or ARM64 on ARM64
hosts), and the Android surface the engine actually uses (bionic, JNI without a JVM, the NDK,
Vulkan/GLES, audio, input, networking) is implemented on the host.

The APK is not in version control; put it in the repo root. Only `lib/arm64-v8a` is used.

## Quick start (Windows)

```text
set OMNIDROID_DYNARMIC_BUILD_DIR=C:\od-build     (a short path; MSVC fails past MAX_PATH)
cargo build --release -p omnidroid
cargo test -p omni-android --release --test gameactivity --no-run
target\release\omnidroid play                     (or: powershell -File tools\play.ps1)
```

Sign in inside the window (Quick Sign-in works), or `omnidroid login` once and then
`omnidroid play --cookie <name>`. `omnidroid help` lists the options. Close the window to
end a session cleanly.

## Hard constraints

- No virtualization of any kind (no QEMU, KVM, Hyper-V, Android VM), no JVM/ART/dex interpreter.
- Only the Android surface Roblox uses, answered faithfully (no faked frames or skipped work).
- Targets: Windows x86-64, Linux x86-64/ARM64, macOS ARM64/x86-64. A platform is claimed only
  after it has been run there (see `docs/STATUS.md`).
- A native, resizable window; instances isolated from each other; memory and CPU on demand
  (no fixed reservation, no reliance on a page file).

## Documentation

| file | what it holds |
|---|---|
| `docs/HANDOFF.md` | current state, how to run, what is open -- start here |
| `docs/STATUS.md` | what works, per component and per platform |
| `docs/ARCHITECTURE.md` | how the runtime is built |
| `docs/DECISIONS.md` | why (D0-D38) |
| `docs/VERIFICATION.md` | how testing has failed here, the rules, and the Global Constraints |
| `docs/ports/` | per-host notes: `windows.md`, `macos.md`, `linux.md` and topic files |
| `docs/briefs/goal-performance.md` | the current goal |
| `docs/research/` | measured facts: the APK, the startup contract (`jni-surface.md` §8), dynarmic, host memory, graphics, prior art |
| `crates/dynarmic-sys/patches/README.md` | the vendored dynarmic patches |
