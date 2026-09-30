//! An MCP (Model Context Protocol) server that lets any MCP-capable agent drive omnidroid.
//!
//! Two families of tools sit behind one stdio JSON-RPC server:
//!
//! * **live** — lifecycle and session tools that boot a real omnidroid instance (through the
//!   `omnidroid` launcher) and read its framebuffer: `start_instance`, `stop_instance`,
//!   `list_instances`, `install_apk`, `launch_app`, `login`, `join_place`, `screenshot`;
//! * **lab** — emulation-layer debugging and dumping over [`omni_debug`], the differentiator:
//!   `lab_load`, `resolve_symbol`, `list_maps`, `read_mem`, `write_mem`, `dump_module`,
//!   `call_function`, `set_breakpoint`, `run_until_stop`, `resume`, `get_registers`, `backtrace`,
//!   `intercept`, `trace_syscalls`, `alloc_data`, `load_code`.
//!
//! Without an account, `start_instance` and `install_apk` act on the host's one **warm device**
//! ([`device`]): Android booted and idle, the APK installed (or reused, by its bytes' hash) and
//! started through the device's control channel -- seconds, not a boot per APK.
//!
//! The transport ([`mcp`]) and the JSON codec ([`json`]) are self-contained; the server ([`server`])
//! holds the configuration and the live/lab state.

pub mod device;
pub mod json;
pub mod mcp;
pub mod server;

pub use server::{Config, Server};
