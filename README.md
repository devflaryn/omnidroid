# Omnidroid

Omnidroid runs any Android app as a native desktop process so you can test and debug it: you
install an APK, start it in a window of its own, drive it, take screenshots, and inspect what the
app is doing at the ARM64 level. Not an emulator and no virtualization: the APK's arm64 libraries
are loaded by our own ELF loader, executed by dynarmic (ARM64 → x86-64 translation, or ARM64 on
ARM64 hosts), and the Android surface the app uses (bionic, JNI without a JVM, the NDK,
Vulkan/GLES, audio, input, networking) is implemented on the host.

The APK is not in version control; pass its path (or put it in the repo root). Only
`lib/arm64-v8a` native code is used.

## Quick start

```text
cargo build --release -p omnidroid
target/release/omnidroid path/to/app.apk                 (Windows: target\release\omnidroid)
```

`omnidroid help` lists every option. Close the window to end a session cleanly, or pass
`--minutes <n>` to bound it.

- `aosp` (the default; `omnidroid <app.apk>` is `omnidroid aosp --apk <app.apk>`) runs any APK on
  the real-AOSP path: a real Android userspace boots (or the host's warm device is used) and the
  APK is installed with `pm install` and started from the launcher Activity its manifest names.
- `play` runs the APK's native code on the emulation layer (fast, in-process). It has no Java
  runtime, so it serves only the apps whose Java side the engine answers itself.
- `which` shows which APK would be chosen without running it.

The launcher knows no app. Per-app behaviour (a sign-in, a link opened once the app is up, the log
lines that say how far it got, default arguments, extra commands) lives in a plugin; see
`plugins/README.md` and `crates/omni-linux/src/app_session.rs`.

## Debugging

- **Screenshots and control**: `--control <file>` reads commands as lines are appended to a file
  (`screenshot <path>`, `headless on|off`, `status`); the same commands are read from stdin.
- **Headless and no-window runs**: `--headless` runs frames without drawing them; `--no-window`
  renders to an off-screen buffer, for a host with no display (a container, a CI runner).
- **Warm device**: `omnidroid aosp --warm` keeps one idle Android up per host; sessions install
  and start the app on it instead of booting again.
- **AI agents**: `crates/omni-mcp` is a Model Context Protocol server. It boots an instance, takes
  screenshots, and sets breakpoints, intercepts functions, calls guest functions with crafted
  inputs and dumps guest libraries from memory. See `crates/omni-mcp/README.md`.

## Features

- **No virtualization.** No QEMU, KVM, Hyper-V or Android VM, and no JVM, ART or dex interpreter.
  The app's native code runs directly on the host, so there is nothing to install beyond Omnidroid
  itself.
- **Faithful Android surface.** Only the Android APIs the app actually uses are implemented, and
  each one answers as Android does: frames are really drawn and work is really done, so what a test
  observes is what the app does.
- **Cross-platform.** Windows x86-64, Linux x86-64 and ARM64, and macOS ARM64 and x86-64. A
  platform is listed as supported only after it has been run there (see `docs/STATUS.md`).
- **Native window.** The app runs in a resizable desktop window of its own.
- **Isolated instances.** Each instance is its own process, with its own window, input, memory and
  storage.
- **On-demand resources.** Memory and CPU are committed as the app uses them, with no fixed
  reservation and no reliance on a page file.

## Documentation

| file | what it holds |
|---|---|
| `docs/STATUS.md` | what works, per component and per platform -- start here |
| `docs/ARCHITECTURE.md` | how the runtime is built |
| `docs/DECISIONS.md` | why (D0-D39) |
| `docs/VERIFICATION.md` | how testing has failed here, the rules, and the Global Constraints |
| `docs/HANDOFF.md` | the running log of recent sessions and open items |
| `docs/ports/` | per-host notes: `windows.md`, `macos.md`, `linux.md` and topic files |
| `docs/research/` | measured facts: the startup contract (`jni-surface.md` §8), dynarmic, host memory, graphics, prior art |
| `plugins/README.md` | writing and installing plugins |
| `crates/omni-mcp/README.md` | the MCP debugging server |
| `crates/dynarmic-sys/patches/README.md` | the vendored dynarmic patches |

The `MORNING-*`, `NIGHT-*` and `briefs/` files are dated working notes; they describe the state
at their date, not the current one.
