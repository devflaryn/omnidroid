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
| `OMNIDROID_BIN` | the `omnidroid` launcher (default: sibling of `omni-mcp`) |
| `OMNI_MCP_REPO` | repo dir to run the launcher in (its `aosp` subcommand runs `cargo test`) |

## Tools

### Lifecycle & session (a live instance)

| Tool | What it does |
|---|---|
| `start_instance` | boot omnidroid for an APK (installs, launches, optional login + join); long-running |
| `stop_instance` | stop a running instance |
| `list_instances` | list instances and whether each is running |
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
