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
            "--help" | "-h" => {
                eprintln!(
                    "omni-mcp: an MCP (stdio) server that drives omnidroid.\n\
                     Speaks JSON-RPC 2.0 on stdin/stdout (one message per line).\n\
                     Config via env: OMNI_MCP_APK, OMNI_MCP_COOKIE, OMNI_MCP_PLACE, OMNI_MCP_GPU,\n\
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
