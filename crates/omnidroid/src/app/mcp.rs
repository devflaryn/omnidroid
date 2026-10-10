//! **The app's MCP server** for coding agents: off by default (`mcp_enabled`), and when on, served
//! on `http://127.0.0.1:<mcp_port>/mcp` from the moment the app opens -- so every agent on this
//! computer drives the same pool. The pool's tools are answered without a lock (a `wait_instance`
//! does not hold the others up); the lab's go one at a time through `omni-mcp`'s server.
//!
//! **Registered for the agents** (`mcp register`, and when MCP is turned on): Claude Code
//! (`claude mcp add --transport http --scope user omnidroid <url>`) and Omni Agent's pi
//! (`~/.pi/agent/omni-agent.json` -> `omniMcp`: `omni-mcp --connect <url>`, the stdio bridge).
//! Any other agent takes the URL, or the bridge as a stdio server.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use omni_mcp::json::{self, Json};
use omni_mcp::mcp::{Dispatch, RpcError};
use omni_mcp::Live;

use super::live::PoolLive;

/// What answers the app's MCP requests.
pub struct AppDispatch {
    pool: Arc<omni_pool::Pool>,
    /// The lab's tools (and the protocol's own methods, the tool list).
    lab: Mutex<omni_mcp::Server>,
}

impl AppDispatch {
    #[must_use]
    pub fn new(pool: Arc<omni_pool::Pool>) -> Self {
        let lab = omni_mcp::Server::with_live(omni_mcp::Config::from_env(), Box::new(PoolLive { pool: Arc::clone(&pool) }));
        Self { pool, lab: Mutex::new(lab) }
    }
}

impl Dispatch for &AppDispatch {
    fn handle(&mut self, method: &str, params: &Json) -> Result<Json, RpcError> {
        if method == "tools/call" {
            let name = params.get("name").and_then(Json::as_str).unwrap_or("");
            let empty = Json::Object(Default::default());
            let arguments = params.get("arguments").unwrap_or(&empty);
            // The pool's tools: a PoolLive of this call's own, so none waits for another.
            let mut live = PoolLive { pool: Arc::clone(&self.pool) };
            if let Some(answer) = live.call(name, arguments) {
                self.pool.say(format!("mcp: {name}"));
                return answer.map(omni_mcp::server::text_result);
            }
        }
        self.lab.lock().handle(method, params)
    }
}

/// The server, while it runs.
pub struct McpServer {
    listening: omni_mcp::http::Listening,
}

impl McpServer {
    /// Serve the pool on 127.0.0.1:`port`.
    ///
    /// # Errors
    /// The port is taken.
    pub fn start(pool: Arc<omni_pool::Pool>, port: u16) -> Result<Self, String> {
        let dispatch = Arc::new(AppDispatch::new(pool));
        let listening = omni_mcp::http::serve(port, dispatch).map_err(|e| format!("127.0.0.1:{port}: {e}"))?;
        Ok(Self { listening })
    }

    #[must_use]
    pub fn url(&self) -> String {
        self.listening.url()
    }

    pub fn stop(&self) {
        self.listening.stop();
    }
}

/// `omni-mcp` beside this launcher (the stdio bridge an agent starts).
#[must_use]
pub fn bridge_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) { "omni-mcp.exe" } else { "omni-mcp" };
    std::env::current_exe().ok()?.parent().map(|d| d.join(name)).filter(|p| p.exists())
}

/// Register the server with Claude Code: its `omnidroid` entry (user scope) replaced by this URL.
///
/// # Errors
/// `claude` is not installed, or refused.
pub fn register_claude(url: &str) -> Result<String, String> {
    let claude = if cfg!(windows) { "claude.cmd" } else { "claude" };
    let run = |args: &[&str]| std::process::Command::new(claude).args(args).stdin(std::process::Stdio::null()).output();
    let _ = run(&["mcp", "remove", "--scope", "user", "omnidroid"]);
    let out = run(&["mcp", "add", "--transport", "http", "--scope", "user", "omnidroid", url]).map_err(|e| format!("Claude Code's `{claude}`: {e}"))?;
    if !out.status.success() {
        return Err(format!("claude mcp add: {}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(format!("Claude Code: `omnidroid` -> {url} (user scope; restart a running session to see its tools)"))
}

/// Omni Agent's pi config (`~/.pi/agent/omni-agent.json`).
fn pi_config() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(Path::new(&home).join(".pi").join("agent").join("omni-agent.json"))
}

/// `config`'s `omniMcp` set to the bridge to `url`, the rest of the file kept.
#[must_use]
pub fn with_pi_bridge(config: Option<Json>, bridge: &Path, url: &str) -> Json {
    let mut root = match config {
        Some(Json::Object(m)) => m,
        _ => Default::default(),
    };
    let mut omni = match root.remove("omniMcp") {
        Some(Json::Object(m)) => m,
        _ => Default::default(),
    };
    omni.insert("command".into(), json::s(bridge.display().to_string()));
    omni.insert("args".into(), Json::Array(vec![json::s("--connect"), json::s(url)]));
    omni.insert("enabled".into(), Json::Bool(true));
    root.insert("omniMcp".into(), Json::Object(omni));
    Json::Object(root)
}

/// Register the server with Omni Agent's pi: `omniMcp` runs the bridge to it.
///
/// # Errors
/// No pi config here (Omni Agent not installed), no `omni-mcp` built, or the file not writable.
pub fn register_pi(url: &str) -> Result<String, String> {
    let path = pi_config().ok_or("no home directory")?;
    if !path.exists() {
        return Err(format!("Omni Agent's pi config is not there ({}): install Omni Agent first", path.display()));
    }
    let bridge = bridge_binary().ok_or("omni-mcp is not built beside omnidroid (cargo build --release -p omni-mcp)")?;
    let old = std::fs::read_to_string(&path).ok().and_then(|t| json::parse(&t).ok());
    let new = with_pi_bridge(old, &bridge, url);
    std::fs::write(&path, format!("{}\n", new.to_string())).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("Omni Agent (pi): omniMcp -> {} --connect {url} (restart pi to pick it up)", bridge.display()))
}

/// What to give any other agent.
#[must_use]
pub fn snippet(url: &str) -> Vec<String> {
    let mut lines = vec![format!("any agent: MCP over HTTP at {url}  (JSON: {{\"mcpServers\":{{\"omnidroid\":{{\"type\":\"http\",\"url\":\"{url}\"}}}}}})")];
    if let Some(b) = bridge_binary() {
        lines.push(format!("a stdio-only agent: command {} --connect {url}", b.display()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_s_config_keeps_its_other_keys_and_runs_the_bridge() {
        let old = json::parse(r#"{"model":"x","omniMcp":{"command":"/old/omni-mcp","timeoutSec":600,"env":{"A":"1"}}}"#).unwrap();
        let new = with_pi_bridge(Some(old), Path::new("/bin/omni-mcp"), "http://127.0.0.1:7457/mcp");
        assert_eq!(new.get("model").and_then(Json::as_str), Some("x"));
        let m = new.get("omniMcp").unwrap();
        assert_eq!(m.get("command").and_then(Json::as_str), Some("/bin/omni-mcp"));
        assert_eq!(m.get("args").unwrap().to_string(), r#"["--connect","http://127.0.0.1:7457/mcp"]"#);
        assert_eq!(m.get("timeoutSec").and_then(Json::as_f64), Some(600.0), "kept");
        let fresh = with_pi_bridge(None, Path::new("/b"), "u");
        assert!(fresh.get("omniMcp").is_some());
    }
}
