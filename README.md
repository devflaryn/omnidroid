# Omnidroid

Omnidroid runs any Android app as a native desktop process so you can test and debug it: you
install an APK, start it in a window of its own, drive it, take screenshots, and inspect what the
app is doing at the ARM64 level. Not an emulator and no virtualization: the APK's arm64 libraries
are loaded by our own ELF loader, executed by dynarmic (ARM64 → x86-64 translation, or ARM64 on
ARM64 hosts), and the Android surface the app uses (bionic, JNI, the NDK, Vulkan/GLES, audio,
input, networking) is implemented on the host.

APKs are not in version control; you give the app their paths. Only `lib/arm64-v8a` native code
is used.

## Getting started

```text
cargo build --release -p omnidroid -p omni-mcp
target/release/omnidroid                                  (Windows: target\release\omnidroid)
```

That opens **the omnidroid app**, a full-screen terminal app (Windows console and Unix terminals
alike) where you run and manage everything. It starts warming an Android as it opens, so your
first launch starts at once. Type at its prompt:

```text
run ~/apks/TalkingTom.apk           one instance, in a window of its own
run ~/apks/TalkingTom.apk x3        three of them
run ~/apks/Game.xapk                a bundle: base APK, splits and OBB files installed together
run ~/apks/apps.zip --package com.a one package of a bundle that holds several
close #2                            close an instance
help                                everything else
```

`omnidroid <apk> [xN]` opens the app and launches the APK straight away.

**Bundles** (`.apkm`, `.xapk`, `.apks`, or any zip of APKs) run like an APK: each APK in the file
says in its manifest which package and split it is, and a package's base APK and its splits are
installed in one `pm install` (splits for ABIs other than arm64-v8a are left out); an `.xapk`'s
OBB files go to the app's OBB directory. A bundle that holds several packages needs one named:
`--package <name>` (at the prompt, on the command line, or `package` over MCP; the error lists
them, and MCP's `apk_info` shows them first).

## The app

The screen shows four panes:

- **Androids**: the Android devices in the pool (`D1`, `D2`, ...). Each row shows the state
  (booting, ready, closing), uptime, idle time, apps installed and instances running.
- **Instances**: every app instance (`#1`, `#2`, ...). Each row shows the package, the Android
  and Android user it runs as, its display, its state (waiting, installing, starting, running,
  ended) and its uptime.
- **Log**: what the pool is doing, and the output of your commands.
- **Command line**: where you type commands.

Each instance runs in a desktop window titled "Omnidroid" of its own, and you can resize it.
Closing that window closes the instance.

### Commands

| command | does |
|---|---|
| `run <apk> [xN]` | launch N instances (1 to 64) of any APK; Tab completes the path, and a path typed alone runs it |
| `run <apk> [xN] --<option> <value>...` | launch with a plugin's options (see [Plugins](#plugins)) |
| `close <#id>... \| all` | close instances |
| `warm` | boot one more Android now |
| `kill <D#>... \| all` | stop Androids |
| `shell <D#> <cmd>` | run a shell command on an Android, its output in the log |
| `plugins [list \| add [--link] <dir> \| remove \| enable \| disable <name>]` | manage plugins |
| `settings`, `set <key> <value>` | list and change the settings |
| `mcp on \| off \| register \| status` | the MCP server for coding agents (off by default) |
| `clear` | forget ended instances |
| `help`, `quit` | |

Short forms: `r` (run), `c` (close), `w` (warm), `k` (kill), `sh` (shell), `h` or `?` (help),
`q` (quit).

### Keys

| key | does |
|---|---|
| Tab | complete the APK path being typed; on an empty line, switch between Androids and Instances |
| Up / Down | select a row |
| Delete | close the selected instance, or stop the selected Android |
| Ctrl+P / Ctrl+N | previous / next command in the history |
| Ctrl+U, Esc | clear the line |
| Ctrl+C | quit; if that would stop running instances, press it (or `quit`) again within 10 s |

### Settings

Settings are kept in `app.conf` beside the app's data:

- macOS: `~/Library/Application Support/Omnidroid/`
- Linux: `~/.local/share/omnidroid/`
- Windows: `%LOCALAPPDATA%\Omnidroid\`

`settings` lists them and `set <key> <value>` changes one.

| key | default | meaning |
|---|---|---|
| `idle_close_minutes` | `5` | close an Android that has run no app this long; `0` never |
| `keep_warm` | `1` | idle Androids kept warm anyway, so the next launch starts at once |
| `warm_on_start` | `true` | warm an Android when the app opens |
| `stop_on_exit` | `true` | stop every Android when the last app window quits |
| `max_devices` | `0` | the most Androids at once; `0` no limit |
| `instance_size` | `1280x720` | a new instance's display and window |
| `gpu` | `auto` | the Androids' GPU backend: `auto`, `vulkan` or `gl` |
| `device_ram_mb` | `0` | each Android's RAM in MiB; `0` the default |
| `mcp_enabled` | `false` | serve MCP for coding agents |
| `mcp_port` | `7457` | the MCP server's port (127.0.0.1 only) |

### Where an instance goes

The app recognises an APK by its file contents, not by its package name or version.

- **The same APK again** runs on the Android that already has it. Each instance gets its own
  Android user, display and window.
- **Another package** goes into a warm Android next to the ones already running.
- **Another build of a package that is running** gets an Android of its own.
- **Another build of a package that is installed but not running** replaces the installed one.

So three instances of build A and five of build B take two Androids.

An Android that has run no app for `idle_close_minutes` is closed, but `keep_warm` Androids stay
up. If you stop the last Android with `kill`, no other is warmed in its place. The app also opens
with none next time, until you `run` or `warm`.

### One pool per computer

Every `omnidroid` window open on a computer shows the same pool.

- The first window starts the pool in a background process of its own (`omnidroid host`).
- A second window joins that pool: the same Androids, instances, log and MCP server. It boots
  nothing.
- Quitting one window while another is open leaves everything running.
- When the last window quits, the Androids stop (unless `stop_on_exit` is `false`; then the next
  window adopts them).

`omnidroid serve [--port <n>]` is the app without its screen: the pool and its MCP server, with
the log on stdout. Use it for a machine with no terminal to show the app on. Windows you open
meanwhile join it.

## Plugins

The launcher itself knows no particular app. Behaviour for one app lives in a plugin: a sign-in,
a link opened once the app is up, the log lines that show how far it got, default arguments and
extra commands. `plugins` lists the installed ones and their options; `plugins/README.md`
explains how to write one.

A plugin's options work on every launch. For example, with the `roblox` plugin:

- At the prompt: `run <apk> x2 --cookie <c> --place <id>`
- From the shell: `omnidroid <apk> x2 --cookie <c> --place <id>`
- Over MCP: `start_instance` with `options: "--cookie <c> --place <id>"`

Each instance signs in as that account, then joins the place. Each instance's cookie is planted
for its own Android user, so two accounts can run side by side on one Android.

## Coding agents (MCP)

`mcp on` serves the app's MCP server at `http://127.0.0.1:7457/mcp`. It is reachable from this
computer only, starts with every launch of the app, and is registered with Claude Code and Omni
Agent's pi. Any other agent can use that URL directly, or `omni-mcp --connect <url>` as a stdio
server. Agents drive the same pool you see in the app.

The tools fall into four groups:

- **Instances**: launch any APK or bundle (`start_instance {apk, package, count, options}`;
  `apk_info` says what a file holds); wait for, list,
  screenshot, type into and close instances; manage the Androids; run shell commands.
- **WebViews**: the `web_*` tools drive the web pages an app shows in its WebViews (sign-in
  forms, captchas), as a DevTools client does. `web_dom` lists every frame's elements with the
  display point that taps each, `web_eval` runs JavaScript in any frame, `web_click` clicks by
  element (`ref`, CSS selector or text), `web_tap` taps by position, `web_type` types, and
  `web_screenshot` captures the page.
- **Debugging**: set breakpoints, intercept functions, call guest functions with crafted inputs,
  read and write guest memory, trace syscalls, and dump guest libraries from memory.
- **Lab**: build, load and trace code against the runtime.

`crates/omni-mcp/README.md` describes every tool.

## Features

- **No virtualization.** No QEMU, KVM, Hyper-V or Android VM. The app's native code runs directly
  on the host, so there is nothing to install beyond Omnidroid itself.
- **Any APK, many at once.** Instances share warm Androids, so a second launch takes seconds, not
  a boot.
- **Faithful Android surface.** Each API answers as Android does: frames are really drawn and work
  is really done, so what a test observes is what the app does.
- **Cross-platform.** Windows x86-64, Linux x86-64 and ARM64, and macOS ARM64 and x86-64.
- **Native windows.** Each instance runs in a resizable desktop window of its own.
- **Isolated instances.** Each instance has its own Android user, display, window, input and
  storage.
- **On-demand resources.** Memory and CPU are committed as the apps use them, with no fixed
  reservation and no reliance on a page file.

## For scripts

The app is the way to use Omnidroid. Behind it, one-shot commands (`aosp`, `play`, `which`,
`plugins`, `modules`) remain for scripts and for the app's own use. `omnidroid help commands`
lists them.
