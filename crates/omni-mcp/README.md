# omni-mcp — drive omnidroid from any MCP agent

`omni-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server, over stdio, that
lets any MCP-capable agent (Claude Code, Claude Desktop, or your own) drive omnidroid: boot a real
instance, take screenshots, and — the differentiator — **debug and dump guest ARM64 libraries at the
emulation layer**: set breakpoints, intercept functions, call a function directly with crafted
inputs, and dump a decrypted/unpacked `.so` out of guest memory. Because the guest is always ARM64
regardless of the host CPU, every debug tool behaves identically on Windows, Linux and macOS, needs
no `ptrace`, and is invisible to the target.

## Build

```
cargo build --release -p omni-mcp        # target/release/omni-mcp[.exe]
```

On Windows set `OMNIDROID_DYNARMIC_BUILD_DIR` first (see the repo's build notes).

## Wire it into an agent

### Claude Code

```
claude mcp add omnidroid -- /ABSOLUTE/PATH/to/target/release/omni-mcp
```

### `.mcp.json` (Claude Desktop, or a project `.mcp.json`)

```json
{
  "mcpServers": {
    "omnidroid": {
      "command": "/ABSOLUTE/PATH/to/target/release/omni-mcp",
      "env": {
        "OMNI_MCP_APK": "/ABSOLUTE/PATH/to/Roblox-2.740.931.apk",
        "OMNI_MCP_COOKIE": "/ABSOLUTE/PATH/to/cookies/account.txt",
        "OMNI_MCP_PLACE": "8737899170",
        "OMNI_MCP_GPU": "auto",
        "OMNI_MCP_STANDBY": "1",
        "OMNIDROID_BIN": "/ABSOLUTE/PATH/to/target/release/omnidroid",
        "OMNI_MCP_REPO": "/ABSOLUTE/PATH/to/omnidroid"
      }
    }
  }
}
```

All of these are optional and overridable per tool call. On Windows use `omni-mcp.exe` /
`omnidroid.exe` and Windows paths.

## Configuration (env, all optional)

| Variable | Meaning |
|---|---|
| `OMNI_MCP_APK` | default APK to boot |
| `OMNI_MCP_COOKIE` | cookie file path or saved account name |
| `OMNI_MCP_PLACE` | place id to join (e.g. `8737899170`) |
| `OMNI_MCP_GPU` | `vulkan` \| `gl` \| `auto` |
| `OMNI_MCP_SIZE` | window size `WxH` |
| `OMNI_MCP_DEVICE_RAM_MB` | device RAM (passed through as `OMNI_DEVICE_RAM_MB`) |
| `OMNI_MCP_MINUTES` | minutes to run |
| `OMNI_MCP_STANDBY` | `1`: keep a standby instance (below) |
| `OMNI_MCP_STANDBY_MINUTES` | how long a standby instance lives (default 720) |
| `OMNIDROID_BIN` | the `omnidroid` launcher (default: sibling of `omni-mcp`) |
| `OMNI_MCP_REPO` | repo dir to run the launcher in (its `aosp` subcommand runs `cargo test`) |

## Fast starts: the saved device and the standby instance

A device's first boot is slow (~8 min to PS99 on the 2026-09-30 Windows host): Android boots new,
the APK is installed and compiled, the app starts, the cookie is planted, the app starts again.
Two things take that off the agent's clock:

- **The saved device** (`omnidroid aosp`, always; `--fresh-device` makes a new one). The first
  session for an APK and account saves its device once signed in, under
  `<temp>/omni-golden/<apk>-<size>-<account>-...`; every later one boots a copy of it and opens the
  place's link at once -- no first boot, no install, no cookie planted. A new cookie file (another
  modification time) or APK is another device. A saved device whose app never signs in (a cookie
  Roblox has ended) is set aside after 10 minutes.
- **The standby instance** (`OMNI_MCP_STANDBY=1`). When a client connects, the server boots one
  instance from the saved device with the configured APK, account and place, and leaves it waiting
  -- in the place. `start_instance` with the same APK and account takes it over: already in the
  place (nothing to wait for), or the place asked for is joined (seconds to "Joining game", the
  place's own load after that). It is started detached and outlives the server; when the server
  exits, a standby it took over is given back still running, for the next session. A standby the
  server disconnects (idle kick, a server shutdown) joins its place again. `stop_instance` on it
  ends it, and a new standby boots. It holds ~4-5 GB of RAM and some CPU while it waits.

Its files are `<temp>/omni-linux-r-standby-<secs>/data/local/tmp/`: `standby` (waiting),
`game-loaded` (the place it is in), `claimed`, `join-place` (write a place id to join it), `stop`.

## Tools

### Lifecycle & session (a live instance)

| Tool | What it does |
|---|---|
| `start_instance` | start an instance (signs in, joins the place); returns at once -- takes over the standby instance when one waits |
| `stop_instance` | stop a running instance (its session shuts the device down and removes it) |
| `list_instances` | each instance's `state`: `booting`, `signed_in`, `joining`, `in_game` (+ `in_place`), `stopped`; and its log |
| `install_apk` | record an APK as the default for the next boot |
| `launch_app` | boot the app (same as `start_instance` on the real-AOSP path) |
| `login` | record a cookie for the next boot |
| `join_place` | record a place id for the next boot |
| `screenshot` | read the latest framebuffer PNG (returns path + dimensions) |

### Emulation-layer debugging & dumping (a lab session)

Load a guest library once with `lab_load`, then:

| Tool | What it does |
|---|---|
| `resolve_symbol` / `list_symbols` | name → guest address |
| `list_maps` | the guest memory map |
| `read_mem` / `write_mem` | read, or force a write into read-only code (COW + invalidate) |
| `dump_module` | dump a module out of guest memory (post-relocation / post-unpack) |
| `call_function` | **call a function directly with crafted inputs** (`X0..X7`) — no game/login/anti-cheat |
| `alloc_data` / `load_code` | place a buffer, or a snippet of A64 code, in guest memory |
| `set_breakpoint` / `run_until_stop` / `resume` | interactive stepping |
| `get_registers` / `backtrace` | inspect at a stop |
| `intercept` | entry/exit hooks, or `replace_return` to skip a body |
| `trace_syscalls` | record every guest `SVC` during a call |

Addresses may be passed as decimal or `0x…` hex **strings** (a 64-bit guest address exceeds a JSON
number's exact-integer range).

## How to call the Frida-style debug tools (one line)

Load a library, then call a function with crafted inputs and read the result — no game needed:

```jsonc
// tools/call lab_load        {"path": "/path/to/lib.so"}
// tools/call call_function   {"symbol": "adler32", "args": [1, "0x40000000", 14]}
// tools/call dump_module     {"name": "lib.so", "out_path": "/tmp/lib.dumped.so"}
// tools/call intercept       {"symbol": "check_license", "replace_return": "0x1"}
```

## Scope

The debug/dump tools are for analysis, dumping and interop of binaries the owner controls. They are
**not** for evading server-side emulator detection or affecting other players.
