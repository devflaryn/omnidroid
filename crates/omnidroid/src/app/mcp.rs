//! **The app's MCP server** for coding agents: off by default (`mcp_enabled`), and when on, served
//! on `http://127.0.0.1:<mcp_port>/mcp` from the moment the app opens -- so every agent on this
//! computer drives the same pool. The pool's tools are answered without a lock (a `wait_instance`
//! does not hold the others up); the lab's go one at a time to **`omni-mcp --lab`**, a process of
//! their own started at the first lab call ([`LabChild`]) -- so this executable carries no CPU
//! backend (dynarmic was 3.65 of its 7.8 MB, 2026-10-22). It is found beside this one
//! (`OMNI_MCP_BIN` names another).
//!
//! **Registered for the agents** (`mcp register`, and when MCP is turned on): Claude Code
//! (`claude mcp add --transport http --scope user omnidroid <url>`) and Omni Agent's pi
//! (`~/.pi/agent/omni-agent.json` -> `omniMcp`: `omni-mcp --connect <url>`, the stdio bridge).
//! Any other agent takes the URL, or the bridge as a stdio server.
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;

use parking_lot::Mutex;

use omni_mcp::json::{self, Json};
use omni_mcp::mcp::{Dispatch, RpcError};
use omni_mcp::Live;

use super::live::PoolLive;

/// What answers the app's MCP requests.
pub struct AppDispatch {
    pool: Arc<omni_pool::Pool>,
    /// The lab's tools: `omni-mcp --lab`, once a lab tool is called.
    lab: Mutex<Option<LabChild>>,
}

impl AppDispatch {
    #[must_use]
    pub fn new(pool: Arc<omni_pool::Pool>) -> Self {
        Self { pool, lab: Mutex::new(None) }
    }

    /// A lab tool's call, in `omni-mcp --lab` (started now if it is not running; started again
    /// once if it ended -- its sessions went with it).
    fn lab_call(&self, params: &Json) -> Result<Json, RpcError> {
        let mut lab = self.lab.lock();
        for attempt in 0..2 {
            if lab.is_none() {
                *lab = Some(LabChild::start().map_err(RpcError::server)?);
            }
            match lab.as_mut().expect("started").request("tools/call", params) {
                Ok(answer) => return answer,
                // The process is gone: a new one, once.
                Err(e) => {
                    *lab = None;
                    if attempt == 1 {
                        return Err(RpcError::server(format!("the lab (omni-mcp --lab) ended: {e}")));
                    }
                }
            }
        }
        unreachable!("returned in the loop")
    }
}

impl Dispatch for &AppDispatch {
    fn handle(&mut self, method: &str, params: &Json) -> Result<Json, RpcError> {
        match method {
            "initialize" => Ok(json::obj([
                ("protocolVersion", json::s(omni_mcp::server::PROTOCOL_VERSION)),
                ("capabilities", json::obj([("tools", json::obj([]))])),
                ("serverInfo", json::obj([("name", json::s("omni-mcp")), ("version", json::s(env!("CARGO_PKG_VERSION")))])),
            ])),
            "notifications/initialized" | "initialized" => Ok(Json::Null),
            "ping" => Ok(json::obj([])),
            "tools/list" => {
                let mut tools = PoolLive { pool: Arc::clone(&self.pool) }.tools();
                tools.extend(omni_mcp::server::lab_tools());
                Ok(json::obj([("tools", Json::Array(tools))]))
            }
            "tools/call" => {
                let name = params.get("name").and_then(Json::as_str).unwrap_or("");
                let empty = Json::Object(Default::default());
                let arguments = params.get("arguments").unwrap_or(&empty);
                // The pool's tools: a PoolLive of this call's own, so none waits for another.
                let mut live = PoolLive { pool: Arc::clone(&self.pool) };
                if let Some(answer) = live.call(name, arguments) {
                    self.pool.say(format!("mcp: {name}"));
                    return answer.map(omni_mcp::server::text_result);
                }
                if omni_mcp::server::LIVE_TOOLS.contains(&name) {
                    return Err(RpcError { code: omni_mcp::mcp::code::METHOD_NOT_FOUND, message: format!("no tool named {name}") });
                }
                self.lab_call(params)
            }
            other => Err(RpcError { code: omni_mcp::mcp::code::METHOD_NOT_FOUND, message: format!("no method {other}") }),
        }
    }
}

/// **`omni-mcp --lab`**, the lab's tools in a process of their own, spoken to over its stdio (MCP:
/// one JSON-RPC message a line). Ended with the app.
struct LabChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl LabChild {
    /// Where `omni-mcp` is: `OMNI_MCP_BIN`, else beside this executable.
    fn program() -> PathBuf {
        if let Some(bin) = std::env::var_os("OMNI_MCP_BIN") {
            return PathBuf::from(bin);
        }
        let name = if cfg!(windows) { "omni-mcp.exe" } else { "omni-mcp" };
        std::env::current_exe().map_or_else(|_| PathBuf::from(name), |exe| exe.with_file_name(name))
    }

    fn start() -> Result<Self, String> {
        let program = Self::program();
        let mut child = Command::new(&program)
            .arg("--lab")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("the lab's tools run in {} (`cargo build --release -p omni-mcp`): {e}", program.display()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else { return Err("omni-mcp --lab: no stdio".into()) };
        let mut lab = Self { child, stdin, stdout: BufReader::new(stdout), next_id: 1 };
        let init = json::obj([("protocolVersion", json::s(omni_mcp::server::PROTOCOL_VERSION)), ("capabilities", json::obj([])), ("clientInfo", json::obj([("name", json::s("omnidroid"))]))]);
        lab.request("initialize", &init)?.map_err(|e| e.message)?;
        Ok(lab)
    }

    /// One request and its answer. The outer error: the process is gone (or spoke nonsense).
    fn request(&mut self, method: &str, params: &Json) -> Result<Result<Json, RpcError>, String> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json::obj([("jsonrpc", json::s("2.0")), ("id", Json::Num(id as f64)), ("method", json::s(method)), ("params", params.clone())]);
        writeln!(self.stdin, "{}", message.to_string()).and_then(|()| self.stdin.flush()).map_err(|e| e.to_string())?;
        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Err("its output ended".into());
            }
            let Ok(answer) = json::parse(line.trim()) else { continue };
            if answer.get("id").and_then(Json::as_f64) != Some(id as f64) {
                continue;
            }
            if let Some(error) = answer.get("error") {
                let code = error.get("code").and_then(Json::as_f64).map_or(omni_mcp::mcp::code::INTERNAL_ERROR, |c| c as i64);
                let message = error.get("message").and_then(Json::as_str).unwrap_or("the lab failed").to_string();
                return Ok(Err(RpcError { code, message }));
            }
            return Ok(Ok(answer.get("result").cloned().unwrap_or(Json::Null)));
        }
    }
}

impl Drop for LabChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

    /// The lab's tools listed beside the pool's, and run in `omni-mcp --lab`: a library loaded and
    /// a symbol resolved there, its session kept between calls. Needs `omni-mcp` built beside the
    /// tests (`cargo build --release -p omni-mcp`); skipped without it.
    #[test]
    fn the_lab_runs_in_omni_mcp_and_keeps_its_session() {
        let Some(bin) = std::env::current_exe().ok().and_then(|t| Some(t.parent()?.parent()?.join(if cfg!(windows) { "omni-mcp.exe" } else { "omni-mcp" }))).filter(|b| b.exists()) else {
            eprintln!("omni-mcp not built beside the tests: skipped");
            return;
        };
        std::env::set_var("OMNI_MCP_BIN", &bin);
        let config = omni_pool::Config { warm_on_start: false, ..Default::default() };
        let pool = omni_pool::Pool::start_alone(config, None, PathBuf::from("omnidroid"));
        let app = AppDispatch::new(Arc::clone(&pool));
        let mut d = &app;
        let list = d.handle("tools/list", &Json::Null).expect("listed");
        let names: Vec<String> = match list.get("tools") {
            Some(Json::Array(t)) => t.iter().filter_map(|t| t.get("name").and_then(Json::as_str).map(str::to_string)).collect(),
            _ => Vec::new(),
        };
        for n in ["start_instance", "wait_instance", "lab_load", "call_function", "resolve_symbol"] {
            assert!(names.iter().any(|x| x == n), "{n} listed: {names:?}");
        }
        assert_eq!(names.iter().filter(|n| *n == "start_instance").count(), 1, "the pool's, once");
        let libz = Path::new(env!("CARGO_MANIFEST_DIR")).join("../omni-debug/tests/fixtures/libz.so");
        let call = |d: &mut &AppDispatch, name: &str, args: Json| d.handle("tools/call", &json::obj([("name", json::s(name)), ("arguments", args)]));
        let loaded = call(&mut d, "lab_load", json::obj([("path", json::s(libz.display().to_string()))])).expect("loaded");
        assert!(loaded.to_string().contains("libz"), "{}", loaded.to_string());
        let resolved = call(&mut d, "resolve_symbol", json::obj([("name", json::s("crc32"))])).expect("resolved in the same session");
        assert!(resolved.to_string().contains("address"), "{}", resolved.to_string());
        assert!(call(&mut d, "no_such_tool", json::obj([])).is_err());
        drop(app);
        pool.shutdown(false);
    }

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
