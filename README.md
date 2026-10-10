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
cargo build --release -p omnidroid -p omni-mcp
target/release/omnidroid                                  (Windows: target\release\omnidroid)
```

That opens **the omnidroid app**, a full-screen terminal app. It warms an Android as it opens, so
the first launch starts at once, and from its prompt:

| command | does |
|---|---|
| `run <apk> [xN]` | launch N instances of any APK, each in a window titled "Omnidroid" of its own (Tab completes the path; `omnidroid <apk> [xN]` opens the app and does this) |
| `close <#id>... \| all` | close instances (or Delete on the selected row); closing a window closes its instance |
| `warm` / `kill <D#>... \| all` | boot one more Android / stop Androids |
| `shell <D#> <cmd>` | a shell command on an Android |
| `plugins ...`, `settings`, `set <key> <value>` | plugins; the settings, kept in `<app-data>/../app.conf` |
| `mcp on \| off \| register \| status` | the MCP server for coding agents (off by default) |

**Where an instance goes** (`crates/omni-pool`): an APK is known by its bytes, not its package and
version. The same APK file again runs on the Android that has it (one Android user per instance,
each on a display and window of its own); another package goes into a warm Android beside the
others; another build of a package that is running gets an Android of its own, and a build that
is installed but not running is replaced. Three instances of build A and five of build B take
two Androids. An Android that has run no app for `idle_close_minutes` (default 5) is closed, while
`keep_warm` (default 1) Androids stay up. Each window is resized on its own.

**Coding agents** use the app's MCP server: `mcp on` serves it on `http://127.0.0.1:7457/mcp`
(this computer only, from every start of the app) and registers it with Claude Code and Omni
Agent's pi. Any other agent takes that URL, or `omni-mcp --connect <url>` as a stdio server. Its
tools launch any APK (`start_instance {apk, count}`), wait for, list, screenshot, type into and
close instances, and manage the Androids, beside the lab's debugging tools
(`crates/omni-mcp/README.md`). `omnidroid serve [--port <n>]` is the app without its screen.

The one-shot commands (`aosp`, `play`, `which`, `plugins`, `modules`) remain for scripts and for
the app's own use: `omnidroid help commands`.

- `aosp` runs any APK on the real-AOSP path: a real Android userspace boots (or the host's warm
  device is used) and the APK is installed with `pm install` and started from the launcher
  Activity its manifest names.
- `play` runs the APK's native code on the emulation layer (fast, in-process). It has no Java
  runtime, so it serves only the apps whose Java side the engine answers itself.

The launcher knows no app. Per-app behaviour (a sign-in, a link opened once the app is up, the log
lines that say how far it got, default arguments, extra commands) lives in a plugin; see
`plugins/README.md` and `crates/omni-linux/src/app_session.rs`.

## Debugging

- **Screenshots and control**: `play --control <file>` reads commands as lines are appended to a
  file (`screenshot <path>`, `headless on|off`, `status`); the same commands are read from stdin.
  The app's instances each keep a screenshot (`<device dir>.d<display>.png`, the MCP `screenshot`).
- **Headless and no-window runs**: `--headless` runs frames without drawing them; `--no-window`
  renders to an off-screen buffer, for a host with no display (a container, a CI runner).
- **A device's displays** (`crates/omni-linux/src/displays.rs`): its control channel's `#host
  display add|remove|bind|list` add and remove displays while it runs, each in a window of its own;
  `OMNI_MULTI_DISPLAY=1` lifts SurfaceFlinger's two-display ceiling.
- **AI agents**: `crates/omni-mcp` is a Model Context Protocol server. Beside the app's tools it
  sets breakpoints, intercepts functions, calls guest functions with crafted inputs and dumps
  guest libraries from memory. See `crates/omni-mcp/README.md`.

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
