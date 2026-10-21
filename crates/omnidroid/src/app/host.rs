//! **One pool for every window**: the Androids, the instances and the MCP server live in one
//! process of their own, the host (`omnidroid host`, started by the first window, or `omnidroid
//! serve`), and every `omnidroid` window open on this computer is a view of it. A second window
//! shows the same Androids and instances and launches into them: it boots no Android of its own,
//! and never touches what the others run. (Before, each window was a pool of its own and adopted
//! the Androids the other had running -- removing their displays, which closed its apps.)
//!
//! * **Found by a file**: `pool.host` beside the settings (`<app-data>/../pool.host`), holding the
//!   control server's port and the host's pid. The host makes it with `create_new`, so two windows
//!   opening at once still make one host; one that does not answer is stale and replaced.
//! * **Spoken to over HTTP** on 127.0.0.1: JSON-RPC (the MCP server's transport, `omni_mcp::http`),
//!   on a port of its own -- `attach`, `state` (the snapshot, the settings and the log since the
//!   last; without a window's `client`, a look that holds no lease), `line` (a command typed at a
//!   window's prompt, or given to a one-shot command such as `omnidroid restart 3`: [`one_shot`]),
//!   `detach`.
//! * **Kept while a window is open or an agent uses it**: each window's `state` polls renew its
//!   lease ([`LEASE`], two minutes). When the last window quits (or goes silent for that long), the
//!   host still runs while the MCP server serves (`mcp_enabled`) or an instance is alive; once
//!   neither holds, it stops the Androids as `stop_on_exit` says and ends. `omnidroid serve` is a
//!   host that ends with its stdin instead, and windows attach to it as well.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use omni_mcp::json::{self, Json};
use omni_mcp::mcp::{Dispatch, RpcError};
use omni_pool::{Config, DeviceInfo, DeviceState, InstanceInfo, InstanceState, Pool, RESTART_DELAY};

use super::command::{self, Command};
use super::mcp::McpServer;

/// A window not heard from this long is gone (its terminal closed under it). Two minutes: a window
/// that is only suspended, slept or stalled for half a minute must not take the pool with it.
pub const LEASE: Duration = Duration::from_secs(120);
/// A host started for a window that never attaches ends after this long.
const UNCLAIMED: Duration = Duration::from_secs(60);

/// Where the host is written down: `pool.host` beside the settings.
#[must_use]
pub fn host_file() -> PathBuf {
    Config::path().and_then(|p| Some(p.parent()?.join("pool.host"))).unwrap_or_else(|| std::env::temp_dir().join("omnidroid-pool.host"))
}

/// The control URL in a host file's text (`<port> <pid>`).
fn url_in(text: &str) -> Option<String> {
    let port: u16 = text.split_whitespace().next()?.parse().ok()?;
    Some(format!("http://127.0.0.1:{port}{}", omni_mcp::http::PATH))
}

/// One JSON-RPC call to a host: its `result`.
///
/// # Errors
/// The host unreachable, or its error.
pub fn call(url: &str, method: &str, params: Json) -> Result<Json, String> {
    let request = json::obj([("jsonrpc", json::s("2.0")), ("id", Json::Num(1.0)), ("method", json::s(method)), ("params", params)]);
    let body = omni_mcp::http::post(url, &request.to_string())?;
    let answer = json::parse(&body).map_err(|e| format!("the pool's answer: {e}"))?;
    if let Some(e) = answer.get("error") {
        return Err(e.get("message").and_then(Json::as_str).unwrap_or("the pool refused").to_string());
    }
    answer.get("result").cloned().ok_or_else(|| "the pool answered nothing".to_string())
}

/// The running host's control URL, if one answers.
#[must_use]
pub fn running() -> Option<String> {
    let url = url_in(&std::fs::read_to_string(host_file()).ok()?)?;
    call(&url, "hello", Json::Null).ok().map(|_| url)
}

/// The running host's URL -- started (`<exe> host`, detached from this terminal) if there is none.
///
/// # Errors
/// It could not be started, or did not answer within a minute (its log is named).
pub fn ensure(exe: &std::path::Path) -> Result<String, String> {
    if let Some(url) = running() {
        return Ok(url);
    }
    let file = host_file();
    let log_path = file.with_extension("host.log");
    let log = std::fs::File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("host").stdin(std::process::Stdio::null()).stdout(log).stderr(err);
    detach(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(60) {
        if let Some(url) = running() {
            // Reaped by a thread, so it is no zombie of this window's while the host runs on.
            std::thread::spawn(move || child.wait());
            return Ok(url);
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Another window's host won the race: it answers in a moment.
            if let Some(url) = (0..20).find_map(|_| {
                std::thread::sleep(Duration::from_millis(250));
                running()
            }) {
                return Ok(url);
            }
            return Err(format!("the pool ended at once ({status}); see {}", log_path.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("the pool did not answer in a minute; see {}", log_path.display()))
}

/// **A command for the pool from a shell** (`omnidroid restart 3`, `omnidroid autorestart on`): the
/// line a window's prompt sends, sent without a window -- it holds no lease, and starts no pool --
/// and the log lines the pool said doing it, printed. Exit status 2 for a line that is no command,
/// 1 when no pool runs or the pool refused it.
#[must_use]
pub fn one_shot(line: &str) -> ExitCode {
    if let Err(e) = command::parse(line) {
        eprintln!("omnidroid: {e}");
        return ExitCode::from(2);
    }
    let Some(url) = running() else {
        eprintln!("omnidroid: no omnidroid pool runs on this computer (`omnidroid` opens the app, `omnidroid serve` runs it without a screen)");
        return ExitCode::FAILURE;
    };
    match call(&url, "line", json::obj([("line", json::s(line))])) {
        Ok(answer) => {
            for l in answer.get("said").and_then(Json::as_array).unwrap_or_default() {
                println!("{}", l.as_str().unwrap_or_default());
            }
            if answer.get("ok").and_then(Json::as_bool).unwrap_or(false) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("omnidroid: the pool at {url}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A host outlives the window that started it: a process group (Unix) or console (Windows) of its
/// own, so the terminal's Ctrl+C and hang-up are not its.
fn detach(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS
        cmd.creation_flags(0x0000_0200 | 0x0000_0008);
    }
}

/// How a host ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Started for the windows: it ends when the last one has gone.
    Windows,
    /// `omnidroid serve`: MCP on (this port, else the setting's) whatever the settings say, the
    /// log on stdout, until stdin closes.
    Serve(Option<u16>),
}

/// The host's state, shared by its control server and its main loop.
pub struct Host {
    pub pool: Arc<Pool>,
    mcp: Mutex<Option<McpServer>>,
    /// Attached windows: id -> last heard from.
    clients: Mutex<BTreeMap<u64, Instant>>,
    next_client: AtomicU64,
    /// A window has ever attached.
    claimed: AtomicBool,
    started: Instant,
}

impl Host {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Arc<Self> {
        Arc::new(Self { pool, mcp: Mutex::new(None), clients: Mutex::new(BTreeMap::new()), next_client: AtomicU64::new(1), claimed: AtomicBool::new(false), started: Instant::now() })
    }

    /// The MCP server's URL, while it runs.
    #[must_use]
    pub fn mcp_url(&self) -> Option<String> {
        self.mcp.lock().as_ref().map(McpServer::url)
    }

    /// Serve MCP on `port` (stopping it where it ran before).
    pub fn mcp_start(&self, port: u16) {
        let mut mcp = self.mcp.lock();
        if let Some(s) = mcp.take() {
            s.stop();
        }
        match McpServer::start(Arc::clone(&self.pool), port) {
            Ok(s) => {
                self.pool.say(format!("MCP: listening on {} (coding agents: `mcp status`)", s.url()));
                *mcp = Some(s);
            }
            Err(e) => self.pool.say(format!("MCP: {e}")),
        }
    }

    fn mcp_stop(&self) {
        if let Some(s) = self.mcp.lock().take() {
            s.stop();
        }
    }

    /// Windows attached now.
    #[must_use]
    pub fn clients(&self) -> usize {
        self.clients.lock().len()
    }

    /// Forget windows not heard from within [`LEASE`].
    fn reap(&self) {
        let mut clients = self.clients.lock();
        let before = clients.len();
        clients.retain(|_, seen| seen.elapsed() < LEASE);
        let gone = before - clients.len();
        drop(clients);
        if gone > 0 {
            self.pool.say(format!("{gone} window{} stopped answering", if gone == 1 { "" } else { "s" }));
        }
    }

    /// Whether a host started for the windows is done: every window gone, and nothing an agent is
    /// using -- no MCP server serving (`mcp_enabled`) and no instance still alive. Closing the
    /// windows must not end the instances an agent started. Nor while an ended instance's window
    /// still shows its card: the app's quit stops the devices (`stop_on_exit`), which would close
    /// that window seconds after it opened -- the one report of the crash the person may get.
    fn done(&self) -> bool {
        if self.clients() > 0 || !(self.claimed.load(Ordering::Acquire) || self.started.elapsed() > UNCLAIMED) {
            return false;
        }
        if self.mcp_url().is_some() || self.pool.shows_cards() {
            return false;
        }
        let (_, instances) = self.pool.snapshot();
        !instances.iter().any(|i| !matches!(i.state, InstanceState::Ended(_)))
    }

    /// One line typed at a window's prompt (or given to a one-shot command, [`one_shot`]), done
    /// here: its answers go to the log every window shows. False when it was refused at once (a
    /// one-shot command's exit status); what is done on a thread of its own (a launch with a
    /// plugin's options, a device's stop, a shell command) reports there later.
    pub fn line(&self, line: &str) -> bool {
        let pool = Arc::clone(&self.pool);
        match command::parse(line) {
            Err(e) if e.is_empty() => true,
            Err(e) => {
                pool.say(e);
                false
            }
            Ok(Command::Run(apk, n, options)) if options.is_empty() => match pool.launch(&apk, n) {
                Ok(_) => true,
                Err(e) => {
                    pool.say(format!("run: {e}"));
                    false
                }
            },
            // The plugins' hooks run first (a moment, or more): not on the window's call.
            Ok(Command::Run(apk, n, options)) => {
                std::thread::spawn(move || {
                    if let Err(e) = super::plugged::extras(&apk, &options).and_then(|extras| pool.launch_with(&apk, n, extras)) {
                        pool.say(format!("run: {e}"));
                    }
                });
                true
            }
            Ok(Command::Close(ids)) => ids.into_iter().fold(true, |ok, id| match pool.close(id) {
                Ok(()) => ok,
                Err(e) => {
                    pool.say(e);
                    false
                }
            }),
            // An ended instance's window still showing its card closes too (`Pool::close`).
            Ok(Command::CloseAll) => {
                let (_, instances) = pool.snapshot();
                for i in instances.iter().filter(|i| !matches!(i.state, InstanceState::Ended(_)) || i.screenshot.is_some()) {
                    let _ = pool.close(i.id);
                }
                true
            }
            // Every id tried, whatever an earlier one answered (`&`, not a short-circuiting `all`).
            Ok(Command::Restart(ids)) => ids.into_iter().fold(true, |ok, id| restart(&pool, id) & ok),
            // The instances on a window: running, or ended with its card shown. One closed (its window
            // gone) stays closed; one restarting already keeps its countdown.
            Ok(Command::RestartAll) => {
                let (_, instances) = pool.snapshot();
                let on_windows: Vec<u32> = instances.iter().filter(|i| i.state == InstanceState::Running || (matches!(i.state, InstanceState::Ended(_)) && i.screenshot.is_some())).map(|i| i.id).collect();
                if on_windows.is_empty() {
                    pool.say("restart all: no instance runs or shows its card");
                }
                on_windows.into_iter().fold(true, |ok, id| restart(&pool, id) & ok)
            }
            Ok(Command::AutoRestart(on, ids)) if ids.is_empty() => match pool.set_auto_restart(None, on) {
                Ok(()) => true,
                Err(e) => {
                    pool.say(e);
                    false
                }
            },
            Ok(Command::AutoRestart(on, ids)) => ids.into_iter().fold(true, |ok, id| match pool.set_auto_restart(Some(id), on) {
                Ok(()) => ok,
                Err(e) => {
                    pool.say(e);
                    false
                }
            }),
            Ok(Command::AutoRestartStatus) => {
                let on = pool.config().auto_restart;
                pool.say(format!("auto restart {} for every instance without its own (`autorestart on|off [#id...]`)", if on { "on" } else { "off" }));
                let (_, instances) = pool.snapshot();
                for i in instances.iter().filter(|i| i.auto_restart != on && !matches!(i.state, InstanceState::Ended(_))) {
                    pool.say(format!("#{}: auto restart {} (its own)", i.id, if i.auto_restart { "on" } else { "off" }));
                }
                true
            }
            Ok(Command::Warm) => {
                let id = pool.warm();
                pool.say(format!("D{id}: warming"));
                true
            }
            Ok(Command::Kill(ids)) => {
                for id in ids {
                    let pool = Arc::clone(&pool);
                    std::thread::spawn(move || {
                        if let Err(e) = pool.kill(id) {
                            pool.say(e);
                        }
                    });
                }
                true
            }
            Ok(Command::KillAll) => {
                let (devices, _) = pool.snapshot();
                for d in devices {
                    let pool = Arc::clone(&pool);
                    std::thread::spawn(move || pool.kill(d.id));
                }
                true
            }
            Ok(Command::Shell(id, cmd)) => match pool.device(id) {
                Some(dev) => {
                    std::thread::spawn(move || match dev.shell_as(&cmd, Some(0), Duration::from_secs(120)) {
                        Ok((code, out)) => {
                            for l in out.lines().take(60) {
                                pool.say(format!("D{id}> {l}"));
                            }
                            pool.say(format!("D{id}: exit {code}"));
                        }
                        Err(e) => pool.say(format!("D{id}: {e}")),
                    });
                    true
                }
                None => {
                    pool.say(format!("no Android D{id}"));
                    false
                }
            },
            Ok(Command::Plugins(args)) => match crate::plugins_lines(&args) {
                Ok(lines) => {
                    lines.into_iter().for_each(|l| pool.say(l));
                    true
                }
                Err(e) => {
                    pool.say(format!("plugins: {e}"));
                    false
                }
            },
            Ok(Command::Settings) => {
                let c = pool.config();
                for (key, what) in omni_pool::config::KEYS {
                    pool.say(format!("{key} = {}   ({what})", c.get(key).unwrap_or_default()));
                }
                true
            }
            Ok(Command::Set(key, value)) => match pool.set(&key, &value) {
                Ok(()) => {
                    if key.starts_with("mcp_") {
                        self.mcp_follow();
                    }
                    true
                }
                Err(e) => {
                    pool.say(e);
                    false
                }
            },
            Ok(Command::Mcp(what)) => {
                self.mcp_command(&what);
                true
            }
            Ok(Command::Clear) => {
                pool.clear_ended();
                true
            }
            // A window's own.
            Ok(Command::Help | Command::Quit) => true,
        }
    }

    /// The server as the settings say: started, stopped or moved.
    fn mcp_follow(&self) {
        let c = self.pool.config();
        if c.mcp_enabled {
            self.mcp_start(c.mcp_port);
        } else {
            self.mcp_stop();
            self.pool.say("MCP: off");
        }
    }

    fn mcp_command(&self, what: &str) {
        let pool = &self.pool;
        match what {
            "on" => {
                if let Err(e) = pool.set("mcp_enabled", "true") {
                    return pool.say(e);
                }
                self.mcp_follow();
                self.mcp_command("register");
            }
            "off" => {
                if let Err(e) = pool.set("mcp_enabled", "false") {
                    return pool.say(e);
                }
                self.mcp_follow();
            }
            "register" => match self.mcp_url() {
                Some(url) => {
                    for r in [super::mcp::register_claude(&url), super::mcp::register_pi(&url)] {
                        pool.say(match r {
                            Ok(m) => m,
                            Err(e) => format!("not registered: {e}"),
                        });
                    }
                    super::mcp::snippet(&url).into_iter().for_each(|l| pool.say(l));
                }
                None => pool.say("MCP is off: `mcp on` first"),
            },
            "status" => match self.mcp_url() {
                Some(url) => {
                    pool.say(format!("MCP: on, {url}"));
                    super::mcp::snippet(&url).into_iter().for_each(|l| pool.say(l));
                }
                None => pool.say("MCP: off (`mcp on` serves it on 127.0.0.1 and registers it for Claude Code and Omni Agent)"),
            },
            other => pool.say(format!("mcp on | off | register | status, not `{other}`")),
        }
    }

    /// A window's lease renewed (or taken again, if it was given up for silence).
    fn renew(&self, id: u64) {
        self.clients.lock().insert(id, Instant::now());
    }
}

/// `restart #id`: its app started again on its window once its card has counted down
/// ([`RESTART_DELAY`]) -- the pool's log says so (`restart asked: in 5 s`) -- or why not.
fn restart(pool: &Arc<Pool>, id: u32) -> bool {
    match pool.restart(id, RESTART_DELAY) {
        Ok(()) => true,
        Err(e) => {
            pool.say(e);
            false
        }
    }
}

fn secs(d: Duration) -> Json {
    Json::Num(d.as_secs_f64())
}

fn device_state_text(s: &DeviceState) -> String {
    match s {
        DeviceState::Booting => "booting".into(),
        DeviceState::Ready => "ready".into(),
        DeviceState::Closing => "closing".into(),
        DeviceState::Gone(why) => format!("gone:{why}"),
    }
}

fn instance_state_text(s: &InstanceState) -> String {
    match s {
        InstanceState::Waiting => "waiting".into(),
        InstanceState::Installing => "installing".into(),
        InstanceState::Starting => "starting".into(),
        InstanceState::Running => "running".into(),
        InstanceState::Closing => "closing".into(),
        InstanceState::Restarting => "restarting".into(),
        InstanceState::Ended(why) => format!("ended:{why}"),
    }
}

fn device_json(d: &DeviceInfo) -> Json {
    json::obj([
        ("id", Json::Num(f64::from(d.id))),
        ("dir", json::s(d.dir.display().to_string())),
        ("state", json::s(device_state_text(&d.state))),
        ("up", secs(d.up_for)),
        ("idle", d.idle_for.map_or(Json::Null, secs)),
        ("installed", Json::Object(d.installed.iter().map(|(k, v)| (k.clone(), json::s(v.clone()))).collect())),
        ("instances", Json::Num(d.instances as f64)),
    ])
}

fn instance_json(i: &InstanceInfo) -> Json {
    let opt = |v: Option<f64>| v.map_or(Json::Null, Json::Num);
    json::obj([
        ("id", Json::Num(f64::from(i.id))),
        ("device", Json::Num(f64::from(i.device))),
        ("package", json::s(i.package.clone())),
        ("sha256", json::s(i.sha256.clone())),
        ("apk", json::s(i.apk.display().to_string())),
        ("user", opt(i.user.map(f64::from))),
        ("display", opt(i.display.map(f64::from))),
        ("screenshot", i.screenshot.as_ref().map_or(Json::Null, |p| json::s(p.display().to_string()))),
        ("state", json::s(instance_state_text(&i.state))),
        ("up", secs(i.up_for)),
        ("details", Json::Array(i.details.iter().cloned().map(json::s).collect())),
        ("auto_restart", Json::Bool(i.auto_restart)),
        ("restarts", Json::Num(f64::from(i.restarts))),
        ("restart_in", i.restart_in.map_or(Json::Null, secs)),
    ])
}

/// The pool as a window sees it, from a `state` answer.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub devices: Vec<DeviceInfo>,
    pub instances: Vec<InstanceInfo>,
    pub config: Config,
    pub mcp: Option<String>,
    /// Windows attached (this one too).
    pub windows: usize,
}

fn duration(v: Option<&Json>) -> Option<Duration> {
    v.and_then(Json::as_f64).filter(|s| s.is_finite() && *s >= 0.0).map(Duration::from_secs_f64)
}

fn text(v: &Json, key: &str) -> String {
    v.get(key).and_then(Json::as_str).unwrap_or_default().to_string()
}

fn num(v: &Json, key: &str) -> Option<f64> {
    v.get(key).and_then(Json::as_f64)
}

fn device_from(v: &Json) -> DeviceInfo {
    let state = text(v, "state");
    DeviceInfo {
        id: num(v, "id").unwrap_or(0.0) as u32,
        dir: PathBuf::from(text(v, "dir")),
        state: match state.as_str() {
            "booting" => DeviceState::Booting,
            "ready" => DeviceState::Ready,
            "closing" => DeviceState::Closing,
            s => DeviceState::Gone(s.strip_prefix("gone:").unwrap_or(s).to_string()),
        },
        up_for: duration(v.get("up")).unwrap_or_default(),
        idle_for: duration(v.get("idle")),
        installed: v.get("installed").and_then(Json::as_object).map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default(),
        instances: num(v, "instances").unwrap_or(0.0) as usize,
    }
}

fn instance_from(v: &Json) -> InstanceInfo {
    let state = text(v, "state");
    InstanceInfo {
        id: num(v, "id").unwrap_or(0.0) as u32,
        device: num(v, "device").unwrap_or(0.0) as u32,
        package: text(v, "package"),
        sha256: text(v, "sha256"),
        apk: PathBuf::from(text(v, "apk")),
        user: num(v, "user").map(|u| u as u32),
        display: num(v, "display").map(|d| d as i32),
        screenshot: v.get("screenshot").and_then(Json::as_str).map(PathBuf::from),
        state: match state.as_str() {
            "waiting" => InstanceState::Waiting,
            "installing" => InstanceState::Installing,
            "starting" => InstanceState::Starting,
            "running" => InstanceState::Running,
            "closing" => InstanceState::Closing,
            "restarting" => InstanceState::Restarting,
            s => InstanceState::Ended(s.strip_prefix("ended:").unwrap_or(s).to_string()),
        },
        up_for: duration(v.get("up")).unwrap_or_default(),
        details: v.get("details").and_then(Json::as_array).unwrap_or_default().iter().filter_map(|l| l.as_str().map(str::to_string)).collect(),
        auto_restart: v.get("auto_restart").and_then(Json::as_bool).unwrap_or(false),
        restarts: num(v, "restarts").unwrap_or(0.0) as u32,
        restart_in: duration(v.get("restart_in")),
    }
}

/// A `state` answer read back.
#[must_use]
pub fn view_from(v: &Json) -> View {
    let mut config = Config::default();
    if let Some(m) = v.get("config").and_then(Json::as_object) {
        for (k, val) in m {
            let _ = config.set(k, val.as_str().unwrap_or_default());
        }
    }
    View {
        devices: v.get("devices").and_then(Json::as_array).unwrap_or_default().iter().map(device_from).collect(),
        instances: v.get("instances").and_then(Json::as_array).unwrap_or_default().iter().map(instance_from).collect(),
        config,
        mcp: v.get("mcp").and_then(Json::as_str).map(str::to_string),
        windows: num(v, "windows").unwrap_or(1.0) as usize,
    }
}

fn client_id(params: &Json) -> Result<u64, RpcError> {
    params.get("client").and_then(Json::as_f64).map(|f| f as u64).ok_or_else(|| RpcError::params("need `client`"))
}

impl Dispatch for &Host {
    fn handle(&mut self, method: &str, params: &Json) -> Result<Json, RpcError> {
        match method {
            "hello" => Ok(json::obj([("pid", Json::Num(f64::from(std::process::id()))), ("version", json::s(env!("CARGO_PKG_VERSION")))])),
            "attach" => {
                let id = self.next_client.fetch_add(1, Ordering::Relaxed);
                self.renew(id);
                self.claimed.store(true, Ordering::Release);
                let n = self.clients();
                if n > 1 {
                    self.pool.say(format!("another window joined ({n} open): they share these Androids"));
                }
                Ok(json::obj([("client", Json::Num(id as f64))]))
            }
            "state" => {
                // Without a window's id: a look (a test's, a script's) that holds no lease.
                if let Ok(id) = client_id(params) {
                    self.renew(id);
                }
                let seen = params.get("seen").and_then(Json::as_f64).unwrap_or(0.0) as u64;
                let (devices, instances) = self.pool.snapshot();
                let (log, logged) = self.pool.log_since(seen);
                let c = self.pool.config();
                Ok(json::obj([
                    ("devices", Json::Array(devices.iter().map(device_json).collect())),
                    ("instances", Json::Array(instances.iter().map(instance_json).collect())),
                    ("config", Json::Object(omni_pool::config::KEYS.iter().map(|(k, _)| ((*k).to_string(), json::s(c.get(k).unwrap_or_default()))).collect())),
                    ("mcp", self.mcp_url().map_or(Json::Null, json::s)),
                    ("windows", Json::Num(self.clients() as f64)),
                    ("log", Json::Array(log.into_iter().map(json::s).collect())),
                    ("logged", Json::Num(logged as f64)),
                ]))
            }
            // Without a window's id: a one-shot command's ([`one_shot`]), holding no lease. Answers
            // whether it was done and the log lines said meanwhile (another's among them, at times).
            "line" => {
                if let Ok(id) = client_id(params) {
                    self.renew(id);
                }
                let line = params.get("line").and_then(Json::as_str).ok_or_else(|| RpcError::params("need `line`"))?;
                let (_, before) = self.pool.log_since(u64::MAX);
                let ok = self.line(line);
                let (said, _) = self.pool.log_since(before);
                Ok(json::obj([("ok", Json::Bool(ok)), ("said", Json::Array(said.into_iter().map(json::s).collect()))]))
            }
            "detach" => {
                let id = client_id(params)?;
                self.clients.lock().remove(&id);
                Ok(json::obj([("windows", Json::Num(self.clients() as f64))]))
            }
            other => Err(RpcError::params(format!("no method {other}"))),
        }
    }
}

/// Take the host file: `Ok(file)` when this process is the host now, `Err(Some(url))` when
/// another host answers.
fn claim(path: &std::path::Path) -> Result<std::fs::File, Option<String>> {
    for _ in 0..3 {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(f) => return Ok(f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(url) = running() {
                    return Err(Some(url));
                }
                // Being written by a host just starting, or left by one that ended: wait a moment
                // for the first, then take the place of the second.
                let young = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a < Duration::from_secs(5));
                if young {
                    std::thread::sleep(Duration::from_secs(2));
                    if let Some(url) = running() {
                        return Err(Some(url));
                    }
                }
                let _ = std::fs::remove_file(path);
            }
            Err(_) => return Err(None),
        }
    }
    Err(None)
}

/// Be the host: the pool, its MCP server, the control server, until [`Mode`] says it ends.
#[must_use]
pub fn run(mode: Mode) -> ExitCode {
    let file = host_file();
    let mut claimed = match claim(&file) {
        Ok(f) => f,
        Err(Some(url)) => {
            // Not an error for a window's host (another window's won the race); one for `serve`.
            eprintln!("omnidroid: the pool already runs ({url}): every window shares it");
            return if mode == Mode::Windows { ExitCode::SUCCESS } else { ExitCode::FAILURE };
        }
        Err(None) => {
            eprintln!("omnidroid: cannot write {}", file.display());
            return ExitCode::FAILURE;
        }
    };
    let path = Config::path();
    let (config, problems) = path.as_deref().map(Config::load).unwrap_or_default();
    let Ok(exe) = std::env::current_exe() else {
        let _ = std::fs::remove_file(&file);
        eprintln!("omnidroid: where this program is unknown");
        return ExitCode::FAILURE;
    };
    let pool = Pool::start(config.clone(), path.clone(), exe);
    pool.say(format!("omnidroid {} -- settings in {}", env!("CARGO_PKG_VERSION"), path.as_ref().map_or_else(|| "(none)".to_string(), |p| p.display().to_string())));
    for p in problems {
        pool.say(format!("settings: {p}"));
    }
    let host = Host::new(Arc::clone(&pool));
    match mode {
        Mode::Serve(port) => host.mcp_start(port.unwrap_or(config.mcp_port)),
        Mode::Windows if config.mcp_enabled => host.mcp_start(config.mcp_port),
        Mode::Windows => {}
    }
    let control = match omni_mcp::http::serve(0, Arc::clone(&host)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("omnidroid: the pool's control server: {e}");
            let _ = std::fs::remove_file(&file);
            pool.shutdown(false);
            return ExitCode::FAILURE;
        }
    };
    {
        use std::io::Write;
        let _ = writeln!(claimed, "{} {}", control.port, std::process::id());
        let _ = claimed.sync_all();
    }
    drop(claimed);
    if let Mode::Serve(_) = mode {
        match host.mcp_url() {
            Some(url) => println!("omnidroid serve: MCP on {url} (until stdin closes)"),
            None => {
                eprintln!("omnidroid: MCP could not start (see the log)");
            }
        }
    }
    let stdin_closed = Arc::new(AtomicBool::new(false));
    if let Mode::Serve(_) = mode {
        let closed = Arc::clone(&stdin_closed);
        std::thread::spawn(move || {
            let mut sink = String::new();
            while std::io::stdin().read_line(&mut sink).is_ok_and(|n| n > 0) {
                sink.clear();
            }
            closed.store(true, Ordering::Release);
        });
    }
    let mut seen = 0;
    let mut tick = 0u64;
    loop {
        let (lines, total) = pool.log_since(seen);
        for line in lines {
            println!("{line}");
        }
        seen = total;
        tick += 1;
        if tick % 4 == 0 {
            host.reap();
        }
        let over = match mode {
            Mode::Serve(_) => stdin_closed.load(Ordering::Acquire),
            Mode::Windows => host.done(),
        };
        // The file gone (deleted by hand) or another host's: no window could find this one.
        let ours = std::fs::read_to_string(&file).ok().is_some_and(|t| url_in(&t).is_some_and(|u| u.contains(&format!(":{}/", control.port))));
        if over || !ours {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    host.mcp_stop();
    let stop = pool.config().stop_on_exit;
    let why = match mode {
        Mode::Serve(_) => "the server's input closed",
        Mode::Windows => "the last window quit",
    };
    pool.say(if stop { format!("{why}: stopping the Androids") } else { format!("{why}: the Androids keep running (stop_on_exit = false)") });
    pool.shutdown(stop);
    if std::fs::read_to_string(&file).ok().is_some_and(|t| url_in(&t).is_some_and(|u| u.contains(&format!(":{}/", control.port)))) {
        let _ = std::fs::remove_file(&file);
    }
    control.stop();
    let (lines, _) = pool.log_since(seen);
    for line in lines {
        println!("{line}");
    }
    println!("omnidroid: the pool stopped");
    ExitCode::SUCCESS
}

/// A window's link to the host.
pub struct Client {
    url: String,
    id: u64,
    /// Log lines the host has logged in all, as of the last `state`.
    seen: u64,
    /// The log this window shows: the host's lines and its own, in the order they came.
    pub log: Vec<String>,
}

/// How many log lines a window keeps.
const LOG_KEPT: usize = 400;

impl Client {
    /// Attach to the host at `url`.
    ///
    /// # Errors
    /// The host does not answer.
    pub fn attach(url: &str) -> Result<Self, String> {
        let r = call(url, "attach", Json::Null)?;
        let id = r.get("client").and_then(Json::as_f64).ok_or("the pool gave no window id")? as u64;
        Ok(Self { url: url.to_string(), id, seen: 0, log: Vec::new() })
    }

    /// A line of this window's own in its log.
    pub fn say(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        let over = self.log.len().saturating_sub(LOG_KEPT);
        self.log.drain(..over);
    }

    /// The pool now (and the log lines since the last call, kept).
    ///
    /// # Errors
    /// The host does not answer.
    pub fn state(&mut self) -> Result<View, String> {
        let r = call(&self.url, "state", json::obj([("client", Json::Num(self.id as f64)), ("seen", Json::Num(self.seen as f64))]))?;
        for line in r.get("log").and_then(Json::as_array).unwrap_or_default() {
            self.say(line.as_str().unwrap_or_default());
        }
        self.seen = r.get("logged").and_then(Json::as_f64).map_or(self.seen, |n| n as u64);
        Ok(view_from(&r))
    }

    /// A command line for the host.
    ///
    /// # Errors
    /// The host does not answer.
    pub fn line(&self, line: &str) -> Result<(), String> {
        call(&self.url, "line", json::obj([("client", Json::Num(self.id as f64)), ("line", json::s(line))])).map(|_| ())
    }

    /// Leave: the windows still attached.
    ///
    /// # Errors
    /// The host does not answer.
    pub fn detach(&self) -> Result<usize, String> {
        let r = call(&self.url, "detach", json::obj([("client", Json::Num(self.id as f64))]))?;
        Ok(r.get("windows").and_then(Json::as_f64).unwrap_or(0.0) as usize)
    }

    /// Wait (at most `limit`) until the host has ended.
    pub fn wait_gone(&self, limit: Duration) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit {
            if call(&self.url, "hello", Json::Null).is_err() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool adopting none of the devices left running: the person's own pool keeps its devices,
    /// and its apps' windows stay open.
    fn test_pool() -> Arc<Pool> {
        let mut config = Config::default();
        config.warm_on_start = false;
        Pool::start_alone(config, None, PathBuf::from("omnidroid"))
    }

    #[test]
    fn two_windows_see_one_pool_and_its_log() {
        let host = Host::new(test_pool());
        let control = omni_mcp::http::serve(0, Arc::clone(&host)).unwrap();
        let url = format!("http://127.0.0.1:{}{}", control.port, omni_mcp::http::PATH);
        let mut a = Client::attach(&url).unwrap();
        let mut b = Client::attach(&url).unwrap();
        assert_eq!(a.state().unwrap().windows, 2);
        a.line("set keep_warm 3").unwrap();
        let view = b.state().unwrap();
        assert_eq!(view.config.keep_warm, 3, "one pool's settings");
        assert!(b.log.iter().any(|l| l.contains("setting keep_warm = 3")), "{:?}", b.log);
        assert!(b.log.iter().any(|l| l.contains("another window joined")));
        assert_eq!(b.detach().unwrap(), 1);
        assert!(!host.done(), "a window is still open");
        assert_eq!(a.detach().unwrap(), 0);
        assert!(host.done(), "the last window gone: the host ends");
        control.stop();
        host.pool.shutdown(false);
    }

    #[test]
    fn restart_and_auto_restart_from_a_window_and_from_a_shell() {
        let host = Host::new(test_pool());
        let control = omni_mcp::http::serve(0, Arc::clone(&host)).unwrap();
        let url = format!("http://127.0.0.1:{}{}", control.port, omni_mcp::http::PATH);
        let mut a = Client::attach(&url).unwrap();
        a.line("autorestart on").unwrap();
        assert!(a.state().unwrap().config.auto_restart, "no ids: the pool's setting");
        a.line("set auto_restart off").unwrap();
        assert!(!a.state().unwrap().config.auto_restart, "the same setting, by its key");
        // A one-shot command's line: no window's id; what the pool said doing it answered.
        let one_shot = |line: &str| call(&url, "line", json::obj([("line", json::s(line))])).unwrap();
        let said = |r: &Json| r.get("said").and_then(Json::as_array).unwrap_or_default().iter().filter_map(|l| l.as_str().map(str::to_string)).collect::<Vec<_>>();
        let r = one_shot("restart #9");
        assert_eq!(r.get("ok").and_then(Json::as_bool), Some(false), "no such instance");
        assert!(said(&r).iter().any(|l| l.contains("#9")), "{:?}", said(&r));
        let r = one_shot("autorestart off 9");
        assert_eq!(r.get("ok").and_then(Json::as_bool), Some(false));
        let r = one_shot("restart all");
        assert_eq!(r.get("ok").and_then(Json::as_bool), Some(true), "nothing to restart is no refusal");
        assert!(said(&r).iter().any(|l| l.contains("no instance runs")), "{:?}", said(&r));
        let r = one_shot("autorestart");
        assert!(said(&r).iter().any(|l| l.contains("auto restart off for every instance")), "{:?}", said(&r));
        assert_eq!(a.state().unwrap().windows, 1, "a one-shot command is no window");
        assert_eq!(a.detach().unwrap(), 0);
        control.stop();
        host.pool.shutdown(false);
    }

    #[test]
    fn states_read_back_as_they_were() {
        let d = DeviceInfo { id: 2, dir: PathBuf::from("/x/omni-warm-1"), state: DeviceState::Gone("boot: no".into()), up_for: Duration::from_secs(5), idle_for: None, installed: [("a.b".to_string(), "ff".to_string())].into(), instances: 1 };
        let back = device_from(&json::parse(&device_json(&d).to_string()).unwrap());
        assert_eq!((back.id, back.state, back.installed, back.idle_for, back.dir), (2, d.state.clone(), d.installed.clone(), None, d.dir.clone()));
        let i = InstanceInfo { id: 7, device: 2, package: "p".into(), sha256: "s".into(), apk: PathBuf::from("/a.apk"), user: Some(10), display: Some(3), screenshot: None, state: InstanceState::Ended("native crash: SIGSEGV".into()), up_for: Duration::from_millis(1500), details: vec!["SIGSEGV at 0x8".into(), String::new()], auto_restart: true, restarts: 2, restart_in: None };
        let back = instance_from(&json::parse(&instance_json(&i).to_string()).unwrap());
        assert_eq!((back.id, back.user, back.display, back.state, back.up_for), (7, Some(10), Some(3), i.state.clone(), Duration::from_millis(1500)));
        assert_eq!(back.details, i.details, "what happened, for every window");
        assert_eq!((back.auto_restart, back.restarts, back.restart_in), (true, 2, None));
        let i = InstanceInfo { state: InstanceState::Restarting, restart_in: Some(Duration::from_secs(3)), ..i };
        let back = instance_from(&json::parse(&instance_json(&i).to_string()).unwrap());
        assert_eq!((back.state, back.restart_in), (InstanceState::Restarting, Some(Duration::from_secs(3))), "a restart's countdown");
    }

    #[test]
    fn a_host_file_names_its_control_url() {
        assert_eq!(url_in("51234 999\n").as_deref(), Some("http://127.0.0.1:51234/mcp"));
        assert_eq!(url_in(""), None);
    }
}
