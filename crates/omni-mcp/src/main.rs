//! The `omni-mcp` binary: an MCP server over stdio.
//!
//! It reads JSON-RPC requests on stdin and writes responses on stdout, one message per line, until
//! EOF. Configuration comes from the environment (see [`omni_mcp::Config`]) and per-call arguments.
//! Point any MCP-capable agent at it; `README.md` has a `.mcp.json` / `claude mcp add` snippet.

use std::io::{self, BufReader};

use omni_mcp::{mcp, Config, Server};

fn main() {
    // A one-shot `--help`/`--tools` so a human can see what it exposes without speaking JSON-RPC.
    let mut args = std::env::args().skip(1);
    if let Some(flag) = args.next() {
        match flag.as_str() {
            // `--connect <url>`: no server of its own -- a bridge from stdio to the omnidroid app's
            // MCP server (`http://127.0.0.1:<port>/mcp`), for an agent that starts its MCP servers
            // as programs.
            "--connect" => {
                let Some(url) = args.next() else {
                    eprintln!("omni-mcp: --connect needs the app's MCP URL (http://127.0.0.1:<port>/mcp)");
                    std::process::exit(2);
                };
                let stdin = io::stdin();
                let stdout = io::stdout();
                if let Err(e) = omni_mcp::http::bridge(&url, stdin.lock(), stdout.lock()) {
                    eprintln!("omni-mcp: {e}");
                    std::process::exit(1);
                }
                return;
            }
            // `--lab`: the lab's tools alone (no instance or device tools, nothing booted) -- the
            // omnidroid app runs its lab tools here, a process of their own.
            "--lab" => {
                let mut server = Server::with_live(Config::from_env(), Box::new(omni_mcp::server::NoLive));
                let stdin = io::stdin();
                let stdout = io::stdout();
                if let Err(e) = mcp::serve(BufReader::new(stdin.lock()), stdout.lock(), &mut server) {
                    eprintln!("omni-mcp: transport error: {e}");
                    std::process::exit(1);
                }
                return;
            }
            "--help" | "-h" => {
                eprintln!(
                    "omni-mcp: an MCP (stdio) server that drives omnidroid.\n\
                     Speaks JSON-RPC 2.0 on stdin/stdout (one message per line).\n\
                     `omni-mcp --connect <url>` is a bridge to the omnidroid app's MCP server instead;\n\
                     `omni-mcp --lab` serves the lab's tools alone (the app's lab).\n\
                     Config via env: OMNI_MCP_APK, OMNI_MCP_ARGS, OMNI_MCP_LINK_ID, OMNI_MCP_GPU,\n\
                     OMNI_MCP_SIZE, OMNI_MCP_DEVICE_RAM_MB, OMNI_MCP_MINUTES, OMNIDROID_BIN, OMNI_MCP_REPO.\n\
                     See crates/omni-mcp/README.md for a .mcp.json snippet."
                );
                return;
            }
            other => {
                eprintln!("omni-mcp: unknown argument {other:?}; it takes none (--help for usage).");
                return;
            }
        }
    }

    let mut server = Server::new(Config::from_env());
    let stdin = io::stdin();
    let stdout = io::stdout();
    if let Err(e) = mcp::serve(BufReader::new(stdin.lock()), stdout.lock(), &mut server) {
        eprintln!("omni-mcp: transport error: {e}");
        std::process::exit(1);
    }
}
